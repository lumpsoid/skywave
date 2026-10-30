//! [`Core`]: the client's public face. It owns the local store, runs the
//! pure [`Model`] and pushes a full [`ViewState`] to subscribers after every
//! change.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};

use tokio::runtime::Runtime;
use tokio::sync::{mpsc as async_mpsc, watch};

use crate::action::Action;
use crate::engine::{Engine, EngineConfig};
use crate::identity::{FileIdentity, Identity, IdentityError};
use crate::model::{Effect, Model};
use crate::seam::{Request, Seam};
use crate::store::{Store, StoreError};
use crate::view::ViewState;

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Identity(#[from] IdentityError),
    #[error("cannot start the sync engine: {0}")]
    Runtime(std::io::Error),
}

/// Handle returned by [`Core::subscribe`], for [`Core::unsubscribe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubscriptionId(u64);

type Callback = Box<dyn FnMut(ViewState) + Send>;

/// The client core. Cheap to clone; every clone is the same core. The sync
/// engine stops when the last clone is dropped.
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

/// The running engine: its runtime and its suspend switch.
struct EngineHandle {
    runtime: Option<Runtime>,
    suspended: watch::Sender<bool>,
}

impl Drop for EngineHandle {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            // Does not block, so it is safe inside another runtime too.
            runtime.shutdown_background();
        }
    }
}

