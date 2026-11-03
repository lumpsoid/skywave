//! What heads render: immutable, fully owned per-screen snapshots
//! (architecture.md "Render states").
//!
//! Heads depend on `skywave-core` only, so every type a head needs is defined
//! or re-exported here. Every value is owned, `Clone` and `Send`, so a
//! snapshot can cross threads and, later, the FFI boundary (D13).

use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Utc};
use skywave_proto::Fingerprint;
use uuid::Uuid;

pub use skywave_proto::{
    AgentWork, ApprovalId, BackgroundTask, BlobId, BodyFormat, CallId, ChangeSet, ChangedFile,
    ChangesResult, CommandId, ContextId, Decision, DeviceId, DiffBody, DirEntry, Effort, EntryKind,
    FileBody, FileChange, FileResult, ImportOutcome, LinkTarget, ListResult, ModelChoice,
    PermissionMode, Refusal, RejectReason, RuleScope, RuleSuggestion, RunWhen, Schedule,
    ScheduleId, Seq, SessionId, ShellEnd, TaskId, TaskKind, ToolKind, TurnUsage,
};

pub use crate::projection::{SessionUsage, TurnRecord};

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
    /// [`Action::Back`](crate::Action::Back) goes somewhere from `screen`.
    /// When false, back leaves the app (or does whatever a head does at
    /// its root); heads show an up arrow only when true.
    pub can_go_back: bool,
    /// This device's saved prompts, the most recently used or saved first.
    pub snippets: Vec<Snippet>,
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
    /// those arriving during a catch-up and resolved before its end, or
    /// answered from this device, and never twice for one approval, not
    /// even across restarts — which approvals were alerted on is stored.
    /// One pending at start that no earlier core alerted on (pulled by a
    /// catch-up that ended before the link was online) is new, and alerts
    /// once the link is online, like one that came in during a catch-up.
    ///
    /// Counts from 0 in every core and only grows: it is always 0 in the
    /// first snapshot, as it is bumped only once online. Heads alert when
    /// it grows within the core's lifetime, taking 0, not the first
    /// snapshot, as where it started. Which approvals a bump was for is
    /// each one's [`PendingApprovalRef::alerted_at`].
    pub alerts: u64,
    /// Bumped once for each shell run of this device's that finished after
    /// running at least [`SHELL_ALERT_AFTER`], by the server's clock (its
    /// `ShellStarted` to its `ShellFinished`), unless its session was on
    /// screen in [`ComposerMode::Shell`] then. A run that never started
    /// never alerts. Read from the `ShellFinished` alone, so it works for a
    /// session not open and in a catch-up too: unlike
    /// [`alerts`](Self::alerts), it is not held back until online.
    ///
    /// Counts from 0 in every core and only grows, but unlike `alerts` it
    /// may already be above 0 in a head's first snapshot (a catch-up at
    /// start can finish first): heads alert when it grows past 0, not past
    /// the first snapshot. Nothing is stored: each batch is applied once.
    pub shell_alerts: u64,
    /// The run [`shell_alerts`](Self::shell_alerts) was last bumped for.
    pub last_shell: Option<ShellAlert>,
}

/// How long a shell run of this device's must have run for its finish to
/// alert ([`Attention::shell_alerts`]).
pub const SHELL_ALERT_AFTER: Duration = Duration::from_secs(10);

/// A finished shell run worth an alert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellAlert {
    pub session_id: SessionId,
    /// The session's title when the run finished, as
    /// [`SessionRow::title`]. Text from the server: heads sanitise it.
    pub title: String,
    /// The command's first line, as the server cut it. Text from the
    /// server: heads sanitise it.
    pub command_line: String,
    pub end: ShellEnd,
}

/// One pending approval, and which session waits on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApprovalRef {
    pub id: ApprovalId,
    pub session_id: SessionId,
    /// The session's title, as [`SessionRow::title`]. Text from the
    /// server: heads sanitise it before writing it anywhere raw.
    pub title: String,
    /// The value [`Attention::alerts`] was bumped to when this approval
    /// alerted in this core, or 0 if it has not alerted here: not yet,
    /// judged no news (answered here), or alerted by an earlier process.
    /// A head that saw `alerts` at `n` notifies for every approval with
    /// `alerted_at > n`, and never for one at 0.
    pub alerted_at: u64,
}

