//! [`Core`]: the client's public face. It owns the local store, runs the
//! pure [`Model`] and pushes a full [`ViewState`] to subscribers after every
//! change.

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, TryLockError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use chrono::Utc;
use tokio::runtime::Runtime;
use tokio::sync::{mpsc as async_mpsc, watch};

use crate::action::Action;
use crate::blobs::{remove_all_blob_files, remove_blob_files};
use crate::engine::{Engine, EngineConfig};
use crate::identity::{FileIdentity, IdentityError};
use crate::lock::{DataDirLock, LockError};
use crate::model::{Effect, Model, Paired};
use crate::seam::{Request, Seam};
use crate::store::{Store, StoreError};
use crate::view::{BlobId, ViewState};

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Identity(#[from] IdentityError),
    /// The data dir is locked, most likely by another client using it.
    #[error(transparent)]
    Lock(#[from] LockError),
    #[error("cannot start the sync engine: {0}")]
    Runtime(std::io::Error),
    /// The device paired with another server since this engine-side
    /// handle was bound ([`Seam::bind`]): what it would write belongs to
    /// the old server's replica, which is gone.
    #[error("the device paired with another server; this work belongs to the old one")]
    Superseded,
    /// [`Core::open`] found a replica this build cannot read: written for
    /// another protocol or schema version, or a stored row that no longer
    /// decodes ([`StoreError::unreadable`]). Heads offer
    /// [`Core::reset_replica`].
    #[error("the local data cannot be read ({reason}); reset it to sync again from the server")]
    ReplicaUnreadable { reason: String },
}

impl CoreError {
    /// A store error, as [`CoreError::ReplicaUnreadable`] when it says the
    /// replica cannot be read.
    fn opening(err: StoreError) -> CoreError {
        if err.unreadable() {
            CoreError::ReplicaUnreadable {
                reason: err.to_string(),
            }
        } else {
            CoreError::Store(err)
        }
    }
}

/// Why [`Core::shutdown`] could not close the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ShutdownError {
    /// Called on a thread of an async runtime (an engine thread, say),
    /// where it cannot block. The core was dropped instead: the engine
    /// stops in the background.
    #[error("cannot shut the core down from inside an async runtime")]
    InsideRuntime,
    /// The engine is stopped, but another handle still holds the store —
    /// a clone of the core, a [`Seam`], or a delivery thread still inside
    /// a callback — so it closes when that goes.
    #[error("the core is still in use elsewhere; its store stays open")]
    InUse,
}

/// Handle returned by [`Core::subscribe`], for [`Core::unsubscribe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubscriptionId(u64);

type Callback = Box<dyn FnMut(ViewState) + Send>;

/// The client core. Cheap to clone; every clone is the same core. The sync
/// engine stops when the last clone is dropped, in the background; use
/// [`shutdown`](Core::shutdown) to wait for it.
#[derive(Clone)]
pub struct Core {
    pub(crate) shared: Arc<Shared>,
    /// `None` for a detached core, and in the engine's own handle (so the
    /// engine does not keep itself alive).
    engine: Option<Arc<EngineHandle>>,
}

pub(crate) struct Shared {
    pub(crate) data_dir: PathBuf,
    pub(crate) state: Mutex<State>,
    requests: Requests,
    /// A detached core's receiver, for [`Seam::take_requests`].
    pub(crate) request_rx: Mutex<Option<Receiver<Request>>>,
    delivery: Delivery,
    /// Last, so it is released only once the store above has closed.
    _lock: DataDirLock,
}

/// Where requests go: to the engine, or to whoever took the receiver.
enum Requests {
    Engine(async_mpsc::UnboundedSender<Request>),
    Detached(Sender<Request>),
}

impl Requests {
    fn send(&self, request: Request) {
        // No receiver means nobody plays the engine; the outbox keeps the
        // work for when one starts.
        match self {
            Requests::Engine(tx) => drop(tx.send(request)),
            Requests::Detached(tx) => drop(tx.send(request)),
        }
    }
}

/// The running engine: its runtime, its suspend switch and the thread
/// that delivers snapshots.
struct EngineHandle {
    /// `None` once [`Core::shutdown`] stopped it.
    runtime: Mutex<Option<Runtime>>,
    suspended: watch::Sender<bool>,
    /// `None` once [`Core::shutdown`] stopped it.
    deliverer: Mutex<Option<Deliverer>>,
}

impl Drop for EngineHandle {
    fn drop(&mut self) {
        let deliverer = self
            .deliverer
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(deliverer) = deliverer.take() {
            // Does not wait: this may run inside a callback on that thread.
            deliverer.stop(None);
        }
        let runtime = self
            .runtime
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(runtime) = runtime.take() {
            // Does not block, so it is safe inside another runtime too.
            runtime.shutdown_background();
        }
    }
}

