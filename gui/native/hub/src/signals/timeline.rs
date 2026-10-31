use rinf::{RustSignal, SignalPiece};
use serde::Serialize;

/// The open session's timeline: a snapshot, then keyed diffs against it.
/// One type, so a snapshot and the diffs after it arrive in order.
///
/// Keys are strings: `s:<seq>` for an event of the log, `p:<command id>`
/// for one of this device's messages not logged yet. A pending message's
/// key changes once, when it is logged: `Removed` the pending key, then
/// `Upserted` the logged one with `replaces` naming the pending key, so a
/// selection or scroll anchor on it can follow.
#[derive(Debug, Clone, PartialEq, Serialize, RustSignal)]
#[expect(
    clippy::large_enum_variant,
    reason = "upserts are most of the traffic; boxing would cost one allocation each"
)]
pub enum TimelineUpdate {
    /// Replaces the whole list: a session opened, the app asked
    /// (`AppReady`), or the change was not a keyed diff. `session_id` is
    /// `None`, and `items` empty, when no session is open.
    Snapshot {
        session_id: Option<String>,
        items: Vec<TimelineRow>,
    },
    /// Drop any row keyed `key` (or `replaces`), then insert `item` at
    /// `index`. Diffs come with ascending indices after all removals, so
    /// applying them in order rebuilds the list.
    Upserted {
        key: String,
        item: TimelineRow,
        index: u32,
        /// The pending message this logged one became, already removed.
        replaces: Option<String>,
    },
    /// Drop the row keyed `key`.
    Removed { key: String },
}

/// One timeline item as the GUI shows it; every label is final.
#[derive(Debug, Clone, PartialEq, Serialize, SignalPiece)]
pub struct TimelineRow {
    pub key: String,
    pub kind: RowKind,
    /// Who wrote it: `you`, `agent`, another device's name; `None` for
    /// lines that are not messages.
    pub author: Option<String>,
    /// This device wrote it.
    pub own: bool,
    /// A message's text (line breaks kept), or the line a notice reads
    /// (`interrupted · the agent stopped its turn`, `turn · 12.4s ·
    /// $0.04`). Empty for tool items: see `tool`.
    pub text: String,
    /// `14:02` (UTC), only where the minute differs from the last time
    /// shown above; `None` otherwise and for pending messages.
    pub time_label: Option<String>,
    /// When the server logged it, ms since the Unix epoch; `None` while
    /// pending.
    pub at_ms: Option<i64>,
    /// A separator goes above: a new turn starts here.
    pub turn_start: bool,
    /// The "new" rule goes above: the unread part starts here. Never
    /// together with `turn_start`.
    pub unread_rule: bool,
    /// The item can be selected (messages and tool items).
    pub selectable: bool,
    /// `ExpandItem` would show more (only logged items with sections).
    pub expandable: bool,
    pub expanded: bool,
    /// Delivery state of this device's message.
    pub outgoing: Option<OutgoingView>,
    /// How this device's message went out: `queued 2m10s ago · sending,
    /// try 3`; ticks while unanswered.
    pub delivery_label: Option<String>,
    /// `EditQueued` and `Discard` take this pending message back.
    pub discardable: bool,
    /// This device's pending message's command id, for `EditQueued` and
    /// `Discard`; `None` for logged rows.
    pub command_id: Option<String>,
    /// What a tool item (or a legacy call, result or approval) did.
    pub tool: Option<ToolFacts>,
    /// The content shown beneath the title, as it shows now (collapsed or
    /// expanded); empty for most messages.
    pub sections: Vec<SectionView>,
}

/// What kind of item a row is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, SignalPiece)]
pub enum RowKind {
    /// A logged message from any device.
    UserMessage,
    /// This device's message, not logged yet.
    PendingMessage,
    AgentMessage,
    /// A tool call with its approval and result folded in.
    Tool,
    /// A call, result or approval of a log without call ids.
    ToolCall,
    ToolResult,
    Approval,
    Interrupted,
    Stopped,
    Renamed,
    Archived,
    AgentError,
    TurnUsage,
}

/// A command's delivery state with its words.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct OutgoingView {
    pub state: OutgoingState,
    /// `queued`, `sending`, `accepted`, `failed: <why>`.
    pub label: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, SignalPiece)]
pub enum OutgoingState {
    Queued,
    Sending,
    Accepted,
    Failed,
}

/// A tool item's head line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct ToolFacts {
    pub kind: ToolKindView,
    /// The agent's own tool name, for display only.
    pub name: String,
    /// The first line of what it acts on: a path, a command.
    pub title: String,
    /// The first line of its detail, if any.
    pub detail: Option<String>,
    pub state: ToolState,
    /// How it stands: `needs approval`, `running 12s`, `exit 1 · 0.4s`,
    /// `approved on phone · 2.1s`, `denied here`; empty for a legacy
    /// call or result. Ticks while running.
    pub state_label: String,
    /// While collapsed, the state of a blob no visible section shows:
    /// `input 4.2 KiB`, `output 1.0 MiB · requested`.
    pub aside: Option<String>,
}

/// Which kind of tool, for its icon. Never its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, SignalPiece)]
pub enum ToolKindView {
    FileWrite,
    FileEdit,
    FileRead,
    Shell,
    Search,
    Fetch,
    Subagent,
    Other,
}

/// A tool item's state, for its colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, SignalPiece)]
pub enum ToolState {
    /// No state: a legacy call or result.
    Plain,
    NeedsApproval,
    Running,
    /// Done, or a standalone approval approved.
    Done,
    Failed,
    Denied,
    Cancelled,
    NoResult,
}

/// Content under an item's title.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct SectionView {
    pub format: SectionFormat,
    /// File content: show `first_line + i` before each line.
    pub numbered: bool,
    pub first_line: u32,
    /// The call's whole input rather than what it did.
    pub input: bool,
    pub lines: Vec<SectionLine>,
    /// What the lines leave out: `… +12 lines` collapsed, `cut short: 400
    /// lines of 1.2 MiB shown` expanded. Never the blob's state: that is
    /// [`BlobFacts::label`], shown beside it.
    pub note: Option<String>,
    /// The whole content's blob, if the lines are not all of it.
    pub blob: Option<BlobFacts>,
    /// Expanded, the blob was shown only up to the display cap (64 KiB or
    /// 400 lines); `OpenFull` opens the whole file.
    pub cut_short: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, SignalPiece)]
pub enum SectionFormat {
    Text,
    Diff,
}

/// One content line with its colour.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct SectionLine {
    pub text: String,
    pub style: LineStyle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, SignalPiece)]
pub enum LineStyle {
    Plain,
    /// A diff's `+` line.
    Added,
    /// A diff's `-` line.
    Removed,
    /// A diff's `+++`/`---` line.
    FileHeader,
    /// A diff's `@@` line.
    Hunk,
    /// What a failed tool said.
    Error,
}

/// A blob behind a section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct BlobFacts {
    /// For `DownloadBlob`.
    pub blob_id: String,
    pub state: BlobStateKind,
    /// Where its download stands: `not downloaded · 4.0 KiB`,
    /// `downloading · 1.0 KiB of 4.0 KiB (25%)`, `downloaded`.
    pub label: String,
    /// `DownloadBlob` would fetch it: not downloaded, or failed.
    pub can_download: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, SignalPiece)]
pub enum BlobStateKind {
    NotDownloaded,
    Requested,
    Downloading,
    Paused,
    Failed,
    Available,
}