/// State of the connection to the server.
#[derive(Debug, Clone, PartialEq)]
pub enum Link {
    /// The device has never paired: there is no server to connect to.
    /// Pairing moves it to `Connecting`; it never comes back, as a paired
    /// device stays paired (a revoked one is `Rejected`).
    Unpaired,
    Connecting,
    Online {
        latency_ms: u32,
    },
    /// Pulling events; the local replica is rendered meanwhile.
    CatchingUp {
        have: Seq,
        server: Seq,
    },
    /// The link is down and waits to try again. Both times are wall-clock
    /// instants fixed when the attempt failed, so every snapshot of one
    /// wait carries the same value; a head counts down to `retry_at`
    /// against its own clock. Wall time, unlike a monotonic clock, keeps
    /// counting while a device sleeps.
    Reconnecting {
        /// Failed attempts in this outage, 1 for the first failure.
        attempt: u32,
        /// When the next attempt is due: the failure plus the backoff
        /// delay, or less after a `RetryNow`. After a sleep the attempt
        /// comes at most [`WALL_CHECK`](crate::WALL_CHECK) after it.
        retry_at: DateTime<Utc>,
        /// When the link first went down in this outage; kept across
        /// attempts until a pull succeeds.
        offline_since: DateTime<Utc>,
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
            OutboxStatus::Done(CommandResult::Rejected { reason }) => {
                Outgoing::Failed(FailReason::Rejected(*reason))
            }
            OutboxStatus::Undeliverable => Outgoing::Failed(FailReason::Undeliverable),
        }
    }
}

/// Why one of this device's commands failed for good.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailReason {
    /// The server answered with a refusal.
    Rejected(RejectReason),
    /// The server refused the request itself rather than answer it: a bug
    /// or a version skew, not a domain answer. Sending it again would fail
    /// the same way.
    Undeliverable,
}

impl FailReason {
    /// [`RejectReason::rules_out_answers`]; an undeliverable answer never
    /// applied, so another may be given.
    pub fn rules_out_answers(self) -> bool {
        match self {
            FailReason::Rejected(reason) => reason.rules_out_answers(),
            FailReason::Undeliverable => false,
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
    /// When the server's answer, or its refusal of the request, was recorded.
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
    Usage(UsageView),
    Workdir(WorkdirView),
}

/// Where the workdir screen is ([`Action::ShowWorkdir`](crate::Action::ShowWorkdir)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkdirPane {
    /// What changed against `HEAD`.
    Changes,
    /// One changed file's diff, from the stored changes. `untracked` tells
    /// apart the two entries one path can have
    /// ([`ChangeSet::files`](skywave_proto::ChangeSet::files)).
    Diff { path: String, untracked: bool },
    /// One directory; `path` relative to the workdir, empty for the
    /// workdir itself.
    Dir { path: String },
    /// One file; `path` relative to the workdir.
    File { path: String },
}

/// A session's workdir as last read from the server
/// ([`Action::OpenWorkdir`](crate::Action::OpenWorkdir)): read on request
/// only, and kept, so what was read shows offline too.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkdirView {
    pub session: SessionId,
    /// The session's title, as [`SessionRow::title`].
    pub title: String,
    pub workdir: Option<String>,
    pub pane: WorkdirPane,
    /// The directory the files pane shows when switched to: the last one
    /// shown, the workdir itself at first.
    pub last_dir: String,
    /// This pane's read (a diff's is the changes').
    pub fetch: WorkdirFetch,
    pub body: WorkdirBody,
}

/// The state of the read a workdir pane shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkdirFetch {
    Idle,
    /// Asked for and not answered yet.
    Loading,
    /// The last ask failed (offline, a timeout, git failing): the stored
    /// answer, if any, still shows.
    Failed(String),
}

/// What a workdir pane shows: the latest stored answer, with its time.
#[derive(Debug, Clone, PartialEq)]
pub enum WorkdirBody {
    /// Never read: offline, or before the answer.
    Nothing,
    Changes(ChangesResult),
    /// `file` from the stored changes, `None` once they no longer list it
    /// (or are no change set); `blob` its diff, if it has one.
    Diff {
        file: Option<ChangedFile>,
        blob: Option<BlobView>,
    },
    Dir(ListResult),
    /// `blob` the content, if the server sent it.
    File {
        result: FileResult,
        blob: Option<BlobView>,
    },
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
    /// This device keeps an unsent message or shell command for the
    /// session (a shell draft holding only a condition does not count).
    pub has_draft: bool,
    /// Agent output (messages, approval requests, errors) this device has
    /// not seen yet. Messages from any device never count.
    pub unread: u64,
    /// What the session has cost so far, in US dollars, when its agent
    /// reports cost.
    pub cost_usd: Option<f64>,
    /// [`Action::DeleteSessionLocally`](crate::Action::DeleteSessionLocally)
    /// would remove it: it is archived, has no pending approval, no running
    /// turn and no command from this device still unanswered.
    pub deletable: bool,
    /// The session adopted an outside conversation whose imported history
    /// is not all in the replica yet (its `ImportFinished` has not been
    /// pulled): a long import arrives over several pulls. Its
    /// [`PendingSession`] may show beside it until then.
    pub importing: bool,
}