/// An engine core's delivery thread, which alone runs subscriber callbacks
/// while it lives.
struct Deliverer {
    shared: Arc<Shared>,
    thread: JoinHandle<()>,
    /// Disconnects when the thread ends.
    exited: Receiver<()>,
}

impl Deliverer {
    /// Name of the delivery thread, for logs and debuggers.
    const NAME: &str = "skywave-deliver";

    fn start(shared: &Arc<Shared>) -> std::io::Result<Deliverer> {
        lock(&shared.delivery.pending).threaded = true;
        let (alive, exited) = mpsc::channel::<()>();
        let run = shared.clone();
        let spawned = thread::Builder::new()
            .name(Deliverer::NAME.into())
            .spawn(move || {
                let _alive = alive;
                run.deliver_until_stopped();
            });
        match spawned {
            Ok(thread) => Ok(Deliverer {
                shared: shared.clone(),
                thread,
                exited,
            }),
            Err(err) => {
                lock(&shared.delivery.pending).threaded = false;
                Err(err)
            }
        }
    }

    /// Stops the thread: from now on whoever publishes delivers. With a
    /// `timeout`, waits that long for a callback in progress to return,
    /// unless this is the delivery thread itself.
    fn stop(self, timeout: Option<Duration>) {
        lock(&self.shared.delivery.pending).threaded = false;
        self.shared.delivery.wake.notify_all();
        let Some(timeout) = timeout else { return };
        if thread::current().id() == self.thread.thread().id() {
            return;
        }
        if let Err(RecvTimeoutError::Disconnected) = self.exited.recv_timeout(timeout) {
            // A callback's panic was contained, so this is always `Ok`.
            let _ = self.thread.join();
        }
    }
}

pub(crate) struct State {
    pub(crate) store: Store,
    pub(crate) model: Model,
    pub(crate) identity: Option<Arc<FileIdentity>>,
    /// Bumped whenever pairing with another server resets the replica; a
    /// [`Seam`] bound to an older one may no longer write.
    pub(crate) server_epoch: u64,
    /// How often each blob left the replica in this process
    /// ([`State::blobs_purged`]): a download holds the count it read
    /// ([`BlobLease`]), so its reports fail once the blob is purged, even
    /// if a workdir answer registers the same id again meanwhile.
    purges: HashMap<BlobId, u64>,
    /// Bumped for every change, so delivery never goes back in time.
    pub(crate) version: u64,
}

/// Which registration of a blob a download writes for: the blob's purge
/// count when the download read its record ([`Seam`]'s leased reports).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BlobLease(u64);

impl Core {
    /// Opens the core over `data_dir` — the replica and, if the device is
    /// paired, its identity — and starts the sync engine with the default
    /// [`EngineConfig`]. The first snapshot renders from local data alone;
    /// the engine connects in the background once the device is paired.
    pub fn open(data_dir: &Path) -> Result<Core, CoreError> {
        Core::open_with(data_dir, EngineConfig::default())
    }

    /// [`open`](Core::open) with an engine tuned by `config`.
    pub fn open_with(data_dir: &Path, config: EngineConfig) -> Result<Core, CoreError> {
        let (requests, request_rx) = async_mpsc::unbounded_channel();
        let shared = Shared::open(data_dir, Requests::Engine(requests), None)?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("skywave-core")
            .enable_all()
            .build()
            .map_err(CoreError::Runtime)?;
        let deliverer = Deliverer::start(&shared).map_err(CoreError::Runtime)?;
        let (suspended, suspended_rx) = watch::channel(false);
        let seam = Seam::new(Core {
            shared: shared.clone(),
            engine: None,
        });
        runtime.spawn(Engine::new(seam, config, request_rx, suspended_rx).run());
        Ok(Core {
            shared,
            engine: Some(Arc::new(EngineHandle {
                runtime: Mutex::new(Some(runtime)),
                suspended,
                deliverer: Mutex::new(Some(deliverer)),
            })),
        })
    }

    /// Opens the core without a sync engine: nothing touches the network,
    /// and requests wait in [`Seam::take_requests`] for whoever plays the
    /// engine (tests).
    ///
    /// Nor has it a delivery thread: whoever changes the core delivers the
    /// new snapshot before the call returns (see
    /// [`subscribe`](Core::subscribe)), which keeps such tests synchronous.
    pub fn open_detached(data_dir: &Path) -> Result<Core, CoreError> {
        let (requests, request_rx) = mpsc::channel();
        let shared = Shared::open(data_dir, Requests::Detached(requests), Some(request_rx))?;
        Ok(Core {
            shared,
            engine: None,
        })
    }

