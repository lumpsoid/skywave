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
use skywave_proto::{CommandResult, DeviceInfo, DirsResult, EventBatch};

use crate::blobs::remove_all_blob_files;
use crate::client::{Core, CoreError, State, lock};
use crate::identity::{FileIdentity, Identity, PairedCredentials};
use crate::model::Paired;
use crate::store::{Applied, BlobPolicy, BlobRecord, BlobStatus, OutboxEntry, Settings};
use crate::transport::TransportError;
use crate::view::{ApprovalId, BlobId, CommandId, DeviceId, Link, Seq, SessionId};

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
    /// These blobs left the replica with a locally deleted session, their
    /// files already removed: stop waiting to download them.
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

    /// Stores a pulled batch (one transaction, cursor included) and shows
    /// it. A batch that would leave a gap is refused with
    /// [`StoreError::Gap`](crate::StoreError::Gap) and changes nothing.
    ///
    /// Its events settle the commands they name that are still unanswered,
    /// as [`Seam::command_result`] would with their `Accepted`: a command
    /// whose reply was lost settles from the log, and the sender moves on.
    pub fn apply_events(&self, batch: &EventBatch) -> Result<Applied, CoreError> {
        let now = Utc::now();
        self.update(|state| {
            let applied = state.store.apply_batch(batch, now)?;
            let alerted = state.model.apply_batch(batch);
            for (id, seq) in &applied.settled {
                let accepted = CommandResult::Accepted { seq: *seq };
                state.model.command_result(*id, accepted, now);
            }
            // The batch is stored either way; a failed load leaves the
            // session empty until it is opened again.
            if let Some(id) = state.model.unloaded_session()
                && let Ok(events) = state.store.session_events(id)
            {
                state.model.session_loaded(id, events);
            }
            mark_alerted(state, &alerted);
            Ok((applied, Vec::new()))
        })
    }

    /// The server listed its devices (`hello`): keep their names and show
    /// them wherever the views name a device.
    pub fn devices_listed(&self, devices: &[DeviceInfo]) -> Result<(), CoreError> {
        self.update(|state| {
            state.store.set_devices(devices)?;
            state.model.devices_named(devices);
            Ok(((), Vec::new()))
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

    /// The server answered the command; the first answer is final.
    pub fn command_result(&self, id: CommandId, result: &CommandResult) -> Result<(), CoreError> {
        let now = Utc::now();
        self.update(|state| {
            state.store.record_result(id, result, now)?;
            state.model.command_result(id, result.clone(), now);
            Ok(((), Vec::new()))
        })
    }

    /// The server refused the command's request without an answer
    /// ([`TransportError::refuses_command`]), or never logged it
    /// ([`Seam::command_unlogged`]): it fails for good and the outbox moves
    /// on.
    pub fn command_undeliverable(&self, id: CommandId) -> Result<(), CoreError> {
        let now = Utc::now();
        self.update(|state| {
            state.store.record_undeliverable(id, now)?;
            state.model.command_undeliverable(id, now);
            Ok(((), Vec::new()))
        })
    }

    /// Settles the command `id`, whose reply was unreadable, from the log:
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
            state.store.record_undeliverable(id, now)?;
            state.model.command_undeliverable(id, now);
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
                state.store.set_server_address(Some(address))?;
            } else {
                // The reset commits (with the address) before the new
                // identity is saved: a crash in between leaves the old
                // identity over an empty replica, which only pulls again,
                // never the old server's log under the new identity.
                let blobs = state.store.reset_for_server(address)?;
                state.server_epoch += 1;
                state.model.replica_reset(state.store.load()?);
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
        self.update(|state| {
            state.store.set_blob_status(id, &status)?;
            state.model.blob_status(id, status);
            Ok(((), Vec::new()))
        })
    }

    /// The download of `id` was given up for `reason`: back to
    /// `NotDownloaded`, the reason shown until it is asked for again.
    pub fn blob_failed(&self, id: BlobId, reason: impl Into<String>) -> Result<(), CoreError> {
        let reason = reason.into();
        self.update(|state| {
            state
                .store
                .set_blob_status(id, &BlobStatus::NotDownloaded)?;
            state.model.blob_failed(id, reason);
            Ok(((), Vec::new()))
        })
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

/// Stores approvals the model marked alerted. Best effort, after the batch
/// or link change that caused it: if the write fails, this process still
/// remembers them, and the next one alerts for them once more rather than
/// never.
fn mark_alerted(state: &mut State, approvals: &[(SessionId, ApprovalId)]) {
    if let Err(error) = state.store.mark_alerted(approvals) {
        tracing::warn!(%error, "cannot store alerted approvals");
    }
}