/// What the usage screen totals: every turn in the replica that reported
/// usage, with the sessions they ran in. Heads pick the dates and group
/// the turns themselves (`skywave_present::usage`), so this holds the
/// turns as logged rather than sums.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageView {
    /// In log order. Archived sessions' turns are here; a session deleted
    /// from this device took its turns with it.
    pub turns: Vec<TurnRecord>,
    /// Every session in the replica, archived ones included.
    pub sessions: Vec<UsageSession>,
    /// How many sessions were deleted from this device, whose turns no
    /// longer count.
    pub deleted: usize,
}

/// A session as the usage screen names it.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageSession {
    pub id: SessionId,
    /// As [`SessionRow::title`]; text from the server.
    pub title: String,
    pub workdir: Option<String>,
    pub archived: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PendingSession {
    pub command_id: CommandId,
    pub workdir: String,
    /// The outside conversation it adopts, by the agent's own id: the
    /// server imports its history when the command arrives. `None`: a
    /// fresh conversation.
    pub conversation: Option<String>,
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
    /// While [`viewing`](Self::viewing) a context, only that context's
    /// items, with no context dividers. Only the items of the agent shown:
    /// the main one, or the [`subagent`](Self::subagent).
    pub timeline: Vec<TimelineItem>,
    /// The contexts a picker offers, in log order: each one with a message
    /// (logged, or this device's on its way) and the current one, this
    /// device's clear on its way included. Empty until the session's
    /// events are loaded.
    pub contexts: Vec<ContextView>,
    /// The context a message sent now goes to: the session's current one,
    /// or the one this device's clear or switch on its way makes current.
    pub context: ContextId,
    /// The context [`Action::ViewContext`](crate::Action::ViewContext)
    /// narrowed the timeline to; `None` shows the whole session. A
    /// [`subagent`](Self::subagent) shown wins over it.
    pub viewing: Option<ContextId>,
    /// The subagent [`Action::ViewSubagent`](crate::Action::ViewSubagent)
    /// shows: the timeline holds only that subagent's own items (no
    /// context dividers, none of this device's messages). `None` shows the
    /// main agent's timeline, without any subagent's items.
    pub subagent: Option<SubagentHeader>,
    /// [`Action::ClearContext`](crate::Action::ClearContext) is worth
    /// offering: the current context has a message, logged or on its way.
    pub can_clear: bool,
    /// This device's latest clear of the session's context, until its
    /// effect is in the replica.
    pub clear: Option<Outgoing>,
    /// This device's latest switch of the session's context, until its
    /// effect is in the replica.
    pub switch: Option<Outgoing>,
    /// The approval the agent is blocked on, if any.
    pub pending_approval: Option<ApprovalPrompt>,
    /// The lasting rules granted with approvals in this session, by any
    /// device, in log order. Only what was granted through Skywave: rules
    /// the agent keeps elsewhere are not seen.
    pub rules: Vec<RuleSuggestion>,
    /// What the session's turns took, the newest one and the sums; `None`
    /// until the agent reports any.
    pub usage: Option<SessionUsage>,
    /// The agent's permission mode and model, and the switches on their way.
    pub agent: AgentView,
    /// This device's latest interrupt of the session, until its effect is in
    /// the replica.
    pub interrupt: Option<Outgoing>,
    /// This device's latest stop of the session, until its effect is in the
    /// replica.
    pub stop: Option<Outgoing>,
    /// What the agent runs outside a turn and has scheduled, as it reported
    /// it last. Shown where the user looks for it, never as an always-on
    /// indicator.
    pub work: AgentWork,
    /// This device's stops of a background task still on their way: each
    /// task named by a [`Action::StopTask`](crate::Action::StopTask) of
    /// this device, with the newest such command's progress. Gone once the
    /// task leaves [`work`](Self::work) or a refusal answering the command
    /// is in the log. A stop the server rejected (e.g. the task had ended)
    /// shows as failed while its task is still listed. Accepted is not
    /// done: the agent may yet fail to stop the task.
    pub task_stops: Vec<(TaskId, Outgoing)>,
    /// What the composer writes: messages to the agent or shell commands.
    /// This device's choice, kept per session; [`ComposerMode::Agent`]
    /// until [`Action::SetComposerMode`](crate::Action::SetComposerMode)
    /// says otherwise.
    pub mode: ComposerMode,
    /// This device's unsent message, as last saved.
    pub draft: Option<Draft>,
    /// This device's unsent shell command, as last saved, with the
    /// condition picked for it ([`Draft::when`]). Kept even with blank
    /// text when a condition other than [`RunWhen::Always`] was picked.
    pub shell_draft: Option<Draft>,
    /// Changes when the core itself rewrites a draft (e.g.
    /// [`Action::EditQueued`](crate::Action::EditQueued)): a head that
    /// keeps its own copy of the text being written replaces it with
    /// `draft` or `shell_draft` then.
    pub draft_rev: u64,
    /// The session's shell runs, whatever [`viewing`](Self::viewing) or
    /// [`subagent`](Self::subagent) narrows the timeline to: they are
    /// session-wide and never in the timeline.
    pub shell: ShellView,
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
    /// Taken from the timeline as shown: one context's while
    /// [`viewing`](Self::viewing) it.
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

/// A session's shell commands as the session view shows them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ShellView {
    /// Whether the server runs shell commands, as its last `hello` said;
    /// stored, so it shows offline too. `false` until a server said so.
    pub enabled: bool,
    /// Where the session's next run starts, as last known: the `cwd` of the
    /// newest `ShellStarted`, or of a newer `ShellFinished` that knew one,
    /// whichever is later in the log (the server starts a run where the
    /// last finished run that knew its cwd ended, or in the workdir when
    /// that is gone, and `ShellStarted` says which). `None` before any.
    pub cwd: Option<String>,
    /// Every device's logged runs in log order, then this device's not yet
    /// in the log, in send order. Empty of logged runs until the session's
    /// events are loaded.
    pub runs: Vec<ShellRun>,
}

