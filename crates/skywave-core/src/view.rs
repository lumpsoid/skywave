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
    ApprovalId, BlobId, BodyFormat, CallId, CommandId, Decision, DeviceId, RuleScope,
    RuleSuggestion, Seq, SessionId, ToolKind,
};

use crate::store::{BlobPolicy, OutboxEntry, OutboxStatus};

/// One full snapshot of everything a head shows. Pushed whole after every
/// change; there are no diffs.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewState {
    /// Link banner data; present on every screen.
    pub link: Link,
    /// Own commands persisted locally and not yet answered by the server
    /// ("offline · N queued").
    pub queued: usize,
    /// Approvals waiting for an answer; present on every screen.
    pub attention: Attention,
    pub screen: Screen,
}

/// Approvals some agent is blocked on (D8), so a head can point at them
/// from any screen and alert the user to new ones.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Attention {
    /// Every approval pending in the replica that this device has not
    /// answered (a refused answer counts as unanswered), oldest first.
    pub approvals: Vec<PendingApprovalRef>,
    /// How many of `approvals` are outside the open session.
    pub elsewhere: usize,
    /// Bumped once whenever new approvals came in while online: never for
    /// those already pending at start, arriving during a catch-up and
    /// resolved before its end, or answered from this device. Heads alert
    /// when it grows; it never goes back.
    pub alerts: u64,
}

/// One pending approval, and which session waits on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApprovalRef {
    pub id: ApprovalId,
    pub session_id: SessionId,
    /// The session's title, as [`SessionRow::title`]. Text from the
    /// server: heads sanitise it before writing it anywhere raw.
    pub title: String,
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

/// How one of this device's commands went out. Every time is this device's
/// clock, so the waits between them hold whatever the server's clock says;
/// the server's own time is the item's `at`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Delivery {
    /// When the command was queued: the time in its UUIDv7 id.
    pub queued_at: DateTime<Utc>,
    /// When it was first claimed for sending; `None` before that, and for
    /// commands sent before this was recorded (`tries` is then 1).
    pub first_sent_at: Option<DateTime<Utc>>,
    /// When the server's answer was recorded.
    pub answered_at: Option<DateTime<Utc>>,
    /// How often it was claimed for sending; more than once means resends.
    pub tries: u32,
}

impl Delivery {
    /// `None` for a command whose id carries no time (not a UUIDv7).
    pub(crate) fn of(entry: &OutboxEntry) -> Option<Self> {
        let (secs, nanos) = entry.command.id.0.get_timestamp()?.to_unix();
        Some(Delivery {
            queued_at: DateTime::from_timestamp(i64::try_from(secs).ok()?, nanos)?,
            first_sent_at: entry.first_sent_at,
            answered_at: entry.answered_at,
            tries: entry.tries,
        })
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
    /// Idle with its agent shut down on request; the next message starts it.
    Stopped,
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
    /// Asked for (`Action::DownloadBlob`) and waiting for a download slot.
    Requested {
        size: u64,
    },
    /// `paused` while the link is down: the download waits for it.
    Downloading {
        got: u64,
        size: u64,
        paused: bool,
    },
    /// The last download was given up (e.g. the sizes disagree); asking
    /// again retries it.
    Failed {
        size: u64,
        reason: String,
    },
    /// Complete content is in the file at `path`.
    Available {
        path: PathBuf,
    },
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
    Session(Box<SessionView>),
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
    /// Which of the two lists this is.
    pub kind: ListKind,
    /// The sessions of this list, most recently active first. Archiving
    /// moves a session between the lists without reordering either.
    pub sessions: Vec<SessionRow>,
    /// How many sessions the other list holds.
    pub other_count: usize,
    /// Sessions this device asked for that are not in the replica yet, or
    /// that the server refused. Oldest first. Always empty on the archived
    /// list: a new session is active.
    pub creating: Vec<PendingSession>,
    /// Workdir completion for the new-session prompt, while one is open
    /// ([`Action::CompleteWorkdir`](crate::Action::CompleteWorkdir)); only
    /// on the active list.
    pub completion: Option<Completion>,
}

/// The session list's two halves: [`Route::SessionList`](crate::Route) and
/// [`Route::ArchivedSessions`](crate::Route).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListKind {
    Active,
    /// Sessions archived with
    /// [`Action::SetSessionArchived`](crate::Action::SetSessionArchived)
    /// and not brought back since. They open and work as usual.
    Archived,
}

