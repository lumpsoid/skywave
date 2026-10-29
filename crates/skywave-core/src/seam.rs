//! The engine-side API of the core: how the transport and pairing client
//! (T13), the sync engine (T14) and blob downloads (T15) learn what to do and
//! report what happened. Heads never use it (D2).
//!
//! Work flows out as [`Request`]s on one channel ([`Seam::take_requests`]);
//! results flow back in through the [`Seam`] methods, each of which persists
//! first, then updates the model and pushes a new snapshot.

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::Receiver;

use skywave_proto::{CommandResult, EventBatch};

use crate::client::{Core, CoreError, State, lock};
use crate::identity::{FileIdentity, Identity, PairedCredentials};
use crate::store::{Applied, BlobPolicy, BlobRecord, BlobStatus, OutboxEntry, Settings};
use crate::view::{BlobId, CommandId, Link, Seq};

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
}

/// Engine-side handle to a [`Core`]; see the module docs.
#[derive(Clone)]
pub struct Seam {
    pub(crate) core: Core,
}

impl Seam {
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

    /// Stores a pulled batch (one transaction, cursor included) and shows
    /// it. A batch that would leave a gap is refused with
    /// [`StoreError::Gap`](crate::StoreError::Gap) and changes nothing.
    pub fn apply_events(&self, batch: &EventBatch) -> Result<Applied, CoreError> {
        self.update(|state| {
            let applied = state.store.apply_batch(batch)?;
            state.model.apply_batch(batch);
            Ok((applied, Vec::new()))
        })
    }

    /// The command was handed to the transport.
    pub fn command_sending(&self, id: CommandId) -> Result<(), CoreError> {
        self.update(|state| {
            state.store.mark_sending(id)?;
            state.model.command_sending(id);
            Ok(((), Vec::new()))
        })
    }

    /// The server answered the command; the first answer is final.
    pub fn command_result(&self, id: CommandId, result: &CommandResult) -> Result<(), CoreError> {
        self.update(|state| {
            state.store.record_result(id, result)?;
            state.model.command_result(id, result.clone());
            Ok(((), Vec::new()))
        })
    }

    pub fn set_link(&self, link: Link) {
        self.core
            .shared
            .update::<(), Infallible>(|state| {
                state.model.set_link(link);
                Ok(((), Vec::new()))
            })
            .unwrap_or_else(|never| match never {});
    }

    /// Pairing with the server at `address` returned these credentials:
    /// store them, switch to the session list and ask for a connection.
    pub fn pairing_succeeded(
        &self,
        address: &str,
        credentials: PairedCredentials,
    ) -> Result<(), CoreError> {
        let data_dir = self.data_dir();
        self.update(|state| {
            // Address first: an address without an identity is harmless,
            // the device simply stays unpaired.
            state.store.set_server_address(Some(address))?;
            let identity = FileIdentity::save(&data_dir, credentials)?;
            state.model.paired(identity.device_id(), address.to_owned());
            state.identity = Some(Arc::new(identity));
            Ok(((), vec![Request::Connect]))
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

    /// Runs a change that persists before touching the model, so a failed
    /// write leaves the model as it was.
    fn update<R>(
        &self,
        change: impl FnOnce(&mut State) -> Result<(R, Vec<Request>), CoreError>,
    ) -> Result<R, CoreError> {
        self.core.shared.update(change)
    }
}
