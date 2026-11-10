//! The engine-side API of the core: how the transport and pairing client
//! (T13), the sync engine (T14) and blob downloads (T15) learn what to do and
//! report what happened. Heads never use it (D2).
//!
//! Work flows out as [`Request`]s on one channel ([`Seam::take_requests`]);
//! results flow back in through the [`Seam`] methods, each of which persists
//! first, then updates the model and pushes a new snapshot.

use std::collections::HashSet;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::Receiver;

use chrono::Utc;
use skywave_proto::{
    ChangesResult, CommandResult, ConversationsResult, DirsResult, EventBatch, HelloResponse,
    LoginFinish, LoginFinishResult, LoginStartResult, StorageReport,
};

use crate::blobs::{remove_all_blob_files, remove_blob_files};
use crate::client::{BlobLease, Core, CoreError, State, failing_on_panic, lock};
use crate::identity::{FileIdentity, Identity, PairedCredentials};
use crate::model::Paired;
use crate::projection::BlobFate;
use crate::store::{
    Applied, BlobPolicy, BlobRecord, BlobStatus, LogCheck, OutboxEntry, PendingCrop, Settings,
    StoreError,
};
use crate::transport::TransportError;
use crate::view::{
    ApprovalId, BlobId, CommandId, DeviceId, Link, Seq, SessionId, UsageLimitsResult,
    UsageLimitsUnavailable,
};
use crate::workdir::{WorkdirAnswer, WorkdirAsk};

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
        address: String,
        code: String,
        fingerprint: String,
    },
    /// A (new) identity is in place: (re)connect with it.
    Connect,
    /// A command was queued; the sender should drain the outbox.
    OutboxChanged,
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
    /// Read the session's workdir as `ask` says, within
    /// [`WORKDIR_TIMEOUT`](crate::workdir::WORKDIR_TIMEOUT). Answer with
    /// [`Seam::workdir_answered`], naming `req`.
    Workdir {
        req: u64,
        ask: WorkdirAsk,
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

    /// The identity, with a handle bound to it for a link or a downloader
    /// using it; `None` before pairing. Once the device pairs with another
    /// server, every write through the bound handle fails with
    /// [`CoreError::Superseded`] and its link changes are ignored, so work
    /// still running for the old server cannot reach the new replica.
    pub(crate) fn bind(&self) -> Option<(Seam, Arc<dyn Identity>)> {
        let state = lock(&self.core.shared.state);
        let identity = state.identity.clone()? as Arc<dyn Identity>;
        let bound = Seam {
            core: self.core.clone(),
            epoch: Some(state.server_epoch),
        };
        Some((bound, identity))
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

    /// The request channel. There is one receiver; later calls get `None`.
    pub fn take_requests(&self) -> Option<Receiver<Request>> {
        lock(&self.core.shared.request_rx).take()
    }

    /// The core's data directory, e.g. for blob files.
    pub fn data_dir(&self) -> PathBuf {
        self.core.shared.data_dir.clone()
    }

    /// The device identity, `None` before pairing.
    pub fn identity(&self) -> Option<Arc<dyn Identity>> {
        let state = lock(&self.core.shared.state);
        state.identity.clone().map(|i| i as Arc<dyn Identity>)
    }

    pub fn settings(&self) -> Result<Settings, CoreError> {
        Ok(lock(&self.core.shared.state).store.settings()?)
    }

    /// Seq of the newest applied event: where the pull resumes.
    pub fn cursor(&self) -> Result<Seq, CoreError> {
        Ok(lock(&self.core.shared.state).store.cursor()?)
    }

    /// Commands awaiting a result, in send order.
    pub fn pending_outbox(&self) -> Result<Vec<OutboxEntry>, CoreError> {
        Ok(lock(&self.core.shared.state).store.pending_outbox()?)
    }

    pub fn blobs(&self) -> Result<Vec<BlobRecord>, CoreError> {
        Ok(lock(&self.core.shared.state).store.blobs()?)
    }

    /// One blob, `None` if no event references it.
    pub fn blob(&self, id: BlobId) -> Result<Option<BlobRecord>, CoreError> {
        Ok(lock(&self.core.shared.state).store.blob(id)?)
    }

    /// One blob with the lease a download of it holds: its reports through
    /// [`Seam::leased_blob_status`] fail with `UnknownBlob` once the blob
    /// is purged, even if the same id is registered again meanwhile.
    pub(crate) fn leased_blob(
        &self,
        id: BlobId,
    ) -> Result<Option<(BlobRecord, BlobLease)>, CoreError> {
        let mut state = lock(&self.core.shared.state);
        let Some(record) = state.store.blob(id)? else {
            return Ok(None);
        };
        Ok(Some((record, state.blob_lease(id))))
    }

    /// The crops this device still makes itself, in log order
    /// ([`Store::pending_crops`](crate::Store::pending_crops)).
    pub fn pending_crops(&self) -> Result<Vec<PendingCrop>, CoreError> {
        Ok(lock(&self.core.shared.state).store.pending_crops()?)
    }

    /// The pending crop that makes `into`, if any: a worker claiming
    /// `into` makes it locally first, however it was queued.
    pub(crate) fn pending_crop_into(&self, into: BlobId) -> Result<Option<PendingCrop>, CoreError> {
        Ok(lock(&self.core.shared.state)
            .store
            .pending_crop_into(into)?)
    }

    /// Whether the server deleted or cropped `id`: it no longer serves it.
    pub(crate) fn blob_fated(&self, id: BlobId) -> Result<bool, CoreError> {
        Ok(lock(&self.core.shared.state).store.blob_fate(id)?.is_some())
    }

    /// What a local crop needs: `crop.into`'s record with the lease its
    /// job holds, as [`Seam::leased_blob`], and the bytes to crop — the
    /// file of `crop.from` and the spans kept — if `from` is available and
    /// its fate still names this crop. `None` if `into` is unknown.
    pub(crate) fn local_crop(
        &self,
        crop: PendingCrop,
    ) -> Result<Option<(BlobRecord, BlobLease, Option<CropSource>)>, CoreError> {
        let mut state = lock(&self.core.shared.state);
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
            (Some(path), Some(BlobFate::Cropped { into, kept, .. })) if into.id == crop.into => {
                Some(CropSource { path, kept })
            }
            _ => None,
        };
        Ok(Some((record, lease, source)))
    }

    /// The local crop of `crop` ended, made or not: the blob it was made
    /// from goes as a server purge does ([`Seam::apply_events`]) — its row
    /// and files, a lease on it, the queue told — unless it went already.
    pub(crate) fn local_crop_ended(&self, crop: PendingCrop) -> Result<(), CoreError> {
        let data_dir = self.data_dir();
        self.update(|state| {
            let purged = state.store.end_local_crop(crop)?;
            state.blobs_purged(&purged);
            state.model.blobs_purged(&purged);
            for blob in &purged {
                remove_blob_files(&data_dir, *blob);
            }
            let mut requests = Vec::new();
            if !purged.is_empty() {
                requests.push(Request::BlobsPurged(purged));
            }
            Ok(((), requests))
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
        self.update(|state| {
            let applied = state.store.apply_batch(batch, now)?;
            state.blobs_purged(&applied.purged);
            let alerted = state.model.apply_batch(batch);
            let mut requests = Vec::new();
            for (id, seq) in &applied.settled {
                let accepted = CommandResult::Accepted { seq: *seq };
                requests.extend(state.model.command_result(*id, accepted, now));
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
            Ok((applied, requests))
        })
    }

    /// The server said `hello`: keep the names of its devices, whether it
    /// runs shell commands, whether it allows bypass mode, when its agent's
    /// login expires, whether a device may log the agent in and whether it
    /// is logged out, and show them wherever the views do, offline too
    /// after a restart.
    pub fn hello_received(&self, hello: &HelloResponse) -> Result<(), CoreError> {
        self.update(|state| {
            state.store.set_devices(&hello.devices)?;
            state.store.set_shell_enabled(hello.shell)?;
            state.store.set_bypass_allowed(hello.bypass)?;
            state.store.set_login_expires(hello.login_expires)?;
            state
                .store
                .set_agent_login_offered(hello.agent_login_offered)?;
            state.store.set_agent_logged_out(hello.agent_logged_out)?;
            state.model.devices_named(&hello.devices);
            state.model.shell_enabled(hello.shell);
            state.model.bypass_allowed(hello.bypass);
            state.model.login_expires(hello.login_expires);
            state
                .model
                .agent_login(hello.agent_login_offered, hello.agent_logged_out);
            Ok(((), Vec::new()))
        })
    }

    /// The run of the server to ask a connection's `hello` about
    /// ([`Store::server_run`](crate::Store::server_run)).
    pub fn server_run(&self) -> Result<Option<skywave_proto::RunId>, CoreError> {
        Ok(lock(&self.core.shared.state).store.server_run()?)
    }

    /// A connection's `hello`, before anything is pulled or sent on it:
    /// whether the server's log still holds what this replica has seen of
    /// it ([`Store::take_server_log`](crate::Store::take_server_log)).
    pub fn log_checked(&self, hello: &HelloResponse) -> Result<LogCheck, CoreError> {
        self.update(|state| {
            let check = state.store.take_server_log(hello.head, &hello.log)?;
            Ok((check, Vec::new()))
        })
    }

    /// Devices `batch` names that this device knows no name for: paired
    /// since the last `hello`, or never listed.
    pub fn unnamed_devices(&self, batch: &EventBatch) -> HashSet<DeviceId> {
        lock(&self.core.shared.state).model.unnamed_devices(batch)
    }

    /// Claims the command for the transport, durably, before it is sent
    /// ([`Store::claim_send`](crate::Store::claim_send)). `false` means it
    /// must not be sent: it is gone from the outbox or already answered.
    /// Shares the state lock with `Core::dispatch`, so a claim and a later
    /// discard of the same command are serialized.
    pub fn command_sending(&self, id: CommandId) -> Result<bool, CoreError> {
        let now = Utc::now();
        self.update(|state| {
            let claimed = state.store.claim_send(id, now)?;
            if claimed {
                state.model.command_sending(id, now);
            }
            Ok((claimed, Vec::new()))
        })
    }

    /// The server answered the command; the first answer is final, and one
    /// for a command already settled (or gone) changes nothing. An
    /// accepted blob delete or crop asks an open storage screen's report
    /// again, as one the log settles does ([`Seam::apply_events`]).
    pub fn command_result(&self, id: CommandId, result: &CommandResult) -> Result<(), CoreError> {
        let now = Utc::now();
        self.update(|state| {
            if !state.store.record_result(id, result, now)? {
                return Ok(((), Vec::new()));
            }
            let refresh = state.model.command_result(id, result.clone(), now);
            Ok(((), refresh.into_iter().collect()))
        })
    }

    /// The server refused the command's request without an answer
    /// ([`TransportError::refuses_command`]), or never logged it
    /// ([`Seam::command_unlogged`]): it fails for good and the outbox moves
    /// on. A command already settled (or gone) stays as it is.
    pub fn command_undeliverable(&self, id: CommandId) -> Result<(), CoreError> {
        let now = Utc::now();
        self.update(|state| {
            if state.store.record_undeliverable(id, now)? {
                state.model.command_undeliverable(id, now);
            }
            Ok(((), Vec::new()))
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
        self.update(|state| {
            let pending = state.store.pending_outbox()?;
            if !pending.iter().any(|e| e.command.id == id) {
                return Ok((true, Vec::new()));
            }
            if state.store.cursor()? < head {
                return Ok((false, Vec::new()));
            }
            if state.store.record_undeliverable(id, now)? {
                state.model.command_undeliverable(id, now);
            }
            Ok((true, Vec::new()))
        })
    }

    pub fn set_link(&self, link: Link) {
        self.core
            .shared
            .update::<(), Infallible>(|state| {
                if self.current(state) {
                    let alerted = state.model.set_link(link);
                    mark_alerted(state, &alerted);
                }
                Ok(((), Vec::new()))
            })
            .unwrap_or_else(|never| match never {});
    }

    /// The answer to [`Request::Workdir`] `req`, for `ask`. In one update:
    /// the model first decides whether `req` is still the latest for `ask`
    /// (a stale answer touches neither the replica nor the model); a domain
    /// answer other than `ChangesResult::Failed` is then stored
    /// ([`Store::put_workdir`](crate::Store::put_workdir)) and the model
    /// takes it with the blobs it registered and purged, whose files go.
    /// A transport error or a `Failed` stays in memory as the read's
    /// failure, and the stored answer stays. An answer for a session no
    /// longer in the replica is dropped.
    pub fn workdir_answered(
        &self,
        req: u64,
        ask: WorkdirAsk,
        result: Result<WorkdirAnswer, TransportError>,
    ) -> Result<(), CoreError> {
        let data_dir = self.data_dir();
        self.update(|state| {
            if !state.model.workdir_awaited(req, &ask) {
                return Ok(((), Vec::new()));
            }
            let answer = match result {
                Err(error) => {
                    state.model.workdir_failed(&ask, error.to_string());
                    return Ok(((), Vec::new()));
                }
                Ok(WorkdirAnswer::Changes(ChangesResult::Failed { reason })) => {
                    state.model.workdir_failed(&ask, reason);
                    return Ok(((), Vec::new()));
                }
                Ok(answer) => answer,
            };
            let stored = match state.store.put_workdir(&ask, &answer) {
                Ok(Some(stored)) => stored,
                Ok(None) => {
                    state.model.workdir_dropped(&ask);
                    return Ok(((), Vec::new()));
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
            let mut requests = Vec::new();
            if !stored.purged.is_empty() {
                requests.push(Request::BlobsPurged(stored.purged));
            }
            requests.extend(wanted);
            Ok(((), requests))
        })
    }

    /// The answer to [`Request::ListDirs`] `req`. Kept in memory only; an
    /// answer to an older request than the latest is ignored.
    pub fn dirs_listed(&self, req: u64, result: Result<DirsResult, TransportError>) {
        self.core
            .shared
            .update::<(), Infallible>(|state| {
                state.model.dirs_listed(req, result);
                Ok(((), Vec::new()))
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
        self.core
            .shared
            .update::<(), Infallible>(|state| {
                state.model.conversations_listed(req, result);
                Ok(((), Vec::new()))
            })
            .unwrap_or_else(|never| match never {});
    }

    /// The answer to [`Request::Storage`] `req`. Kept in memory only; an
    /// answer to an older request than the latest is ignored.
    pub fn storage_reported(&self, req: u64, result: Result<StorageReport, TransportError>) {
        self.core
            .shared
            .update::<(), Infallible>(|state| {
                state.model.storage_reported(req, result);
                Ok(((), Vec::new()))
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
        self.update(|state| {
            if !state.model.usage_limits_awaited(req) {
                return Ok(((), Vec::new()));
            }
            let answer = match result {
                Err(error) => {
                    state
                        .model
                        .usage_limits_failed(UsageLimitsUnavailable::from(error));
                    return Ok(((), Vec::new()));
                }
                Ok(UsageLimitsResult::Failed { reason }) => {
                    state
                        .model
                        .usage_limits_failed(UsageLimitsUnavailable::Failed(reason));
                    return Ok(((), Vec::new()));
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
            Ok(((), Vec::new()))
        })
    }

    /// The answer to [`Request::LoginStart`] `req`. Kept in memory only;
    /// an answer to any request but the one the login waits on is ignored.
    pub fn login_started(&self, req: u64, result: Result<LoginStartResult, TransportError>) {
        self.core
            .shared
            .update::<(), Infallible>(|state| {
                state.model.login_started(req, result);
                Ok(((), Vec::new()))
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
        self.update(|state| {
            if !state.model.login_awaited(req) {
                return Ok(((), Vec::new()));
            }
            let stored = match &result {
                Ok(LoginFinishResult::LoggedIn { expires }) => state
                    .store
                    .set_agent_logged_out(false)
                    .and_then(|()| state.store.set_login_expires(*expires)),
                _ => Ok(()),
            };
            // The login ended either way; a failed write only leaves the
            // stored hello fields to the next hello.
            state.model.login_finished(req, result, stored.is_ok());
            stored?;
            Ok(((), Vec::new()))
        })
    }

    /// The answer to [`Request::CountLines`] `req`, an error when the file
    /// could not be read. Kept in memory only; an answer to any request but
    /// the one the crop page on screen waits on is ignored.
    pub fn lines_counted(&self, req: u64, result: std::io::Result<u64>) {
        self.core
            .shared
            .update::<(), Infallible>(|state| {
                state.model.lines_counted(req, result);
                Ok(((), Vec::new()))
            })
            .unwrap_or_else(|never| match never {});
    }

    /// Pairing with the server at `address` returned these credentials:
    /// store them, switch to the session list and ask for a connection.
    ///
    /// Pairing again with the same server (the same CA,
    /// [`PairedCredentials::same_server`]) keeps the replica. Pairing with
    /// another one — or with none known — first resets it
    /// ([`Store::reset_for_server`](crate::Store::reset_for_server)) and
    /// removes the blob files, so the device starts over like a fresh
    /// install; handles bound to the old server stop writing ([`Seam::bind`]).
    pub fn pairing_succeeded(
        &self,
        address: &str,
        credentials: PairedCredentials,
    ) -> Result<(), CoreError> {
        let data_dir = self.data_dir();
        self.update(|state| {
            let same_server = state
                .identity
                .as_ref()
                .is_some_and(|old| old.credentials().same_server(&credentials));
            let mut requests = Vec::new();
            if same_server {
                // Address first: an address without an identity is
                // harmless, the device simply stays unpaired.
                state.store.switch_server_address(address)?;
            } else {
                // The reset commits (with the address) before the new
                // identity is saved: a crash in between leaves the old
                // identity over an empty replica, which only pulls again,
                // never the old server's log under the new identity.
                let blobs = state.store.reset_for_server(address)?;
                state.server_epoch += 1;
                state.model.replica_reset(state.store.load()?);
                state.blobs_purged(&blobs);
                remove_all_blob_files(&data_dir);
                if !blobs.is_empty() {
                    requests.push(Request::BlobsPurged(blobs));
                }
            }
            let identity = FileIdentity::save(&data_dir, credentials)?;
            state
                .model
                .paired(Paired::of(&identity), address.to_owned());
            state.identity = Some(Arc::new(identity));
            requests.push(Request::Connect);
            Ok(((), requests))
        })
    }

    /// Pairing failed; the pairing screen shows `reason`.
    pub fn pairing_failed(&self, reason: impl Into<String>) {
        let reason = reason.into();
        self.core
            .shared
            .update::<(), Infallible>(|state| {
                state.model.pairing_failed(reason);
                Ok(((), Vec::new()))
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
        self.update(|state| {
            check_lease(state, id, lease)?;
            state.store.set_blob_status(id, &status)?;
            state.model.blob_status(id, status);
            Ok(((), Vec::new()))
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
        self.update(|state| {
            check_lease(state, id, lease)?;
            let status = BlobStatus::Failed { reason };
            state.store.set_blob_status(id, &status)?;
            state.model.blob_status(id, status);
            Ok(((), Vec::new()))
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
        self.update(|state| {
            let Some(BlobRecord {
                status: BlobStatus::Downloading { got, path },
                ..
            }) = state.store.blob(id)?
            else {
                return Ok(((), Vec::new()));
            };
            let status = BlobStatus::Stopped { got, path };
            state.store.set_blob_status(id, &status)?;
            state.model.blob_status(id, status);
            Ok(((), Vec::new()))
        })
    }

    /// The blob queue paused every download on a full device disk, or
    /// resumed them; the views say so. Nothing is stored: a restart
    /// resumes.
    pub(crate) fn blobs_disk_full(&self, full: bool) {
        self.core
            .shared
            .update::<(), Infallible>(|state| {
                state.model.set_blobs_disk_full(full);
                Ok(((), Vec::new()))
            })
            .unwrap_or_else(|never| match never {});
    }

    /// Runs a change that persists before touching the model, so a failed
    /// write leaves the model as it was. A handle bound to an older server
    /// changes nothing ([`CoreError::Superseded`]).
    fn update<R>(
        &self,
        change: impl FnOnce(&mut State) -> Result<(R, Vec<Request>), CoreError>,
    ) -> Result<R, CoreError> {
        self.core.shared.update(|state| {
            if !self.current(state) {
                return Err(CoreError::Superseded);
            }
            change(state)
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
