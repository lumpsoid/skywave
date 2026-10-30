//! Plain-data mirrors of [`skywave_core::ViewState`] and every type reachable
//! from it, converted one way: core → FFI (D16).
//!
//! Mapping: ids → `String` (hyphenated UUID), `Seq` → `u64`, counts
//! (`usize`) → `u64`, `Duration` → `u64` millis, `DateTime<Utc>` → `i64`
//! unix-epoch millis, `PathBuf` → `String`. Fields holding millis carry a `_ms` suffix. Tuple
//! variants become named fields so Kotlin reads `screen.view`, not
//! `screen.v1`. No logic lives here (D2): every value is copied, never
//! derived.

use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Utc};
use skywave_core as sc;

// ---------------------------------------------------------------------------
// Scalar mapping helpers

pub(crate) fn id(uuid: uuid::Uuid) -> String {
    uuid.to_string()
}

fn count(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// A cursor `chars` code points into `text`, as UTF-16 code units (how
/// Kotlin/Java strings index). Past the end clamps to the end.
pub(crate) fn chars_to_utf16(text: &str, chars: usize) -> u64 {
    count(text.chars().take(chars).map(char::len_utf16).sum())
}

/// A cursor `units` UTF-16 code units into `text`, as code points (how the
/// core counts). Inside a surrogate pair rounds down to before the pair;
/// past the end clamps to the end.
pub(crate) fn utf16_to_chars(text: &str, units: u64) -> usize {
    let mut seen = 0u64;
    text.chars()
        .take_while(|c| {
            seen += c.len_utf16() as u64;
            seen <= units
        })
        .count()
}

fn duration_ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

fn epoch_ms(at: DateTime<Utc>) -> i64 {
    at.timestamp_millis()
}

fn path(p: PathBuf) -> String {
    p.to_string_lossy().into_owned()
}

fn opt<T, U: From<T>>(value: Option<T>) -> Option<U> {
    value.map(U::from)
}

fn vec<T, U: From<T>>(values: Vec<T>) -> Vec<U> {
    values.into_iter().map(U::from).collect()
}

// ---------------------------------------------------------------------------
// Root

/// One full snapshot of everything a head shows; pushed whole after every
/// change.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct ViewState {
    pub link: Link,
    /// Own commands persisted locally and not yet answered by the server.
    pub queued: u64,
    pub attention: Attention,
    pub screen: Screen,
    /// `Action::Back` goes somewhere from `screen`; when false, back leaves
    /// the app. Heads show an up arrow only when true.
    pub can_go_back: bool,
}

impl From<sc::ViewState> for ViewState {
    fn from(v: sc::ViewState) -> Self {
        let sc::ViewState {
            link,
            queued,
            attention,
            screen,
            can_go_back,
        } = v;
        ViewState {
            link: link.into(),
            queued: count(queued),
            attention: attention.into(),
            screen: screen.into(),
            can_go_back,
        }
    }
}

/// Approvals some agent is blocked on, present on every screen.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct Attention {
    /// Oldest first.
    pub approvals: Vec<PendingApprovalRef>,
    /// How many of `approvals` are outside the open session.
    pub elsewhere: u64,
    /// Grows whenever new approvals came in; heads alert when it does.
    pub alerts: u64,
}

