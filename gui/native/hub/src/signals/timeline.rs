use rinf::{RustSignal, SignalPiece};
use serde::Serialize;

use super::crop::BlobTargetView;

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
    /// An agent message's `text` as markdown blocks; empty for every other
    /// row. Drawn in place of `text` unless a search marks matches in it.
    pub markdown: Vec<MarkdownBlock>,
    /// What the row is marked with beside its author or name: `imported`
    /// on the imported kinds; `None` for every other.
    pub mark: Option<String>,
    /// `14:02` (UTC), only where the minute differs from the last time
    /// shown above; `None` otherwise and for pending messages.
    pub time_label: Option<String>,
    /// When the server logged it, ms since the Unix epoch; for imported
    /// history when the agent recorded it. `None` while pending.
    pub at_ms: Option<i64>,
    /// A separator goes above: a new turn starts here.
    pub turn_start: bool,
    /// The "new" rule goes above: the unread part starts here. Never
    /// together with `turn_start`.
    pub unread_rule: bool,
    /// The item is one the TUI can select (messages and tool items): it
    /// takes a tap and has a menu with Copy.
    pub selectable: bool,
    /// `ExpandItem` would show more (only logged items with sections).
    pub expandable: bool,
    pub expanded: bool,
    /// Delivery state of this device's message.
    pub outgoing: Option<OutgoingView>,
    /// How this device's message went out: `queued 14:02:11 · sending,
    /// try 3`.
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
    /// A tool call's output kept as a blob, and what may be done to it:
    /// its menu goes beside the section whose `blob` is `blob_id`.
    pub blob_target: Option<BlobTargetView>,
    /// A message's images in the order attached: a queued message's, or a
    /// logged message's; empty for every other row.
    pub images: Vec<RowImage>,
    /// A queued message's non-image files, in order; empty for every other
    /// row.
    pub files: Vec<RowFile>,
    /// The message was refused because a file it writes is already in the
    /// workdir with other content: `ResendReplacing` sends it again, replacing
    /// that file.
    pub can_resend_replacing: bool,
}

/// A non-image file on a queued message's row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct RowFile {
    /// The name it is written as in the workdir root.
    pub name: String,
    /// `812 B`, `1.2 MiB`.
    pub size_label: String,
    /// The sender found another file at that name in the workdir.
    pub found: bool,
}

/// An image on a message's row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct RowImage {
    /// The image's blob, for `OpenFull`; none for a queued message's image
    /// or a deleted one.
    pub blob_id: Option<String>,
    /// The file to show, when it is on this device: an available blob, or
    /// a queued message's own file.
    pub path: Option<String>,
    /// What a placeholder says, e.g. `340.0 KiB` or `image deleted`.
    pub label: String,
    /// Its download would fetch it: not downloaded, or failed.
    pub can_download: bool,
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
    /// A context was cleared or an earlier one made current again: what
    /// follows went to that context.
    ContextDivider,
    AgentError,
    TurnUsage,
    /// The agent switched its mode or model.
    AgentSetting,
    /// The agent refused a switch and kept what it had: a warning.
    SettingRefused,
    /// The agent runs a version the server was not checked against: a
    /// warning.
    AgentVersion,
    /// History of an adopted conversation, from before it came to
    /// Skywave: a record, shown apart from the session's own items and
    /// marked with `mark`. A user message of it.
    ImportedUserMessage,
    /// An agent message of imported history.
    ImportedAgentMessage,
    /// A tool call of imported history with its result folded in, or a
    /// result whose call was not imported (`tool.name` `result`). Never
    /// running and never gated.
    ImportedTool,
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
    /// The call id to open with `ViewSubagent`: set only for a call that
    /// starts a subagent whose steps are known.
    pub opens: Option<String>,
    /// How the subagent it starts stands: `running · 14 steps · Write
    /// src/sync.rs`, `finished · 14 steps`. Shown instead of
    /// `state_label`, which then says how the launch itself went; `None`
    /// for any other call, and while its approval is pending or it failed.
    pub status: Option<String>,
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
    /// Shown above the lines: what the section is, when its place does not
    /// say (the input an approval asked about, differing from the call's).
    pub label: Option<String>,
    pub lines: Vec<SectionLine>,
    /// The whole of a long agent message, expanded, as markdown blocks:
    /// drawn in place of `lines` unless a search marks matches in them.
    /// Empty for every other section.
    pub markdown: Vec<MarkdownBlock>,
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

/// One block of a message as markdown (present's `markdown::Block`): a
/// list item's or a quote's content sits `depth` lists and `quote` quotes
/// deep, the first block of an item carrying its `marker`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct MarkdownBlock {
    pub kind: MarkdownKind,
    pub depth: u8,
    pub quote: u8,
    /// `•`, `3.`, `☐` or `☑`.
    pub marker: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub enum MarkdownKind {
    Paragraph {
        spans: Vec<MarkdownSpan>,
    },
    /// `level` 1 to 6.
    Heading {
        level: u8,
        spans: Vec<MarkdownSpan>,
    },
    /// `text`'s lines joined by `\n`; `lang` from the fence, for its label.
    Code {
        lang: Option<String>,
        text: String,
    },
    Rule,
    /// One `align` per column; a row may have fewer cells.
    Table {
        align: Vec<MarkdownAlign>,
        head: Vec<MarkdownCell>,
        rows: Vec<MarkdownRow>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, SignalPiece)]
pub enum MarkdownAlign {
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct MarkdownRow {
    pub cells: Vec<MarkdownCell>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct MarkdownCell {
    pub spans: Vec<MarkdownSpan>,
}

/// A run of text in one style; `text` may hold `\n`, a hard line break.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct MarkdownSpan {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    /// Inline code.
    pub code: bool,
    pub strike: bool,
    /// A web or mail link the GUI may open, after showing where it goes.
    pub link: Option<String>,
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
