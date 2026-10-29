//! What heads render: immutable, fully owned per-screen snapshots
//! (spec §7 ViewState, architecture.md "Render states").
//!
//! Heads depend on `skywave-core` only, so every type a head needs is defined
//! or re-exported here. Every value is owned, `Clone` and `Send`, so a
//! snapshot can cross threads and, later, the FFI boundary (D13).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

pub use skywave_proto::RejectReason as FailReason;
pub use skywave_proto::{ApprovalId, BlobId, CommandId, Decision, DeviceId, Seq, SessionId};

use crate::store::{BlobPolicy, BlobRecord, BlobStatus, OutboxStatus};

/// One full snapshot of everything a head shows. Pushed whole after every
/// change; there are no diffs.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewState {
    /// Link banner data; present on every screen.
    pub link: Link,
    /// Own commands persisted locally and not yet answered by the server
    /// ("offline · N queued").
    pub queued: usize,
    pub screen: Screen,
}

/// State of the connection to the server.
#[derive(Debug, Clone, PartialEq)]
pub enum Link {
    Connecting,
    Online {
        latency_ms: u32,
    },
    /// Pulling events; the local replica is rendered meanwhile.
    CatchingUp {
        have: Seq,
        server: Seq,
    },
    Reconnecting {
        attempt: u32,
        next_in: Duration,
        offline_since: Instant,
    },
    /// The server refused this device for good; retries stop until the user
    /// re-pairs. Local data stays readable.
    Rejected(LinkRejection),
}

/// Why the server refused the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkRejection {
    /// The device's certificate was revoked (`skywaved devices revoke`).
    Revoked,
    /// Client and server speak different protocol versions.
    ProtocolMismatch { client: u32, server: u32 },
}

/// Delivery state of one of this device's commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outgoing {
    /// Persisted locally.
    Queued,
    Sending,
    /// The server acked and placed it in the log at this seq.
    Accepted(Seq),
    /// Refused for good.
    Failed(FailReason),
}

impl From<&OutboxStatus> for Outgoing {
    fn from(status: &OutboxStatus) -> Self {
        use skywave_proto::CommandResult;
        match status {
            OutboxStatus::Queued => Outgoing::Queued,
            OutboxStatus::Sending => Outgoing::Sending,
            OutboxStatus::Done(CommandResult::Accepted { seq }) => Outgoing::Accepted(*seq),
            OutboxStatus::Done(CommandResult::Rejected { reason }) => Outgoing::Failed(*reason),
        }
    }
}

/// What a session's agent is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionState {
    Idle,
    AgentWorking,
    /// Blocked until some device answers this approval (D8).
    AwaitingApproval(ApprovalId),
    Errored(ErrorInfo),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorInfo {
    pub message: String,
}

/// Local state of one blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobState {
    NotDownloaded {
        size: u64,
    },
    Downloading {
        got: u64,
        size: u64,
    },
    /// Complete content is in the file at `path`.
    Available {
        path: PathBuf,
    },
}

impl From<&BlobRecord> for BlobState {
    fn from(record: &BlobRecord) -> Self {
        let size = record.size;
        match &record.status {
            BlobStatus::NotDownloaded => BlobState::NotDownloaded { size },
            BlobStatus::Downloading { got, .. } => BlobState::Downloading { got: *got, size },
            BlobStatus::Available { path } => BlobState::Available { path: path.clone() },
        }
    }
}

/// A blob referenced from the timeline, with its download state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobView {
    pub id: BlobId,
    pub state: BlobState,
}

/// The screen to show, with its data.
#[derive(Debug, Clone, PartialEq)]
pub enum Screen {
    Pairing(PairingView),
    SessionList(SessionListView),
    Session(SessionView),
    Settings(SettingsView),
}

#[derive(Debug, Clone, PartialEq)]
pub struct PairingView {
    /// The current pairing when this is a re-pair; `None` before the first.
    pub paired: Option<PairedInfo>,
    pub status: PairingStatus,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PairedInfo {
    pub device_id: DeviceId,
    pub server_address: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingStatus {
    /// Waiting for the user to enter address, code and fingerprint.
    Idle,
    InProgress {
        address: String,
    },
    Failed {
        address: String,
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionListView {
    /// Most recently active first.
    pub sessions: Vec<SessionRow>,
    /// Sessions this device asked for that are not in the replica yet, or
    /// that the server refused. Oldest first.
    pub creating: Vec<PendingSession>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionRow {
    pub id: SessionId,
    pub workdir: Option<String>,
    pub state: SessionState,
    pub pending_approvals: usize,
    pub last_activity: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PendingSession {
    pub command_id: CommandId,
    pub workdir: String,
    pub outgoing: Outgoing,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionView {
    pub id: SessionId,
    pub workdir: Option<String>,
    pub state: SessionState,
    /// Log order; this device's messages not yet in the log come last.
    pub timeline: Vec<TimelineItem>,
    /// The approval the agent is blocked on, if any.
    pub pending_approval: Option<ApprovalPrompt>,
    /// This device's latest interrupt of the session, until its effect is in
    /// the replica.
    pub interrupt: Option<Outgoing>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TimelineItem {
    /// An event from the server log.
    Logged {
        seq: Seq,
        at: DateTime<Utc>,
        entry: Entry,
    },
    /// A message from this device that is not in the log yet.
    Pending {
        command_id: CommandId,
        text: String,
        outgoing: Outgoing,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    UserMessage {
        text: String,
        from: DeviceId,
        /// Sent from this device.
        own: bool,
        /// Delivery state when this device sent it.
        outgoing: Option<Outgoing>,
    },
    AgentMessage {
        text: String,
    },
    ToolCall {
        name: String,
        summary: String,
        input: Option<BlobView>,
    },
    ToolResult {
        summary: String,
        output: Option<BlobView>,
    },
    Approval {
        id: ApprovalId,
        tool: String,
        summary: String,
        input: Option<BlobView>,
        status: ApprovalStatus,
    },
    /// The agent's turn was stopped by an interrupt.
    Interrupted,
    /// The agent reported an error.
    AgentErrored {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalStatus {
    Pending,
    /// First answer wins (D5); `by_me` is false when another device answered.
    Resolved {
        decision: Decision,
        by: DeviceId,
        by_me: bool,
    },
    Cancelled {
        reason: String,
    },
}

/// The approval prompt of a blocked session.
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalPrompt {
    pub id: ApprovalId,
    pub tool: String,
    pub summary: String,
    pub input: Option<BlobView>,
    /// This device's answer, if it gave one; queued like any command.
    pub answer: Option<Answer>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Answer {
    pub decision: Decision,
    pub outgoing: Outgoing,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SettingsView {
    pub blob_policy: BlobPolicy,
    pub server_address: Option<String>,
    pub device_id: Option<DeviceId>,
}
