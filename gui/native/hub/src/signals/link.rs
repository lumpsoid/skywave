use rinf::{RustSignal, SignalPiece};
use serde::Serialize;

/// The link as every screen shows it: a status line in the app bar's
/// title while the link is on its way, and a banner over the screen only
/// for what the user must act on. Sent whole whenever it changes,
/// including once a second while a retry counts down; Dart replaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, RustSignal)]
pub struct LinkUpdate {
    pub state: LinkState,
    /// The banner's text while the link stopped until the user acts:
    /// `device revoked · re-pair to reconnect · 2 queued`, `server lost
    /// history (had 9, server at 4) · sync stopped`. Empty in every other
    /// state: the status line says those. No key hints.
    pub label: String,
    /// The app bar's status line while the link is on its way back:
    /// `Connecting…`, `Syncing · 4/9 · 2 queued`, `Offline · retry in 07s ·
    /// server unreachable`. The countdown goes before the reason, so a
    /// narrow app bar cuts the reason, not the countdown. `None` while
    /// online (a queued send says so on its own row), unpaired, or
    /// stopped (the banner says it).
    pub status_label: Option<String>,
    /// The link for Settings, always said: `Connected · 12 ms`,
    /// `Connecting…`, `Syncing · 4/9`, `Offline · server unreachable · retry
    /// in 07s`, `Device revoked`, `Not paired`. Carries the round trip while online, so it
    /// changes on every ping: only the tile showing it should rebuild.
    pub connection_label: String,
    /// While reconnecting: whole seconds until the next attempt, rounded
    /// up; 0 once it is due. `None` in every other state.
    pub retry_in_s: Option<u32>,
    /// `RetryNow` would do something: skip the wait while reconnecting, or
    /// ask the server again after a protocol mismatch or a server
    /// certificate change.
    pub can_retry_now: bool,
    /// The link stopped until the user acts (re-pair, a protocol
    /// mismatch, or the server's log lost history): retries have stopped.
    pub rejected: bool,
    /// `ResetReplica` would do something: the server lost history this
    /// device holds. The reset drops the replica (and queued sends) for a
    /// fresh copy of the server's log, so ask before sending it.
    pub can_reset_replica: bool,
    /// Own commands persisted here and not answered by the server yet.
    pub queued: u32,
    /// Approvals waiting outside the open session; `OpenNextApproval`
    /// opens the next one.
    pub approvals_elsewhere: u32,
    /// `1 approval elsewhere`, `3 approvals elsewhere`; `None` for none.
    pub elsewhere_label: Option<String>,
    /// `agent login expires in 2 days · log in again on the server`, `agent
    /// login expired · …`, within three days of the agent's login expiring
    /// on the server; `None` otherwise. Says `from settings` instead of
    /// `on the server` where the server offers a login from a device. The
    /// banner shows it whatever the link, even while online with nothing
    /// else to say. `None` while the agent is logged out: `logged_out_label`
    /// says so instead.
    pub login_label: Option<String>,
    /// `agent not logged in · log in from settings` (or `… on the server`
    /// where the server offers no login from a device) while the server
    /// says its agent is logged out; `None` otherwise. Shown as
    /// `login_label` is, ahead of it.
    pub logged_out_label: Option<String>,
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
    /// Only pairing again brings the link back: this device was revoked,
    /// the server's CA or names changed, or this device's identity is
    /// damaged. [`ScreenUpdate::repair`](super::ScreenUpdate::repair) says
    /// which.
    RepairNeeded,
    /// Client and server speak different protocol versions.
    ProtocolMismatch,
    /// The server's log lost events this device holds; sync stopped until
    /// the user resets the replica ([`LinkUpdate::can_reset_replica`]).
    HistoryLost,
}