    /// Starts the replica in `data_dir` again for the same server, after
    /// [`open`](Core::open) failed with [`CoreError::ReplicaUnreadable`]:
    /// the replicated log, cursor, sessions, outbox and blobs (rows and
    /// files) go, while the device identity, settings, drafts and snippets
    /// stay ([`Store::reset_replica`]). The next open syncs from seq 0.
    ///
    /// Needs no open core, but takes the data dir's lock, so it fails with
    /// [`LockError::InUse`] while a core has the dir open. Works on a
    /// readable replica too.
    pub fn reset_replica(data_dir: &Path) -> Result<(), CoreError> {
        let _lock = DataDirLock::take(data_dir)?;
        Store::reset_replica(data_dir)?;
        // Once the rows are gone, as when pairing with another server.
        remove_all_blob_files(data_dir);
        tracing::info!(dir = %data_dir.display(), "replica reset");
        Ok(())
    }

    /// Pauses the sync engine's network loops, e.g. while an Android app is
    /// in the background. Nothing is lost: actions still persist and queue,
    /// and an unanswered command is resent with its id later. No-op for a
    /// detached core.
    pub fn suspend(&self) {
        if let Some(engine) = &self.engine {
            engine.suspended.send_replace(true);
        }
    }

    /// Undoes [`suspend`](Core::suspend): the link starts again from
    /// `Connecting`, unless the server rejected the device.
    pub fn resume(&self) {
        if let Some(engine) = &self.engine {
            engine.suspended.send_replace(false);
        }
    }

    /// Applies a user action. Everything the action changes is written to
    /// the local store before any request reaches the network side; when
    /// this returns `Ok`, the action survives a crash.
    pub fn dispatch(&self, action: Action) -> Result<(), CoreError> {
        let data_dir = &self.shared.data_dir;
        self.shared.update(|state| {
            let effects = state.model.dispatch(action);
            let mut requests = Vec::new();
            for effect in effects {
                let done = match effect {
                    Effect::Enqueue(command) => state.store.enqueue(&command),
                    Effect::SaveBlobPolicy(policy) => state.store.set_blob_policy(policy),
                    Effect::SaveServerAddress(address) => {
                        state.store.set_server_address(Some(&address))
                    }
                    Effect::SaveDraft((id, mode), draft) => {
                        state.store.set_draft(id, mode, draft.as_ref())
                    }
                    Effect::SaveComposerMode(id, mode) => state.store.set_composer_mode(id, mode),
                    Effect::SaveReadMark(id, mark) => state.store.set_read_mark(id, mark),
                    Effect::SaveSnippet(snippet) => state.store.set_snippet(&snippet),
                    Effect::DeleteSnippet(id) => state.store.delete_snippet(id),
                    // Refused when the store disagrees with the model; the
                    // resync below then shows what the store holds.
                    Effect::Withdraw { id, draft } => state
                        .store
                        .withdraw(id, draft.as_ref().map(|(key, d)| (*key, d))),
                    Effect::LoadSession(id) => state
                        .store
                        .session_events(id)
                        .map(|events| state.model.session_loaded(id, events)),
                    // The files go once the rows are gone; a download still
                    // writing one finds its blob unknown and removes it too.
                    Effect::DeleteLocally(id) => state
                        .store
                        .delete_session_locally(id, Utc::now())
                        .map(|blobs| {
                            state.model.session_deleted(id, &blobs);
                            state.blobs_purged(&blobs);
                            for blob in &blobs {
                                remove_blob_files(data_dir, *blob);
                            }
                            requests.push(Request::BlobsPurged(blobs));
                        }),
                    Effect::ResetReplica => {
                        state.store.reset_for_lost_history().and_then(|blobs| {
                            // The link is stopped, but a handle bound to
                            // the old replica must not write into the new.
                            state.server_epoch += 1;
                            state.model.replica_reset(state.store.load()?);
                            state.blobs_purged(&blobs);
                            remove_all_blob_files(data_dir);
                            if !blobs.is_empty() {
                                requests.push(Request::BlobsPurged(blobs));
                            }
                            Ok(())
                        })
                    }
                    Effect::Request(request) => {
                        requests.push(request);
                        Ok(())
                    }
                };
                if let Err(err) = done {
                    state.resync();
                    return Err(err.into());
                }
            }
            Ok(((), requests))
        })
    }

