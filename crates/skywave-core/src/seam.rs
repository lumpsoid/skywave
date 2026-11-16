//! The engine-side API of the core: how the transport and pairing client
//! (T13), the sync engine (T14) and blob downloads (T15) learn what to do and
//! report what happened. Heads never use it (D2).
//!
//! Work flows out as [`Request`]s on one channel ([`Seam::take_requests`]);
//! results flow back in through the [`Seam`] methods, each of which persists
//! first, then updates the model and pushes a new snapshot.

use std::collections::HashSet;
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Receiver;

use chrono::Utc;
use skywave_proto::{
    ChangesResult, CommandResult, ContentHash, ConversationsResult, DirsResult, EventBatch,
    HelloResponse, LogResult, LoginFinish, LoginFinishResult, LoginStartResult, StorageReport,
    WorktreeList,
};

use crate::address::ServerAddress;
use crate::attachment::attachment_path;
use crate::blobs::{blob_dir, remove_all_blob_files, remove_blob_files};
use crate::client::{BlobLease, Core, CoreError, State, blocking, failing_on_panic, lock};
use crate::identity::{PairedCredentials, Pairing};
use crate::projection::{BlobFate, blob_refs};
use crate::store::{
    Applied, AttachmentRefusal, BlobPolicy, BlobRecord, BlobStatus, LogCheck, OutboxEntry,
    PendingCrop, ServerFacts, Settings, StoreError,
};
use crate::transport::TransportError;
use crate::view::{
    ApprovalId, BlobId, CommandId, DeviceId, DeviceStorage, PairedLink, Seq, SessionId,
    UsageLimitsResult, UsageLimitsUnavailable,
};
use crate::workdir::{PushOutcome, WorkdirAnswer, WorkdirAsk};

/// The bytes a local crop is made from ([`Seam::local_crop`]).
#[derive(Debug, Clone)]
pub(crate) struct CropSource {
    /// The complete file of the cropped blob.
    pub(crate) path: PathBuf,
    /// The line spans the server kept.
    pub(crate) kept: Vec<skywave_proto::LineSpan>,
}

/// Work the core asks the engine to do. Sent only after the action that
/// caused it is persisted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Pair with the server at `address`, verifying its certificate against
    /// `fingerprint`. Answer with [`Seam::pairing_succeeded`] or
    /// [`Seam::pairing_failed`].
    Pair {
        address: ServerAddress,
        code: String,
        fingerprint: String,
    },
    /// A (new) pairing is in place, or its address changed: (re)connect
    /// with it.
    Connect,
    /// A command was queued; the sender should drain the outbox.
    OutboxChanged,
    /// A draft gained images: the stager should upload the ones the server
    /// lacks.
    DraftImagesChanged,
    /// The user wants this blob's content.
    DownloadBlob(BlobId),
    BlobPolicyChanged(BlobPolicy),
    /// These blobs left the replica (with a locally deleted session, or no
    /// longer named by a refreshed workdir answer), their files already
    /// removed: stop waiting to download them.
    BlobsPurged(Vec<BlobId>),
    /// The user wants the link to stop waiting out its backoff.
    RetryNow,
    /// List the server directory `dir` (empty: the roots) for workdir
    /// completion, within a short timeout. Answer with
    /// [`Seam::dirs_listed`], naming `req`.
    ListDirs {
        req: u64,
        dir: String,
    },
    /// List the conversations the server's agent keeps, for the
    /// conversation picker, within
    /// [`CONVERSATIONS_TIMEOUT`](crate::CONVERSATIONS_TIMEOUT).
    /// Answer with [`Seam::conversations_listed`], naming `req`.
    ListConversations {
        req: u64,
    },
    /// Ask the server for its storage report, for the storage screen,
    /// within [`STORAGE_TIMEOUT`](crate::STORAGE_TIMEOUT). Answer with
    /// [`Seam::storage_reported`], naming `req`.
    Storage {
        req: u64,
    },
    /// Read this device's storage report (what its replica, blobs and
    /// attachments take on its disk), for the storage screen, off the state
    /// lock. Needs no server. Answer with [`Seam::device_storage_read`],
    /// naming `req`.
    DeviceStorage {
        req: u64,
    },
    /// Ask the server to compact its database and report its storage, for
    /// the storage screen, within [`RECLAIM_TIMEOUT`](crate::RECLAIM_TIMEOUT).
    /// Answer with [`Seam::storage_reclaimed`], naming `req`.
    ReclaimStorage {
        req: u64,
    },
    /// Give this device's database's free pages back to the disk, then read
    /// this device's storage again, for the storage screen, off the state
    /// lock. Needs no server. Answer with [`Seam::device_storage_read`],
    /// naming `req`.
    ReclaimDeviceStorage {
        req: u64,
    },
    /// Ask the server for its worktree list, for the worktrees screen,
    /// within [`WORKTREES_TIMEOUT`](crate::WORKTREES_TIMEOUT). Answer with
    /// [`Seam::worktrees_listed`], naming `req`.
    Worktrees {
        req: u64,
    },
    /// Read the session's workdir as `ask` says, within
    /// [`WORKDIR_TIMEOUT`](crate::workdir::WORKDIR_TIMEOUT). Answer with
    /// [`Seam::workdir_answered`], naming `req`.
    Workdir {
        req: u64,
        ask: WorkdirAsk,
    },
    /// Push the device file `source` into the session's workdir at `path`
    /// (see [`Action::PushWorkdirFile`](crate::Action::PushWorkdirFile)).
    /// Answer with [`Seam::workdir_pushed`], naming `req`.
    PushWorkdirFile {
        req: u64,
        session: SessionId,
        source: PathBuf,
        path: String,
        replace: Option<ContentHash>,
    },
    /// Ask the server for its agent's usage limits, for the usage limits
    /// page, within [`USAGE_LIMITS_TIMEOUT`](crate::USAGE_LIMITS_TIMEOUT).
    /// The engine answers with [`Seam::usage_limits_answered`], naming
    /// `req`.
    UsageLimits {
        req: u64,
    },
    /// Ask the server to start logging its agent in (or join the login it
    /// has open), within [`LOGIN_TIMEOUT`](crate::LOGIN_TIMEOUT). The
    /// engine answers with [`Seam::login_started`], naming `req`.
    LoginStart {
        req: u64,
    },
    /// Send the code the login page showed for the flow `finish` names,
    /// within [`LOGIN_TIMEOUT`](crate::LOGIN_TIMEOUT). The engine answers
    /// with [`Seam::login_finished`], naming `req`. The code is a secret:
    /// [`LoginFinish`]'s `Debug` leaves it out.
    LoginFinish {
        req: u64,
        finish: LoginFinish,
    },
    /// Count the lines of the downloaded blob at `path`, for the crop page,
    /// as a crop counts them ([`skywave_proto::crop::count`]), off the
    /// model's thread: up to 64 MiB are read. Answer with
    /// [`Seam::lines_counted`], naming `req`.
    CountLines {
        req: u64,
        path: PathBuf,
    },
}

