//! Skywave client core: all client logic lives here; heads only render.
//!
//! Heads use [`Core::open`], [`Core::dispatch`] with an [`Action`], and
//! [`Core::subscribe`] to receive [`ViewState`] snapshots (spec §7). They
//! depend on this crate only, never on `skywave-proto`. `Core::open` also
//! starts the sync engine ([`EngineConfig`]), which pairs, pulls the log,
//! drains the outbox and reconnects; heads that background call
//! [`Core::suspend`] and [`Core::resume`].
//!
//! - [`view`]: the snapshot types heads render.
//! - [`action`]: the user intents heads dispatch.
//! - [`seam`]: the engine-side API for transport, sync and blob downloads.
//! - [`store`]: the local replica (SQLite) — events, session projection,
//!   outbox, cursor, blobs, settings.
//! - [`projection`]: the pure fold from events to session summaries.
//! - [`identity`]: the device's pairing credentials behind [`Identity`].
//! - [`transport`]: the [`Transport`] trait to the server and its typed
//!   errors; [`HttpsTransport`] is the mTLS implementation.
//! - blob downloads ([`blob_dir`]) follow the [`BlobPolicy`] apart from
//!   event sync.
//! - [`search`]: what a search of the open session's timeline matches.
//! - [`pairing`]: the pairing client, plus code, fingerprint and pairing
//!   URI parsing for heads.

pub mod action;
mod address;
mod backoff;
mod blobs;
mod client;
mod completion;
mod engine;
mod https;
pub mod identity;
mod model;
pub mod pairing;
pub mod projection;
pub mod seam;
pub mod search;
pub mod store;
mod tls;
pub mod transport;
pub mod view;

pub use action::{Action, Route};
pub use address::{AddressError, DEFAULT_PORT, ServerAddress};
pub use backoff::{Backoff, Jitter};
pub use blobs::blob_dir;
pub use client::{Core, CoreError, SubscriptionId};
pub use engine::{Connector, DEFAULT_WAIT_SECS, EngineConfig, default_device_name};
pub use https::{HttpsTransport, TransportConfig};
pub use identity::{DeviceKey, FileIdentity, Identity, IdentityError, PairedCredentials};
pub use pairing::{
    PairError, PairUri, Paired, normalize_code, pair, pair_and_report, parse_pair_uri,
};
pub use projection::SessionSummary;
pub use seam::{Request, Seam};
pub use search::Query;
pub use skywave_proto::PROTOCOL_VERSION;
pub use store::{
    Applied, BlobPolicy, BlobRecord, BlobStatus, OutboxEntry, OutboxStatus, ReadMark, Settings,
    Snapshot, Store, StoreError,
};
pub use tls::Fingerprint;
pub use transport::{BlobBody, BlobDownload, BoxFuture, Transport, TransportError};
pub use view::{
    Answer, ApprovalId, ApprovalPrompt, ApprovalStatus, Attention, BlobId, BlobState, BlobView,
    BodyFormat, BodyView, CallId, Candidate, CandidateSource, CommandId, Completion,
    CompletionStatus, Decision, Delivery, DeviceId, Draft, Entry, ErrorInfo, FailReason, Link,
    LinkRejection, Outgoing, PairedInfo, PairingStatus, PairingView, PendingApprovalRef,
    PendingSession, Screen, Seq, SessionId, SessionListView, SessionRow, SessionState, SessionView,
    SettingsView, TimelineItem, ToolApproval, ToolKind, ToolOutcome, ToolView, ViewState,
};
