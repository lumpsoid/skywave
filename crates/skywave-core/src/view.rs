//! What heads render: immutable, fully owned per-screen snapshots
//! (architecture.md "Render states").
//!
//! Heads depend on `skywave-core` only, so every type a head needs is defined
//! or re-exported here. Every value is owned, `Clone` and `Send`, so a
//! snapshot can cross threads and, later, the FFI boundary (D13).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use skywave_proto::{Fingerprint, ImageType, Worktree, WorktreeList};
use uuid::Uuid;

pub use skywave_proto::{
    AgentWork, ApprovalId, BackgroundTask, BlobId, BodyFormat, CallId, ChangeSet, ChangedFile,
    ChangesResult, CommandId, ContextId, Decision, DeviceId, DiffBody, DirEntry, Effort, EntryKind,
    FileBody, FileChange, FileResult, ImportOutcome, LinkTarget, ListResult, LogResult,
    ModelChoice, PermissionMode, Question, QuestionAnswer, QuestionOption, Refusal, RejectReason,
    RuleScope, RuleSuggestion, RunWhen, Schedule, ScheduleId, Seq, SessionId, ShellEnd,
    SubagentUsage, TaskId, TaskKind, ToolKind, TurnUsage,
};
pub use skywave_proto::{
    BlobStorage, BlobUse, LineSpan, SessionStorage, ShellRunStorage, StorageReport,
};
pub use skywave_proto::{LimitSeverity, LimitWindow, UsageLimit, UsageLimitsResult};

pub use crate::projection::{SessionUsage, TurnRecord};

use crate::address::ServerAddress;
use crate::attachment::AttachedImage;
use crate::store::{AttachmentRefusal, BlobPolicy, OutboxEntry, OutboxStatus};
use crate::workdir::PushOutcome;

/// One full snapshot of everything a head shows. Pushed whole after every
/// change; there are no diffs.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewState {
    /// Link banner data; present on every screen.
    pub link: Link,
    /// Own commands persisted locally and not yet settled: neither answered
    /// by the server nor given up on as undeliverable ("offline · N queued").
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
    /// Shell commands from every session on this device, newest first, each
    /// once (its newest run), at most a thousand.
    pub shell_history: Arc<Vec<String>>,
    /// When the agent's login on the server expires, as the server's last
    /// `hello` said; `None` when it does not or the server cannot tell.
    /// Present on every screen: heads warn as it nears
    /// (`skywave_present::login`).
    pub login_expires: Option<DateTime<Utc>>,
    /// Whether this device may log the server's agent in, as the server's
    /// last `hello` said; `false` until one said so.
    pub agent_login_offered: bool,
    /// Whether the server's agent is logged out, as the server's last
    /// `hello` said: its latest definite word, `false` when logged in or
    /// not known. Present on every screen, offered or not.
    pub agent_logged_out: bool,
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
    /// The device is not paired: there is no server to connect to. Pairing
    /// moves it to `Connecting`; it never comes back, as a paired device
    /// stays paired (a revoked one is `Rejected`). Only the core decides
    /// it, from the pairing: the sync engine reports a [`PairedLink`].
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
        /// Failed connections since the last one that made progress (a
        /// pull returned or a command was settled), 1 for the first.
        attempt: u32,
        /// When the next attempt is due: the failure plus the backoff
        /// delay, or less after a `RetryNow`. After a sleep the attempt
        /// comes at most [`WALL_CHECK`](crate::WALL_CHECK) after it.
        retry_at: DateTime<Utc>,
        /// When the link went down in this offline stretch: the first
        /// failure since `hello` last succeeded, kept across the attempts
        /// that fail before it.
        offline_since: DateTime<Utc>,
        /// Why the attempt that started this wait failed.
        reason: LinkFailure,
    },
    /// The link cannot work until the user acts, so retries stop until the
    /// next `Connect` (re-pairing, or a retry where
    /// [`LinkRejection::can_retry`] says one may help). Local data stays
    /// readable.
    Rejected(LinkRejection),
}

/// The [`Link`] of a paired device: what the sync engine reports
/// ([`Seam::set_link`](crate::Seam::set_link)). It has no `Unpaired`, which
/// only the pairing decides. Each variant means what [`Link`]'s does.
#[derive(Debug, Clone, PartialEq)]
pub enum PairedLink {
    Connecting,
    Online {
        latency_ms: u32,
    },
    CatchingUp {
        have: Seq,
        server: Seq,
    },
    Reconnecting {
        attempt: u32,
        retry_at: DateTime<Utc>,
        offline_since: DateTime<Utc>,
        reason: LinkFailure,
    },
    Rejected(LinkRejection),
}