    /// Registers `callback`, calls it with the current snapshot, then
    /// again after every change. Snapshots arrive in order; one may be
    /// skipped in favour of a newer one (the first, too, if a change
    /// overtakes it). Callbacks may call [`dispatch`](Core::dispatch) but
    /// must not call `subscribe`, `unsubscribe` or
    /// [`shutdown`](Core::shutdown), which would wait for themselves.
    ///
    /// An engine core ([`open`](Core::open)) runs every callback on its
    /// delivery thread, `skywave-deliver`, one at a time — the first
    /// snapshot too, so this returns without waiting for it. `dispatch`
    /// and the engine only mark a change and return; the delivery thread
    /// renders the latest snapshot and hands it on, skipping those that a
    /// newer change overtook. A slow callback delays only later snapshots.
    ///
    /// A detached core ([`open_detached`](Core::open_detached)) has no
    /// delivery thread: whoever changes the core — or calls `subscribe` or
    /// `unsubscribe` — runs the callbacks before returning, unless another
    /// thread is delivering already, which then delivers its snapshot too.
    pub fn subscribe(&self, callback: impl FnMut(ViewState) + Send + 'static) -> SubscriptionId {
        let id = {
            let mut subs = lock(&self.shared.delivery.subscribers);
            let id = SubscriptionId(subs.next_id);
            subs.next_id += 1;
            subs.list.push(Subscriber {
                id,
                last: None,
                callback: Box::new(callback),
            });
            id
        };
        // Nothing changed, but this subscriber has seen nothing yet.
        self.shared.publish();
        id
    }

    /// Detaches the callback `id`; once this returns it is not called
    /// again, since it waits for a callback in progress to return. Like
    /// [`subscribe`](Core::subscribe), it must not be called from a
    /// callback.
    pub fn unsubscribe(&self, id: SubscriptionId) {
        lock(&self.shared.delivery.subscribers)
            .list
            .retain(|s| s.id != id);
        // Snapshots published while we held the list are still waiting.
        self.shared.deliver_inline();
    }

    /// The current snapshot, for heads that poll instead of subscribing.
    pub fn view(&self) -> ViewState {
        lock(&self.shared.state).model.view()
    }

    /// Stops the core for good and closes its store, so the same data dir
    /// can be opened again at once. Blocks until every engine task and the
    /// delivery thread have stopped — at most `timeout` in all for work in
    /// the middle of a step or a callback — and then drops the store,
    /// unless another handle still holds it.
    ///
    /// The engine stops even while other clones exist; they keep working
    /// against the local store, but nothing syncs, and as in a detached
    /// core their changes are delivered by whoever makes them. With other
    /// clones (or a [`Seam`]) alive, or engine work or a callback still
    /// running after `timeout`, the store closes when the last of them goes
    /// and this returns [`ShutdownError::InUse`]. It must not be called on
    /// a thread of an async runtime, where it would block a runtime thread
    /// or wait for itself: there it only drops this handle and returns
    /// [`ShutdownError::InsideRuntime`]. Nor should a callback call it: it
    /// cannot wait for the delivery thread it runs on, so the store stays
    /// open until the callback returns, and this returns
    /// [`ShutdownError::InUse`].
    pub fn shutdown(self, timeout: Duration) -> Result<(), ShutdownError> {
        if tokio::runtime::Handle::try_current().is_ok() {
            // Dropping `self` is the non-blocking fallback.
            return Err(ShutdownError::InsideRuntime);
        }
        let deadline = Instant::now() + timeout;
        let Core { shared, engine } = self;
        if let Some(engine) = engine {
            let runtime = lock(&engine.runtime).take();
            if let Some(runtime) = runtime {
                // Drops every task, and with them the engine's handles on
                // `shared`.
                runtime.shutdown_timeout(timeout);
            }
            let deliverer = lock(&engine.deliverer).take();
            if let Some(deliverer) = deliverer {
                // Its handle on `shared` goes when it ends.
                deliverer.stop(Some(deadline.saturating_duration_since(Instant::now())));
            }
        }
        // The last handle closes the store (and its SQLite connection) here.
        Arc::try_unwrap(shared)
            .map(drop)
            .map_err(|_| ShutdownError::InUse)
    }

    /// The engine-side API (T13 transport and pairing, T14 sync, T15
    /// blobs). Heads never use it.
    pub fn seam(&self) -> Seam {
        Seam::new(self.clone())
    }
}

impl Shared {
    fn open(
        data_dir: &Path,
        requests: Requests,
        request_rx: Option<Receiver<Request>>,
    ) -> Result<Arc<Shared>, CoreError> {
        let lock = DataDirLock::take(data_dir)?;
        let store = Store::open(data_dir).map_err(CoreError::opening)?;
        let identity = FileIdentity::load(data_dir)?.map(Arc::new);
        let snapshot = store.load().map_err(CoreError::opening)?;
        let model = Model::new(snapshot, identity.as_deref().map(Paired::of));
        Ok(Arc::new(Shared {
            data_dir: data_dir.to_path_buf(),
            state: Mutex::new(State {
                store,
                model,
                identity,
                server_epoch: 0,
                purges: HashMap::new(),
                version: 0,
            }),
            requests,
            request_rx: Mutex::new(request_rx),
            delivery: Delivery::default(),
            _lock: lock,
        }))
    }