/// Engine-side handle to a [`Core`]; see the module docs.
#[derive(Clone)]
pub struct Seam {
    pub(crate) core: Core,
    /// The server epoch this handle is bound to ([`Seam::bind`]); `None`
    /// for an unbound handle, which may always write.
    epoch: Option<u64>,
}

impl Seam {
    pub(crate) fn new(core: Core) -> Seam {
        Seam { core, epoch: None }
    }

    /// The pairing, with a handle bound to it for a link or a downloader
    /// using it; `None` before pairing. Once the device pairs with another
    /// server, every write through the bound handle fails with
    /// [`CoreError::Superseded`] and its link changes are ignored, so work
    /// still running for the old server cannot reach the new replica.
    pub(crate) fn bind(&self) -> Option<(Seam, Arc<Pairing>)> {
        self.with_state(|state| {
            let pairing = state.pairing.clone()?;
            let bound = Seam {
                core: self.core.clone(),
                epoch: Some(state.server_epoch),
            };
            Some((bound, pairing))
        })
    }

    /// Spawns `work`, as `what`, on the current runtime; a panic in it
    /// fails the core ([`Core::on_failure`]).
    pub(crate) fn spawn(
        &self,
        what: &'static str,
        work: impl Future<Output = ()> + Send + 'static,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(failing_on_panic(
            Arc::downgrade(&self.core.shared),
            what,
            work,
        ))
    }

    /// Whether this handle may still write to `state`.
    fn current(&self, state: &State) -> bool {
        self.epoch.is_none_or(|epoch| epoch == state.server_epoch)
    }

    /// Locks the state for `access`, off the async workers (see `blocking`).
    fn with_state<R>(&self, access: impl FnOnce(&mut State) -> R) -> R {
        blocking(|| access(&mut lock(&self.core.shared.state)))
    }

    /// The request channel. There is one receiver; later calls get `None`.
    pub fn take_requests(&self) -> Option<Receiver<Request>> {
        lock(&self.core.shared.request_rx).take()
    }

    /// The core's data directory, e.g. for blob files.
    pub fn data_dir(&self) -> PathBuf {
        self.core.shared.data_dir.clone()
    }

    /// The device's pairing — its identity and the server's addresses —
    /// `None` before pairing.
    pub fn pairing(&self) -> Option<Arc<Pairing>> {
        self.with_state(|state| state.pairing.clone())
    }

    pub fn settings(&self) -> Result<Settings, CoreError> {
        Ok(self.with_state(|state| state.store.settings())?)
    }

    /// Seq of the newest applied event: where the pull resumes.
    pub fn cursor(&self) -> Result<Seq, CoreError> {
        Ok(self.with_state(|state| state.store.cursor())?)
    }

    /// Commands awaiting a result, in send order.
    pub fn pending_outbox(&self) -> Result<Vec<OutboxEntry>, CoreError> {
        Ok(self.with_state(|state| state.store.pending_outbox())?)
    }

    /// The images agent drafts hold, each hash once: a session's in the
    /// order they were attached.
    pub fn draft_image_hashes(&self) -> Result<Vec<ContentHash>, CoreError> {
        let drafts = self.with_state(|state| state.store.draft_images())?;
        let mut seen = HashSet::new();
        Ok(drafts
            .values()
            .flatten()
            .map(|image| image.sha256)
            .filter(|sha256| seen.insert(*sha256))
            .collect())
    }

    pub fn blobs(&self) -> Result<Vec<BlobRecord>, CoreError> {
        Ok(self.with_state(|state| state.store.blobs())?)
    }

    /// One blob, `None` if no event references it.
    pub fn blob(&self, id: BlobId) -> Result<Option<BlobRecord>, CoreError> {
        Ok(self.with_state(|state| state.store.blob(id))?)
    }

    /// One blob with the lease a download of it holds: its reports through
    /// [`Seam::leased_blob_status`] fail with `UnknownBlob` once the blob
    /// is purged, even if the same id is registered again meanwhile.
    pub(crate) fn leased_blob(
        &self,
        id: BlobId,
    ) -> Result<Option<(BlobRecord, BlobLease)>, CoreError> {
        self.with_state(|state| {
            let Some(record) = state.store.blob(id)? else {
                return Ok(None);
            };
            Ok(Some((record, state.blob_lease(id))))
        })
    }

    /// The crops this device still makes itself, in log order
    /// ([`Store::pending_crops`](crate::Store::pending_crops)).
    pub fn pending_crops(&self) -> Result<Vec<PendingCrop>, CoreError> {
        Ok(self.with_state(|state| state.store.pending_crops())?)
    }

    /// The pending crop that makes `into`, if any: a worker claiming
    /// `into` makes it locally first, however it was queued.
    pub(crate) fn pending_crop_into(&self, into: BlobId) -> Result<Option<PendingCrop>, CoreError> {
        Ok(self.with_state(|state| state.store.pending_crop_into(into))?)
    }

    /// Whether the server deleted or cropped `id`: it no longer serves it.
    pub(crate) fn blob_fated(&self, id: BlobId) -> Result<bool, CoreError> {
        Ok(self
            .with_state(|state| state.store.blob_fate(id))?
            .is_some())
    }

    /// What a local crop needs: `crop.into`'s record with the lease its
    /// job holds, as [`Seam::leased_blob`], and the bytes to crop — the
    /// file of `crop.from` and the spans kept — if `from` is available and
    /// its fate still names this crop. `None` if `into` is unknown.
    pub(crate) fn local_crop(
        &self,
        crop: PendingCrop,
    ) -> Result<Option<(BlobRecord, BlobLease, Option<CropSource>)>, CoreError> {
        self.with_state(|state| {
            let Some(record) = state.store.blob(crop.into)? else {
                return Ok(None);
            };
            let lease = state.blob_lease(crop.into);
            let path = match state.store.blob(crop.from)? {
                Some(BlobRecord {
                    status: BlobStatus::Available { path },
                    ..
                }) => Some(path),
                _ => None,
            };
            let source = match (path, state.store.blob_fate(crop.from)?) {
                (Some(path), Some(BlobFate::Cropped { into, kept, .. }))
                    if into.id == crop.into =>
                {
                    Some(CropSource { path, kept })
                }
                _ => None,
            };
            Ok(Some((record, lease, source)))
        })
    }

    /// The local crop of `crop` ended, made or not: the blob it was made
    /// from goes as a server purge does ([`Seam::apply_events`]) — its row
    /// and files, a lease on it, the queue told — unless it went already.
    pub(crate) fn local_crop_ended(&self, crop: PendingCrop) -> Result<(), CoreError> {
        let data_dir = self.data_dir();
        self.update(|state, requests| {
            let purged = state.store.end_local_crop(crop)?;
            state.blobs_purged(&purged);
            state.model.blobs_purged(&purged);
            for blob in &purged {
                remove_blob_files(&data_dir, *blob);
            }
            if !purged.is_empty() {
                requests.push(Request::BlobsPurged(purged));
            }
            Ok(())
        })
    }

    /// Stores a pulled batch (one transaction, cursor included) and shows
    /// it. A batch that would leave a gap is refused with
    /// [`StoreError::Gap`](crate::StoreError::Gap) and changes nothing.
    ///
    /// Its events settle the commands they name that are still unanswered,
    /// as [`Seam::command_result`] would with their `Accepted`: a command
    /// whose reply was lost settles from the log, and the sender moves on.
    ///
    /// The blobs its `BlobDeleted` and `BlobCropped` purged go as in
    /// [`Seam::workdir_answered`]: a download of one stops at its next
    /// report (the lease), and their files go.
    pub fn apply_events(&self, batch: &EventBatch) -> Result<Applied, CoreError> {
        let now = Utc::now();
        let data_dir = self.data_dir();
        // Before the state lock: copying and syncing a local image takes
        // long enough to hold off every dispatch and snapshot.
        let mut candidates = blocking(|| own_image_candidates(&data_dir, batch));
        if !candidates.is_empty() {
            // Only a blob this batch registers can use its file: skip the ones
            // the replica already knows instead of linking them for nothing.
            self.with_state(|state| candidates.retain(|(id, _)| !state.model.knows_blob(*id)));
        }
        let prepared = blocking(|| prepare_own_images(&data_dir, &candidates));
        let result = self.update(|state, requests| {
            let mut applied = state.store.apply_batch(batch, now)?;
            state.blobs_purged(&applied.purged);
            let mut alerted = state.model.apply_batch(batch);
            // A session the batch dropped keeps no alert: its rows went with it.
            alerted.retain(|(id, _)| applied.dropped.iter().all(|(dropped, _)| dropped != id));
            for (id, seq) in &applied.settled {
                let accepted = CommandResult::Accepted { seq: *seq };
                requests.extend(state.model.command_result(*id, accepted, now));
            }
            // After the settled ones: the store has deleted the outbox rows
            // of a dropped session, so the model must settle them first.
            // What a local delete does (`Effect::DeleteLocally`), for a
            // session the server deleted.
            for (id, blobs) in &applied.dropped {
                state.model.session_deleted(*id, blobs);
                state.blobs_purged(blobs);
                for blob in blobs {
                    remove_blob_files(&data_dir, *blob);
                }
                if !blobs.is_empty() {
                    requests.push(Request::BlobsPurged(blobs.clone()));
                }
            }
            // The batch is stored either way; a failed load leaves the
            // session empty until it is opened again.
            if let Some(id) = state.model.unloaded_session()
                && let Ok(events) = state.store.session_events(id)
            {
                state.model.session_loaded(id, events);
            }
            mark_alerted(state, &alerted);
            for blob in &applied.purged {
                remove_blob_files(&data_dir, *blob);
            }
            if !applied.purged.is_empty() {
                requests.push(Request::BlobsPurged(applied.purged.clone()));
            }
            // After the model took the batch: it registers the blobs there.
            own_images_available(state, &data_dir, &prepared, &mut applied);
            Ok(applied)
        });
        blocking(|| {
            for id in &prepared {
                match std::fs::remove_file(own_image_temp(&data_dir, *id)) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        tracing::warn!(?id, %error, "cannot remove a local image's temporary file");
                    }
                }
            }
        });
        result
    }

    /// The server said `hello`: keep the names of its devices, whether it
    /// runs shell commands, whether it allows bypass mode, when its agent's
    /// login expires, whether a device may log the agent in and whether it
    /// is logged out, and show them wherever the views do, offline too
    /// after a restart.
    pub fn hello_received(&self, hello: &HelloResponse) -> Result<(), CoreError> {
        self.update(|state, _| {
            let facts = ServerFacts::of(hello);
            state.store.set_devices(&hello.devices)?;
            state.store.set_server_facts(&facts)?;
            state.model.devices_named(&hello.devices);
            state.model.server_facts(facts);
            Ok(())
        })
    }

    /// The run of the server to ask a connection's `hello` about
    /// ([`Store::server_run`](crate::Store::server_run)).
    pub fn server_run(&self) -> Result<Option<skywave_proto::RunId>, CoreError> {
        Ok(self.with_state(|state| state.store.server_run())?)
    }

    /// A connection's `hello`, before anything is pulled or sent on it:
    /// whether the server's log still holds what this replica has seen of
    /// it ([`Store::take_server_log`](crate::Store::take_server_log)).
    pub fn log_checked(&self, hello: &HelloResponse) -> Result<LogCheck, CoreError> {
        self.update(|state, _| Ok(state.store.take_server_log(hello.head, &hello.log)?))
    }

    /// Devices `batch` names that this device knows no name for: paired
    /// since the last `hello`, or never listed.
    pub fn unnamed_devices(&self, batch: &EventBatch) -> HashSet<DeviceId> {
        self.with_state(|state| state.model.unnamed_devices(batch))
    }

    /// Claims the command for the transport, durably, before it is sent
    /// ([`Store::claim_send`](crate::Store::claim_send)). `false` means it
    /// must not be sent: it is gone from the outbox or already answered.
    /// Shares the state lock with `Core::dispatch`, so a claim and a later
    /// discard of the same command are serialized.
    pub fn command_sending(&self, id: CommandId) -> Result<bool, CoreError> {
        let now = Utc::now();
        self.update(|state, _| {
            let claimed = state.store.claim_send(id, now)?;
            if claimed {
                state.model.command_sending(id, now);
            }
            Ok(claimed)
        })
    }

    /// The server answered the command; the first answer is final, and one
    /// for a command already settled (or gone) changes nothing. An
    /// accepted blob delete or crop asks an open storage screen's report
    /// again, as one the log settles does ([`Seam::apply_events`]).
    pub fn command_result(&self, id: CommandId, result: &CommandResult) -> Result<(), CoreError> {
        let now = Utc::now();
        self.update(|state, requests| {
            if !state.store.record_result(id, result, now)? {
                return Ok(());
            }
            requests.extend(state.model.command_result(id, result.clone(), now));
            Ok(())
        })
    }

    /// The server refused the command's request without an answer
    /// ([`TransportError::refuses_command`]), or never logged it
    /// ([`Seam::command_unlogged`]): it fails for good and the outbox moves
    /// on. A command already settled (or gone) stays as it is.
    pub fn command_undeliverable(&self, id: CommandId) -> Result<(), CoreError> {
        let now = Utc::now();
        self.update(|state, _| {
            if state.store.record_undeliverable(id, now)? {
                state.model.command_undeliverable(id, now);
            }
            Ok(())
        })
    }

    /// The device gave up on the image of the message `id` before posting
    /// it: the server refused its upload, or the local file is gone. The
    /// message is never sent; a command already settled (or gone) stays as
    /// it is.
    pub fn command_attachment_refused(
        &self,
        id: CommandId,
        reason: AttachmentRefusal,
    ) -> Result<(), CoreError> {
        let now = Utc::now();
        self.update(|state, _| {
            if state.store.record_attachment_refused(id, reason, now)? {
                state.model.command_attachment_refused(id, reason, now);
            }
            Ok(())
        })
    }

    /// The workdir holds a file of the message `id` at its name with other
    /// content: the device gives up on the message and keeps `existing`, the
    /// hash found there, on that file ([`AttachmentRefusal::FileExists`]).
    /// The message is never sent; a command already settled (or gone) stays
    /// as it is.
    pub fn command_file_exists(
        &self,
        id: CommandId,
        position: usize,
        existing: ContentHash,
    ) -> Result<(), CoreError> {
        let now = Utc::now();
        self.update(|state, _| {
            if state
                .store
                .record_file_exists(id, position, existing, now)?
            {
                state.model.command_file_exists(id, position, existing, now);
            }
            Ok(())
        })
    }

    /// Settles the command `id`, whose reply was unreadable (or which got
    /// [`MAX_SERVER_ERRORS`](crate::MAX_SERVER_ERRORS) server errors), from the log:
    /// returns whether it is settled. `head` is the server's log head read
    /// after that reply. An accepted command logs its event before the
    /// server replies, so once the replica has applied the log through
    /// `head` without settling it, the server never logged it, so never
    /// applied it: it fails as [`Seam::command_undeliverable`] would. Before
    /// then it stays pending and the answer is `false`.
    ///
    /// The cursor and the outbox are read under one lock with the write, so
    /// a batch applied meanwhile cannot make an accepted command fail.
    pub fn command_unlogged(&self, id: CommandId, head: Seq) -> Result<bool, CoreError> {
        let now = Utc::now();
        self.update(|state, _| {
            let pending = state.store.pending_outbox()?;
            if !pending.iter().any(|e| e.command.id == id) {
                return Ok(true);
            }
            if state.store.cursor()? < head {
                return Ok(false);
            }
            if state.store.record_undeliverable(id, now)? {
                state.model.command_undeliverable(id, now);
            }
            Ok(true)
        })
    }

    pub fn set_link(&self, link: PairedLink) {
        blocking(|| {
            self.core.shared.update::<(), Infallible>(|state, _| {
                if self.current(state) {
                    let alerted = state.model.set_link(link);
                    mark_alerted(state, &alerted);
                }
                Ok(())
            })
        })
        .unwrap_or_else(|never| match never {});
    }

    /// The answer to [`Request::Workdir`] `req`, for `ask`. In one update:
    /// the model first decides whether `req` is still the latest for `ask`
    /// (a stale answer touches neither the replica nor the model); a domain
    /// answer other than `ChangesResult::Failed` or `LogResult::Failed` is
    /// then stored ([`Store::put_workdir`](crate::Store::put_workdir)) and
    /// the model takes it with the blobs it registered and purged, whose
    /// files go. A transport error or a `Failed` stays in memory as the
    /// read's failure, and the stored answer stays. An answer for a session
    /// no longer in the replica is dropped.
    pub fn workdir_answered(
        &self,
        req: u64,
        ask: WorkdirAsk,
        result: Result<WorkdirAnswer, TransportError>,
    ) -> Result<(), CoreError> {
        let data_dir = self.data_dir();
        self.update(|state, requests| {
            if !state.model.workdir_awaited(req, &ask) {
                return Ok(());
            }
            let answer = match result {
                Err(error) => {
                    state.model.workdir_failed(&ask, error.to_string());
                    return Ok(());
                }
                Ok(WorkdirAnswer::Changes(ChangesResult::Failed { reason }))
                | Ok(WorkdirAnswer::Log(LogResult::Failed { reason })) => {
                    state.model.workdir_failed(&ask, reason);
                    return Ok(());
                }
                Ok(answer) => answer,
            };
            let stored = match state.store.put_workdir(&ask, &answer) {
                Ok(Some(stored)) => stored,
                Ok(None) => {
                    state.model.workdir_dropped(&ask);
                    return Ok(());
                }
                Err(error) => {
                    state.model.workdir_failed(&ask, error.to_string());
                    return Err(error.into());
                }
            };
            state.blobs_purged(&stored.purged);
            let wanted = state.model.workdir_stored(ask, answer, &stored);
            for blob in &stored.purged {
                remove_blob_files(&data_dir, *blob);
            }
            if !stored.purged.is_empty() {
                requests.push(Request::BlobsPurged(stored.purged));
            }
            requests.extend(wanted);
            Ok(())
        })
    }

    /// The answer to [`Request::PushWorkdirFile`] `req`, for the push to
    /// `path` of `session`. In one update: the model keeps it only if `req`
    /// is still the push's latest. Nothing is stored; a push is in memory.
    pub fn workdir_pushed(
        &self,
        req: u64,
        session: SessionId,
        path: String,
        result: Result<PushOutcome, TransportError>,
    ) -> Result<(), CoreError> {
        self.update(|state, _requests| {
            state.model.workdir_pushed(req, session, path, result);
            Ok(())
        })
    }

    /// The answer to [`Request::ListDirs`] `req`. Kept in memory only; an
    /// answer to an older request than the latest is ignored.
    pub fn dirs_listed(&self, req: u64, result: Result<DirsResult, TransportError>) {
        blocking(|| {
            self.core.shared.update::<(), Infallible>(|state, _| {
                state.model.dirs_listed(req, result);
                Ok(())
            })
        })
        .unwrap_or_else(|never| match never {});
    }

    /// The answer to [`Request::ListConversations`] `req`. Kept in memory
    /// only; an answer to an older request than the latest, or one that
    /// arrives after the picker closed, is ignored.
    pub fn conversations_listed(
        &self,
        req: u64,
        result: Result<ConversationsResult, TransportError>,
    ) {
        blocking(|| {
            self.core.shared.update::<(), Infallible>(|state, _| {
                state.model.conversations_listed(req, result);
                Ok(())
            })
        })
        .unwrap_or_else(|never| match never {});
    }

    /// The answer to [`Request::Storage`] `req`. Kept in memory only; an
    /// answer to an older request than the latest is ignored.
    pub fn storage_reported(&self, req: u64, result: Result<StorageReport, TransportError>) {
        blocking(|| {
            self.core.shared.update::<(), Infallible>(|state, _| {
                state.model.storage_reported(req, result);
                Ok(())
            })
        })
        .unwrap_or_else(|never| match never {});
    }

    /// The answer to [`Request::DeviceStorage`] `req`. Kept in memory only;
    /// an answer to an older request than the latest is ignored.
    pub fn device_storage_read(&self, req: u64, result: Result<DeviceStorage, StoreError>) {
        blocking(|| {
            self.core.shared.update::<(), Infallible>(|state, _| {
                state.model.device_storage_read(req, result);
                Ok(())
            })
        })
        .unwrap_or_else(|never| match never {});
    }

    /// Runs [`Store::reclaim_space`](crate::Store::reclaim_space) under the
    /// state lock; call it from a blocking thread.
    pub fn reclaim_device_space(&self) -> Result<(), StoreError> {
        self.core
            .shared
            .update::<(), StoreError>(|state, _| state.store.reclaim_space())
    }

    /// The answer to [`Request::ReclaimStorage`] `req`. Kept in memory only;
    /// an answer to an older request than the latest is ignored.
    pub fn storage_reclaimed(&self, req: u64, result: Result<StorageReport, TransportError>) {
        blocking(|| {
            self.core.shared.update::<(), Infallible>(|state, _| {
                state.model.storage_reclaimed(req, result);
                Ok(())
            })
        })
        .unwrap_or_else(|never| match never {});
    }

    /// The answer to [`Request::Worktrees`] `req`. Kept in memory only; an
    /// answer to an older request than the latest is ignored.
    pub fn worktrees_listed(&self, req: u64, result: Result<WorktreeList, TransportError>) {
        blocking(|| {
            self.core.shared.update::<(), Infallible>(|state, _| {
                state.model.worktrees_listed(req, result);
                Ok(())
            })
        })
        .unwrap_or_else(|never| match never {});
    }

    /// The answer to [`Request::UsageLimits`] `req`. In one update: an
    /// answer to an older request than the latest touches neither the
    /// replica nor the model; a domain answer other than `Failed` is stored
    /// ([`Store::put_usage_limits`](crate::Store::put_usage_limits)) and
    /// shown. A transport error or a `Failed` stays in memory as the read's
    /// failure, and the stored answer stays.
    pub fn usage_limits_answered(
        &self,
        req: u64,
        result: Result<UsageLimitsResult, TransportError>,
    ) -> Result<(), CoreError> {
        self.update(|state, _| {
            if !state.model.usage_limits_awaited(req) {
                return Ok(());
            }
            let answer = match result {
                Err(error) => {
                    state
                        .model
                        .usage_limits_failed(UsageLimitsUnavailable::from(error));
                    return Ok(());
                }
                Ok(UsageLimitsResult::Failed { reason }) => {
                    state
                        .model
                        .usage_limits_failed(UsageLimitsUnavailable::Failed(reason));
                    return Ok(());
                }
                Ok(answer) => answer,
            };
            if let Err(error) = state.store.put_usage_limits(&answer) {
                state
                    .model
                    .usage_limits_failed(UsageLimitsUnavailable::Failed(error.to_string()));
                return Err(error.into());
            }
            state.model.usage_limits_stored(answer);
            Ok(())
        })
    }

    /// The answer to [`Request::LoginStart`] `req`. Kept in memory only;
    /// an answer to any request but the one the login waits on is ignored.
    pub fn login_started(&self, req: u64, result: Result<LoginStartResult, TransportError>) {
        blocking(|| {
            self.core.shared.update::<(), Infallible>(|state, _| {
                state.model.login_started(req, result);
                Ok(())
            })
        })
        .unwrap_or_else(|never| match never {});
    }

    /// The answer to [`Request::LoginFinish`] `req`. An answer to any
    /// request but the one the login waits on touches neither the replica
    /// nor the model. A `LoggedIn` is stored as a `hello` saying so would
    /// be — the agent no longer logged out, its login expiring at
    /// `expires` — and shown at once. Everything else stays in memory.
    pub fn login_finished(
        &self,
        req: u64,
        result: Result<LoginFinishResult, TransportError>,
    ) -> Result<(), CoreError> {
        self.update(|state, _| {
            if !state.model.login_awaited(req) {
                return Ok(());
            }
            let stored = match &result {
                Ok(LoginFinishResult::LoggedIn { expires }) => {
                    state.store.settings().and_then(|settings| {
                        let mut facts = settings.server;
                        facts.logged_in(*expires);
                        state.store.set_server_facts(&facts)
                    })
                }
                _ => Ok(()),
            };
            // The login ended either way; a failed write only leaves the
            // stored hello fields to the next hello.
            state.model.login_finished(req, result, stored.is_ok());
            stored?;
            Ok(())
        })
    }

    /// The answer to [`Request::CountLines`] `req`, an error when the file
    /// could not be read. Kept in memory only; an answer to any request but
    /// the one the crop page on screen waits on is ignored.
    pub fn lines_counted(&self, req: u64, result: std::io::Result<u64>) {
        blocking(|| {
            self.core.shared.update::<(), Infallible>(|state, _| {
                state.model.lines_counted(req, result);
                Ok(())
            })
        })
        .unwrap_or_else(|never| match never {});
    }

    /// Pairing with the server at `address` returned these credentials:
    /// store them, switch to the session list and ask for a connection.
    ///
    /// Pairing again with the same server (the same CA,
    /// [`PairedCredentials::same_server`]) keeps the replica, and the
    /// address it replaces stays among the others. Pairing with another
    /// one — or with none known — first resets the replica
    /// ([`Store::reset_for_server`](crate::Store::reset_for_server)) and
    /// removes the blob files, so the device starts over like a fresh
    /// install; handles bound to the old server stop writing ([`Seam::bind`]).
    ///
    /// The new pairing replaces the old one whole, last
    /// ([`Pairing`]): if storing it fails, the device keeps its old pairing
    /// — identity and addresses — and connects with it again.
    pub fn pairing_succeeded(
        &self,
        address: &ServerAddress,
        credentials: PairedCredentials,
    ) -> Result<(), CoreError> {
        let data_dir = self.data_dir();
        self.update(|state, requests| {
            let pairing = match state
                .pairing
                .as_ref()
                .filter(|old| old.credentials.same_server(&credentials))
            {
                Some(old) => {
                    let mut addresses = old.addresses.clone();
                    addresses.switch_to(address);
                    Pairing {
                        credentials,
                        addresses,
                    }
                }
                None => {
                    // The reset commits before the new pairing is saved: a
                    // failure or a crash in between leaves the old pairing
                    // over an empty replica, which only pulls again, never
                    // the old server's log under the new pairing. What the
                    // reset purged goes before the model reloads, which may
                    // fail.
                    let blobs = state.store.reset_for_server()?;
                    state.server_epoch += 1;
                    state.blobs_purged(&blobs);
                    remove_all_blob_files(&data_dir);
                    if !blobs.is_empty() {
                        requests.push(Request::BlobsPurged(blobs));
                    }
                    state.model.replica_reset(state.store.load()?);
                    Pairing::new(credentials, address.clone())
                }
            };
            // With the new pairing, or, if it cannot be saved, the old one
            // its handles may no longer write for once the replica was
            // reset.
            requests.push(Request::Connect);
            Ok(state.save_pairing(&data_dir, pairing)?)
        })
    }

    /// Pairing failed; the pairing screen shows `reason`.
    pub fn pairing_failed(&self, reason: impl Into<String>) {
        let reason = reason.into();
        blocking(|| {
            self.core.shared.update::<(), Infallible>(|state, _| {
                state.model.pairing_failed(reason);
                Ok(())
            })
        })
        .unwrap_or_else(|never| match never {});
    }

    /// Download progress or completion of a blob.
    pub fn blob_status(&self, id: BlobId, status: BlobStatus) -> Result<(), CoreError> {
        self.report_blob(id, None, status)
    }

    /// [`Seam::blob_status`] for the download holding `lease`
    /// ([`Seam::leased_blob`]).
    pub(crate) fn leased_blob_status(
        &self,
        id: BlobId,
        lease: BlobLease,
        status: BlobStatus,
    ) -> Result<(), CoreError> {
        self.report_blob(id, Some(lease), status)
    }

    fn report_blob(
        &self,
        id: BlobId,
        lease: Option<BlobLease>,
        status: BlobStatus,
    ) -> Result<(), CoreError> {
        self.update(|state, _| {
            check_lease(state, id, lease)?;
            state.store.set_blob_status(id, &status)?;
            state.model.blob_status(id, status);
            Ok(())
        })
    }

    /// The download of `id` was given up for `reason`: it is
    /// [`BlobStatus::Failed`], stored, so it stays so across restarts and
    /// shows the reason until it is asked for again.
    pub fn blob_failed(&self, id: BlobId, reason: impl Into<String>) -> Result<(), CoreError> {
        self.fail_blob(id, None, reason.into())
    }

    /// [`Seam::blob_failed`] for the download holding `lease`.
    pub(crate) fn leased_blob_failed(
        &self,
        id: BlobId,
        lease: BlobLease,
        reason: String,
    ) -> Result<(), CoreError> {
        self.fail_blob(id, Some(lease), reason)
    }

    fn fail_blob(
        &self,
        id: BlobId,
        lease: Option<BlobLease>,
        reason: String,
    ) -> Result<(), CoreError> {
        self.update(|state, _| {
            check_lease(state, id, lease)?;
            let status = BlobStatus::Failed { reason };
            state.store.set_blob_status(id, &status)?;
            state.model.blob_status(id, status);
            Ok(())
        })
    }

    /// The blob queue dropped `id`, a download nobody asked for, as the
    /// policy is `OnDemand`: a blob left `Downloading` becomes
    /// [`BlobStatus::Stopped`] with the progress it recorded, its `.part`
    /// kept; any other status stays. No lease: no worker holds the blob
    /// (the queue calls this as its worker lets go, or for a blob nobody
    /// held), and a row purged and registered again meanwhile is not
    /// `Downloading`.
    pub(crate) fn blob_stopped(&self, id: BlobId) -> Result<(), CoreError> {
        self.update(|state, _| {
            let Some(BlobRecord {
                status: BlobStatus::Downloading { got, path },
                ..
            }) = state.store.blob(id)?
            else {
                return Ok(());
            };
            let status = BlobStatus::Stopped { got, path };
            state.store.set_blob_status(id, &status)?;
            state.model.blob_status(id, status);
            Ok(())
        })
    }

    /// The blob queue paused every download on a full device disk, or
    /// resumed them; the views say so. Nothing is stored: a restart
    /// resumes.
    pub(crate) fn blobs_disk_full(&self, full: bool) {
        blocking(|| {
            self.core.shared.update::<(), Infallible>(|state, _| {
                state.model.set_blobs_disk_full(full);
                Ok(())
            })
        })
        .unwrap_or_else(|never| match never {});
    }

    /// Runs a change that persists before touching the model, so a failed
    /// write leaves the model as it was. A handle bound to an older server
    /// changes nothing ([`CoreError::Superseded`]). What it pushes is sent
    /// also on `Err` (`Shared::update`).
    fn update<R>(
        &self,
        change: impl FnOnce(&mut State, &mut Vec<Request>) -> Result<R, CoreError>,
    ) -> Result<R, CoreError> {
        blocking(|| {
            self.core.shared.update(|state, requests| {
                if !self.current(state) {
                    return Err(CoreError::Superseded);
                }
                change(state, requests)
            })
        })
    }
}