impl From<PairedLink> for Link {
    fn from(link: PairedLink) -> Link {
        match link {
            PairedLink::Connecting => Link::Connecting,
            PairedLink::Online { latency_ms } => Link::Online { latency_ms },
            PairedLink::CatchingUp { have, server } => Link::CatchingUp { have, server },
            PairedLink::Reconnecting {
                attempt,
                retry_at,
                offline_since,
                reason,
            } => Link::Reconnecting {
                attempt,
                retry_at,
                offline_since,
                reason,
            },
            PairedLink::Rejected(rejection) => Link::Rejected(rejection),
        }
    }
}

/// Why a connection failed in a way worth retrying: what a head shows
/// beside the countdown while [`Link::Reconnecting`]. Coarse on purpose:
/// it says where things broke, not the transport's own message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkFailure {
    /// No connection: DNS, TCP connect or a refused connection.
    Unreachable,
    /// Connecting or a request took too long.
    TimedOut,
    /// The connection broke during a request.
    ConnectionLost,
    /// The TLS handshake failed for a reason other than the server's
    /// identity (which is [`LinkRejection::ServerChanged`] or
    /// [`LinkRejection::NameMismatch`]).
    Handshake,
    /// The server is in another run than the connection expected: it
    /// restarted.
    ServerRestarted,
    /// The server answered with this HTTP status: a 5xx, or a 4xx that is
    /// about neither the device nor one command.
    ServerError(u16),
    /// The server's reply could not be understood.
    BadResponse,
    /// The local replica could not be read or written.
    Storage,
}

/// Why the link stopped for good.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkRejection {
    /// The device's certificate was revoked (`skywaved devices revoke`).
    Revoked,
    /// The server's certificate does not chain to the CA this device
    /// pinned when it paired: the server's CA was regenerated, or something
    /// else answers at the address. Every handshake would fail the same way.
    ServerChanged,
    /// The server's certificate does not cover the address this device
    /// connects to: its names changed (a host rename, or a reissue that
    /// dropped the name) while the CA stayed.
    NameMismatch,
    /// This device's stored identity (certificate or key) cannot be used
    /// for TLS: the file is damaged.
    BadIdentity,
    /// Client and server speak different protocol versions.
    ProtocolMismatch { client: u32, server: u32 },
    /// The server's log lost events this replica holds (a crash dropped
    /// its last commits, or it was restored from an older backup or
    /// replaced). The replica, the only copy of what was lost, stays
    /// readable and nothing is sent until the user resets it
    /// ([`Action::ResetReplica`](crate::Action::ResetReplica)). `have` is
    /// how far this device had seen the log, `server` the head it reports.
    HistoryLost { have: Seq, server: Seq },
}

impl LinkRejection {
    /// Only pairing again (a new identity, or a new pinned CA or address)
    /// brings the link back, so a head offers re-pairing.
    pub fn needs_repair(self) -> bool {
        match self {
            LinkRejection::Revoked
            | LinkRejection::ServerChanged
            | LinkRejection::NameMismatch
            | LinkRejection::BadIdentity => true,
            LinkRejection::ProtocolMismatch { .. } | LinkRejection::HistoryLost { .. } => false,
        }
    }

    /// Something on the server's side may fix it without this device
    /// changing, so [`Action::RetryNow`](crate::Action::RetryNow) connects
    /// again: the server was updated, its old CA or a dropped name restored,
    /// or a network that intercepted TLS left behind.
    pub fn can_retry(self) -> bool {
        match self {
            LinkRejection::ProtocolMismatch { .. }
            | LinkRejection::ServerChanged
            | LinkRejection::NameMismatch => true,
            LinkRejection::Revoked
            | LinkRejection::BadIdentity
            | LinkRejection::HistoryLost { .. } => false,
        }
    }
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
            OutboxStatus::AttachmentRefused(reason) => {
                Outgoing::Failed(FailReason::AttachmentRefused(*reason))
            }
        }
    }
}

/// Why one of this device's commands failed for good.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailReason {
    /// The server answered with a refusal.
    Rejected(RejectReason),
    /// The server refused the request itself rather than answer it (a bug
    /// or a version skew, not a domain answer), kept failing it with server
    /// errors ([`MAX_SERVER_ERRORS`](crate::MAX_SERVER_ERRORS)), or never
    /// logged a command whose reply was unreadable. It never applied, and
    /// it is not sent again.
    Undeliverable,
    /// The device gave up on the message's image before sending it; it
    /// never applied.
    AttachmentRefused(AttachmentRefusal),
}