impl ShellView {
    /// The commands run in the session, newest first, for a head to recall
    /// into its shell composer: every device's logged runs, then this
    /// device's not yet logged, a command repeated back to back listed once.
    pub fn commands(&self) -> Vec<&str> {
        let mut commands: Vec<&str> = Vec::new();
        for run in self.runs.iter().rev() {
            if commands.last() != Some(&run.command.as_str()) {
                commands.push(&run.command);
            }
        }
        commands
    }
}

/// One shell run of a session.
#[derive(Debug, Clone, PartialEq)]
pub struct ShellRun {
    /// The run's id: its `RunShell` command's. What
    /// [`Action::StopShell`](crate::Action::StopShell),
    /// [`Action::Discard`](crate::Action::Discard) and
    /// [`Action::EditQueued`](crate::Action::EditQueued) name.
    pub run: CommandId,
    /// As written, unparsed. Text from the server for another device's.
    pub command: String,
    pub when: RunWhen,
    /// Who asked for it.
    pub by: DeviceRef,
    pub state: ShellRunState,
    /// When the server logged its request; `None` while
    /// [`Pending`](ShellRunState::Pending) (its [`delivery`](Self::delivery)
    /// says when this device queued it).
    pub requested_at: Option<DateTime<Utc>>,
    /// When it started, by the server's clock; `None` until then and for a
    /// run that never started.
    pub started_at: Option<DateTime<Utc>>,
    /// When it finished, by the server's clock.
    pub finished_at: Option<DateTime<Utc>>,
    /// The output's tail: the newest `ShellOutput`'s while running, the
    /// `ShellFinished` preview once finished; empty before any output.
    /// ANSI stripped, never ending in a line break. Text from the server.
    pub tail_or_preview: String,
    /// The output's line count so far, or in all once finished.
    pub lines: u32,
    /// The whole output, once finished, when the preview does not hold it.
    pub output: Option<BlobView>,
    /// The preview is cut: [`output`](Self::output) has the rest.
    pub truncated: bool,
    /// This device's `RunShell`'s progress, while it is in the outbox.
    pub outgoing: Option<Outgoing>,
    /// [`Action::Discard`](crate::Action::Discard) and
    /// [`Action::EditQueued`](crate::Action::EditQueued) take it back: the
    /// server never saw it, or refused it.
    pub discardable: bool,
    /// A stop of it is on its way or logged: any device's
    /// `ShellStopRequested`, or this device's `StopShell` not refused. A
    /// stopping run may still be running for a while (the server's grace
    /// before it kills).
    pub stopping: bool,
    pub delivery: Option<Delivery>,
}