pub(crate) struct State {
    pub(crate) store: Store,
    pub(crate) model: Model,
    pub(crate) identity: Option<Arc<FileIdentity>>,
    /// Bumped for every published snapshot, so delivery never goes back in
    /// time.
    version: u64,
}

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
        let (suspended, suspended_rx) = watch::channel(false);
        let seam = Seam {
            core: Core {
                shared: shared.clone(),
                engine: None,
            },
        };
        runtime.spawn(Engine::new(seam, config, request_rx, suspended_rx).run());
        Ok(Core {
            shared,
            engine: Some(Arc::new(EngineHandle {
                runtime: Some(runtime),
                suspended,
            })),
        })
    }

    /// Opens the core without a sync engine: nothing touches the network,
    /// and requests wait in [`Seam::take_requests`] for whoever plays the
    /// engine (tests).
    pub fn open_detached(data_dir: &Path) -> Result<Core, CoreError> {
        let (requests, request_rx) = mpsc::channel();
        let shared = Shared::open(data_dir, Requests::Detached(requests), Some(request_rx))?;
        Ok(Core {
            shared,
            engine: None,
        })
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
        self.shared.update(|state| {
            let effects = state.model.dispatch(action);
            let mut requests = Vec::new();
            for effect in effects {
                let done = match effect {
                    Effect::Enqueue(command) => state.store.enqueue(&command),
                    Effect::SaveBlobPolicy(policy) => state.store.set_blob_policy(policy),
                    Effect::SaveDraft(id, draft) => state.store.set_draft(id, draft.as_ref()),
                    Effect::SaveReadMark(id, mark) => state.store.set_read_mark(id, mark),
                    // Refused when the store disagrees with the model; the
                    // resync below then shows what the store holds.
                    Effect::Withdraw { id, draft } => state
                        .store
                        .withdraw(id, draft.as_ref().map(|(session, d)| (*session, d))),
                    Effect::LoadSession(id) => state
                        .store
                        .session_events(id)
                        .map(|events| state.model.session_loaded(id, events)),
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

    /// Registers `callback`, calls it with the current snapshot right away,
    /// then again after every change. Snapshots arrive in order; one may be
    /// skipped in favour of a newer one. Callbacks may call
    /// [`dispatch`](Core::dispatch) but must not call `subscribe` or
    /// `unsubscribe`.
    pub fn subscribe(&self, callback: impl FnMut(ViewState) + Send + 'static) -> SubscriptionId {
        let delivery = &self.shared.delivery;
        let id = {
            let mut subs = lock(&delivery.subscribers);
            let (version, view) = lock(&self.shared.state).snapshot();
            let id = SubscriptionId(subs.next_id);
            subs.next_id += 1;
            let mut callback: Callback = Box::new(callback);
            callback(view);
            subs.list.push(Subscriber {
                id,
                last: version,
                callback,
            });
            id
        };
        // Snapshots published while we held the list are still waiting.
        delivery.drain();
        id
    }

    pub fn unsubscribe(&self, id: SubscriptionId) {
        lock(&self.shared.delivery.subscribers)
            .list
            .retain(|s| s.id != id);
    }

    /// The current snapshot, for heads that poll instead of subscribing.
    pub fn view(&self) -> ViewState {
        lock(&self.shared.state).model.view()
    }

    /// The engine-side API (T13 transport and pairing, T14 sync, T15
    /// blobs). Heads never use it.
    pub fn seam(&self) -> Seam {
        Seam { core: self.clone() }
    }
}

impl Shared {
    fn open(
        data_dir: &Path,
        requests: Requests,
        request_rx: Option<Receiver<Request>>,
    ) -> Result<Arc<Shared>, CoreError> {
        let store = Store::open(data_dir)?;
        let identity = FileIdentity::load(data_dir)?.map(Arc::new);
        let model = Model::new(store.load()?, identity.as_ref().map(|i| i.device_id()));
        Ok(Arc::new(Shared {
            data_dir: data_dir.to_path_buf(),
            state: Mutex::new(State {
                store,
                model,
                identity,
                version: 0,
            }),
            requests,
            request_rx: Mutex::new(request_rx),
            delivery: Delivery::default(),
        }))
    }

    /// Runs `change` under the state lock, hands its requests to the engine
    /// and publishes the new snapshot — also after an error, since the
    /// model may have been resynced.
    pub(crate) fn update<R, E>(
        &self,
        change: impl FnOnce(&mut State) -> Result<(R, Vec<Request>), E>,
    ) -> Result<R, E> {
        let (result, (version, view)) = {
            let mut state = lock(&self.state);
            let result = change(&mut state);
            (result, state.snapshot())
        };
        let outcome = result.map(|(value, requests)| {
            for request in requests {
                self.requests.send(request);
            }
            value
        });
        self.delivery.publish(version, view);
        outcome
    }
}

impl State {
    fn snapshot(&mut self) -> (u64, ViewState) {
        self.version += 1;
        (self.version, self.model.view())
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
    /// Version of the last snapshot this subscriber saw.
    last: u64,
    callback: Callback,
}

#[derive(Default)]
struct Subscribers {
    next_id: u64,
    list: Vec<Subscriber>,
}

/// Hands snapshots to subscribers in version order without holding the state
/// lock, so a callback may dispatch. Whoever holds the subscriber list
/// delivers; others leave their snapshot in `pending` for it.
#[derive(Default)]
struct Delivery {
    pending: Mutex<Option<(u64, ViewState)>>,
    subscribers: Mutex<Subscribers>,
}

impl Delivery {
    fn publish(&self, version: u64, view: ViewState) {
        {
            let mut pending = lock(&self.pending);
            if pending.as_ref().is_none_or(|(v, _)| *v < version) {
                *pending = Some((version, view));
            }
        }
        self.drain();
    }

    fn drain(&self) {
        loop {
            let mut subs = match self.subscribers.try_lock() {
                Ok(guard) => guard,
                Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
                // The holder delivers what we left in `pending`.
                Err(TryLockError::WouldBlock) => return,
            };
            let (version, view) = {
                let mut pending = lock(&self.pending);
                match pending.take() {
                    Some(next) => next,
                    None => {
                        // Release the list while `pending` is still locked,
                        // so a publisher either sees us holding nothing or
                        // left its snapshot where we just looked.
                        drop(subs);
                        return;
                    }
                }
            };
            for sub in subs.list.iter_mut().filter(|s| s.last < version) {
                sub.last = version;
                (sub.callback)(view.clone());
            }
        }
    }
}

/// Locks a mutex, carrying on after a panic elsewhere: every critical
/// section leaves its data consistent.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