impl FailReason {
    /// [`RejectReason::rules_out_answers`]; an undeliverable answer never
    /// applied, so another may be given.
    pub fn rules_out_answers(self) -> bool {
        match self {
            FailReason::Rejected(reason) => reason.rules_out_answers(),
            FailReason::Undeliverable | FailReason::AttachmentRefused(_) => false,
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
    /// The turn is idle but the agent still has background work: a task
    /// (ambient ones too) or a schedule.
    BackgroundWork,
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
    /// `paused` names what the download waits for, if anything.
    Downloading {
        got: u64,
        size: u64,
        paused: Option<Pause>,
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

/// What a download in progress waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pause {
    /// The link is down; it resumes once the link is back.
    Offline,
    /// This device's disk is full: every blob download stopped, keeping
    /// what it got, until a download is asked for (or the app restarts).
    DiskFull,
}

/// A blob referenced from the timeline, with its download state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobView {
    /// The blob holding what is left of the content: the event's own, or
    /// the last crop's when [`change`](Self::change) says it was cropped.
    pub id: BlobId,
    pub state: BlobState,
    /// What the server did to this content, if anything.
    pub change: Option<BlobChange>,
    /// This device's latest delete or crop of this blob until its effect is
    /// in the replica; a refusal stays until discarded.
    pub pending: Option<BlobPending>,
}

/// An image on a user message: its type, and the image's blob as it stands:
/// downloadable (or this device's own file), deleted, or cropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageImage {
    pub media_type: ImageType,
    pub content: FullView,
}

/// This device's own delete or crop of a blob
/// ([`Action::DeleteBlob`](crate::Action::DeleteBlob),
/// [`Action::CropBlob`](crate::Action::CropBlob)) on its way, or refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobPending {
    /// For [`Action::Discard`](crate::Action::Discard).
    pub command: CommandId,
    pub kind: BlobCommand,
    pub outgoing: Outgoing,
    /// Whether [`Action::Discard`](crate::Action::Discard) takes it back:
    /// until the server accepts it.
    pub discardable: bool,
}

/// Which of the two a [`BlobPending`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobCommand {
    Delete,
    Crop,
}

/// What the server did to a blob's content that left some of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobChange {
    /// Cropped to `kept` of the original content's `of` lines; the
    /// [`BlobView`]'s `id` is the cropped blob. After a crop of a crop,
    /// `kept` is what the last crop kept and `by` its device, while `of`
    /// stays the original's count: every crop keeps lines of the original.
    Cropped { kept: u32, of: u32, by: DeviceRef },
}

/// The whole of some content a preview or summary does not hold all of
/// ([`skywave_proto::Full`]). Where it is optional, `None` means the
/// preview holds it all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FullView {
    /// Kept as a blob, with its download state.
    Blob(BlobView),
    /// Carried in the event, so here already: small content, never a blob.
    Inline(String),
    /// The server could not keep it: only the preview or summary is left.
    Lost,
    /// Deleted on the server, and this device has no copy: only the
    /// preview or summary is left.
    Deleted { by: DeviceRef },
}

impl FullView {
    /// The blob holding the whole content, unless there is none.
    pub fn blob(&self) -> Option<&BlobView> {
        match self {
            FullView::Blob(blob) => Some(blob),
            FullView::Inline(_) | FullView::Lost | FullView::Deleted { .. } => None,
        }
    }

    /// The whole content when it came inline in the event.
    pub fn inline(&self) -> Option<&str> {
        match self {
            FullView::Inline(text) => Some(text),
            FullView::Blob(_) | FullView::Lost | FullView::Deleted { .. } => None,
        }
    }
}

/// The screen to show, with its data.
#[derive(Debug, Clone, PartialEq)]
pub enum Screen {
    Pairing(PairingView),
    SessionList(SessionListView),
    Session(Box<SessionView>),
    Settings(SettingsView),
    ServerAddresses(ServerAddressesView),
    Usage(UsageView),
    Storage(StorageView),
    Worktrees(WorktreesView),
    UsageLimits(UsageLimitsView),
    SessionInfo(Box<SessionInfoView>),
    Workdir(WorkdirView),
    Crop(CropView),
}

/// The page choosing which lines of a blob's content to keep
/// ([`Action::OpenCrop`](crate::Action::OpenCrop)). It holds no content:
/// a head reads the lines it shows from [`CropContent::Ready`]'s `path`
/// through a [`LineIndex`](crate::LineIndex).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CropView {
    /// The blob a crop names ([`BlobView::id`]).
    pub blob: BlobId,
    pub session: SessionId,
    pub holds: BlobUse,
    /// The item's title, or the run's command line, as the head opened it.
    pub title: String,
    pub content: CropContent,
    /// The selection ([`Action::SetCropKeep`](crate::Action::SetCropKeep)),
    /// as the head gave it; empty until chosen.
    pub keep: Vec<LineSpan>,
    /// This device's own delete or crop of the blob, as
    /// [`BlobView::pending`] shows it.
    pub pending: Option<BlobPending>,
}