/// A report of a download holding `lease` is for a registration of `id`
/// that was purged since: the blob is unknown to it, whatever row has the
/// id now.
fn check_lease(state: &State, id: BlobId, lease: Option<BlobLease>) -> Result<(), CoreError> {
    match lease {
        Some(lease) if !state.lease_current(id, lease) => Err(StoreError::UnknownBlob(id).into()),
        _ => Ok(()),
    }
}

/// Stores approvals the model marked alerted. Best effort, after the batch
/// or link change that caused it: if the write fails, this process still
/// remembers them, and the next one alerts for them once more rather than
/// never.
fn mark_alerted(state: &mut State, approvals: &[(SessionId, ApprovalId)]) {
    if let Err(error) = state.store.mark_alerted(approvals) {
        tracing::warn!(%error, "cannot store alerted approvals");
    }
}

/// Makes the device's own message images available, rather than downloading
/// them, in the locked change that applies the batch. Each blob the batch
/// registered as not downloaded whose file `prepare_own_images` made is
/// renamed from `<id>.own` onto its blob file, marked available, and taken
/// out of `applied.blobs`, so the engine does not queue it. Doing it in that
/// one change means no snapshot shows such a blob as not downloaded. A blob
/// whose file was not made stays as it is and downloads. A store error is
/// logged, and the blob it touches downloads as normal.
fn own_images_available(
    state: &mut State,
    data_dir: &Path,
    prepared: &HashSet<BlobId>,
    applied: &mut Applied,
) {
    let own = match state.store.own_images(&applied.blobs) {
        Ok(own) => own,
        Err(error) => {
            tracing::warn!(%error, "cannot look up the device's own images; they download");
            return;
        }
    };
    for (id, _) in own {
        if !prepared.contains(&id) {
            tracing::debug!(?id, "cannot use the local image; it downloads");
            continue;
        }
        let path = blob_dir(data_dir).join(id.0.to_string());
        if let Err(error) = std::fs::rename(own_image_temp(data_dir, id), &path) {
            tracing::debug!(?id, %error, "cannot use the local image; it downloads");
            continue;
        }
        let status = BlobStatus::Available { path };
        if let Err(error) = state.store.set_blob_status(id, &status) {
            tracing::warn!(?id, %error, "cannot store the local image; it downloads");
            continue;
        }
        state.model.blob_status(id, status);
        applied.blobs.retain(|blob| *blob != id);
    }
}