impl ShellRun {
    /// This device's run that failed for good on its way (refused, or
    /// undeliverable): it will never run, though it stays in sight to take
    /// back.
    pub fn refused(&self) -> bool {
        matches!(self.outgoing, Some(Outgoing::Failed(_)))
    }
}

/// Where a shell run is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellRunState {
    /// This device's run not in the log yet: queued here, on its way,
    /// accepted but not pulled, or refused.
    Pending,
    /// Logged, waiting for its turn in the session's queue.
    Queued,
    /// Started and not finished.
    Running,
    Finished(ShellEnd),
}

/// One of a session's contexts as a picker lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextView {
    pub id: ContextId,
    /// As the log names it (a session's first context from before contexts
    /// existed reads "start").
    pub name: String,
    /// First line of the context's first message, logged or else this
    /// device's on its way.
    pub first_prompt: Option<String>,
    /// Its messages: every device's logged ones, plus this device's on
    /// their way (not refused).
    pub messages: usize,
    /// Time of the context's newest logged event; `None` for a context
    /// this device's clear has not logged yet.
    pub last_activity: Option<DateTime<Utc>>,
    /// Messages sent now go here ([`SessionView::context`]).
    pub current: bool,
    /// This device's latest clear or switch making it current, or move of
    /// it, until its effect is in the replica.
    pub outgoing: Option<Outgoing>,
    /// The session it was moved into; it is never current here again.
    pub moved_to: Option<SessionRef>,
    /// This device's move of it that [`Action::Discard`](crate::Action::Discard)
    /// takes back: refused (it shows in [`outgoing`](Self::outgoing)), or
    /// not sent yet.
    pub discard_move: Option<CommandId>,
    /// [`Action::MoveContext`](crate::Action::MoveContext) is worth
    /// offering: its agent has worked in it (so there is a conversation to
    /// fork), it is not moved nor is this device's move of it on its way,
    /// and no message of this device to it is still outside the replica
    /// (it would go out before the clear that comes with moving the
    /// current context, and the clear would interrupt the turn it starts).
    pub can_move: bool,
}

/// A session's agent settings as the session view shows them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentView {
    /// The mode the agent last said it runs in; `None` until it says.
    pub mode: Option<PermissionMode>,
    /// The model the agent last said it runs, as it names it.
    pub model: Option<String>,
    /// A mode switch not yet answered: this device's while on its way, else
    /// any device's waiting for the agent.
    pub mode_change: Option<SettingChange<PermissionMode>>,
    /// A model switch likewise.
    pub model_change: Option<SettingChange<String>>,
    /// The effort the agent last said it runs at: `None` until it says,
    /// `Some(None)` when its model takes no effort level.
    pub effort: Option<Option<Effort>>,
    /// An effort switch likewise.
    pub effort_change: Option<SettingChange<Effort>>,
    /// The agent's newest refusal of a mode, until the next switch.
    pub mode_refused: Option<Refusal<PermissionMode>>,
    /// The agent's newest refusal of a model, until the next switch.
    pub model_refused: Option<Refusal<String>>,
    /// The agent's newest refusal of an effort, until the next switch.
    pub effort_refused: Option<Refusal<Effort>>,
    /// What [`Action::SetModel`](crate::Action::SetModel) offers: the
    /// newest list any session's agent reported. Empty until one did; a
    /// head may still send a model by name.
    pub models: Vec<ModelChoice>,
}

impl AgentView {
    /// The offered model that is `model` (an id the agent resolves, or the
    /// name it resolves to), preferring one whose id is `model`.
    pub fn choice(&self, model: &str) -> Option<&ModelChoice> {
        self.models.iter().find(|m| m.id == model).or_else(|| {
            self.models
                .iter()
                .find(|m| m.resolved.as_deref() == Some(model))
        })
    }

    /// What to call the model running: its offered label, else its name.
    pub fn model_label(&self) -> Option<&str> {
        let model = self.model.as_deref()?;
        Some(self.choice(model).map_or(model, |m| m.label.as_str()))
    }

