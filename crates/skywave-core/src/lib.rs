//! Skywave client core: all client logic lives here; heads only render.
//!
//! Heads use [`Core::open`], [`Core::dispatch`] with an [`Action`], and
//! [`Core::subscribe`] to receive [`ViewState`] snapshots (spec §7). They
//! depend on this crate only, never on `skywave-proto`.
//!
//! - [`view`]: the snapshot types heads render.
//! - [`action`]: the user intents heads dispatch.
//! - [`seam`]: the engine-side API for transport, sync and blob downloads.
//! - [`store`]: the local replica (SQLite) — events, session projection,
//!   outbox, cursor, blobs, settings.
//! - [`projection`]: the pure fold from events to session summaries.
//! - [`identity`]: the device's pairing credentials behind [`Identity`].

pub mod action;
mod client;
pub mod identity;
mod model;
pub mod projection;
pub mod seam;
pub mod store;
pub mod view;

pub use action::{Action, Route};
pub use client::{Core, CoreError, SubscriptionId};
pub use identity::{DeviceKey, FileIdentity, Identity, IdentityError, PairedCredentials};
pub use projection::SessionSummary;
pub use seam::{Request, Seam};
pub use skywave_proto::PROTOCOL_VERSION;
pub use store::{
    Applied, BlobPolicy, BlobRecord, BlobStatus, OutboxEntry, OutboxStatus, Settings, Snapshot,
    Store, StoreError,
};
pub use view::{
    Answer, ApprovalId, ApprovalPrompt, ApprovalStatus, BlobId, BlobState, BlobView, CommandId,
    Decision, DeviceId, Entry, ErrorInfo, FailReason, Link, LinkRejection, Outgoing, PairedInfo,
    PairingStatus, PairingView, PendingSession, Screen, Seq, SessionId, SessionListView,
    SessionRow, SessionState, SessionView, SettingsView, TimelineItem, ViewState,
};
