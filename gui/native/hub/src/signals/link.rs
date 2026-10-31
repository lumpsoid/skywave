use rinf::{RustSignal, SignalPiece};
use serde::Serialize;

/// The link banner, shown on every screen. Sent whole whenever it changes,
/// including once a second while a retry counts down; Dart replaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, RustSignal)]
pub struct LinkUpdate {
    pub state: LinkState,
    /// The banner's text, e.g. `online · 2 queued`, `offline · 2 queued ·
    /// retry in 07s (attempt 3)`, `device revoked · re-pair to reconnect`.
    /// Empty when there is nothing to say, and the banner hides: while
    /// unpaired (the pairing screen says what to do), and while online
    /// with nothing queued and no approvals elsewhere. No key hints.
    pub label: String,
    /// While reconnecting: whole seconds until the next attempt, rounded
    /// up; 0 once it is due. `None` in every other state.
    pub retry_in_s: Option<u32>,
    /// `RetryNow` would skip the wait: only while reconnecting.
    pub can_retry_now: bool,
    /// The server refused this device for good (revoked, or a protocol
    /// mismatch): retries have stopped.
    pub rejected: bool,
    /// Own commands persisted here and not answered by the server yet.
    pub queued: u32,
    /// Approvals waiting outside the open session; `OpenNextApproval`
    /// opens the next one.
    pub approvals_elsewhere: u32,
    /// `1 approval elsewhere`, `3 approvals elsewhere`; `None` for none.
    pub elsewhere_label: Option<String>,
}

/// The link's state, for the banner's colour and icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, SignalPiece)]
pub enum LinkState {
    /// Never paired: no server to connect to. The banner is empty.
    Unpaired,
    Connecting,
    Online,
    /// Pulling events; the local replica shows meanwhile.
    CatchingUp,
    /// Down, waiting to try again ([`LinkUpdate::retry_in_s`]).
    Reconnecting,
    /// This device's certificate was revoked: re-pair to reconnect.
    Revoked,
    /// Client and server speak different protocol versions.
    ProtocolMismatch,
}