    /// The effort levels to offer: those the model running takes, or the one
    /// a pending switch asks for. All of them when no offered model matches
    /// (one typed by name, or no list yet), for the agent's answer to
    /// settle; none when the model takes none.
    pub fn efforts(&self) -> &[Effort] {
        let model = self
            .model_change
            .as_ref()
            .map(|change| change.value.as_str())
            .or(self.model.as_deref());
        match model.and_then(|model| self.choice(model)) {
            Some(choice) => &choice.efforts,
            None => &Effort::ALL,
        }
    }
}

/// A switch of a setting not yet answered.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingChange<T> {
    pub value: T,
    /// This device's command while it is on its way; `None` once it is in
    /// the log and only the agent's answer is awaited.
    pub outgoing: Option<Outgoing>,
}

/// A mode, model or effort as a timeline entry names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentSetting {
    Mode(PermissionMode),
    /// As the agent names it.
    Model(String),
    /// `None` when the model now takes no effort level.
    Effort(Option<Effort>),
}

/// What a session's composer writes on this device: a message to the agent
/// or a shell command for the server. Never synced (D5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ComposerMode {
    #[default]
    Agent,
    Shell,
}

/// A message or shell command being written on this device, kept in the
/// local store and never synced (D5). A session has one per
/// [`ComposerMode`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    pub text: String,
    /// Position in `text`, counted in Unicode scalar values (`char`s), so it
    /// means the same on both sides of the FFI.
    pub cursor: usize,
    /// The condition the shell command runs on once sent. Only a shell
    /// draft has one; an agent draft's is always [`RunWhen::Always`].
    pub when: RunWhen,
}

/// Identifies a [`Snippet`]; made on this device and never sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SnippetId(pub Uuid);

impl SnippetId {
    /// A fresh, time-ordered (UUIDv7) id.
    pub fn new() -> SnippetId {
        SnippetId(Uuid::now_v7())
    }
}

impl Default for SnippetId {
    fn default() -> SnippetId {
        SnippetId::new()
    }
}

impl From<Uuid> for SnippetId {
    fn from(uuid: Uuid) -> SnippetId {
        SnippetId(uuid)
    }
}

/// A prompt saved on this device to use again in any session, kept in the
/// local store and never synced (D5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snippet {
    pub id: SnippetId,
    /// What the user called it; `None` shows its text's first line.
    pub name: Option<String>,
    pub text: String,
    /// The composer it is for: a prompt or a shell command. A text is
    /// saved once per kind.
    pub kind: ComposerMode,
    /// Orders snippets by when they were last saved or used: higher is
    /// later.
    pub used: u64,
}

#[derive(Debug, Clone, PartialEq)]
#[expect(
    clippy::large_enum_variant,
    reason = "nearly every item is `Logged`; boxing it would cost an allocation per item to save space only on the few `Pending` ones"
)]
pub enum TimelineItem {
    /// An event from the server log.
    Logged {
        seq: Seq,
        at: DateTime<Utc>,
        /// The context the event belongs to; a context divider has the
        /// context it makes current.
        context: ContextId,
        /// The subagent's `Agent` call the item belongs to; `None` for the
        /// main agent's. An approval or a result standalone takes its
        /// call's.
        parent: Option<CallId>,
        entry: Entry,
    },
    /// A message from this device that is not in the log yet.
    Pending {
        command_id: CommandId,
        /// The context it was written in, which must still be current
        /// when it reaches the server.
        context: ContextId,
        text: String,
        outgoing: Outgoing,
        /// [`Action::Discard`](crate::Action::Discard) and
        /// [`Action::EditQueued`](crate::Action::EditQueued) take it back:
        /// the server never saw it, or refused it.
        discardable: bool,
        delivery: Option<Delivery>,
    },
}

/// A device the log names, as the views show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRef {
    pub id: DeviceId,
    /// The name it paired with, once the server listed it; `None` for a
    /// device this one has not heard of yet.
    pub name: Option<String>,
    /// This device.
    pub me: bool,
}

impl DeviceRef {
    /// How heads name the device: its name, or else the end of its id
    /// (the start of a UUIDv7 is a timestamp, the end is random).
    pub fn label(&self) -> String {
        match &self.name {
            Some(name) => name.clone(),
            None => {
                let id = self.id.0.simple().to_string();
                format!("device …{}", &id[id.len() - 8..])
            }
        }
    }
}