/// The blobs `batch` references whose bytes are an attachment this device
/// holds, one entry per id: the candidates `prepare_own_images` makes files
/// for. Attachment files are removed only at store open, so a file found
/// here is still there when the batch applies.
fn own_image_candidates(data_dir: &Path, batch: &EventBatch) -> Vec<(BlobId, ContentHash)> {
    let mut seen = HashSet::new();
    let mut candidates = Vec::new();
    for (_, event) in batch.iter() {
        for blob in blob_refs(&event.kind) {
            let id = blob.id;
            if seen.contains(&id) || !attachment_path(data_dir, blob.sha256).exists() {
                continue;
            }
            seen.insert(id);
            candidates.push((id, blob.sha256));
        }
    }
    candidates
}

/// Makes `<id>.own` for each candidate (see `own_image_candidates`), and
/// returns the ids made: the only ones `own_images_available` may rename
/// into place. A file that cannot be made is logged, and its blob downloads.
fn prepare_own_images(data_dir: &Path, candidates: &[(BlobId, ContentHash)]) -> HashSet<BlobId> {
    let mut prepared = HashSet::new();
    for (id, sha256) in candidates {
        match prepare_own_image(data_dir, *id, *sha256) {
            Ok(()) => {
                prepared.insert(*id);
            }
            Err(error) => {
                tracing::debug!(?id, %error, "cannot use the local image; it downloads");
            }
        }
    }
    prepared
}