/// Where the content a [`CropView`] chooses lines of stands on this device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CropContent {
    /// This device doesn't hold the content, and opening the page
    /// downloads nothing: [`Action::DownloadBlob`](crate::Action::DownloadBlob)
    /// does. The state shows `NotDownloaded`, then its progress (paused
    /// offline), or a failure to retry.
    NotHere(BlobState),
    /// Held; its lines are being counted.
    Counting,
    /// Held, with `lines` lines as a crop counts them
    /// ([`skywave_proto::crop::count`]).
    Ready { path: PathBuf, lines: u32 },
    /// Held, but its file could not be read to count its lines; opening the
    /// page again counts again.
    Unreadable { reason: String },
    /// Deleted or cropped meanwhile (a fate arrived), or gone from this
    /// replica: nothing to crop here.
    Gone,
}

/// Where the workdir screen is ([`Action::ShowWorkdir`](crate::Action::ShowWorkdir)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkdirPane {
    /// What changed against `HEAD`.
    Changes,
    /// The newest commits on `HEAD`.
    Commits,
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
    /// The files of the session asked for download
    /// ([`Action::DownloadWorkdirFile`](crate::Action::DownloadWorkdirFile))
    /// that are stored, in flight or failed, sorted by path.
    pub downloads: Vec<WorkdirDownload>,
    /// The pushes into this session's workdir ([`Action::PushWorkdirFile`](crate::Action::PushWorkdirFile)), sorted by path.
    pub pushes: Vec<WorkdirPush>,
}

/// A file of the workdir asked for download ([`Action::DownloadWorkdirFile`]).
#[derive(Debug, Clone, PartialEq)]
pub struct WorkdirDownload {
    pub path: String,
    /// The read of the download ask: loading, failed, or idle.
    pub fetch: WorkdirFetch,
    /// The server's answer, once stored (`FileBody::TooLarge` = over the cap).
    pub result: Option<FileResult>,
    /// The content, when the answer carries a blob: its progress, and
    /// `BlobState::Available { path }` once it is on the device.
    pub blob: Option<BlobView>,
}

/// A device file pushed into the session's workdir.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkdirPush {
    /// The workdir path written to, relative, `/` separated.
    pub path: String,
    /// The device file sent.
    pub source: PathBuf,
    pub state: PushState,
}