/// A session the log names, as the views show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRef {
    pub id: SessionId,
    /// Its title as this device knows it; `None` for a session it deleted
    /// or has not pulled yet.
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    UserMessage {
        /// The send command that logged it; a [`TimelineItem::Pending`] with
        /// this id became this message.
        command_id: CommandId,
        text: String,
        /// Who sent it; [`DeviceRef::me`] when this device did.
        from: DeviceRef,
        /// Delivery state when this device sent it.
        outgoing: Option<Outgoing>,
        /// How it went out, when this device sent it.
        delivery: Option<Delivery>,
    },
    AgentMessage {
        text: String,
    },
    /// One tool call with its approval and result folded in, placed at the
    /// call's seq.
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
        /// This device's answer, if it gave one (see [`ToolApproval::answer`]).
        answer: Option<Answer>,
    },
    /// The agent's turn was stopped by an interrupt.
    Interrupted,
    /// The session's agent was shut down on request.
    Stopped,
    /// A device cleared the session's context: the context `name` started
    /// afresh and became current.
    ContextStarted {
        name: String,
        by: DeviceRef,
    },
    /// A device made the earlier context `name` current again. A switch to
    /// the context already current has no entry.
    ContextSwitched {
        name: String,
        by: DeviceRef,
    },
    /// A device moved the context `name` into the session `to`, where it
    /// goes on; here it is never current again.
    ContextMoved {
        name: String,
        to: SessionRef,
        by: DeviceRef,
    },
    /// The session's first item when it was made by moving the context
    /// `name` out of `from`: its conversation goes on here.
    MovedFrom {
        from: SessionRef,
        name: String,
    },
    /// The session's first item when it was made by adopting the agent's
    /// outside conversation `conversation`. What the server could read of
    /// that conversation follows as [`Entry::Imported`] items, then one
    /// [`Entry::ImportFinished`]; the session's own entries come after.
    Adopted {
        conversation: String,
    },
    /// One item of an adopted conversation's history, as the agent kept it
    /// outside Skywave: a record, never unread news and never running.
    /// `at` is when the agent recorded it, or when the server logged it if
    /// the agent did not say. Part of the session's first context.
    Imported {
        at: DateTime<Utc>,
        entry: ImportedEntry,
    },
    /// Where an adopted conversation's imported history ends and the
    /// session's own begins, and how the import went. Not shown when the
    /// timeline is narrowed to one context.
    ImportFinished {
        outcome: ImportOutcome,
    },
    /// The session was renamed; `None` restored its default title.
    Renamed {
        name: Option<String>,
        by: DeviceRef,
    },
    /// The session was archived (`true`) or brought back from the archive.
    /// A message brings it back too, without an entry of its own.
    Archived {
        archived: bool,
        by: DeviceRef,
    },
    /// The agent reported an error.
    AgentErrored {
        message: String,
    },
    /// A turn ended and its agent reported what it took. A figure the agent
    /// does not report is `None`.
    TurnUsage(TurnUsage),
    /// The agent switched its mode or model, or refused to (`refused` holds
    /// why; it keeps what it had). `by` is the device that asked for it;
    /// `None` when the agent switched on its own (leaving plan mode) or
    /// restored what it ran with before.
    AgentSetting {
        setting: AgentSetting,
        by: Option<DeviceRef>,
        refused: Option<String>,
    },
}

/// What an [`Entry::Imported`] holds.
#[derive(Debug, Clone, PartialEq)]
pub enum ImportedEntry {
    /// A prompt the user gave the agent outside Skywave; no device sent it.
    UserMessage {
        text: String,
    },
    AgentMessage {
        text: String,
    },
    /// An imported call with its imported result folded in. Without a
    /// result its outcome is [`ToolOutcome::NoResult`], never `Running`:
    /// nothing is waiting on it. It has no approval and no subagent status.
    Tool(Box<ToolView>),
    /// An imported result no imported call links to (the agent's own call
    /// was not on the imported branch), or a second one for a call.
    ToolResult {
        summary: String,
        output: Option<BlobView>,
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
    /// How the call's own outcome stands. A [`ToolKind::Subagent`] call's
    /// says how its launch went (a background one is `Done` once launched);
    /// heads show [`subagent`](Self::subagent) instead.
    pub outcome: ToolOutcome,
    /// What the subagent a [`ToolKind::Subagent`] call runs is doing;
    /// `None` for any other call.
    pub subagent: Option<SubagentStatus>,
}

/// How the subagent an `Agent` call runs stands, from its own items.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentStatus {
    /// The background task that runs it while that task is listed in
    /// [`SessionView::work`]; `None` for a foreground subagent, and once
    /// the task left.
    pub task: Option<TaskId>,
    /// Still working: its task is listed, or (with no task) its call is
    /// still running.
    pub live: bool,
    /// The tool calls it made.
    pub steps: u32,
    /// What it did last: its newest tool call's title, or its newest
    /// message's first line, whichever came later.
    pub last: Option<String>,
}