/// Directories that complete what the user typed as a workdir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// The input the candidates are for.
    pub input: String,
    /// Paths that extend `input`, without a trailing `/`: the server's
    /// listing once it is known, else recent workdirs from the replica. A
    /// relative input matches the roots and recent workdirs by any path
    /// component.
    pub candidates: Vec<Candidate>,
    pub status: CompletionStatus,
}

impl Completion {
    /// What the input becomes when the user completes it: the only
    /// candidate with a trailing `/`, or the candidates' longest common
    /// prefix if it extends the input. `None` when nothing would change.
    pub fn fill(&self) -> Option<String> {
        match self.candidates.as_slice() {
            [] => None,
            [only] => Some(format!("{}/", only.path)),
            [first, rest @ ..] => {
                let mut common = first.path.as_str();
                for c in rest {
                    let len = common
                        .char_indices()
                        .zip(c.path.chars())
                        .find(|((_, a), b)| a != b)
                        .map_or(common.len().min(c.path.len()), |((i, _), _)| i);
                    common = &common[..len];
                }
                (common.len() > self.input.len() && common.starts_with(self.input.as_str()))
                    .then(|| common.to_owned())
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub path: String,
    pub source: CandidateSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateSource {
    /// Listed by the server just now.
    Server,
    /// A workdir of a session in the replica, newest first.
    Recent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionStatus {
    /// The server was asked; the candidates may still change.
    Loading,
    /// The server answered; `truncated` when it listed only part of a large
    /// directory.
    Ready { truncated: bool },
    /// No listing: offline, refused, timed out, or a server without the
    /// endpoint. The candidates are recent workdirs only.
    Unavailable,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionRow {
    pub id: SessionId,
    /// What to call the session: its name, else the first line of its first
    /// message, else its directory's name, else the end of its id (see
    /// [`SessionSummary::title`](crate::SessionSummary::title)). A rename
    /// from this device shows here at once. Text from the server: heads
    /// sanitise it before writing it anywhere raw.
    pub title: String,
    /// `title` is a name the user gave, not the default.
    pub named: bool,
    /// This device's latest rename of the session, until its effect is in
    /// the replica. `title` already shows it unless it failed.
    pub rename: Option<Outgoing>,
    pub workdir: Option<String>,
    pub state: SessionState,
    pub pending_approvals: usize,
    pub last_activity: DateTime<Utc>,
    /// This device keeps an unsent message for the session.
    pub has_draft: bool,
    /// Agent output (messages, approval requests, errors) this device has
    /// not seen yet. Messages from any device never count.
    pub unread: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PendingSession {
    pub command_id: CommandId,
    pub workdir: String,
    pub outgoing: Outgoing,
    /// [`Action::Discard`](crate::Action::Discard) takes it back: the server
    /// never saw it, or refused it.
    pub discardable: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionView {
    pub id: SessionId,
    /// As [`SessionRow::title`].
    pub title: String,
    /// As [`SessionRow::named`].
    pub named: bool,
    /// As [`SessionRow::rename`].
    pub rename: Option<Outgoing>,
    /// The session is on the archived list, this device's own archiving
    /// or message on its way included. A message brings it back.
    pub archived: bool,
    pub workdir: Option<String>,
    pub state: SessionState,
    /// Log order; this device's messages not yet in the log come last.
    pub timeline: Vec<TimelineItem>,
    /// The approval the agent is blocked on, if any.
    pub pending_approval: Option<ApprovalPrompt>,
    /// The lasting rules granted with approvals in this session, by any
    /// device, in log order. Only what was granted through Skywave: rules
    /// the agent keeps elsewhere are not seen.
    pub rules: Vec<RuleSuggestion>,
    /// This device's latest interrupt of the session, until its effect is in
    /// the replica.
    pub interrupt: Option<Outgoing>,
    /// This device's latest stop of the session, until its effect is in the
    /// replica.
    pub stop: Option<Outgoing>,
    /// This device's unsent message, as last saved.
    pub draft: Option<Draft>,
    /// Changes when the core itself rewrites a draft (e.g.
    /// [`Action::EditQueued`](crate::Action::EditQueued)): a head that
    /// keeps its own copy of the text being written replaces it with
    /// `draft` then.
    pub draft_rev: u64,
    /// Seq of the session's newest event: what
    /// [`Action::MarkRead`](crate::Action::MarkRead) marks through once the
    /// end of the timeline was on screen.
    pub last_seq: Seq,
    /// Where the news starts: the first event after what this device had
    /// read when the session was opened, if agent output follows. Fixed
    /// while the session stays open; marking it read does not move it.
    pub unread_from: Option<Seq>,
}

impl SessionView {
    /// The messages sent into the session, newest first, for a head to
    /// recall into its composer: every device's logged messages, then this
    /// device's not yet logged, a message repeated back to back listed once.
    pub fn prompts(&self) -> Vec<&str> {
        let mut prompts: Vec<&str> = Vec::new();
        for item in self.timeline.iter().rev() {
            let text = match item {
                TimelineItem::Logged {
                    entry: Entry::UserMessage { text, .. },
                    ..
                }
                | TimelineItem::Pending { text, .. } => text.as_str(),
                TimelineItem::Logged { .. } => continue,
            };
            if prompts.last() != Some(&text) {
                prompts.push(text);
            }
        }
        prompts
    }
}

/// A message being written on this device, kept in the local store and never
/// synced (D5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    pub text: String,
    /// Position in `text`, counted in Unicode scalar values (`char`s), so it
    /// means the same on both sides of the FFI.
    pub cursor: usize,
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
        /// [`Action::Discard`](crate::Action::Discard) and
        /// [`Action::EditQueued`](crate::Action::EditQueued) take it back:
        /// the server never saw it, or refused it.
        discardable: bool,
        delivery: Option<Delivery>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    UserMessage {
        /// The send command that logged it; a [`TimelineItem::Pending`] with
        /// this id became this message.
        command_id: CommandId,
        text: String,
        from: DeviceId,
        /// Sent from this device.
        own: bool,
        /// Delivery state when this device sent it.
        outgoing: Option<Outgoing>,
        /// How it went out, when this device sent it.
        delivery: Option<Delivery>,
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
    /// The session's agent was shut down on request.
    Stopped,
    /// The session was renamed; `None` restored its default title.
    Renamed {
        name: Option<String>,
        by: DeviceId,
        /// Renamed from this device.
        own: bool,
    },
    /// The session was archived (`true`) or brought back from the archive.
    /// A message brings it back too, without an entry of its own.
    Archived {
        archived: bool,
        by: DeviceId,
        /// From this device.
        own: bool,
    },
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
    /// `rule` is the lasting rule granted with an approval, if any.
    Resolved {
        decision: Decision,
        by: DeviceId,
        by_me: bool,
        rule: Option<RuleSuggestion>,
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
    /// Lasting rules the agent offers to grant with an approval; an answer
    /// names one by its index
    /// ([`Action::ResolveApproval::rule`](crate::Action::ResolveApproval)).
    pub suggestions: Vec<RuleSuggestion>,
    /// This device's answer, if it gave one; queued like any command.
    pub answer: Option<Answer>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    pub decision: Decision,
    /// The rule granted with the approval, if any.
    pub rule: Option<RuleSuggestion>,
    pub outgoing: Outgoing,
    pub delivery: Option<Delivery>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SettingsView {
    pub blob_policy: BlobPolicy,
    pub server_address: Option<String>,
    pub device_id: Option<DeviceId>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn logged(seq: u64, entry: Entry) -> TimelineItem {
        TimelineItem::Logged {
            seq: Seq(seq),
            at: DateTime::UNIX_EPOCH,
            entry,
        }
    }

    fn message(seq: u64, text: &str, own: bool) -> TimelineItem {
        logged(
            seq,
            Entry::UserMessage {
                command_id: CommandId::new(),
                text: text.into(),
                from: DeviceId::new(),
                own,
                outgoing: None,
                delivery: None,
            },
        )
    }

    fn pending(text: &str) -> TimelineItem {
        TimelineItem::Pending {
            command_id: CommandId::new(),
            text: text.into(),
            outgoing: Outgoing::Queued,
            discardable: true,
            delivery: None,
        }
    }

    #[test]
    fn prompts_are_newest_first_without_repeats() {
        let view = SessionView {
            id: SessionId::new(),
            title: "x".into(),
            named: false,
            rename: None,
            archived: false,
            workdir: None,
            state: SessionState::Idle,
            timeline: vec![
                message(1, "first", true),
                logged(2, Entry::AgentMessage { text: "ok".into() }),
                message(3, "again", false),
                message(4, "again", true),
                logged(5, Entry::Interrupted),
                message(6, "first", true),
                pending("queued"),
                pending("queued"),
            ],
            pending_approval: None,
            rules: Vec::new(),
            interrupt: None,
            stop: None,
            draft: None,
            draft_rev: 0,
            last_seq: Seq(6),
            unread_from: None,
        };
        assert_eq!(view.prompts(), ["queued", "first", "again", "first"]);
    }
}