/// Where a push stands. Nothing is stored: a push lives as long as the model.
#[derive(Debug, Clone, PartialEq)]
pub enum PushState {
    Pushing,
    /// A transport failure, or offline; nothing is known to be written.
    Failed(String),
    Done(PushOutcome),
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
    /// The commit log the commits pane shows.
    Log(LogResult),
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
    /// The address the device connects to.
    pub server_address: ServerAddress,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingStatus {
    /// Waiting for the user to enter address, code and fingerprint.
    Idle,
    InProgress {
        address: ServerAddress,
    },
    /// The address as typed: it may be what failed to parse.
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
    /// The server agent's conversations to continue one in a new session,
    /// while the picker is open
    /// ([`Action::ListConversations`](crate::Action::ListConversations));
    /// only on the active list.
    pub conversations: Option<ConversationPicker>,
}

/// The conversations the server's agent keeps, as the server last listed
/// them on request. In memory only: a listing of another machine's
/// terminal sessions goes stale at once and is cheap to ask for again.
/// Continuing one is [`Action::CreateSession`](crate::Action::CreateSession)
/// with the item's `workdir` and `id` as `conversation`.
#[derive(Debug, Clone, PartialEq)]
pub struct ConversationPicker {
    pub status: ConversationsStatus,
    /// Newest first, as the server sent them; empty until it answers, and
    /// when it could not.
    pub items: Vec<ConversationItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConversationsStatus {
    /// Asked; no answer yet.
    Loading,
    /// The server answered; `truncated` when it listed only the newest.
    Ready { truncated: bool },
    /// No listing, and why.
    Unavailable(ConversationsUnavailable),
}

/// Why the conversation picker has no listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConversationsUnavailable {
    /// The link was down when it opened, or the server could not be
    /// reached; nothing is asked for later.
    Offline,
    /// The server's agent cannot list its conversations.
    AgentCannotList,
    /// The request failed otherwise (refused, timed out, broken), as the
    /// transport words it.
    Failed(String),
}

/// One conversation the server's agent had, in or outside Skywave. Texts
/// come from the server: heads sanitise them before writing them raw.
#[derive(Debug, Clone, PartialEq)]
pub struct ConversationItem {
    /// The agent's id for it: what `CreateSession::conversation` takes.
    pub id: String,
    /// The server directory it ran in: the new session's workdir.
    pub workdir: String,
    /// The agent's title for it.
    pub title: Option<String>,
    /// The first line of its first message.
    pub first_prompt: Option<String>,
    pub last_active: DateTime<Utc>,
    /// User and agent messages in it.
    pub messages: u32,
    /// The Skywave session one of whose contexts runs it now, if the server
    /// says so and that session is in this device's replica (not deleted
    /// here, already pulled): it is already in Skywave.
    pub session: Option<SessionId>,
    /// Sessions in the replica that adopted it, most recently active first,
    /// archived ones included. Adopting it
    /// again is allowed; heads say it was continued before.
    pub continued_in: Vec<SessionId>,
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
    /// The input the candidates are for. While a new directory loads it is
    /// the earlier input the kept candidates were for (status `Loading`).
    pub input: String,
    /// Paths that extend `input`, without a trailing `/`: the server's
    /// listing once it is known, else recent workdirs from the replica. A
    /// relative input matches the roots and recent workdirs by any path
    /// component, recent workdirs first.
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
    /// session (a shell draft holding only a condition does not count), or
    /// images for its next message.
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
    /// The git worktree the session runs in; heads group sessions by its repo.
    pub worktree: Option<Worktree>,
    /// The worktree was removed: work for the session is refused.
    pub worktree_removed: bool,
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

/// What the server's data takes on its disk, as the server last reported
/// it ([`Route::Storage`](crate::Route::Storage)). In memory only: opening
/// the screen asks again, and so does
/// [`Action::RefreshStorage`](crate::Action::RefreshStorage).
#[derive(Debug, Clone, PartialEq)]
pub struct StorageView {
    pub status: StorageStatus,
    /// The latest report; `None` until one arrives. A failed refresh keeps
    /// the one before it.
    pub report: Option<StorageReport>,
    /// The replica's title (as [`SessionRow::title`]) of each session the
    /// report names; one missing is not in this replica (deleted here, or
    /// not pulled yet).
    pub titles: std::collections::HashMap<SessionId, String>,
    /// This device's own delete or crop of each blob that has one, as a
    /// [`BlobView::pending`] shows it, so a row of it offers nothing more.
    pub pending: std::collections::HashMap<BlobId, BlobPending>,
    /// Each blob the report lists that this replica's log has already
    /// deleted or cropped, by any device: the report predates that, so a
    /// row of it offers nothing.
    pub gone: std::collections::HashSet<BlobId>,
}

/// Where the storage screen's report stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageStatus {
    /// Asked; the report shown, if any, is the one before.
    Loading,
    /// The report is the server's latest answer.
    Ready,
    /// The last ask failed; the report shown, if any, is older.
    Unavailable(StorageUnavailable),
}

/// Why the storage screen has no fresh report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageUnavailable {
    /// Not connected: nothing was asked.
    Offline,
    /// The request failed, worded by the transport.
    Failed(String),
}

/// Where the worktrees screen's list stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreesStatus {
    /// Asked; the list shown, if any, is the one before.
    Loading,
    /// The list is the server's latest answer.
    Ready,
    /// The last ask failed; the list shown, if any, is older.
    Unavailable(WorktreesUnavailable),
}

/// Why the worktrees screen has no fresh list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreesUnavailable {
    /// Not connected: nothing was asked.
    Offline,
    /// The request failed, worded by the transport.
    Failed(String),
}

/// The worktrees screen.
#[derive(Debug, Clone, PartialEq)]
pub struct WorktreesView {
    pub status: WorktreesStatus,
    /// The latest list; kept while a refresh is asked or fails.
    pub list: Option<WorktreeList>,
    /// The sessions running in each listed worktree, by its path, in the
    /// replica's session order; a worktree no session runs in has no key.
    pub sessions: std::collections::HashMap<String, Vec<WorktreeSession>>,
}

/// A session running in a listed worktree.
#[derive(Debug, Clone, PartialEq)]
pub struct WorktreeSession {
    pub id: SessionId,
    pub title: String,
    pub archived: bool,
    /// Created in a skywave worktree, so `RemoveWorktree` applies.
    pub worktree: bool,
    pub worktree_removed: bool,
    /// The latest `RemoveWorktree` for this session, until its effect is in the replica.
    pub removal: Option<Outgoing>,
}

