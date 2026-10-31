//! How the head reads a session timeline: which items can be selected,
//! where turns start, and what content an item shows beneath its title,
//! with the blobs behind it. A head's input handling and its rendering both
//! go through here, so what opening an item shows is what the screen shows,
//! and what a copy takes is what it shows.

use skywave_core::{
    BlobState, BlobView, BodyFormat, BodyView, CommandId, Delivery, Entry, Outgoing, Query, Seq,
    SessionView, TimelineItem, ToolKind, ToolOutcome, ToolView,
};

/// Most preview lines an item shows while collapsed.
pub const PREVIEW_LINES: usize = 8;

/// A timeline item's identity, stable as items arrive; scrolling and the
/// selection anchor to it. A pending message's key changes once, when it is
/// logged; [`Entry::UserMessage`] carries the command id to follow it.
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

/// Messages (any device's, logged or pending), tool items and agent messages
/// can be selected; tool items expand.
pub fn selectable(item: &TimelineItem) -> Option<ItemKey> {
    match item {
        TimelineItem::Pending { .. }
        | TimelineItem::Logged {
            entry:
                Entry::UserMessage { .. }
                | Entry::Tool(_)
                | Entry::ToolCall { .. }
                | Entry::ToolResult { .. }
                | Entry::Approval { .. }
                | Entry::AgentMessage { .. },
            ..
        } => Some(ItemKey::of(item)),
        _ => None,
    }
}

/// The timeline index of the item `key` names. A pending message that is
/// gone is found as the logged message it became.
pub fn find(view: &SessionView, key: ItemKey) -> Option<usize> {
    let here = view
        .timeline
        .iter()
        .position(|item| ItemKey::of(item) == key);
    here.or_else(|| {
        let ItemKey::Pending(command) = key else {
            return None;
        };
        view.timeline.iter().position(|item| {
            matches!(
                item,
                TimelineItem::Logged {
                    entry: Entry::UserMessage { command_id, .. },
                    ..
                } if *command_id == command
            )
        })
    })
}

/// The pending message `selected` names, with its delivery state, if the
/// core marks it discardable: what a head may edit or take back.
pub fn discardable(view: &SessionView, selected: Option<ItemKey>) -> Option<(CommandId, Outgoing)> {
    let Some(ItemKey::Pending(id)) = selected else {
        return None;
    };
    view.timeline.iter().find_map(|item| match item {
        TimelineItem::Pending {
            command_id,
            outgoing,
            discardable: true,
            ..
        } if *command_id == id => Some((id, *outgoing)),
        _ => None,
    })
}

