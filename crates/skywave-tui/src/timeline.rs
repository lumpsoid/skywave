//! How the head reads a session timeline: which items can be selected,
//! where turns start, and what content an item shows beneath its title,
//! with the blobs behind it. Keys ([`app`](crate::app)) and rendering
//! ([`ui`](crate::ui)) both go through here, so what Enter opens is what the
//! screen shows.

use skywave_core::{
    BlobState, BlobView, BodyFormat, CommandId, Entry, Seq, TimelineItem, ToolKind, ToolOutcome,
    ToolView,
};

/// Most preview lines an item shows while collapsed.
pub const PREVIEW_LINES: usize = 8;

/// A timeline item's identity, stable as items arrive; scrolling anchors to
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemKey {
    Logged(Seq),
    Pending(CommandId),
}

impl ItemKey {
    pub fn of(item: &TimelineItem) -> ItemKey {
        match item {
            TimelineItem::Logged { seq, .. } => ItemKey::Logged(*seq),
            TimelineItem::Pending { command_id, .. } => ItemKey::Pending(*command_id),
        }
    }
}

/// Tool items and agent messages can be selected; tool items expand.
pub fn selectable(item: &TimelineItem) -> Option<Seq> {
    match item {
        TimelineItem::Logged {
            seq,
            entry:
                Entry::Tool(_)
                | Entry::ToolCall { .. }
                | Entry::ToolResult { .. }
                | Entry::Approval { .. }
                | Entry::AgentMessage { .. },
            ..
        } => Some(*seq),
        _ => None,
    }
}

/// A user message, logged or not, starts a turn.
pub fn starts_turn(item: &TimelineItem) -> bool {
    matches!(
        item,
        TimelineItem::Pending { .. }
            | TimelineItem::Logged {
                entry: Entry::UserMessage { .. },
                ..
            }
    )
}

/// Content shown beneath an item's title: a bounded preview, expanding to
/// its blob.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Section<'a> {
    /// Head of the content, with real newlines; may be empty.
    pub text: &'a str,
    /// Line count of the whole content, when known.
    pub lines: Option<u32>,
    pub format: BodyFormat,
    /// File content: shown with line numbers.
    pub numbered: bool,
    /// The tool failed and this is what it said.
    pub error: bool,
    /// The whole content, when `text` does not hold all of it.
    pub blob: Option<&'a BlobView>,
    /// Shown only while the item is expanded.
    pub hidden: bool,
}

impl<'a> Section<'a> {
    fn text(text: &'a str) -> Section<'a> {
        Section {
            text,
            lines: None,
            format: BodyFormat::Text,
            numbered: false,
            error: false,
            blob: None,
            hidden: false,
        }
    }

    /// A whole input, shown only when expanded.
    fn input(blob: &'a BlobView) -> Section<'a> {
        Section {
            blob: Some(blob),
            hidden: true,
            ..Section::text("")
        }
    }
}

/// The sections under an item's title, in display order. Empty for items
/// that are only a line of text.
pub fn sections(entry: &Entry) -> Vec<Section<'_>> {
    let mut out = Vec::new();
    match entry {
        Entry::Tool(view) => return tool_sections(view),
        Entry::ToolCall { summary, input, .. } | Entry::Approval { summary, input, .. } => {
            if let Some((_, rest)) = summary.split_once('\n') {
                out.push(Section::text(rest));
            }
            out.extend(input.as_ref().map(Section::input));
        }
        Entry::ToolResult { summary, output } => out.push(Section {
            blob: output.as_ref(),
            ..Section::text(summary)
        }),
        _ => {}
    }
    out
}

/// A grouped call: its body (e.g. the file written or the diff), then its
/// output. Output beside a body is shown only when it is an error; the rest
/// waits for Enter. The whole input is offered when nothing else has a blob.
fn tool_sections(view: &ToolView) -> Vec<Section<'_>> {
    let mut out = Vec::new();
    if let Some(body) = &view.body {
        out.push(Section {
            text: &body.preview,
            lines: Some(body.lines),
            format: body.format,
            numbered: body.format == BodyFormat::Text && is_file(view.kind),
            error: false,
            blob: body.full.as_ref(),
            hidden: false,
        });
    }
    if let ToolOutcome::Done {
        is_error,
        preview,
        lines,
        output,
        ..
    } = &view.outcome
        && (!preview.is_empty() || output.is_some())
    {
        out.push(Section {
            text: preview,
            lines: Some(*lines),
            format: BodyFormat::Text,
            numbered: view.kind == ToolKind::FileRead && view.body.is_none() && !is_error,
            error: *is_error,
            blob: output.as_ref(),
            hidden: !is_error && view.body.is_some(),
        });
    }
    if out.iter().all(|s| s.blob.is_none())
        && let Some(input) = &view.input
    {
        out.push(Section::input(input));
    }
    out
}

/// Kinds whose content is a file's lines.
fn is_file(kind: ToolKind) -> bool {
    matches!(kind, ToolKind::FileWrite | ToolKind::FileRead)
}

/// The first blob behind `entry` that is not here yet; what `d` fetches.
pub fn wanted_blob(entry: &Entry) -> Option<&BlobView> {
    sections(entry)
        .into_iter()
        .filter_map(|s| s.blob)
        .find(|blob| !matches!(blob.state, BlobState::Available { .. }))
}