/// The usage limits of the account the server's agent runs as
/// ([`Route::UsageLimits`](crate::Route::UsageLimits)): the latest answer
/// the replica keeps, shown offline too, and where a read of a fresh one
/// stands. Opening the page asks only when nothing is stored;
/// [`Action::RefreshUsageLimits`](crate::Action::RefreshUsageLimits) asks
/// again.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageLimitsView {
    pub status: UsageLimitsStatus,
    /// The latest stored answer, never a `Failed`; `None` until one came.
    /// A `Read` names when the agent reported it.
    pub answer: Option<UsageLimitsResult>,
}

/// One session's particulars
/// ([`Action::OpenSessionInfo`](crate::Action::OpenSessionInfo)): what it
/// is, what its turns took, how its agent is set, every subagent it ran,
/// and the usage limits of the account the agent runs as. All from the
/// replica, so it reads offline; only the limits are asked for, on
/// opening while none are stored.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionInfoView {
    pub id: SessionId,
    /// As [`SessionRow::title`].
    pub title: String,
    pub workdir: Option<String>,
    /// What the session's turns took, as [`SessionView::usage`].
    pub usage: Option<SessionUsage>,
    /// The session's turns that reported usage, in log order: what a head
    /// sums by model.
    pub turns: Vec<TurnRecord>,
    /// As [`SessionView::agent`].
    pub agent: AgentView,
    /// The subagents the session ran, in any context and nested ones too,
    /// in the order they were launched, each as its view's header shows
    /// it.
    pub subagents: Vec<SubagentHeader>,
    /// As the usage limits page's.
    pub limits: UsageLimitsView,
}

/// Where a read of the usage limits stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsageLimitsStatus {
    /// Nothing asked, or the last ask was answered: `answer` is the latest.
    Idle,
    /// Asked; `answer`, if any, is the one before.
    Loading,
    /// The last ask got no answer; `answer`, if any, is older.
    Unavailable(UsageLimitsUnavailable),
}

/// Why the last read of the usage limits got no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsageLimitsUnavailable {
    /// Not connected: nothing was asked.
    Offline,
    /// The request failed, worded by the transport, or the agent could not
    /// say, in the server's words.
    Failed(String),
}

/// Logging the server's agent in from this device, on the settings screen:
/// what the server's last `hello` said (as [`ViewState`]'s fields), and
/// where this device's login stands ([`crate::Action::StartAgentLogin`]).
#[derive(Debug, Clone, PartialEq)]
pub struct LoginView {
    /// As [`ViewState::agent_login_offered`].
    pub offered: bool,
    /// As [`ViewState::agent_logged_out`]; a `LoggedIn` answer clears it at
    /// once.
    pub logged_out: bool,
    /// As [`ViewState::login_expires`]; a `LoggedIn` answer sets it at once.
    pub expires: Option<DateTime<Utc>>,
    pub flow: LoginFlowView,
}

/// Where this device's login of the server's agent stands. In memory only:
/// a restart, a dismissal or another server starts it over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginFlowView {
    /// No login started, or the last one dismissed.
    Idle,
    /// Asked the server to start one.
    Starting,
    /// The user opens `url`, logs in there and pastes the code it shows
    /// ([`crate::Action::FinishAgentLogin`]). `refused` says why this
    /// device did not send the last code pasted; nothing reached the
    /// server, so a corrected code can follow.
    Open {
        url: String,
        refused: Option<LoginCodeRefused>,
    },
    /// The pasted code is on its way.
    Finishing { url: String },
    /// The pasted code got no answer: the device was offline, the request
    /// failed or the server gave up waiting. The flow and the code are
    /// kept; [`crate::Action::RetryAgentLogin`] sends them again.
    NoAnswer { url: String, why: LoginNoAnswer },
    /// The login ended.
    Done(LoginOutcome),
}

/// Why a pasted code got no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginNoAnswer {
    /// Not connected: nothing was sent.
    Offline,
    /// The request failed (a timeout or a `503` included), worded by the
    /// transport.
    Failed(String),
}

/// How a login ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginOutcome {
    /// The agent is logged in.
    LoggedIn,
    /// Another login is being started or finished on the server.
    Busy,
    /// The server's agent has no login a device can run.
    Unavailable,
    /// The server knows no such login (never started, given up on, or
    /// replaced): start again.
    NoFlow,
    /// Not connected: nothing was asked.
    Offline,
    /// The login failed, in the server's words or the transport's: a start
    /// that got no answer (a `503` included), a code the server refused,
    /// or a finish refused for good (a `4xx`). Start again.
    Failed(String),
}

