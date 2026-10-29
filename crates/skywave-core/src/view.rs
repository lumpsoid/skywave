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
pub use skywave_proto::{
    ApprovalId, BlobId, BodyFormat, CallId, CommandId, Decision, DeviceId, Seq, SessionId, ToolKind,
};

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
    /// One tool call with its approval and result folded in, placed at the
    /// call's seq (spec §7 "Tool timeline").
    Tool(Box<ToolView>),
    /// A tool call no [`CallId`] links to anything (v1 logs); its result and
    /// approval, if any, are entries of their own.
    ToolCall {
        name: String,
        summary: String,
        input: Option<BlobView>,
    },
    /// A tool result with no logged call to fold into (v1 logs, or a result
    /// for a call that is not in the log).
    ToolResult {
        summary: String,
        output: Option<BlobView>,
    },
    /// An approval with no logged call to fold into (v1 logs, or a request
    /// gating no logged call).
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

/// A tool call as one timeline item: what it does (decided by the server,
/// D4), whether it needed an approval, and how it ended. Heads branch on
/// [`kind`](Self::kind), never on [`name`](Self::name).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolView {
    pub call: CallId,
    /// The agent's own tool name, for display only.
    pub name: String,
    pub kind: ToolKind,
    /// One line, e.g. a path or command.
    pub title: String,
    /// Optional second line.
    pub detail: Option<String>,
    /// Content worth showing beneath the title, e.g. a diff.
    pub body: Option<BodyView>,
    /// The call's whole input.
    pub input: Option<BlobView>,
    /// The approval that gated the call, if it needed one.
    pub approval: Option<ToolApproval>,
    pub outcome: ToolOutcome,
}

/// A bounded preview of some content, with the whole of it as a blob.
#[derive(Debug, Clone, PartialEq)]
pub struct BodyView {
    pub format: BodyFormat,
    /// Head of the content, with real newlines.
    pub preview: String,
    /// Line count of the whole content.
    pub lines: u32,
    /// The whole content, when `preview` does not hold all of it.
    pub full: Option<BlobView>,
}

/// The approval a tool call waited on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolApproval {
    pub id: ApprovalId,
    pub status: ApprovalStatus,
}

/// How a tool call stands (spec §7 "Tool timeline").
#[derive(Debug, Clone, PartialEq)]
pub enum ToolOutcome {
    /// No result yet, possibly waiting on its approval; `since` is the
    /// call's time.
    Running { since: DateTime<Utc> },
    /// The tool finished; `took` runs from the call to its result.
    Done {
        is_error: bool,
        /// Short outcome, e.g. `exit 1`.
        status: Option<String>,
        /// Head of the output, with real newlines.
        preview: String,
        /// Line count of the whole output.
        lines: u32,
        /// The whole output, when `preview` does not hold all of it.
        output: Option<BlobView>,
        took: Duration,
    },
    /// Its approval was denied, so the tool did not run; `by_me` is false
    /// when another device answered.
    Denied { by: DeviceId, by_me: bool },
    /// Its approval was cancelled before anyone answered.
    Cancelled { reason: String },
    /// The turn ended without a result.
    NoResult,
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
    /// The tool call the approval gates, an [`Entry::Tool`] in the timeline;
    /// `None` for an approval with no logged call.
    pub call: Option<CallId>,
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