impl From<sc::Attention> for Attention {
    fn from(a: sc::Attention) -> Self {
        let sc::Attention {
            approvals,
            elsewhere,
            alerts,
        } = a;
        Attention {
            approvals: vec(approvals),
            elsewhere: count(elsewhere),
            alerts,
        }
    }
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct PendingApprovalRef {
    pub id: String,
    pub session_id: String,
    /// Text from the server: sanitise before writing it anywhere raw.
    pub title: String,
    /// The `alerts` value this approval alerted at in this core, 0 if it
    /// has not alerted here (not yet, no news, or an earlier process
    /// alerted). Notify for those above the last `alerts` seen.
    pub alerted_at: u64,
}

impl From<sc::PendingApprovalRef> for PendingApprovalRef {
    fn from(p: sc::PendingApprovalRef) -> Self {
        let sc::PendingApprovalRef {
            id: approval,
            session_id,
            title,
            alerted_at,
        } = p;
        PendingApprovalRef {
            id: id(approval.0),
            session_id: id(session_id.0),
            title,
            alerted_at,
        }
    }
}

// ---------------------------------------------------------------------------
// Link

/// State of the connection to the server.
#[derive(Debug, Clone, PartialEq, uniffi::Enum)]
pub enum Link {
    /// Never paired: there is no server to connect to.
    Unpaired,
    Connecting,
    Online {
        latency_ms: u32,
    },
    /// Pulling events; the local replica is rendered meanwhile.
    CatchingUp {
        have: u64,
        server: u64,
    },
    /// Both times are unix-epoch millis, fixed by the core when the
    /// attempt failed: every snapshot of one wait carries the same values.
    /// A head counts down to `retry_at_ms` against its own wall clock.
    Reconnecting {
        attempt: u32,
        /// When the next attempt is due.
        retry_at_ms: i64,
        /// When the link first went down in this outage.
        offline_since_ms: i64,
    },
    /// Refused for good until the user re-pairs.
    Rejected {
        rejection: LinkRejection,
    },
}

impl From<sc::Link> for Link {
    fn from(l: sc::Link) -> Self {
        match l {
            sc::Link::Unpaired => Link::Unpaired,
            sc::Link::Connecting => Link::Connecting,
            sc::Link::Online { latency_ms } => Link::Online { latency_ms },
            sc::Link::CatchingUp { have, server } => Link::CatchingUp {
                have: have.0,
                server: server.0,
            },
            sc::Link::Reconnecting {
                attempt,
                retry_at,
                offline_since,
            } => Link::Reconnecting {
                attempt,
                retry_at_ms: epoch_ms(retry_at),
                offline_since_ms: epoch_ms(offline_since),
            },
            sc::Link::Rejected(rejection) => Link::Rejected {
                rejection: rejection.into(),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum LinkRejection {
    Revoked,
    ProtocolMismatch { client: u32, server: u32 },
}

impl From<sc::LinkRejection> for LinkRejection {
    fn from(r: sc::LinkRejection) -> Self {
        match r {
            sc::LinkRejection::Revoked => LinkRejection::Revoked,
            sc::LinkRejection::ProtocolMismatch { client, server } => {
                LinkRejection::ProtocolMismatch { client, server }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Delivery of own commands

/// Delivery state of one of this device's commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum Outgoing {
    Queued,
    Sending,
    /// Placed in the log at this seq.
    Accepted {
        seq: u64,
    },
    Failed {
        reason: FailReason,
    },
}

impl From<sc::Outgoing> for Outgoing {
    fn from(o: sc::Outgoing) -> Self {
        match o {
            sc::Outgoing::Queued => Outgoing::Queued,
            sc::Outgoing::Sending => Outgoing::Sending,
            sc::Outgoing::Accepted(seq) => Outgoing::Accepted { seq: seq.0 },
            sc::Outgoing::Failed(reason) => Outgoing::Failed {
                reason: reason.into(),
            },
        }
    }
}

/// Why the server refused a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FailReason {
    WorkdirNotAllowed,
    WorkdirNotFound,
    UnknownSession,
    UnknownApproval,
    ApprovalAlreadyResolved,
    /// The approval was cancelled before the answer arrived.
    ApprovalCancelled,
    NoActiveTurn,
    UnknownRule,
}

impl From<sc::FailReason> for FailReason {
    fn from(r: sc::FailReason) -> Self {
        match r {
            sc::FailReason::WorkdirNotAllowed => FailReason::WorkdirNotAllowed,
            sc::FailReason::WorkdirNotFound => FailReason::WorkdirNotFound,
            sc::FailReason::UnknownSession => FailReason::UnknownSession,
            sc::FailReason::UnknownApproval => FailReason::UnknownApproval,
            sc::FailReason::ApprovalAlreadyResolved => FailReason::ApprovalAlreadyResolved,
            sc::FailReason::ApprovalCancelled => FailReason::ApprovalCancelled,
            sc::FailReason::NoActiveTurn => FailReason::NoActiveTurn,
            sc::FailReason::UnknownRule => FailReason::UnknownRule,
        }
    }
}

/// How one of this device's commands went out, in this device's clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct Delivery {
    pub queued_at_ms: i64,
    pub first_sent_at_ms: Option<i64>,
    pub answered_at_ms: Option<i64>,
    pub tries: u32,
}

impl From<sc::Delivery> for Delivery {
    fn from(d: sc::Delivery) -> Self {
        let sc::Delivery {
            queued_at,
            first_sent_at,
            answered_at,
            tries,
        } = d;
        Delivery {
            queued_at_ms: epoch_ms(queued_at),
            first_sent_at_ms: first_sent_at.map(epoch_ms),
            answered_at_ms: answered_at.map(epoch_ms),
            tries,
        }
    }
}

// ---------------------------------------------------------------------------
// Sessions and blobs

/// What a session's agent is doing.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum SessionState {
    Idle,
    AgentWorking,
    AwaitingApproval { approval_id: String },
    Errored { error: ErrorInfo },
    Stopped,
}

impl From<sc::SessionState> for SessionState {
    fn from(s: sc::SessionState) -> Self {
        match s {
            sc::SessionState::Idle => SessionState::Idle,
            sc::SessionState::AgentWorking => SessionState::AgentWorking,
            sc::SessionState::AwaitingApproval(approval) => SessionState::AwaitingApproval {
                approval_id: id(approval.0),
            },
            sc::SessionState::Errored(error) => SessionState::Errored {
                error: error.into(),
            },
            sc::SessionState::Stopped => SessionState::Stopped,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ErrorInfo {
    pub message: String,
}

impl From<sc::ErrorInfo> for ErrorInfo {
    fn from(e: sc::ErrorInfo) -> Self {
        let sc::ErrorInfo { message } = e;
        ErrorInfo { message }
    }
}

/// Local state of one blob.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum BlobState {
    NotDownloaded {
        size: u64,
    },
    Requested {
        size: u64,
    },
    Downloading {
        got: u64,
        size: u64,
        paused: bool,
    },
    Failed {
        size: u64,
        reason: String,
    },
    /// Complete content is in the file at `path`.
    Available {
        path: String,
    },
}

impl From<sc::BlobState> for BlobState {
    fn from(b: sc::BlobState) -> Self {
        match b {
            sc::BlobState::NotDownloaded { size } => BlobState::NotDownloaded { size },
            sc::BlobState::Requested { size } => BlobState::Requested { size },
            sc::BlobState::Downloading { got, size, paused } => {
                BlobState::Downloading { got, size, paused }
            }
            sc::BlobState::Failed { size, reason } => BlobState::Failed { size, reason },
            sc::BlobState::Available { path: p } => BlobState::Available { path: path(p) },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct BlobView {
    pub id: String,
    pub state: BlobState,
}

impl From<sc::BlobView> for BlobView {
    fn from(b: sc::BlobView) -> Self {
        let sc::BlobView { id: blob, state } = b;
        BlobView {
            id: id(blob.0),
            state: state.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Screens

/// The screen to show, with its data.
#[derive(Debug, Clone, PartialEq, uniffi::Enum)]
#[expect(
    clippy::large_enum_variant,
    reason = "UniFFI cannot carry a Box; values are lowered to a buffer at once"
)]
pub enum Screen {
    Pairing { view: PairingView },
    SessionList { view: SessionListView },
    Session { view: SessionView },
    Settings { view: SettingsView },
}

impl From<sc::Screen> for Screen {
    fn from(s: sc::Screen) -> Self {
        match s {
            sc::Screen::Pairing(view) => Screen::Pairing { view: view.into() },
            sc::Screen::SessionList(view) => Screen::SessionList { view: view.into() },
            sc::Screen::Session(view) => Screen::Session {
                view: (*view).into(),
            },
            sc::Screen::Settings(view) => Screen::Settings { view: view.into() },
        }
    }
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct PairingView {
    /// The current pairing when this is a re-pair.
    pub paired: Option<PairedInfo>,
    pub status: PairingStatus,
}

impl From<sc::PairingView> for PairingView {
    fn from(p: sc::PairingView) -> Self {
        let sc::PairingView { paired, status } = p;
        PairingView {
            paired: opt(paired),
            status: status.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct PairedInfo {
    pub device_id: String,
    pub server_address: Option<String>,
}

impl From<sc::PairedInfo> for PairedInfo {
    fn from(p: sc::PairedInfo) -> Self {
        let sc::PairedInfo {
            device_id,
            server_address,
        } = p;
        PairedInfo {
            device_id: id(device_id.0),
            server_address,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum PairingStatus {
    Idle,
    InProgress { address: String },
    Failed { address: String, reason: String },
}

impl From<sc::PairingStatus> for PairingStatus {
    fn from(s: sc::PairingStatus) -> Self {
        match s {
            sc::PairingStatus::Idle => PairingStatus::Idle,
            sc::PairingStatus::InProgress { address } => PairingStatus::InProgress { address },
            sc::PairingStatus::Failed { address, reason } => {
                PairingStatus::Failed { address, reason }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct SessionListView {
    pub kind: ListKind,
    /// Most recently active first.
    pub sessions: Vec<SessionRow>,
    /// How many sessions the other list holds.
    pub other_count: u64,
    /// Sessions asked for and not in the replica yet, or refused; oldest first.
    pub creating: Vec<PendingSession>,
    /// Workdir completion for the new-session prompt, while one is open.
    pub completion: Option<Completion>,
}

impl From<sc::SessionListView> for SessionListView {
    fn from(s: sc::SessionListView) -> Self {
        let sc::SessionListView {
            kind,
            sessions,
            other_count,
            creating,
            completion,
        } = s;
        SessionListView {
            kind: kind.into(),
            sessions: vec(sessions),
            other_count: count(other_count),
            creating: vec(creating),
            completion: opt(completion),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ListKind {
    Active,
    Archived,
}

impl From<sc::ListKind> for ListKind {
    fn from(k: sc::ListKind) -> Self {
        match k {
            sc::ListKind::Active => ListKind::Active,
            sc::ListKind::Archived => ListKind::Archived,
        }
    }
}

/// Directories that complete what the user typed as a workdir.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct Completion {
    pub input: String,
    pub candidates: Vec<Candidate>,
    pub status: CompletionStatus,
    /// What the input becomes when the user completes it (Tab): the only
    /// candidate with a trailing `/`, or the candidates' longest common
    /// prefix if it extends the input. `None` when nothing would change.
    pub fill: Option<String>,
}

impl From<sc::Completion> for Completion {
    fn from(c: sc::Completion) -> Self {
        let fill = c.fill();
        let sc::Completion {
            input,
            candidates,
            status,
        } = c;
        Completion {
            input,
            candidates: vec(candidates),
            status: status.into(),
            fill,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct Candidate {
    pub path: String,
    pub source: CandidateSource,
}

impl From<sc::Candidate> for Candidate {
    fn from(c: sc::Candidate) -> Self {
        let sc::Candidate { path, source } = c;
        Candidate {
            path,
            source: source.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum CandidateSource {
    Server,
    Recent,
}

impl From<sc::CandidateSource> for CandidateSource {
    fn from(s: sc::CandidateSource) -> Self {
        match s {
            sc::CandidateSource::Server => CandidateSource::Server,
            sc::CandidateSource::Recent => CandidateSource::Recent,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum CompletionStatus {
    Loading,
    Ready { truncated: bool },
    Unavailable,
}

impl From<sc::CompletionStatus> for CompletionStatus {
    fn from(s: sc::CompletionStatus) -> Self {
        match s {
            sc::CompletionStatus::Loading => CompletionStatus::Loading,
            sc::CompletionStatus::Ready { truncated } => CompletionStatus::Ready { truncated },
            sc::CompletionStatus::Unavailable => CompletionStatus::Unavailable,
        }
    }
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct SessionRow {
    pub id: String,
    /// Text from the server: sanitise before writing it anywhere raw.
    pub title: String,
    pub named: bool,
    pub rename: Option<Outgoing>,
    pub workdir: Option<String>,
    pub state: SessionState,
    pub pending_approvals: u64,
    pub last_activity_ms: i64,
    pub has_draft: bool,
    pub unread: u64,
    pub cost_usd: Option<f64>,
    pub deletable: bool,
}

impl From<sc::SessionRow> for SessionRow {
    fn from(r: sc::SessionRow) -> Self {
        let sc::SessionRow {
            id: session,
            title,
            named,
            rename,
            workdir,
            state,
            pending_approvals,
            last_activity,
            has_draft,
            unread,
            cost_usd,
            deletable,
        } = r;
        SessionRow {
            id: id(session.0),
            title,
            named,
            rename: opt(rename),
            workdir,
            state: state.into(),
            pending_approvals: count(pending_approvals),
            last_activity_ms: epoch_ms(last_activity),
            has_draft,
            unread,
            cost_usd,
            deletable,
        }
    }
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct PendingSession {
    pub command_id: String,
    pub workdir: String,
    pub outgoing: Outgoing,
    pub discardable: bool,
}

impl From<sc::PendingSession> for PendingSession {
    fn from(p: sc::PendingSession) -> Self {
        let sc::PendingSession {
            command_id,
            workdir,
            outgoing,
            discardable,
        } = p;
        PendingSession {
            command_id: id(command_id.0),
            workdir,
            outgoing: outgoing.into(),
            discardable,
        }
    }
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct SessionView {
    pub id: String,
    pub title: String,
    pub named: bool,
    pub rename: Option<Outgoing>,
    pub archived: bool,
    pub workdir: Option<String>,
    pub state: SessionState,
    /// Log order; this device's messages not yet in the log come last.
    pub timeline: Vec<TimelineItem>,
    pub pending_approval: Option<ApprovalPrompt>,
    pub rules: Vec<RuleSuggestion>,
    pub usage: Option<SessionUsage>,
    pub interrupt: Option<Outgoing>,
    pub stop: Option<Outgoing>,
    pub draft: Option<Draft>,
    /// Changes when the core rewrites the draft: refill the composer then.
    pub draft_rev: u64,
    /// Seq of the session's newest event, for `Action.MarkRead`.
    pub last_seq: u64,
    pub unread_from: Option<u64>,
    /// The messages sent into the session, newest first, for recalling into
    /// the composer: every device's logged messages, then this device's not
    /// yet logged, a message repeated back to back listed once.
    pub prompts: Vec<String>,
}

impl From<sc::SessionView> for SessionView {
    fn from(s: sc::SessionView) -> Self {
        let prompts = s.prompts().into_iter().map(str::to_owned).collect();
        let sc::SessionView {
            id: session,
            title,
            named,
            rename,
            archived,
            workdir,
            state,
            timeline,
            pending_approval,
            rules,
            usage,
            interrupt,
            stop,
            draft,
            draft_rev,
            last_seq,
            unread_from,
        } = s;
        SessionView {
            id: id(session.0),
            title,
            named,
            rename: opt(rename),
            archived,
            workdir,
            state: state.into(),
            timeline: vec(timeline),
            pending_approval: opt(pending_approval),
            rules: vec(rules),
            usage: opt(usage),
            interrupt: opt(interrupt),
            stop: opt(stop),
            draft: opt(draft),
            draft_rev,
            last_seq: last_seq.0,
            unread_from: unread_from.map(|seq| seq.0),
            prompts,
        }
    }
}

/// A message being written on this device.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct Draft {
    pub text: String,
    /// Position in `text` in UTF-16 code units, ready for a Kotlin
    /// `TextRange`. The core counts code points; this is converted.
    pub cursor_utf16: u64,
}

impl From<sc::Draft> for Draft {
    fn from(d: sc::Draft) -> Self {
        let sc::Draft { text, cursor } = d;
        let cursor_utf16 = chars_to_utf16(&text, cursor);
        Draft { text, cursor_utf16 }
    }
}

// ---------------------------------------------------------------------------
// Timeline

#[derive(Debug, Clone, PartialEq, uniffi::Enum)]
#[expect(
    clippy::large_enum_variant,
    reason = "UniFFI cannot carry a Box; values are lowered to a buffer at once"
)]
pub enum TimelineItem {
    /// An event from the server log.
    Logged { seq: u64, at_ms: i64, entry: Entry },
    /// A message from this device that is not in the log yet.
    Pending {
        command_id: String,
        text: String,
        outgoing: Outgoing,
        discardable: bool,
        delivery: Option<Delivery>,
    },
}

impl From<sc::TimelineItem> for TimelineItem {
    fn from(t: sc::TimelineItem) -> Self {
        match t {
            sc::TimelineItem::Logged { seq, at, entry } => TimelineItem::Logged {
                seq: seq.0,
                at_ms: epoch_ms(at),
                entry: entry.into(),
            },
            sc::TimelineItem::Pending {
                command_id,
                text,
                outgoing,
                discardable,
                delivery,
            } => TimelineItem::Pending {
                command_id: id(command_id.0),
                text,
                outgoing: outgoing.into(),
                discardable,
                delivery: opt(delivery),
            },
        }
    }
}

/// One logged event as the timeline shows it.
#[derive(Debug, Clone, PartialEq, uniffi::Enum)]
#[expect(
    clippy::large_enum_variant,
    reason = "UniFFI cannot carry a Box; values are lowered to a buffer at once"
)]
pub enum Entry {
    UserMessage {
        command_id: String,
        text: String,
        from: String,
        own: bool,
        outgoing: Option<Outgoing>,
        delivery: Option<Delivery>,
    },
    AgentMessage {
        text: String,
    },
    /// One tool call with its approval and result folded in.
    Tool {
        tool: ToolView,
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
        id: String,
        tool: String,
        summary: String,
        input: Option<BlobView>,
        status: ApprovalStatus,
        /// This device's answer, if it gave one.
        #[uniffi(default)]
        answer: Option<Answer>,
    },
    Interrupted,
    Stopped,
    /// `name` is `None` when the default title was restored.
    Renamed {
        name: Option<String>,
        by: String,
        own: bool,
    },
    Archived {
        archived: bool,
        by: String,
        own: bool,
    },
    AgentErrored {
        message: String,
    },
    TurnUsage {
        usage: TurnUsage,
    },
}

impl From<sc::Entry> for Entry {
    fn from(e: sc::Entry) -> Self {
        match e {
            sc::Entry::UserMessage {
                command_id,
                text,
                from,
                own,
                outgoing,
                delivery,
            } => Entry::UserMessage {
                command_id: id(command_id.0),
                text,
                from: id(from.0),
                own,
                outgoing: opt(outgoing),
                delivery: opt(delivery),
            },
            sc::Entry::AgentMessage { text } => Entry::AgentMessage { text },
            sc::Entry::Tool(tool) => Entry::Tool {
                tool: (*tool).into(),
            },
            sc::Entry::ToolCall {
                name,
                summary,
                input,
            } => Entry::ToolCall {
                name,
                summary,
                input: opt(input),
            },
            sc::Entry::ToolResult { summary, output } => Entry::ToolResult {
                summary,
                output: opt(output),
            },
            sc::Entry::Approval {
                id: approval,
                tool,
                summary,
                input,
                status,
                answer,
            } => Entry::Approval {
                id: id(approval.0),
                tool,
                summary,
                input: opt(input),
                status: status.into(),
                answer: opt(answer),
            },
            sc::Entry::Interrupted => Entry::Interrupted,
            sc::Entry::Stopped => Entry::Stopped,
            sc::Entry::Renamed { name, by, own } => Entry::Renamed {
                name,
                by: id(by.0),
                own,
            },
            sc::Entry::Archived { archived, by, own } => Entry::Archived {
                archived,
                by: id(by.0),
                own,
            },
            sc::Entry::AgentErrored { message } => Entry::AgentErrored { message },
            sc::Entry::TurnUsage(usage) => Entry::TurnUsage {
                usage: usage.into(),
            },
        }
    }
}

/// A tool call as one timeline item. Branch on `kind`, never on `name`.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct ToolView {
    pub call: String,
    pub name: String,
    pub kind: ToolKind,
    pub title: String,
    pub detail: Option<String>,
    pub body: Option<BodyView>,
    pub input: Option<BlobView>,
    pub approval: Option<ToolApproval>,
    pub outcome: ToolOutcome,
}

impl From<sc::ToolView> for ToolView {
    fn from(t: sc::ToolView) -> Self {
        let sc::ToolView {
            call,
            name,
            kind,
            title,
            detail,
            body,
            input,
            approval,
            outcome,
        } = t;
        ToolView {
            call: id(call.0),
            name,
            kind: kind.into(),
            title,
            detail,
            body: opt(body),
            input: opt(input),
            approval: opt(approval),
            outcome: outcome.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct BodyView {
    pub format: BodyFormat,
    pub preview: String,
    pub lines: u32,
    pub full: Option<BlobView>,
}

impl From<sc::BodyView> for BodyView {
    fn from(b: sc::BodyView) -> Self {
        let sc::BodyView {
            format,
            preview,
            lines,
            full,
        } = b;
        BodyView {
            format: format.into(),
            preview,
            lines,
            full: opt(full),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ToolApproval {
    pub id: String,
    pub status: ApprovalStatus,
    /// This device's answer, if it gave one; once settled, where an answer
    /// that did not apply stays in sight.
    #[uniffi(default)]
    pub answer: Option<Answer>,
}

impl From<sc::ToolApproval> for ToolApproval {
    fn from(a: sc::ToolApproval) -> Self {
        let sc::ToolApproval {
            id: approval,
            status,
            answer,
        } = a;
        ToolApproval {
            id: id(approval.0),
            status: status.into(),
            answer: opt(answer),
        }
    }
}

/// How a tool call stands.
#[derive(Debug, Clone, PartialEq, uniffi::Enum)]
pub enum ToolOutcome {
    /// No result yet; `since_ms` is the call's time.
    Running {
        since_ms: i64,
    },
    Done {
        is_error: bool,
        status: Option<String>,
        preview: String,
        lines: u32,
        output: Option<BlobView>,
        /// From the call to its result.
        took_ms: u64,
    },
    Denied {
        by: String,
        by_me: bool,
    },
    Cancelled {
        reason: String,
    },
    NoResult,
}

impl From<sc::ToolOutcome> for ToolOutcome {
    fn from(o: sc::ToolOutcome) -> Self {
        match o {
            sc::ToolOutcome::Running { since } => ToolOutcome::Running {
                since_ms: epoch_ms(since),
            },
            sc::ToolOutcome::Done {
                is_error,
                status,
                preview,
                lines,
                output,
                took,
            } => ToolOutcome::Done {
                is_error,
                status,
                preview,
                lines,
                output: opt(output),
                took_ms: duration_ms(took),
            },
            sc::ToolOutcome::Denied { by, by_me } => ToolOutcome::Denied {
                by: id(by.0),
                by_me,
            },
            sc::ToolOutcome::Cancelled { reason } => ToolOutcome::Cancelled { reason },
            sc::ToolOutcome::NoResult => ToolOutcome::NoResult,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum ApprovalStatus {
    Pending,
    /// First answer wins; `rule` is the lasting rule granted, if any.
    Resolved {
        decision: Decision,
        by: String,
        by_me: bool,
        rule: Option<RuleSuggestion>,
    },
    Cancelled {
        reason: String,
    },
}

impl From<sc::ApprovalStatus> for ApprovalStatus {
    fn from(s: sc::ApprovalStatus) -> Self {
        match s {
            sc::ApprovalStatus::Pending => ApprovalStatus::Pending,
            sc::ApprovalStatus::Resolved {
                decision,
                by,
                by_me,
                rule,
            } => ApprovalStatus::Resolved {
                decision: decision.into(),
                by: id(by.0),
                by_me,
                rule: opt(rule),
            },
            sc::ApprovalStatus::Cancelled { reason } => ApprovalStatus::Cancelled { reason },
        }
    }
}

/// The approval prompt of a blocked session.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct ApprovalPrompt {
    pub id: String,
    /// The tool call the approval gates; `None` for one with no logged call.
    pub call: Option<String>,
    pub tool: String,
    pub summary: String,
    pub input: Option<BlobView>,
    /// An answer names one by its index (`Action.ResolveApproval.rule`).
    pub suggestions: Vec<RuleSuggestion>,
    /// This device's answer, if it gave one.
    pub answer: Option<Answer>,
}

impl From<sc::ApprovalPrompt> for ApprovalPrompt {
    fn from(p: sc::ApprovalPrompt) -> Self {
        let sc::ApprovalPrompt {
            id: approval,
            call,
            tool,
            summary,
            input,
            suggestions,
            answer,
        } = p;
        ApprovalPrompt {
            id: id(approval.0),
            call: call.map(|c| id(c.0)),
            tool,
            summary,
            input: opt(input),
            suggestions: vec(suggestions),
            answer: opt(answer),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct Answer {
    pub decision: Decision,
    pub rule: Option<RuleSuggestion>,
    pub outgoing: Outgoing,
    pub delivery: Option<Delivery>,
    /// Refused while the approval still waits: this device may answer
    /// again. A refused answer that is not retryable did not apply.
    #[uniffi(default = false)]
    pub retryable: bool,
}

impl From<sc::Answer> for Answer {
    fn from(a: sc::Answer) -> Self {
        let sc::Answer {
            decision,
            rule,
            outgoing,
            delivery,
            retryable,
        } = a;
        Answer {
            decision: decision.into(),
            rule: opt(rule),
            outgoing: outgoing.into(),
            delivery: opt(delivery),
            retryable,
        }
    }
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct SettingsView {
    pub blob_policy: BlobPolicy,
    pub server_address: Option<String>,
    pub device_id: Option<String>,
    /// The pinned server CA's fingerprint as `skywaved` prints it:
    /// uppercase hex pairs joined by colons.
    pub server_fingerprint: Option<String>,
}

impl From<sc::SettingsView> for SettingsView {
    fn from(s: sc::SettingsView) -> Self {
        let sc::SettingsView {
            blob_policy,
            server_address,
            device_id,
            server_fingerprint,
        } = s;
        SettingsView {
            blob_policy: blob_policy.into(),
            server_address,
            device_id: device_id.map(|d| id(d.0)),
            server_fingerprint: server_fingerprint.map(|f| f.to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// Types core re-exports from the protocol

/// Answer to an approval request. Converts both ways: actions carry it too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum Decision {
    Approve,
    Deny,
}

impl From<sc::Decision> for Decision {
    fn from(d: sc::Decision) -> Self {
        match d {
            sc::Decision::Approve => Decision::Approve,
            sc::Decision::Deny => Decision::Deny,
        }
    }
}

impl From<Decision> for sc::Decision {
    fn from(d: Decision) -> Self {
        match d {
            Decision::Approve => sc::Decision::Approve,
            Decision::Deny => sc::Decision::Deny,
        }
    }
}

/// A lasting permission rule; show `label`, never parse it.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct RuleSuggestion {
    pub label: String,
    pub scope: RuleScope,
}

impl From<sc::RuleSuggestion> for RuleSuggestion {
    fn from(r: sc::RuleSuggestion) -> Self {
        let sc::RuleSuggestion { label, scope } = r;
        RuleSuggestion {
            label,
            scope: scope.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum RuleScope {
    Session,
    Project,
    User,
}

impl From<sc::RuleScope> for RuleScope {
    fn from(s: sc::RuleScope) -> Self {
        match s {
            sc::RuleScope::Session => RuleScope::Session,
            sc::RuleScope::Project => RuleScope::Project,
            sc::RuleScope::User => RuleScope::User,
        }
    }
}

/// What a tool does, for choosing an icon and layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ToolKind {
    FileWrite,
    FileEdit,
    FileRead,
    Shell,
    Search,
    Fetch,
    Subagent,
    Other,
}

impl From<sc::ToolKind> for ToolKind {
    fn from(k: sc::ToolKind) -> Self {
        match k {
            sc::ToolKind::FileWrite => ToolKind::FileWrite,
            sc::ToolKind::FileEdit => ToolKind::FileEdit,
            sc::ToolKind::FileRead => ToolKind::FileRead,
            sc::ToolKind::Shell => ToolKind::Shell,
            sc::ToolKind::Search => ToolKind::Search,
            sc::ToolKind::Fetch => ToolKind::Fetch,
            sc::ToolKind::Subagent => ToolKind::Subagent,
            sc::ToolKind::Other => ToolKind::Other,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum BodyFormat {
    Text,
    /// A unified diff.
    Diff,
}

impl From<sc::BodyFormat> for BodyFormat {
    fn from(f: sc::BodyFormat) -> Self {
        match f {
            sc::BodyFormat::Text => BodyFormat::Text,
            sc::BodyFormat::Diff => BodyFormat::Diff,
        }
    }
}

/// What one turn took; a figure the agent does not report is `None`.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct TurnUsage {
    pub duration_ms: Option<u64>,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    pub cost_usd: Option<f64>,
    pub model: Option<String>,
}

impl From<sc::TurnUsage> for TurnUsage {
    fn from(u: sc::TurnUsage) -> Self {
        let sc::TurnUsage {
            duration_ms,
            tokens_in,
            tokens_out,
            cost_usd,
            model,
        } = u;
        TurnUsage {
            duration_ms,
            tokens_in,
            tokens_out,
            cost_usd,
            model,
        }
    }
}

/// What a session's turns took: the newest one and the sums.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct SessionUsage {
    pub last_turn: TurnUsage,
    pub total: TurnUsage,
}

impl From<sc::SessionUsage> for SessionUsage {
    fn from(u: sc::SessionUsage) -> Self {
        let sc::SessionUsage { last_turn, total } = u;
        SessionUsage {
            last_turn: last_turn.into(),
            total: total.into(),
        }
    }
}

/// Whether blobs download alongside metadata or on explicit intent.
/// Converts both ways: actions carry it too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum BlobPolicy {
    Eager,
    OnDemand,
}

impl From<sc::BlobPolicy> for BlobPolicy {
    fn from(p: sc::BlobPolicy) -> Self {
        match p {
            sc::BlobPolicy::Eager => BlobPolicy::Eager,
            sc::BlobPolicy::OnDemand => BlobPolicy::OnDemand,
        }
    }
}

impl From<BlobPolicy> for sc::BlobPolicy {
    fn from(p: BlobPolicy) -> Self {
        match p {
            BlobPolicy::Eager => sc::BlobPolicy::Eager,
            BlobPolicy::OnDemand => sc::BlobPolicy::OnDemand,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::{DateTime, Utc};

    use super::*;

    const AT_MS: i64 = 1_700_000_000_123;

    fn at() -> DateTime<Utc> {
        DateTime::from_timestamp_millis(AT_MS).unwrap()
    }

    fn s(id: impl Into<uuid::Uuid>) -> String {
        id.into().to_string()
    }

    fn blob(id: sc::BlobId) -> sc::BlobView {
        sc::BlobView {
            id,
            state: sc::BlobState::NotDownloaded { size: 7 },
        }
    }

    fn blob_mirror(id: sc::BlobId) -> BlobView {
        BlobView {
            id: s(id.0),
            state: BlobState::NotDownloaded { size: 7 },
        }
    }

    fn rule() -> sc::RuleSuggestion {
        sc::RuleSuggestion {
            label: "allow edits".into(),
            scope: sc::RuleScope::Session,
        }
    }

    fn rule_mirror() -> RuleSuggestion {
        RuleSuggestion {
            label: "allow edits".into(),
            scope: RuleScope::Session,
        }
    }

    fn delivery() -> sc::Delivery {
        sc::Delivery {
            queued_at: at(),
            first_sent_at: Some(at() + chrono::Duration::milliseconds(5)),
            answered_at: None,
            tries: 2,
        }
    }

    fn delivery_mirror() -> Delivery {
        Delivery {
            queued_at_ms: AT_MS,
            first_sent_at_ms: Some(AT_MS + 5),
            answered_at_ms: None,
            tries: 2,
        }
    }

    fn turn() -> sc::TurnUsage {
        sc::TurnUsage {
            duration_ms: Some(1500),
            tokens_in: Some(10),
            tokens_out: None,
            cost_usd: Some(0.25),
            model: Some("m".into()),
        }
    }

    fn turn_mirror() -> TurnUsage {
        TurnUsage {
            duration_ms: Some(1500),
            tokens_in: Some(10),
            tokens_out: None,
            cost_usd: Some(0.25),
            model: Some("m".into()),
        }
    }

    fn convert<C, M: From<C>>(values: Vec<C>) -> Vec<M> {
        values.into_iter().map(M::from).collect()
    }

    #[test]
    fn link_converts_every_variant() {
        let converted: Vec<Link> = convert(vec![
            sc::Link::Unpaired,
            sc::Link::Connecting,
            sc::Link::Online { latency_ms: 42 },
            sc::Link::CatchingUp {
                have: sc::Seq(3),
                server: sc::Seq(9),
            },
            sc::Link::Rejected(sc::LinkRejection::Revoked),
        ]);
        assert_eq!(
            converted,
            [
                Link::Unpaired,
                Link::Connecting,
                Link::Online { latency_ms: 42 },
                Link::CatchingUp { have: 3, server: 9 },
                Link::Rejected {
                    rejection: LinkRejection::Revoked
                },
            ]
        );

        let core = sc::Link::Reconnecting {
            attempt: 4,
            retry_at: at() + chrono::Duration::milliseconds(2500),
            offline_since: at(),
        };
        let reconnecting = Link::from(core.clone());
        assert_eq!(
            reconnecting,
            Link::Reconnecting {
                attempt: 4,
                retry_at_ms: at().timestamp_millis() + 2500,
                offline_since_ms: at().timestamp_millis(),
            }
        );
        // No clock is read on the way: the same link converted later is
        // the same value.
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(Link::from(core), reconnecting);
    }

    #[test]
    fn link_rejection_converts_every_variant() {
        let converted: Vec<LinkRejection> = convert(vec![
            sc::LinkRejection::Revoked,
            sc::LinkRejection::ProtocolMismatch {
                client: 4,
                server: 5,
            },
        ]);
        assert_eq!(
            converted,
            [
                LinkRejection::Revoked,
                LinkRejection::ProtocolMismatch {
                    client: 4,
                    server: 5
                },
            ]
        );
    }

    #[test]
    fn outgoing_converts_every_variant() {
        let converted: Vec<Outgoing> = convert(vec![
            sc::Outgoing::Queued,
            sc::Outgoing::Sending,
            sc::Outgoing::Accepted(sc::Seq(11)),
            sc::Outgoing::Failed(sc::FailReason::NoActiveTurn),
        ]);
        assert_eq!(
            converted,
            [
                Outgoing::Queued,
                Outgoing::Sending,
                Outgoing::Accepted { seq: 11 },
                Outgoing::Failed {
                    reason: FailReason::NoActiveTurn
                },
            ]
        );
    }

    #[test]
    fn fail_reason_converts_every_variant() {
        let converted: Vec<FailReason> = convert(vec![
            sc::FailReason::WorkdirNotAllowed,
            sc::FailReason::WorkdirNotFound,
            sc::FailReason::UnknownSession,
            sc::FailReason::UnknownApproval,
            sc::FailReason::ApprovalAlreadyResolved,
            sc::FailReason::ApprovalCancelled,
            sc::FailReason::NoActiveTurn,
            sc::FailReason::UnknownRule,
        ]);
        assert_eq!(
            converted,
            [
                FailReason::WorkdirNotAllowed,
                FailReason::WorkdirNotFound,
                FailReason::UnknownSession,
                FailReason::UnknownApproval,
                FailReason::ApprovalAlreadyResolved,
                FailReason::ApprovalCancelled,
                FailReason::NoActiveTurn,
                FailReason::UnknownRule,
            ]
        );
    }

    #[test]
    fn delivery_maps_times_to_epoch_millis() {
        assert_eq!(Delivery::from(delivery()), delivery_mirror());
    }

    #[test]
    fn session_state_converts_every_variant() {
        let approval = sc::ApprovalId::new();
        let converted: Vec<SessionState> = convert(vec![
            sc::SessionState::Idle,
            sc::SessionState::AgentWorking,
            sc::SessionState::AwaitingApproval(approval),
            sc::SessionState::Errored(sc::ErrorInfo {
                message: "boom".into(),
            }),
            sc::SessionState::Stopped,
        ]);
        assert_eq!(
            converted,
            [
                SessionState::Idle,
                SessionState::AgentWorking,
                SessionState::AwaitingApproval {
                    approval_id: s(approval.0)
                },
                SessionState::Errored {
                    error: ErrorInfo {
                        message: "boom".into()
                    }
                },
                SessionState::Stopped,
            ]
        );
    }

    #[test]
    fn blob_state_converts_every_variant() {
        let converted: Vec<BlobState> = convert(vec![
            sc::BlobState::NotDownloaded { size: 1 },
            sc::BlobState::Requested { size: 2 },
            sc::BlobState::Downloading {
                got: 3,
                size: 4,
                paused: true,
            },
            sc::BlobState::Failed {
                size: 5,
                reason: "sizes disagree".into(),
            },
            sc::BlobState::Available {
                path: "/data/blobs/x".into(),
            },
        ]);
        assert_eq!(
            converted,
            [
                BlobState::NotDownloaded { size: 1 },
                BlobState::Requested { size: 2 },
                BlobState::Downloading {
                    got: 3,
                    size: 4,
                    paused: true
                },
                BlobState::Failed {
                    size: 5,
                    reason: "sizes disagree".into()
                },
                BlobState::Available {
                    path: "/data/blobs/x".into()
                },
            ]
        );
    }

    fn session_view(id: sc::SessionId) -> sc::SessionView {
        let approval = sc::ApprovalId::new();
        sc::SessionView {
            id,
            title: "title".into(),
            named: true,
            rename: Some(sc::Outgoing::Sending),
            archived: false,
            workdir: Some("/w".into()),
            state: sc::SessionState::AwaitingApproval(approval),
            timeline: vec![sc::TimelineItem::Logged {
                seq: sc::Seq(1),
                at: at(),
                entry: sc::Entry::AgentMessage { text: "hi".into() },
            }],
            pending_approval: Some(sc::ApprovalPrompt {
                id: approval,
                call: None,
                tool: "Bash".into(),
                summary: "ls".into(),
                input: None,
                suggestions: vec![rule()],
                answer: Some(sc::Answer {
                    decision: sc::Decision::Approve,
                    rule: Some(rule()),
                    outgoing: sc::Outgoing::Queued,
                    delivery: Some(delivery()),
                    retryable: true,
                }),
            }),
            rules: vec![rule()],
            usage: Some(sc::SessionUsage::new(&turn())),
            interrupt: None,
            stop: Some(sc::Outgoing::Accepted(sc::Seq(2))),
            draft: Some(sc::Draft {
                text: "héllo".into(),
                cursor: 2,
            }),
            draft_rev: 3,
            last_seq: sc::Seq(9),
            unread_from: Some(sc::Seq(5)),
        }
    }

    fn session_view_mirror(id: sc::SessionId, view: &sc::SessionView) -> SessionView {
        let sc::SessionState::AwaitingApproval(approval) = view.state else {
            unreachable!()
        };
        SessionView {
            id: s(id.0),
            title: "title".into(),
            named: true,
            rename: Some(Outgoing::Sending),
            archived: false,
            workdir: Some("/w".into()),
            state: SessionState::AwaitingApproval {
                approval_id: s(approval.0),
            },
            timeline: vec![TimelineItem::Logged {
                seq: 1,
                at_ms: AT_MS,
                entry: Entry::AgentMessage { text: "hi".into() },
            }],
            pending_approval: Some(ApprovalPrompt {
                id: s(approval.0),
                call: None,
                tool: "Bash".into(),
                summary: "ls".into(),
                input: None,
                suggestions: vec![rule_mirror()],
                answer: Some(Answer {
                    decision: Decision::Approve,
                    rule: Some(rule_mirror()),
                    outgoing: Outgoing::Queued,
                    delivery: Some(delivery_mirror()),
                    retryable: true,
                }),
            }),
            rules: vec![rule_mirror()],
            usage: Some(SessionUsage {
                last_turn: turn_mirror(),
                total: turn_mirror(),
            }),
            interrupt: None,
            stop: Some(Outgoing::Accepted { seq: 2 }),
            draft: Some(Draft {
                text: "héllo".into(),
                cursor_utf16: 2,
            }),
            draft_rev: 3,
            last_seq: 9,
            unread_from: Some(5),
            prompts: vec![],
        }
    }

    #[test]
    fn a_cursor_crosses_in_utf16_units() {
        // (text, code points, UTF-16 units)
        for (text, chars, units) in [
            ("", 0, 0),
            ("hello", 3, 3),
            ("héllo", 2, 2),
            ("привет", 4, 4),
            ("a😀b", 1, 1),
            ("a😀b", 2, 3),
            ("a😀b", 3, 4),
            ("😀😀", 2, 4),
        ] {
            assert_eq!(chars_to_utf16(text, chars), units, "{text:?} chars {chars}");
            assert_eq!(utf16_to_chars(text, units), chars, "{text:?} units {units}");
        }
    }

    #[test]
    fn a_utf16_cursor_inside_a_surrogate_pair_rounds_down() {
        assert_eq!(utf16_to_chars("a😀b", 2), 1);
        assert_eq!(utf16_to_chars("😀", 1), 0);
    }

    #[test]
    fn a_cursor_past_the_end_clamps() {
        assert_eq!(utf16_to_chars("a😀", 99), 2);
        assert_eq!(utf16_to_chars("", u64::MAX), 0);
        assert_eq!(chars_to_utf16("a😀", 99), 3);
        assert_eq!(chars_to_utf16("", usize::MAX), 0);
    }

    #[test]
    fn a_draft_exposes_its_cursor_in_utf16_units() {
        let draft = Draft::from(sc::Draft {
            text: "😀 привет".into(),
            cursor: 3,
        });
        assert_eq!(draft.cursor_utf16, 4);
    }

    #[test]
    fn session_view_carries_the_prompts_newest_first() {
        let user = |text: &str| sc::TimelineItem::Logged {
            seq: sc::Seq(1),
            at: at(),
            entry: sc::Entry::UserMessage {
                command_id: sc::CommandId::new(),
                text: text.into(),
                from: sc::DeviceId::new(),
                own: false,
                outgoing: None,
                delivery: None,
            },
        };
        let mut view = session_view(sc::SessionId::new());
        view.timeline = vec![
            user("first"),
            user("again"),
            user("again"),
            sc::TimelineItem::Logged {
                seq: sc::Seq(2),
                at: at(),
                entry: sc::Entry::AgentMessage { text: "ok".into() },
            },
            sc::TimelineItem::Pending {
                command_id: sc::CommandId::new(),
                text: "queued".into(),
                outgoing: sc::Outgoing::Queued,
                discardable: true,
                delivery: None,
            },
        ];
        let expected: Vec<String> = view.prompts().into_iter().map(str::to_owned).collect();
        assert_eq!(expected, ["queued", "again", "first"]);
        assert_eq!(SessionView::from(view).prompts, expected);
    }

    #[test]
    fn completion_carries_its_fill() {
        let completion = |input: &str, paths: &[&str]| sc::Completion {
            input: input.into(),
            candidates: paths
                .iter()
                .map(|p| sc::Candidate {
                    path: (*p).into(),
                    source: sc::CandidateSource::Recent,
                })
                .collect(),
            status: sc::CompletionStatus::Ready { truncated: false },
        };
        let fill = |c: sc::Completion| Completion::from(c).fill;
        assert_eq!(fill(completion("/w", &[])), None);
        assert_eq!(fill(completion("/w", &["/w/a"])), Some("/w/a/".into()));
        assert_eq!(
            fill(completion("/w", &["/w/abc", "/w/abd"])),
            Some("/w/ab".into())
        );
        assert_eq!(fill(completion("/w/ab", &["/w/abc", "/w/abd"])), None);
    }

    #[test]
    fn screen_converts_every_variant() {
        let device = sc::DeviceId::new();
        let session = sc::SessionId::new();
        let command = sc::CommandId::new();
        let view = session_view(session);
        let view_mirror = session_view_mirror(session, &view);
        let converted: Vec<Screen> = convert(vec![
            sc::Screen::Pairing(sc::PairingView {
                paired: Some(sc::PairedInfo {
                    device_id: device,
                    server_address: Some("host:1".into()),
                }),
                status: sc::PairingStatus::Idle,
            }),
            sc::Screen::SessionList(sc::SessionListView {
                kind: sc::ListKind::Active,
                sessions: vec![sc::SessionRow {
                    id: session,
                    title: "t".into(),
                    named: false,
                    rename: None,
                    workdir: None,
                    state: sc::SessionState::Idle,
                    pending_approvals: 1,
                    last_activity: at(),
                    has_draft: true,
                    unread: 4,
                    cost_usd: Some(1.5),
                    deletable: false,
                }],
                other_count: 2,
                creating: vec![sc::PendingSession {
                    command_id: command,
                    workdir: "/w".into(),
                    outgoing: sc::Outgoing::Queued,
                    discardable: true,
                }],
                completion: Some(sc::Completion {
                    input: "/w".into(),
                    candidates: vec![sc::Candidate {
                        path: "/w/a".into(),
                        source: sc::CandidateSource::Server,
                    }],
                    status: sc::CompletionStatus::Loading,
                }),
            }),
            sc::Screen::Session(Box::new(view)),
            sc::Screen::Settings(sc::SettingsView {
                blob_policy: sc::BlobPolicy::Eager,
                server_address: None,
                device_id: Some(device),
                server_fingerprint: Some(sc::Fingerprint::of_der(b"ca")),
            }),
        ]);
        assert_eq!(
            converted,
            [
                Screen::Pairing {
                    view: PairingView {
                        paired: Some(PairedInfo {
                            device_id: s(device.0),
                            server_address: Some("host:1".into()),
                        }),
                        status: PairingStatus::Idle,
                    }
                },
                Screen::SessionList {
                    view: SessionListView {
                        kind: ListKind::Active,
                        sessions: vec![SessionRow {
                            id: s(session.0),
                            title: "t".into(),
                            named: false,
                            rename: None,
                            workdir: None,
                            state: SessionState::Idle,
                            pending_approvals: 1,
                            last_activity_ms: AT_MS,
                            has_draft: true,
                            unread: 4,
                            cost_usd: Some(1.5),
                            deletable: false,
                        }],
                        other_count: 2,
                        creating: vec![PendingSession {
                            command_id: s(command.0),
                            workdir: "/w".into(),
                            outgoing: Outgoing::Queued,
                            discardable: true,
                        }],
                        completion: Some(Completion {
                            input: "/w".into(),
                            candidates: vec![Candidate {
                                path: "/w/a".into(),
                                source: CandidateSource::Server,
                            }],
                            status: CompletionStatus::Loading,
                            fill: Some("/w/a/".into()),
                        }),
                    }
                },
                Screen::Session { view: view_mirror },
                Screen::Settings {
                    view: SettingsView {
                        blob_policy: BlobPolicy::Eager,
                        server_address: None,
                        device_id: Some(s(device.0)),
                        server_fingerprint: Some(sc::Fingerprint::of_der(b"ca").to_string()),
                    }
                },
            ]
        );
    }

    #[test]
    fn view_state_converts_whole() {
        let session = sc::SessionId::new();
        let approval = sc::ApprovalId::new();
        let view = session_view(session);
        let view_mirror = session_view_mirror(session, &view);
        let converted = ViewState::from(sc::ViewState {
            link: sc::Link::Online { latency_ms: 3 },
            queued: 2,
            attention: sc::Attention {
                approvals: vec![sc::PendingApprovalRef {
                    id: approval,
                    session_id: session,
                    title: "t".into(),
                    alerted_at: 5,
                }],
                elsewhere: 1,
                alerts: 6,
            },
            screen: sc::Screen::Session(Box::new(view)),
            can_go_back: true,
        });
        assert_eq!(
            converted,
            ViewState {
                link: Link::Online { latency_ms: 3 },
                queued: 2,
                attention: Attention {
                    approvals: vec![PendingApprovalRef {
                        id: s(approval.0),
                        session_id: s(session.0),
                        title: "t".into(),
                        alerted_at: 5,
                    }],
                    elsewhere: 1,
                    alerts: 6,
                },
                screen: Screen::Session { view: view_mirror },
                can_go_back: true,
            }
        );
    }

    #[test]
    fn pairing_status_converts_every_variant() {
        let converted: Vec<PairingStatus> = convert(vec![
            sc::PairingStatus::Idle,
            sc::PairingStatus::InProgress {
                address: "a".into(),
            },
            sc::PairingStatus::Failed {
                address: "a".into(),
                reason: "r".into(),
            },
        ]);
        assert_eq!(
            converted,
            [
                PairingStatus::Idle,
                PairingStatus::InProgress {
                    address: "a".into()
                },
                PairingStatus::Failed {
                    address: "a".into(),
                    reason: "r".into()
                },
            ]
        );
    }

    #[test]
    fn list_kind_converts_every_variant() {
        let converted: Vec<ListKind> = convert(vec![sc::ListKind::Active, sc::ListKind::Archived]);
        assert_eq!(converted, [ListKind::Active, ListKind::Archived]);
    }

    #[test]
    fn candidate_source_converts_every_variant() {
        let converted: Vec<CandidateSource> = convert(vec![
            sc::CandidateSource::Server,
            sc::CandidateSource::Recent,
        ]);
        assert_eq!(
            converted,
            [CandidateSource::Server, CandidateSource::Recent]
        );
    }

    #[test]
    fn completion_status_converts_every_variant() {
        let converted: Vec<CompletionStatus> = convert(vec![
            sc::CompletionStatus::Loading,
            sc::CompletionStatus::Ready { truncated: true },
            sc::CompletionStatus::Unavailable,
        ]);
        assert_eq!(
            converted,
            [
                CompletionStatus::Loading,
                CompletionStatus::Ready { truncated: true },
                CompletionStatus::Unavailable,
            ]
        );
    }

    #[test]
    fn timeline_item_converts_every_variant() {
        let command = sc::CommandId::new();
        let converted: Vec<TimelineItem> = convert(vec![
            sc::TimelineItem::Logged {
                seq: sc::Seq(4),
                at: at(),
                entry: sc::Entry::Interrupted,
            },
            sc::TimelineItem::Pending {
                command_id: command,
                text: "hi".into(),
                outgoing: sc::Outgoing::Sending,
                discardable: false,
                delivery: Some(delivery()),
            },
        ]);
        assert_eq!(
            converted,
            [
                TimelineItem::Logged {
                    seq: 4,
                    at_ms: AT_MS,
                    entry: Entry::Interrupted
                },
                TimelineItem::Pending {
                    command_id: s(command.0),
                    text: "hi".into(),
                    outgoing: Outgoing::Sending,
                    discardable: false,
                    delivery: Some(delivery_mirror()),
                },
            ]
        );
    }

    #[test]
    fn entry_converts_every_variant() {
        let command = sc::CommandId::new();
        let device = sc::DeviceId::new();
        let approval = sc::ApprovalId::new();
        let call = sc::CallId::new();
        let b = sc::BlobId::new();
        let converted: Vec<Entry> = convert(vec![
            sc::Entry::UserMessage {
                command_id: command,
                text: "hi".into(),
                from: device,
                own: true,
                outgoing: Some(sc::Outgoing::Accepted(sc::Seq(1))),
                delivery: Some(delivery()),
            },
            sc::Entry::AgentMessage { text: "ok".into() },
            sc::Entry::Tool(Box::new(sc::ToolView {
                call,
                name: "Edit".into(),
                kind: sc::ToolKind::FileEdit,
                title: "src/a.rs".into(),
                detail: Some("2 lines".into()),
                body: Some(sc::BodyView {
                    format: sc::BodyFormat::Diff,
                    preview: "-a\n+b".into(),
                    lines: 2,
                    full: Some(blob(b)),
                }),
                input: Some(blob(b)),
                approval: Some(sc::ToolApproval {
                    id: approval,
                    status: sc::ApprovalStatus::Pending,
                    answer: None,
                }),
                outcome: sc::ToolOutcome::NoResult,
            })),
            sc::Entry::ToolCall {
                name: "Bash".into(),
                summary: "ls".into(),
                input: Some(blob(b)),
            },
            sc::Entry::ToolResult {
                summary: "done".into(),
                output: None,
            },
            sc::Entry::Approval {
                id: approval,
                tool: "Bash".into(),
                summary: "rm".into(),
                input: None,
                status: sc::ApprovalStatus::Cancelled {
                    reason: "gone".into(),
                },
                answer: Some(sc::Answer {
                    decision: sc::Decision::Deny,
                    rule: None,
                    outgoing: sc::Outgoing::Failed(sc::FailReason::ApprovalCancelled),
                    delivery: None,
                    retryable: false,
                }),
            },
            sc::Entry::Interrupted,
            sc::Entry::Stopped,
            sc::Entry::Renamed {
                name: Some("n".into()),
                by: device,
                own: false,
            },
            sc::Entry::Archived {
                archived: true,
                by: device,
                own: true,
            },
            sc::Entry::AgentErrored {
                message: "boom".into(),
            },
            sc::Entry::TurnUsage(turn()),
        ]);
        assert_eq!(
            converted,
            [
                Entry::UserMessage {
                    command_id: s(command.0),
                    text: "hi".into(),
                    from: s(device.0),
                    own: true,
                    outgoing: Some(Outgoing::Accepted { seq: 1 }),
                    delivery: Some(delivery_mirror()),
                },
                Entry::AgentMessage { text: "ok".into() },
                Entry::Tool {
                    tool: ToolView {
                        call: s(call.0),
                        name: "Edit".into(),
                        kind: ToolKind::FileEdit,
                        title: "src/a.rs".into(),
                        detail: Some("2 lines".into()),
                        body: Some(BodyView {
                            format: BodyFormat::Diff,
                            preview: "-a\n+b".into(),
                            lines: 2,
                            full: Some(blob_mirror(b)),
                        }),
                        input: Some(blob_mirror(b)),
                        approval: Some(ToolApproval {
                            id: s(approval.0),
                            status: ApprovalStatus::Pending,
                            answer: None,
                        }),
                        outcome: ToolOutcome::NoResult,
                    }
                },
                Entry::ToolCall {
                    name: "Bash".into(),
                    summary: "ls".into(),
                    input: Some(blob_mirror(b)),
                },
                Entry::ToolResult {
                    summary: "done".into(),
                    output: None,
                },
                Entry::Approval {
                    id: s(approval.0),
                    tool: "Bash".into(),
                    summary: "rm".into(),
                    input: None,
                    status: ApprovalStatus::Cancelled {
                        reason: "gone".into()
                    },
                    answer: Some(Answer {
                        decision: Decision::Deny,
                        rule: None,
                        outgoing: Outgoing::Failed {
                            reason: FailReason::ApprovalCancelled
                        },
                        delivery: None,
                        retryable: false,
                    }),
                },
                Entry::Interrupted,
                Entry::Stopped,
                Entry::Renamed {
                    name: Some("n".into()),
                    by: s(device.0),
                    own: false,
                },
                Entry::Archived {
                    archived: true,
                    by: s(device.0),
                    own: true,
                },
                Entry::AgentErrored {
                    message: "boom".into()
                },
                Entry::TurnUsage {
                    usage: turn_mirror()
                },
            ]
        );
    }

    #[test]
    fn tool_outcome_converts_every_variant() {
        let device = sc::DeviceId::new();
        let b = sc::BlobId::new();
        let converted: Vec<ToolOutcome> = convert(vec![
            sc::ToolOutcome::Running { since: at() },
            sc::ToolOutcome::Done {
                is_error: true,
                status: Some("exit 1".into()),
                preview: "err".into(),
                lines: 1,
                output: Some(blob(b)),
                took: Duration::from_millis(1234),
            },
            sc::ToolOutcome::Denied {
                by: device,
                by_me: true,
            },
            sc::ToolOutcome::Cancelled { reason: "r".into() },
            sc::ToolOutcome::NoResult,
        ]);
        assert_eq!(
            converted,
            [
                ToolOutcome::Running { since_ms: AT_MS },
                ToolOutcome::Done {
                    is_error: true,
                    status: Some("exit 1".into()),
                    preview: "err".into(),
                    lines: 1,
                    output: Some(blob_mirror(b)),
                    took_ms: 1234,
                },
                ToolOutcome::Denied {
                    by: s(device.0),
                    by_me: true
                },
                ToolOutcome::Cancelled { reason: "r".into() },
                ToolOutcome::NoResult,
            ]
        );
    }

    #[test]
    fn approval_status_converts_every_variant() {
        let device = sc::DeviceId::new();
        let converted: Vec<ApprovalStatus> = convert(vec![
            sc::ApprovalStatus::Pending,
            sc::ApprovalStatus::Resolved {
                decision: sc::Decision::Deny,
                by: device,
                by_me: false,
                rule: Some(rule()),
            },
            sc::ApprovalStatus::Cancelled { reason: "r".into() },
        ]);
        assert_eq!(
            converted,
            [
                ApprovalStatus::Pending,
                ApprovalStatus::Resolved {
                    decision: Decision::Deny,
                    by: s(device.0),
                    by_me: false,
                    rule: Some(rule_mirror()),
                },
                ApprovalStatus::Cancelled { reason: "r".into() },
            ]
        );
    }

    #[test]
    fn decision_converts_every_variant_both_ways() {
        let converted: Vec<Decision> = convert(vec![sc::Decision::Approve, sc::Decision::Deny]);
        assert_eq!(converted, [Decision::Approve, Decision::Deny]);
        let back: Vec<sc::Decision> = convert(converted);
        assert_eq!(back, [sc::Decision::Approve, sc::Decision::Deny]);
    }

    #[test]
    fn rule_scope_converts_every_variant() {
        let converted: Vec<RuleScope> = convert(vec![
            sc::RuleScope::Session,
            sc::RuleScope::Project,
            sc::RuleScope::User,
        ]);
        assert_eq!(
            converted,
            [RuleScope::Session, RuleScope::Project, RuleScope::User]
        );
    }

    #[test]
    fn tool_kind_converts_every_variant() {
        let converted: Vec<ToolKind> = convert(vec![
            sc::ToolKind::FileWrite,
            sc::ToolKind::FileEdit,
            sc::ToolKind::FileRead,
            sc::ToolKind::Shell,
            sc::ToolKind::Search,
            sc::ToolKind::Fetch,
            sc::ToolKind::Subagent,
            sc::ToolKind::Other,
        ]);
        assert_eq!(
            converted,
            [
                ToolKind::FileWrite,
                ToolKind::FileEdit,
                ToolKind::FileRead,
                ToolKind::Shell,
                ToolKind::Search,
                ToolKind::Fetch,
                ToolKind::Subagent,
                ToolKind::Other,
            ]
        );
    }

    #[test]
    fn body_format_converts_every_variant() {
        let converted: Vec<BodyFormat> = convert(vec![sc::BodyFormat::Text, sc::BodyFormat::Diff]);
        assert_eq!(converted, [BodyFormat::Text, BodyFormat::Diff]);
    }

    #[test]
    fn blob_policy_converts_every_variant_both_ways() {
        let converted: Vec<BlobPolicy> =
            convert(vec![sc::BlobPolicy::Eager, sc::BlobPolicy::OnDemand]);
        assert_eq!(converted, [BlobPolicy::Eager, BlobPolicy::OnDemand]);
        let back: Vec<sc::BlobPolicy> = convert(converted);
        assert_eq!(back, [sc::BlobPolicy::Eager, sc::BlobPolicy::OnDemand]);
    }

    #[test]
    fn detached_core_snapshot_converts() {
        let dir = tempfile::tempdir().unwrap();
        let core = sc::Core::open_detached(dir.path()).unwrap();
        let view = ViewState::from(core.view());
        assert!(matches!(view.screen, Screen::Pairing { .. }), "{view:?}");
    }
}