/// Why this device did not send a pasted code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginCodeRefused {
    /// Nothing but whitespace.
    Blank,
    /// Longer than the server takes, in UTF-8 bytes once trimmed.
    TooLong { max_bytes: usize },
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
    /// The question the agent is blocked on, if any: an approval whose
    /// answer is a choice. At most one of the two is set.
    pub pending_question: Option<QuestionPrompt>,
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
    /// This device's images for the next message, in order
    /// ([`Action::Attach`](crate::Action::Attach)). The files are at
    /// [`attachment::attachment_path`](crate::attachment::attachment_path).
    pub draft_images: Vec<AttachedImage>,
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
    pub output: Option<FullView>,
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
    /// fork; history imported by an adoption is not enough, D32), it is not
    /// moved nor is this device's move of it on its way,
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
    /// Whether the server allows [`PermissionMode::Bypass`], as its last
    /// `hello` said (kept for offline); `false` until one said so. Where
    /// it does not, the server refuses a switch to it.
    pub bypass_allowed: bool,
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
        /// The images it carries, in order; each file is at
        /// [`attachment_path`](crate::attachment::attachment_path).
        images: Vec<AttachedImage>,
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
        /// The message's images, in the order attached.
        images: Vec<MessageImage>,
        /// Who sent it; [`DeviceRef::me`] when this device did.
        from: DeviceRef,
        /// Delivery state when this device sent it.
        outgoing: Option<Outgoing>,
        /// How it went out, when this device sent it.
        delivery: Option<Delivery>,
    },
    /// `text` is the whole message unless `full` is set: then it is the
    /// message's head, and `full` the whole of it.
    AgentMessage {
        text: String,
        full: Option<FullView>,
    },
    /// One tool call with its approval and result folded in, placed at the
    /// call's seq.
    Tool(Box<ToolView>),
    /// A tool call no [`CallId`] links to anything (v1 logs); its result and
    /// approval, if any, are entries of their own.
    ToolCall {
        name: String,
        summary: String,
        input: Option<FullView>,
    },
    /// A tool result with no logged call to fold into (v1 logs, or a result
    /// for a call that is not in the log).
    ToolResult {
        summary: String,
        output: Option<FullView>,
    },
    /// An approval with no logged call to fold into (v1 logs, or a request
    /// gating no logged call).
    Approval {
        id: ApprovalId,
        tool: String,
        summary: String,
        input: Option<FullView>,
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
    ContextStarted { name: String, by: DeviceRef },
    /// A device made the earlier context `name` current again. A switch to
    /// the context already current has no entry.
    ContextSwitched { name: String, by: DeviceRef },
    /// A device moved the context `name` into the session `to`, where it
    /// goes on; here it is never current again.
    ContextMoved {
        name: String,
        to: SessionRef,
        by: DeviceRef,
    },
    /// The session's first item when it was made by moving the context
    /// `name` out of `from`: its conversation goes on here.
    MovedFrom { from: SessionRef, name: String },
    /// The session's first item when it was made by adopting the agent's
    /// outside conversation `conversation`. What the server could read of
    /// that conversation follows as [`Entry::Imported`] items, then one
    /// [`Entry::ImportFinished`]; the session's own entries come after.
    Adopted { conversation: String },
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
    ImportFinished { outcome: ImportOutcome },
    /// The session was renamed; `None` restored its default title.
    Renamed { name: Option<String>, by: DeviceRef },
    /// The session was archived (`true`) or brought back from the archive.
    /// A message brings it back too, without an entry of its own.
    Archived { archived: bool, by: DeviceRef },
    /// The session's git worktree and its branch were removed; its work is refused from here on.
    WorktreeRemoved { by: DeviceRef },
    /// The agent reported an error.
    AgentErrored { message: String },
    /// The agent started as `version`, not `confirmed`, the version the
    /// server was checked against: a warning that its turns may misbehave.
    AgentVersionUnconfirmed { version: String, confirmed: String },
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
    /// `text` and `full` as on [`Entry::AgentMessage`].
    UserMessage {
        text: String,
        full: Option<FullView>,
    },
    AgentMessage {
        text: String,
        full: Option<FullView>,
    },
    /// An imported call with its imported result folded in. Without a
    /// result its outcome is [`ToolOutcome::NoResult`], never `Running`:
    /// nothing is waiting on it. It has no approval and no subagent status.
    Tool(Box<ToolView>),
    /// An imported result no imported call links to (the agent's own call
    /// was not on the imported branch), or a second one for a call.
    ToolResult {
        summary: String,
        output: Option<FullView>,
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
    /// The call's whole input, when its display does not show all of it.
    pub input: Option<FullView>,
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

impl ToolView {
    /// The approval's own input ([`ToolApproval::input`]), unless it is the
    /// very blob the call's input is: what the agent asked to run when that
    /// differs from what the call says.
    pub fn approval_input(&self) -> Option<&FullView> {
        let asked = self.approval.as_ref()?.input.as_ref()?;
        let call = self.input.as_ref().and_then(FullView::blob);
        match (asked.blob(), call) {
            (Some(asked), Some(call)) if asked.id == call.id => None,
            _ => Some(asked),
        }
    }
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
    /// What its agent reported it ran once it finished (D46); `None` while
    /// it runs, and for one stopped or never reported on.
    pub usage: Option<SubagentUsage>,
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
    pub full: Option<FullView>,
}

/// The approval a tool call waited on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolApproval {
    pub id: ApprovalId,
    /// A question (`QuestionAsked`), answered with a choice, not an
    /// approval.
    pub question: bool,
    pub status: ApprovalStatus,
    /// This device's answer, if it gave one. Once the approval is settled,
    /// this is where an answer that did not apply stays in sight: refused
    /// because another device answered first or the approval was cancelled.
    pub answer: Option<Answer>,
    /// The request's own input (a `BlobUse::ApprovalInput` blob): the
    /// server logs one only when it differs from the call's input, so
    /// `None` means the call's input is what was asked.
    pub input: Option<FullView>,
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
        output: Option<FullView>,
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
    /// A question's answer won (D5); `by` is who answered. What was picked
    /// shows in the call's result.
    Answered {
        by: DeviceRef,
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
    pub input: Option<FullView>,
    /// Lasting rules the agent offers to grant with an approval; an answer
    /// names one by its index
    /// ([`Action::ResolveApproval::rule`](crate::Action::ResolveApproval)).
    pub suggestions: Vec<RuleSuggestion>,
    /// This device's answer, if it gave one; queued like any command.
    pub answer: Option<Answer>,
}

/// The question prompt of a blocked session: the agent asks the user to
/// pick from options ([`Action::AnswerQuestion`](crate::Action::AnswerQuestion)),
/// or to decline with a denial.
#[derive(Debug, Clone, PartialEq)]
pub struct QuestionPrompt {
    pub id: ApprovalId,
    /// The tool call that asks, an [`Entry::Tool`] in the timeline; `None`
    /// for a question with no logged call.
    pub call: Option<CallId>,
    /// The title of the subagent asking, as [`ApprovalPrompt::from`].
    pub from: Option<String>,
    /// At least one.
    pub questions: Vec<Question>,
    /// This device's answer, if it gave one: an approval with
    /// `answers`, or a denial that declines; queued like any command.
    pub answer: Option<Answer>,
    /// The answers of this device's `answer`, one per question; empty with
    /// none or a denial.
    pub answers: Vec<QuestionAnswer>,
}

/// The `tool` of an [`Entry::Approval`] that is a question, with no logged
/// call to fold into.
pub const QUESTION_TOOL: &str = "question";

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

/// The paired server's addresses ([`Route::ServerAddresses`](crate::Route::ServerAddresses)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerAddressesView {
    /// The address the link uses.
    pub current: Option<ServerAddress>,
    /// The same server's other addresses, in the order they were left
    /// ([`ServerAddresses::others`](crate::ServerAddresses::others)).
    pub others: Vec<ServerAddress>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SettingsView {
    pub blob_policy: BlobPolicy,
    /// Blob downloads, `Eager` ones included, are paused because this
    /// device's disk is full ([`Pause::DiskFull`]).
    pub blobs_disk_full: bool,
    pub server_address: Option<ServerAddress>,
    /// The same server's other addresses, to switch to
    /// ([`ServerAddresses::others`](crate::ServerAddresses::others)).
    pub other_addresses: Vec<ServerAddress>,
    pub device_id: Option<DeviceId>,
    /// The fingerprint of the server CA pinned at pairing, to compare with
    /// what `skywaved` prints; `None` if unknown.
    pub server_fingerprint: Option<Fingerprint>,
    /// Logging the server's agent in from this device.
    pub login: LoginView,
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
                images: Vec::new(),
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
            images: Vec::new(),
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
                logged(
                    2,
                    Entry::AgentMessage {
                        text: "ok".into(),
                        full: None,
                    },
                ),
                message(3, "again", false),
                message(4, "again", true),
                logged(5, Entry::Interrupted),
                message(6, "first", true),
                pending("queued"),
                pending("queued"),
            ],
            pending_approval: None,
            pending_question: None,
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
            draft_images: Vec::new(),
            shell_draft: None,
            draft_rev: 0,
            shell: ShellView::default(),
            last_seq: Seq(6),
            unread_from: None,
        };
        assert_eq!(view.prompts(), ["queued", "first", "again", "first"]);
    }
}
