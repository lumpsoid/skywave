//! [`SkywaveError`], the one error type of the exported API (Kotlin
//! `SkywaveException`). Core errors cross as their message; a head shows
//! it, it does not branch on it.

use skywave_core as sc;

/// Why a call into the core failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, uniffi::Error)]
pub enum SkywaveError {
    /// An action field `name` (e.g. `session_id`) holds `input`, which is
    /// not a UUID.
    #[error("{name} is not a valid id: {input:?}")]
    InvalidId { name: String, input: String },
    /// The local store could not be read or written.
    #[error("{reason}")]
    Store { reason: String },
    /// The device identity could not be loaded or saved.
    #[error("{reason}")]
    Identity { reason: String },
    /// The sync engine could not start.
    #[error("{reason}")]
    Runtime { reason: String },
    /// The device paired with another server meanwhile.
    #[error("{reason}")]
    Superseded { reason: String },
    /// The core was shut down; open a new one.
    #[error("the core is shut down")]
    ShutDown,
    /// `subscribe`, `unsubscribe` or `shutdown` was called from inside a
    /// `ViewListener`, which would deadlock delivery.
    #[error("{call} may not be called from inside a ViewListener")]
    Reentrant { call: String },
    /// Not a valid pairing link (QR code).
    #[error("{reason}")]
    InvalidPairUri { reason: String },
    /// A file could not be read.
    #[error("{reason}")]
    Io { reason: String },
}

impl From<sc::CoreError> for SkywaveError {
    fn from(e: sc::CoreError) -> Self {
        let reason = e.to_string();
        match e {
            sc::CoreError::Store(_) => SkywaveError::Store { reason },
            sc::CoreError::Identity(_) => SkywaveError::Identity { reason },
            sc::CoreError::Runtime(_) => SkywaveError::Runtime { reason },
            sc::CoreError::Superseded => SkywaveError::Superseded { reason },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_errors_keep_their_message() {
        let e = SkywaveError::from(sc::CoreError::Superseded);
        assert_eq!(
            e,
            SkywaveError::Superseded {
                reason: sc::CoreError::Superseded.to_string()
            }
        );
    }
}
