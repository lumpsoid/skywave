//! User intents a head hands to [`Core::dispatch`](crate::Core::dispatch).

use crate::store::BlobPolicy;
use crate::view::{ApprovalId, BlobId, Decision, SessionId};

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Pair with a server using what `skywaved pair` printed. Also re-pairs
    /// a paired device, e.g. after `Rejected(Revoked)`.
    Pair {
        address: String,
        code: String,
        fingerprint: String,
    },
    /// Start a session in a server directory under a configured root (D14).
    CreateSession {
        workdir: String,
    },
    SendMessage {
        session_id: SessionId,
        text: String,
    },
    ResolveApproval {
        id: ApprovalId,
        decision: Decision,
    },
    /// Stop the agent's current turn.
    Interrupt {
        session_id: SessionId,
    },
    /// Shut down the session's agent; the next message starts it again.
    StopSession {
        session_id: SessionId,
    },
    OpenSession(SessionId),
    /// Fetch a blob's content (e.g. to expand a tool output).
    DownloadBlob(BlobId),
    SetBlobPolicy(BlobPolicy),
    Navigate(Route),
}

/// Screens reachable without a parameter; a session opens with
/// [`Action::OpenSession`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    SessionList,
    Settings,
    Pairing,
}
