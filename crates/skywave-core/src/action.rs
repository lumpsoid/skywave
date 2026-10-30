//! User intents a head hands to [`Core::dispatch`](crate::Core::dispatch).

use crate::store::BlobPolicy;
use crate::view::{ApprovalId, BlobId, CommandId, Decision, Seq, SessionId};

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
    /// Keep the session's unsent message on this device; blank text drops
    /// it. `cursor` counts `char`s. Heads save when the user pauses or
    /// leaves, not per keystroke: every save is a durable write.
    SaveDraft {
        session_id: SessionId,
        text: String,
        cursor: usize,
    },
    /// Take back one of this device's messages or session creations that
    /// the server never saw, or dismiss one it refused. Only items shown as
    /// `discardable` can be; anything else is a no-op.
    Discard(CommandId),
    /// Take back a `discardable` message into its session's draft, ahead of
    /// what the draft held, to edit and send again. The session view's
    /// `draft_rev` changes so heads refill their composer.
    EditQueued(CommandId),
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
    /// Open the session of the next approval in
    /// [`Attention::approvals`](crate::Attention::approvals) outside the
    /// open session: the oldest, or, when the open session has one, the
    /// first after that, wrapping round, so repeating it cycles. A no-op
    /// when there is none.
    OpenNextApproval,
    /// The user has seen the open session through `through`: its newest
    /// item was on screen. Only ever moves forward, never past the session's
    /// newest event, and only for the open session. Heads mark sparingly
    /// (every mark is a durable write) and never move the session view's
    /// `unread_from` with it.
    MarkRead {
        session_id: SessionId,
        through: Seq,
    },
    /// Fetch a blob's content (e.g. to expand a tool output).
    DownloadBlob(BlobId),
    SetBlobPolicy(BlobPolicy),
    /// While the link is `Reconnecting`, skip the rest of the wait and
    /// reconnect now. The attempt count and `offline_since` carry on.
    RetryNow,
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