/// Makes `<id>.own` from the attachment `sha256`: a hard link, or a copy
/// where linking fails, synced before it is used. It stays under that name
/// until `own_images_available` renames it onto the blob file, so the blob
/// file is never a half-written copy. A temporary file an earlier attempt
/// left is replaced.
fn prepare_own_image(data_dir: &Path, id: BlobId, sha256: ContentHash) -> std::io::Result<()> {
    std::fs::create_dir_all(blob_dir(data_dir))?;
    let tmp = own_image_temp(data_dir, id);
    match std::fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let src = attachment_path(data_dir, sha256);
    if std::fs::hard_link(&src, &tmp).is_err() {
        std::fs::copy(&src, &tmp)?;
        std::fs::OpenOptions::new()
            .write(true)
            .open(&tmp)?
            .sync_all()?;
    }
    Ok(())
}

/// Where the own image of `id` is made until it is renamed onto its blob file.
fn own_image_temp(data_dir: &Path, id: BlobId) -> PathBuf {
    blob_dir(data_dir).join(format!("{}.own", id.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{Action, Route};
    use crate::view::Screen;
    use skywave_proto::EventKind;

    fn credentials(ca: &str) -> PairedCredentials {
        PairedCredentials {
            device_id: DeviceId::new(),
            device_key_pem: "KEY".into(),
            device_cert_pem: "CERT".into(),
            ca_cert_pem: ca.into(),
        }
    }

    /// Storing the pairing with another server fails after the replica was
    /// reset for it: the device keeps its old pairing whole — identity and
    /// address, shown and stored — and connects with it again. The call
    /// fails, yet the blob queue still hears of what the reset purged.
    #[test]
    fn a_failed_save_keeps_the_old_pairing_whole() {
        let dir = tempfile::tempdir().unwrap();
        let core = Core::open_detached(dir.path()).unwrap();
        let seam = core.seam();
        let requests = seam.take_requests().unwrap();
        let old_address: ServerAddress = "old:7443".parse().unwrap();
        let old = credentials("CA");
        seam.pairing_succeeded(&old_address, old.clone()).unwrap();
        assert_eq!(requests.try_iter().last(), Some(Request::Connect));
        let (id, blob) = (SessionId::new(), BlobId::new());
        let event = |kind: EventKind| skywave_proto::Event {
            session_id: id,
            at: Utc::now(),
            command_id: None,
            context: skywave_proto::ContextId::first_of(id),
            kind,
        };
        seam.apply_events(&EventBatch {
            from: Seq(1),
            events: vec![
                event(EventKind::SessionCreated {
                    workdir: "/srv/a".into(),
                    context_name: "start".into(),
                    from: None,
                    worktree: None,
                }),
                event(EventKind::ToolResult(skywave_proto::ToolResult {
                    call: None,
                    is_error: false,
                    status: None,
                    preview: "log".into(),
                    lines: 1,
                    output: Some(skywave_proto::Full::Blob(skywave_proto::BlobRef {
                        id: blob,
                        size: 3,
                        sha256: skywave_proto::ContentHash::of(b"abc"),
                    })),
                })),
            ],
        })
        .unwrap();
        requests.try_iter().for_each(drop);

        // Something in the way of the new record's temporary file.
        let blocker = dir.path().join(crate::identity::TEMP_FILE);
        std::fs::create_dir(&blocker).unwrap();
        let err = seam
            .pairing_succeeded(&"new:7443".parse().unwrap(), credentials("ANOTHER CA"))
            .unwrap_err();
        assert!(matches!(err, CoreError::Identity(_)), "{err}");
        let expected = Pairing::new(old.clone(), old_address.clone());
        assert_eq!(seam.pairing().as_deref(), Some(&expected));
        assert_eq!(
            requests.try_iter().collect::<Vec<_>>(),
            [Request::BlobsPurged(vec![blob]), Request::Connect]
        );
        let settings = |core: &Core| {
            core.dispatch(Action::Navigate(Route::Settings)).unwrap();
            match core.view().screen {
                Screen::Settings(settings) => (settings.device_id, settings.server_address),
                other => panic!("expected settings, got {other:?}"),
            }
        };
        let shown = (Some(old.device_id), Some(old_address));
        assert_eq!(settings(&core), shown);

        drop((core, seam));
        std::fs::remove_dir(&blocker).unwrap();
        let core = Core::open_detached(dir.path()).unwrap();
        assert_eq!(core.seam().pairing().as_deref(), Some(&expected));
        assert_eq!(settings(&core), shown);
    }

    #[test]
    fn prepare_own_image_replaces_a_stale_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path();
        let sha256 = ContentHash::of(b"image");
        let src = attachment_path(data_dir, sha256);
        std::fs::create_dir_all(src.parent().unwrap()).unwrap();
        std::fs::write(&src, b"image").unwrap();
        let id = BlobId::new();
        std::fs::create_dir_all(blob_dir(data_dir)).unwrap();
        std::fs::write(own_image_temp(data_dir, id), b"stale").unwrap();

        prepare_own_image(data_dir, id, sha256).unwrap();

        assert_eq!(
            std::fs::read(own_image_temp(data_dir, id)).unwrap(),
            b"image"
        );
        assert!(!blob_dir(data_dir).join(id.0.to_string()).exists());
    }
}