    /// Runs `change` under the state lock, hands its requests to the engine
    /// and publishes the change — also after an error, since the model may
    /// have been resynced. The snapshot is rendered at delivery, not here.
    ///
    /// The requests are sent before the lock is released (sending never
    /// blocks), so the engine receives them in the order the changes were
    /// made. The blob queue relies on it: a `BlobsPurged` for a blob always
    /// reaches it before the `DownloadBlob` of a later change that
    /// registered the same id again, never after.
    pub(crate) fn update<R, E>(
        &self,
        change: impl FnOnce(&mut State) -> Result<(R, Vec<Request>), E>,
    ) -> Result<R, E> {
        let outcome = {
            let mut state = lock(&self.state);
            let result = change(&mut state);
            state.version += 1;
            result.map(|(value, requests)| {
                for request in requests {
                    self.requests.send(request);
                }
                value
            })
        };
        self.publish();
        outcome
    }

    /// Marks a snapshot due and has it delivered: by the delivery thread
    /// if there is one, else right here.
    fn publish(&self) {
        let threaded = {
            let mut pending = lock(&self.delivery.pending);
            pending.due = true;
            pending.threaded
        };
        if threaded {
            self.delivery.wake.notify_one();
        } else {
            self.drain(false);
        }
    }

    /// Delivers what is due here, unless a delivery thread does.
    fn deliver_inline(&self) {
        if !lock(&self.delivery.pending).threaded {
            self.drain(false);
        }
    }

    /// The delivery thread's loop. Once stopped, it delivers what is left
    /// if nobody else is delivering, as any publisher would.
    fn deliver_until_stopped(&self) {
        let delivery = &self.delivery;
        loop {
            {
                let mut pending = lock(&delivery.pending);
                while !pending.due && pending.threaded {
                    pending = delivery
                        .wake
                        .wait(pending)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                }
                if !pending.threaded {
                    break;
                }
                pending.due = false;
            }
            // One round per hold of the list, so a stream of changes does
            // not keep `subscribe` and `unsubscribe` waiting.
            self.hand_out(&mut lock(&delivery.subscribers), true);
        }
        self.drain(true);
    }

    /// Delivers what is due if nobody else holds the subscriber list; the
    /// holder delivers it otherwise.
    fn drain(&self, contain: bool) {
        let subs = match self.delivery.subscribers.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return,
        };
        self.deliver(subs, contain);
    }

    /// Hands out snapshots until nothing is due, then lets go of the list.
    fn deliver(&self, mut subs: MutexGuard<'_, Subscribers>, contain: bool) {
        loop {
            {
                let mut pending = lock(&self.delivery.pending);
                if !pending.due {
                    // Release the list while `pending` is still locked, so
                    // a publisher either sees us holding nothing or marked
                    // its change before we looked.
                    drop(subs);
                    return;
                }
                pending.due = false;
            }
            self.hand_out(&mut subs, contain);
        }
    }

    /// Renders the latest snapshot and hands it to every subscriber that
    /// has not seen it. With `contain`, a panicking callback is logged and
    /// skipped, so it cannot end the delivery thread.
    fn hand_out(&self, subs: &mut Subscribers, contain: bool) {
        let seen = |sub: &Subscriber, version| sub.last.is_some_and(|last| last >= version);
        let (version, view) = {
            let state = lock(&self.state);
            if subs.list.iter().all(|sub| seen(sub, state.version)) {
                // Nobody to render it for.
                return;
            }
            (state.version, state.model.view())
        };
        for sub in subs.list.iter_mut() {
            if seen(sub, version) {
                continue;
            }
            sub.last = Some(version);
            let call = AssertUnwindSafe(|| (sub.callback)(view.clone()));
            if !contain {
                call();
            } else if let Err(panic) = catch_unwind(call) {
                let reason = panic
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| panic.downcast_ref::<&str>().copied())
                    .unwrap_or("unknown panic");
                tracing::error!(reason, "subscriber callback panicked");
            }
        }
    }
}

impl State {
    /// These blobs left the replica: a download that read one before now
    /// writes for a registration that is gone ([`BlobLease`]).
    pub(crate) fn blobs_purged(&mut self, ids: &[BlobId]) {
        for id in ids {
            *self.purges.entry(*id).or_default() += 1;
        }
    }