/// The subagent a [`SessionView`] shows.
#[derive(Debug, Clone, PartialEq)]
pub struct SubagentHeader {
    /// Its `Agent` call.
    pub call: CallId,
    /// As the call's [`ToolView::title`].
    pub title: String,
    /// As the call's [`ToolView::detail`] (e.g. the kind of subagent).
    pub detail: Option<String>,
    /// What it was asked: the call's [`ToolView::body`].
    pub prompt: Option<BodyView>,
    pub status: SubagentStatus,
    /// The subagents it runs under, outermost first, each `Agent` call with
    /// its title; empty for one the main agent launched.
    pub trail: Vec<(CallId, String)>,
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
    /// This device's answer, if it gave one. Once the approval is settled,
    /// this is where an answer that did not apply stays in sight: refused
    /// because another device answered first or the approval was cancelled.
    pub answer: Option<Answer>,
}

/// How a tool call stands.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolOutcome {
    /// No result yet, possibly waiting on its approval; `since` is the
    /// call's time.
    Running { since: DateTime<Utc> },
    /// The tool finished; `took` runs from the call to its result. For an
    /// imported call it is zero (unknown) unless the agent timed both.
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
    /// Its approval was denied, so the tool did not run.
    Denied { by: DeviceRef },
    /// Its approval was cancelled before anyone answered.
    Cancelled { reason: String },
    /// The server logged that it will never get a result
    /// ([`EventKind::CallsUnanswered`](skywave_proto::EventKind::CallsUnanswered)).
    NoResult,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalStatus {
    Pending,
    /// First answer wins (D5); `by` is who answered. `rule` is the lasting
    /// rule granted with an approval, if any.
    Resolved {
        decision: Decision,
        by: DeviceRef,
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
    /// The title of the subagent whose call it gates; `None` for the main
    /// agent's (or a call not logged). Its item shows only in that
    /// subagent's view.
    pub from: Option<String>,
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
    /// Refused while the approval still waits, so this device may answer
    /// again: the refusal was of the answer itself (`UnknownRule`). An
    /// answer refused because the approval is closed on the server
    /// ([`FailReason::rules_out_answers`]: answered elsewhere, cancelled)
    /// did not apply and cannot be given again; nor can any once the
    /// approval is settled.
    pub retryable: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SettingsView {
    pub blob_policy: BlobPolicy,
    pub server_address: Option<String>,
    pub device_id: Option<DeviceId>,
    /// The fingerprint of the server CA pinned at pairing, to compare with
    /// what `skywaved` prints; `None` if unknown.
    pub server_fingerprint: Option<Fingerprint>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn logged(seq: u64, entry: Entry) -> TimelineItem {
        TimelineItem::Logged {
            parent: None,
            seq: Seq(seq),
            at: DateTime::UNIX_EPOCH,
            context: ContextId::new(),
            entry,
        }
    }

    fn message(seq: u64, text: &str, own: bool) -> TimelineItem {
        logged(
            seq,
            Entry::UserMessage {
                command_id: CommandId::new(),
                text: text.into(),
                from: DeviceRef {
                    id: DeviceId::new(),
                    name: None,
                    me: own,
                },
                outgoing: None,
                delivery: None,
            },
        )
    }

    fn pending(text: &str) -> TimelineItem {
        TimelineItem::Pending {
            command_id: CommandId::new(),
            context: ContextId::new(),
            text: text.into(),
            outgoing: Outgoing::Queued,
            discardable: true,
            delivery: None,
        }
    }

    #[test]
    fn prompts_are_newest_first_without_repeats() {
        let view = SessionView {
            subagent: None,
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
            usage: None,
            agent: AgentView::default(),
            contexts: Vec::new(),
            context: ContextId::new(),
            viewing: None,
            can_clear: false,
            clear: None,
            switch: None,
            interrupt: None,
            stop: None,
            work: AgentWork::default(),
            task_stops: Vec::new(),
            mode: ComposerMode::Agent,
            draft: None,
            shell_draft: None,
            draft_rev: 0,
            shell: ShellView::default(),
            last_seq: Seq(6),
            unread_from: None,
        };
        assert_eq!(view.prompts(), ["queued", "first", "again", "first"]);
    }
}