/// How an own message went out, with its delivery state: what its detail
/// line shows when selected. `None` for other devices' messages and for
/// everything else.
pub fn delivery(item: &TimelineItem) -> Option<(Outgoing, Delivery)> {
    match item {
        TimelineItem::Pending {
            outgoing,
            delivery: Some(delivery),
            ..
        }
        | TimelineItem::Logged {
            entry:
                Entry::UserMessage {
                    outgoing: Some(outgoing),
                    delivery: Some(delivery),
                    ..
                },
            ..
        } => Some((*outgoing, *delivery)),
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
    /// The call's whole input rather than what it did.
    pub input: bool,
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
            input: false,
        }
    }

    /// A whole input, shown only when expanded.
    fn input(blob: &'a BlobView) -> Section<'a> {
        Section {
            blob: Some(blob),
            hidden: true,
            input: true,
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
/// output. Output beside a body is shown only when it is an error or the
/// body is a shell command; the rest waits for the item to expand. The
/// whole input is offered when nothing else has a blob.
pub fn tool_sections(view: &ToolView) -> Vec<Section<'_>> {
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
            input: false,
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
            hidden: !is_error && view.body.is_some() && view.kind != ToolKind::Shell,
            input: false,
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

/// The timeline item the pending approval gates, with its timeline index:
/// its grouped call, else (no logged call) the approval itself.
pub fn gated_item(view: &SessionView) -> Option<(usize, &TimelineItem)> {
    let prompt = view.pending_approval.as_ref()?;
    view.timeline
        .iter()
        .enumerate()
        .find(|(_, item)| match item {
            TimelineItem::Logged {
                entry: Entry::Tool(tool),
                ..
            } => prompt.call == Some(tool.call),
            TimelineItem::Logged {
                entry: Entry::Approval { id, .. },
                ..
            } => prompt.call.is_none() && *id == prompt.id,
            _ => false,
        })
}

/// `query` matches `entry` only where it shows once expanded: in a hidden
/// section, or past the lines a section shows while collapsed. A search
/// jumping to it expands it.
pub fn match_hidden(entry: &Entry, query: &Query) -> bool {
    let first_line = |text: &str| text.split('\n').next().unwrap_or_default().to_owned();
    let mut head: Vec<String> = match entry {
        Entry::Tool(view) => {
            let mut head = vec![view.name.clone(), first_line(&view.title)];
            head.extend(view.detail.as_deref().map(first_line));
            if let ToolOutcome::Done {
                status: Some(status),
                ..
            } = &view.outcome
            {
                head.push(status.clone());
            }
            head
        }
        Entry::ToolCall { name, summary, .. }
        | Entry::Approval {
            tool: name,
            summary,
            ..
        } => vec![name.clone(), first_line(summary)],
        Entry::ToolResult { .. } => Vec::new(),
        _ => return false,
    };
    for section in sections(entry).iter().filter(|s| !s.hidden) {
        head.extend(section.text.lines().take(PREVIEW_LINES).map(str::to_owned));
    }
    !head.iter().any(|text| query.matches(text))
}

/// The first blob behind `entry` that nobody asked for yet, or whose
/// download failed; what a download request fetches.
pub fn wanted_blob(entry: &Entry) -> Option<&BlobView> {
    sections(entry)
        .into_iter()
        .filter_map(|s| s.blob)
        .find(|blob| {
            matches!(
                blob.state,
                BlobState::NotDownloaded { .. } | BlobState::Failed { .. }
            )
        })
}

/// What a copy takes from an item: text at hand, or a blob's whole
/// content, never the preview of one.
#[derive(Debug, Clone, PartialEq)]
pub enum Content {
    Text(String),
    /// With `strip` dropped from the content's start if it begins with it.
    Blob {
        blob: BlobView,
        strip: Option<&'static str>,
    },
}

impl Content {
    fn of(section: &Section) -> Content {
        match section.blob {
            Some(blob) => Content::Blob {
                blob: blob.clone(),
                strip: None,
            },
            None => Content::Text(section.text.to_owned()),
        }
    }
}

/// What a shell call's title, and so its body, starts with (see
/// `ToolDisplay::title`); a copied command goes without it.
const PROMPT: &str = "$ ";

/// A message's text, logged or pending.
fn message(item: &TimelineItem) -> Option<&str> {
    match item {
        TimelineItem::Pending { text, .. }
        | TimelineItem::Logged {
            entry: Entry::UserMessage { text, .. } | Entry::AgentMessage { text },
            ..
        } => Some(text),
        _ => None,
    }
}

/// The main copy target: a message's text; else the last section shown
/// while collapsed: a call's output, the diff or file an edit or write
/// shows in its stead, its body while no result is here; else the call, as
/// [`call_content`].
pub fn main_content(item: &TimelineItem) -> Option<Content> {
    if let Some(text) = message(item) {
        return Some(Content::Text(text.to_owned()));
    }
    let TimelineItem::Logged { entry, .. } = item else {
        return None;
    };
    sections(entry)
        .iter()
        .rev()
        .find(|s| !s.hidden)
        .map(Content::of)
        .or_else(|| call_content(item))
}

/// The call copy target: a shell call's whole command (its body when it
/// has one, else its title) without the `$ ` prompt; title and detail of
/// any other call; a message's text.
pub fn call_content(item: &TimelineItem) -> Option<Content> {
    if let Some(text) = message(item) {
        return Some(Content::Text(text.to_owned()));
    }
    let TimelineItem::Logged { entry, .. } = item else {
        return None;
    };
    let unprompted = |text: &str| text.strip_prefix(PROMPT).unwrap_or(text).to_owned();
    match entry {
        // Decided on the kind, never the tool's name (D3).
        Entry::Tool(view) if view.kind == ToolKind::Shell => Some(match &view.body {
            Some(BodyView {
                full: Some(blob), ..
            }) => Content::Blob {
                blob: blob.clone(),
                strip: Some(PROMPT),
            },
            Some(body) => Content::Text(unprompted(&body.preview)),
            None => Content::Text(unprompted(&view.title)),
        }),
        Entry::Tool(view) => Some(Content::Text(match &view.detail {
            Some(detail) => format!("{}\n{detail}", view.title),
            None => view.title.clone(),
        })),
        Entry::ToolCall { summary, .. }
        | Entry::Approval { summary, .. }
        | Entry::ToolResult { summary, .. } => Some(Content::Text(summary.clone())),
        _ => None,
    }
}