    /// The lease a download of `id` reading its record now holds.
    pub(crate) fn blob_lease(&self, id: BlobId) -> BlobLease {
        BlobLease(self.purges.get(&id).copied().unwrap_or(0))
    }

    /// Rebuilds the model from the store after a failed write, so it never
    /// shows what was not persisted. Best effort: if the store cannot be
    /// read either, the model keeps its state.
    pub(crate) fn resync(&mut self) {
        let Ok(snapshot) = self.store.load() else {
            return;
        };
        self.model.reload(snapshot);
        let open = self.model.open_session();
        if let Some((id, Ok(events))) = open.map(|id| (id, self.store.session_events(id))) {
            self.model.session_loaded(id, events);
        }
    }
}

struct Subscriber {
    id: SubscriptionId,
    /// Version of the last snapshot this subscriber saw; `None` before the
    /// first.
    last: Option<u64>,
    callback: Callback,
}

#[derive(Default)]
struct Subscribers {
    next_id: u64,
    list: Vec<Subscriber>,
}

/// Hands snapshots to subscribers in version order without holding the state
/// lock, so a callback may dispatch. Whoever holds the subscriber list
/// delivers: the delivery thread while it runs, else whoever marked the
/// change; others leave it marked for the holder.
#[derive(Default)]
struct Delivery {
    pending: Mutex<Pending>,
    /// Wakes the delivery thread.
    wake: Condvar,
    subscribers: Mutex<Subscribers>,
}

#[derive(Default)]
struct Pending {
    /// Some subscriber may not have seen the latest snapshot.
    due: bool,
    /// A delivery thread runs and alone delivers.
    threaded: bool,
}

