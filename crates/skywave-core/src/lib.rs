//! Skywave client core: all client logic lives here; heads only render.
//!
//! - [`store`]: the local replica (SQLite) — events, session projection,
//!   outbox, cursor, blobs, settings.
//! - [`projection`]: the pure fold from events to session summaries.
//! - [`identity`]: the device's pairing credentials behind [`Identity`].

pub mod identity;
pub mod projection;
pub mod store;

pub use identity::{DeviceKey, FileIdentity, Identity, IdentityError, PairedCredentials};
pub use projection::SessionSummary;
pub use skywave_proto::PROTOCOL_VERSION;
pub use store::{
    Applied, BlobPolicy, BlobRecord, BlobStatus, OutboxEntry, OutboxStatus, Settings, Snapshot,
    Store, StoreError,
};