/// Locks a mutex, carrying on after a panic elsewhere: every critical
/// section leaves its data consistent.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::action::Route;
    use crate::store::DB_FILE;
    use crate::view::Screen;

    const TIMEOUT: Duration = Duration::from_secs(5);

    /// SQLite removes the write-ahead log when its last connection closes.
    fn wal_exists(dir: &Path) -> bool {
        dir.join(format!("{DB_FILE}-wal")).exists()
    }

    fn navigate(core: &Core) {
        core.dispatch(Action::Navigate(Route::Settings)).unwrap();
    }

    #[test]
    fn shutdown_closes_the_store_so_the_dir_reopens_at_once() {
        let dir = tempfile::tempdir().unwrap();
        for _ in 0..3 {
            let core = Core::open(dir.path()).unwrap();
            navigate(&core);
            let shared = Arc::downgrade(&core.shared);
            assert!(wal_exists(dir.path()));
            assert_eq!(core.shutdown(TIMEOUT), Ok(()));
            // The engine's tasks, and their handles on the store, are gone.
            assert!(shared.upgrade().is_none());
            assert!(!wal_exists(dir.path()));
        }
    }

    #[test]
    fn shutdown_of_a_detached_core_closes_its_store() {
        let dir = tempfile::tempdir().unwrap();
        let core = Core::open_detached(dir.path()).unwrap();
        navigate(&core);
        let shared = Arc::downgrade(&core.shared);
        assert_eq!(core.shutdown(TIMEOUT), Ok(()));
        assert!(shared.upgrade().is_none());
        assert!(!wal_exists(dir.path()));
    }

    #[test]
    fn shutdown_stops_the_engine_but_leaves_the_store_to_a_live_clone() {
        let dir = tempfile::tempdir().unwrap();
        let core = Core::open(dir.path()).unwrap();
        let clone = core.clone();
        let engine = Arc::downgrade(clone.engine.as_ref().unwrap());
        let shared = Arc::downgrade(&core.shared);
        assert_eq!(core.shutdown(TIMEOUT), Err(ShutdownError::InUse));
        assert!(lock(&engine.upgrade().unwrap().runtime).is_none());
        // The clone still works on the local store; the last one closes it.
        navigate(&clone);
        assert!(matches!(clone.view().screen, Screen::Pairing(_)));
        drop(clone);
        assert!(shared.upgrade().is_none());
        assert!(!wal_exists(dir.path()));
    }

    #[test]
    fn shutdown_inside_a_runtime_falls_back_instead_of_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let core = Core::open(dir.path()).unwrap();
        let shared = Arc::downgrade(&core.shared);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let outcome = runtime.block_on(async { core.shutdown(TIMEOUT) });
        assert_eq!(outcome, Err(ShutdownError::InsideRuntime));
        // The background stop still lets go of the store soon after.
        let deadline = Instant::now() + Duration::from_secs(10);
        while shared.upgrade().is_some() {
            assert!(Instant::now() < deadline, "the store never closed");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    // ---- delivery ----------------------------------------------------------

    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// Polls `done` until it holds.
    fn eventually(what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Every snapshot a subscriber received.
    #[derive(Clone, Default)]
    struct Views(Arc<Mutex<Vec<ViewState>>>);

    impl Views {
        fn subscribe(core: &Core) -> (Views, SubscriptionId) {
            let views = Views::default();
            let sink = views.clone();
            let id = core.subscribe(move |view| lock(&sink.0).push(view));
            (views, id)
        }

        fn count(&self) -> usize {
            lock(&self.0).len()
        }

        fn last(&self) -> Option<ViewState> {
            lock(&self.0).last().cloned()
        }

        /// Waits until this subscriber has seen what the core shows now.
        fn caught_up(&self, core: &Core) {
            eventually("the latest snapshot", || {
                self.last().is_some_and(|last| last == core.view())
            });
        }
    }

    /// A callback that, while armed, reports its thread and then blocks
    /// until released.
    struct Gate {
        armed: AtomicBool,
        entered: Mutex<Sender<Option<String>>>,
        release: Mutex<Receiver<()>>,
        calls: AtomicUsize,
        returned: AtomicUsize,
    }

    impl Gate {
        fn new() -> (Arc<Gate>, Receiver<Option<String>>, Sender<()>) {
            let (entered, entered_rx) = mpsc::channel();
            let (release, release_rx) = mpsc::channel();
            let gate = Arc::new(Gate {
                armed: AtomicBool::new(true),
                entered: Mutex::new(entered),
                release: Mutex::new(release_rx),
                calls: AtomicUsize::new(0),
                returned: AtomicUsize::new(0),
            });
            (gate, entered_rx, release)
        }

        fn subscribe(self: &Arc<Gate>, core: &Core) -> SubscriptionId {
            let gate = self.clone();
            core.subscribe(move |_| gate.call())
        }

        fn call(&self) {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let name = thread::current().name().map(str::to_owned);
            let _ = lock(&self.entered).send(name);
            if self.armed.load(Ordering::SeqCst) {
                let _ = lock(&self.release).recv_timeout(TIMEOUT);
            }
            self.returned.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn dispatch_returns_while_a_callback_blocks_the_delivery_thread() {
        let dir = tempfile::tempdir().unwrap();
        let core = Core::open(dir.path()).unwrap();
        let (gate, entered, release) = Gate::new();
        gate.subscribe(&core);
        // The first snapshot goes out on the delivery thread too, which the
        // gate now holds.
        let thread = entered.recv_timeout(TIMEOUT).unwrap();
        assert_eq!(thread.as_deref(), Some(Deliverer::NAME));

        let started = Instant::now();
        let dispatcher = thread::spawn({
            let core = core.clone();
            move || {
                for _ in 0..10 {
                    navigate(&core);
                }
            }
        });
        dispatcher.join().unwrap();
        navigate(&core);
        assert!(
            started.elapsed() < TIMEOUT / 2,
            "dispatch waited for delivery"
        );
        assert_eq!(gate.calls.load(Ordering::SeqCst), 1);

        // Once released, the latest snapshot follows, on the same thread.
        gate.armed.store(false, Ordering::SeqCst);
        release.send(()).unwrap();
        let (views, _) = Views::subscribe(&core);
        views.caught_up(&core);
        eventually("the gate's second call", || {
            gate.returned.load(Ordering::SeqCst) >= 2
        });
        for thread in entered.try_iter() {
            assert_eq!(thread.as_deref(), Some(Deliverer::NAME));
        }
        assert_eq!(core.shutdown(TIMEOUT), Ok(()));
    }

    #[test]
    fn every_subscriber_ends_on_the_latest_snapshot_under_concurrent_changes() {
        let dir = tempfile::tempdir().unwrap();
        let core = Core::open(dir.path()).unwrap();
        let seam = core.seam();
        let (watching, _) = Views::subscribe(&core);
        // A callback that dispatches from the delivery thread, through a
        // handle that does not keep the engine alive.
        let (reacting, reacting_id) = {
            let views = Views::default();
            let sink = views.clone();
            let handle = Core {
                shared: core.shared.clone(),
                engine: None,
            };
            let id = core.subscribe(move |view| {
                let mut seen = lock(&sink.0);
                seen.push(view);
                if seen.len() <= 3 {
                    drop(seen);
                    navigate(&handle);
                }
            });
            (views, id)
        };
        let threads: Vec<_> = (0..4)
            .map(|t| {
                let seam = seam.clone();
                thread::spawn(move || {
                    for i in 0..50 {
                        seam.set_link(crate::view::Link::Online {
                            latency_ms: t * 1000 + i,
                        });
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        // Each of the callback's own dispatches is heard.
        eventually("the callback's dispatches", || reacting.count() > 3);
        watching.caught_up(&core);
        reacting.caught_up(&core);
        // Its handle on the core goes with it.
        core.unsubscribe(reacting_id);
        drop(seam);
        assert_eq!(core.shutdown(TIMEOUT), Ok(()));
    }

    #[test]
    fn unsubscribe_waits_for_a_callback_in_progress_and_ends_the_calls() {
        let dir = tempfile::tempdir().unwrap();
        let core = Core::open(dir.path()).unwrap();
        let (gate, entered, release) = Gate::new();
        let id = gate.subscribe(&core);
        entered.recv_timeout(TIMEOUT).unwrap();

        let (done_tx, done) = mpsc::channel();
        let unsubscriber = thread::spawn({
            let core = core.clone();
            move || {
                core.unsubscribe(id);
                let _ = done_tx.send(());
            }
        });
        assert!(
            done.recv_timeout(Duration::from_millis(200)).is_err(),
            "unsubscribe returned during the callback"
        );
        release.send(()).unwrap();
        done.recv_timeout(TIMEOUT).unwrap();
        unsubscriber.join().unwrap();
        assert_eq!(gate.returned.load(Ordering::SeqCst), 1);

        // Changes after it reach the others, never the gate.
        gate.armed.store(false, Ordering::SeqCst);
        let (views, _) = Views::subscribe(&core);
        views.caught_up(&core);
        let before = views.count();
        navigate(&core);
        eventually("the change", || views.count() > before);
        assert_eq!(gate.calls.load(Ordering::SeqCst), 1);
        assert_eq!(core.shutdown(TIMEOUT), Ok(()));
    }

    #[test]
    fn shutdown_waits_for_a_callback_in_progress_and_joins_the_thread() {
        let dir = tempfile::tempdir().unwrap();
        let core = Core::open(dir.path()).unwrap();
        navigate(&core);
        let (gate, entered, release) = Gate::new();
        gate.subscribe(&core);
        entered.recv_timeout(TIMEOUT).unwrap();
        let shared = Arc::downgrade(&core.shared);
        let releaser = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            release.send(()).unwrap();
        });
        assert_eq!(core.shutdown(TIMEOUT), Ok(()));
        assert_eq!(gate.returned.load(Ordering::SeqCst), 1);
        assert!(shared.upgrade().is_none());
        assert!(!wal_exists(dir.path()));
        releaser.join().unwrap();
    }

    #[test]
    fn shutdown_gives_up_on_a_stuck_callback_and_the_store_closes_after_it() {
        let dir = tempfile::tempdir().unwrap();
        let core = Core::open(dir.path()).unwrap();
        navigate(&core);
        let (gate, entered, release) = Gate::new();
        gate.subscribe(&core);
        entered.recv_timeout(TIMEOUT).unwrap();
        let shared = Arc::downgrade(&core.shared);
        let started = Instant::now();
        let outcome = core.shutdown(Duration::from_millis(100));
        assert!(started.elapsed() < TIMEOUT / 2);
        assert_eq!(outcome, Err(ShutdownError::InUse));
        assert!(wal_exists(dir.path()));
        release.send(()).unwrap();
        // The delivery thread drops the last handle as it ends.
        eventually("the store to close", || !wal_exists(dir.path()));
        assert!(shared.upgrade().is_none());
    }

    #[test]
    fn a_panicking_callback_does_not_end_delivery() {
        let dir = tempfile::tempdir().unwrap();
        let core = Core::open(dir.path()).unwrap();
        core.subscribe(|_| panic!("callback blew up"));
        let (views, _) = Views::subscribe(&core);
        views.caught_up(&core);
        let before = views.count();
        navigate(&core);
        eventually("a later snapshot", || views.count() > before);
        views.caught_up(&core);
        assert_eq!(core.shutdown(TIMEOUT), Ok(()));
    }

    #[test]
    fn a_detached_core_delivers_before_the_call_returns() {
        let dir = tempfile::tempdir().unwrap();
        let core = Core::open_detached(dir.path()).unwrap();
        let caller = thread::current().id();
        let on_caller = Arc::new(AtomicBool::new(true));
        let flag = on_caller.clone();
        let views = Views::default();
        let sink = views.clone();
        core.subscribe(move |view| {
            flag.fetch_and(thread::current().id() == caller, Ordering::SeqCst);
            lock(&sink.0).push(view);
        });
        assert_eq!(views.count(), 1);
        navigate(&core);
        assert_eq!(views.last(), Some(core.view()));
        assert!(on_caller.load(Ordering::SeqCst));
    }
}
