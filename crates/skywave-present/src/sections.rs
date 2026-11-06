//! How the head reads a session timeline: which items can be selected,
//! where turns start, and what content an item shows beneath its title,
//! with the blobs behind it. A head's input handling and its rendering both
//! go through here, so what opening an item shows is what the screen shows,
//! and what a copy takes is what it shows.

use std::borrow::Cow;

use skywave_core::{
    ApprovalPrompt, BlobChange, BlobState, BlobView, BodyFormat, BodyView, CommandId, Delivery,
    DeviceRef, Entry, FullView, ImportedEntry, Outgoing, Query, Seq, SessionView, TimelineItem,
    ToolKind, ToolOutcome, ToolView,
};

use crate::format::device_word;

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
/// can be selected, imported ones too; tool items expand.
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
                | Entry::AgentMessage { .. }
                | Entry::Imported { .. },
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
    /// Line count of the whole content, when known: what a crop kept once
    /// the server cropped it ([`whole_lines`]).
    pub lines: Option<u32>,
    pub format: BodyFormat,
    /// File content: shown with line numbers.
    pub numbered: bool,
    /// The tool failed and this is what it said.
    pub error: bool,
    /// The whole content, when `text` does not hold all of it: its blob, or
    /// word that the server could not keep it ([`Section::lost`]).
    pub full: Option<&'a FullView>,
    /// Shown only while the item is expanded.
    pub hidden: bool,
    /// The call's whole input rather than what it did.
    pub input: bool,
}

/// What a head says of an input the server could not keep: the summary is
/// all there is. Matters most on an approval, decided on that summary.
pub const INPUT_LOST: &str = "the server could not store the full input: you are seeing a summary";

/// What a head says of other content the server could not keep: the
/// preview is all there is.
pub const CONTENT_LOST: &str = "the server could not store the rest: you are seeing only its start";

impl<'a> Section<'a> {
    fn text(text: &'a str) -> Section<'a> {
        Section {
            text,
            lines: None,
            format: BodyFormat::Text,
            numbered: false,
            error: false,
            full: None,
            hidden: false,
            input: false,
        }
    }

    /// A whole input, shown only when expanded.
    fn input(full: &'a FullView) -> Section<'a> {
        Section {
            full: Some(full),
            hidden: true,
            input: true,
            ..Section::text("")
        }
    }

    /// The blob holding the whole content, unless there is none or it was
    /// lost.
    pub fn blob(&self) -> Option<&'a BlobView> {
        self.full.and_then(FullView::blob)
    }

    /// What to say beside this section when the server could not keep its
    /// whole content ([`INPUT_LOST`], [`CONTENT_LOST`]); `None` otherwise.
    pub fn lost(&self) -> Option<&'static str> {
        matches!(self.full, Some(FullView::Lost)).then_some(if self.input {
            INPUT_LOST
        } else {
            CONTENT_LOST
        })
    }

    /// What to say beside this section when its preview is all there is:
    /// the server could not keep the rest ([`Section::lost`]), or it was
    /// deleted there and this device has no copy ([`deleted_note`]).
    pub fn note(&self) -> Option<Cow<'static, str>> {
        match self.full? {
            FullView::Deleted { by } => Some(Cow::Owned(deleted_note(by))),
            _ => self.lost().map(Cow::Borrowed),
        }
    }
}

/// The line count of the whole content behind a preview whose event says
/// `lines`: what the crop kept once the server cropped it (the blob there
/// holds only those), else `lines`. The one rule every count of
/// "lines left out" goes through, so no head says lines exist that no
/// download brings.
pub fn whole_lines(lines: u32, full: Option<&FullView>) -> u32 {
    match full
        .and_then(FullView::blob)
        .and_then(|b| b.change.as_ref())
    {
        Some(BlobChange::Cropped { kept, .. }) => *kept,
        None => lines,
    }
}

/// The tombstone on content deleted on the server that this device has no
/// copy of: `Deleted by laptop`.
pub fn deleted_note(by: &DeviceRef) -> String {
    format!("Deleted by {}", device_word(by))
}

/// What a head says of a blob the server changed, beside its download
/// state: `Cropped by laptop: kept 12 of 400 lines`. `None` for a blob as
/// it was made.
pub fn change_note(blob: &BlobView) -> Option<String> {
    match blob.change.as_ref()? {
        BlobChange::Cropped { kept, of, by } => Some(format!(
            "Cropped by {}: kept {kept} of {of} {}",
            device_word(by),
            if *of == 1 { "line" } else { "lines" }
        )),
    }
}

/// The first of `sections` whose whole content the server could not keep,
/// as [`Section::lost`] words it: what an approval prompt says up front,
/// hidden section or not, since the user decides on what it shows.
pub fn lost(sections: &[Section]) -> Option<&'static str> {
    sections.iter().find_map(Section::lost)
}

/// What an approval prompt says up front when the server could not keep
/// some of what the user decides on: the request's own input first (it
/// can differ from its call's, which `sections`, the gated call's, show),
/// else the first lost section.
pub fn approval_lost(prompt: &ApprovalPrompt, sections: &[Section]) -> Option<&'static str> {
    match prompt.input {
        Some(FullView::Lost) => Some(INPUT_LOST),
        _ => lost(sections),
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
        Entry::ToolResult { summary, output }
        | Entry::Imported {
            entry: ImportedEntry::ToolResult { summary, output },
            ..
        } => out.push(Section {
            full: output.as_ref(),
            ..Section::text(summary)
        }),
        // An imported call shows like a live one.
        Entry::Imported {
            entry: ImportedEntry::Tool(view),
            ..
        } => return tool_sections(view),
        _ => {}
    }
    out
}

/// A grouped call: its body (e.g. the file written or the diff), then its
/// output. Output beside a body is shown only when it is an error or the
/// body is a shell command; the rest waits for the item to expand. The
/// whole input is offered when nothing else has a blob, and a lost input
/// always stands, so the loss is said.
pub fn tool_sections(view: &ToolView) -> Vec<Section<'_>> {
    let mut out = Vec::new();
    if let Some(body) = &view.body {
        out.push(Section {
            text: &body.preview,
            lines: Some(whole_lines(body.lines, body.full.as_ref())),
            format: body.format,
            numbered: body.format == BodyFormat::Text && is_file(view.kind),
            error: false,
            full: body.full.as_ref(),
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
        // A subagent's row says how it stands (`format::subagent_status`);
        // its launch result ("Async agent launched") says nothing of that.
        && (*is_error || view.subagent.is_none())
    {
        out.push(Section {
            text: preview,
            lines: Some(whole_lines(*lines, output.as_ref())),
            format: BodyFormat::Text,
            numbered: view.kind == ToolKind::FileRead && view.body.is_none() && !is_error,
            error: *is_error,
            full: output.as_ref(),
            hidden: !is_error && view.body.is_some() && view.kind != ToolKind::Shell,
            input: false,
        });
    }
    // A lost or deleted input always stands, so the loss is said: no other
    // section's blob holds the input.
    if let Some(input) = &view.input
        && (input.blob().is_none() || out.iter().all(|s| s.blob().is_none()))
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
        Entry::Tool(view)
        | Entry::Imported {
            entry: ImportedEntry::Tool(view),
            ..
        } => {
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
        Entry::ToolResult { .. }
        | Entry::Imported {
            entry: ImportedEntry::ToolResult { .. },
            ..
        } => Vec::new(),
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
        .filter_map(|s| s.blob())
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
        match section.blob() {
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

/// A message's text, logged, pending or imported.
fn message(item: &TimelineItem) -> Option<&str> {
    match item {
        TimelineItem::Pending { text, .. }
        | TimelineItem::Logged {
            entry:
                Entry::UserMessage { text, .. }
                | Entry::AgentMessage { text }
                | Entry::Imported {
                    entry:
                        ImportedEntry::UserMessage { text } | ImportedEntry::AgentMessage { text },
                    ..
                },
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
    match entry {
        Entry::Tool(view)
        | Entry::Imported {
            entry: ImportedEntry::Tool(view),
            ..
        } => Some(tool_call_content(view)),
        Entry::ToolCall { summary, .. }
        | Entry::Approval { summary, .. }
        | Entry::ToolResult { summary, .. }
        | Entry::Imported {
            entry: ImportedEntry::ToolResult { summary, .. },
            ..
        } => Some(Content::Text(summary.clone())),
        _ => None,
    }
}

/// [`call_content`] of a grouped call, live or imported.
fn tool_call_content(view: &ToolView) -> Content {
    let unprompted = |text: &str| text.strip_prefix(PROMPT).unwrap_or(text).to_owned();
    // Decided on the kind, never the tool's name (D3).
    if view.kind != ToolKind::Shell {
        return Content::Text(match &view.detail {
            Some(detail) => format!("{}\n{detail}", view.title),
            None => view.title.clone(),
        });
    }
    match &view.body {
        Some(BodyView {
            full: Some(FullView::Blob(blob)),
            ..
        }) => Content::Blob {
            blob: blob.clone(),
            strip: Some(PROMPT),
        },
        Some(body) => Content::Text(unprompted(&body.preview)),
        None => Content::Text(unprompted(&view.title)),
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use skywave_core::{BlobId, CallId};

    use super::*;

    fn call(body: Option<FullView>, input: Option<FullView>) -> ToolView {
        ToolView {
            call: CallId::new(),
            name: "Write".into(),
            kind: ToolKind::FileWrite,
            title: "a.txt".into(),
            detail: None,
            body: Some(BodyView {
                format: BodyFormat::Text,
                preview: "x".into(),
                lines: 40,
                full: body,
            }),
            input,
            approval: None,
            outcome: ToolOutcome::Running { since: Utc::now() },
            subagent: None,
        }
    }

    #[test]
    fn content_the_server_lost_is_said_and_never_a_blob() {
        let kept = FullView::Blob(BlobView {
            id: BlobId::new(),
            state: BlobState::NotDownloaded { size: 10 },
            change: None,
            pending: None,
        });

        // A lost input stands where a kept one would, hidden, saying so.
        let view = call(None, Some(FullView::Lost));
        let shown = tool_sections(&view);
        assert_eq!(shown.len(), 2);
        assert!(shown[1].input && shown[1].hidden);
        assert_eq!(shown[1].blob(), None);
        assert_eq!(lost(&shown), Some(INPUT_LOST));

        // A lost body says so too; its preview is all there is.
        let view = call(Some(FullView::Lost), None);
        let shown = tool_sections(&view);
        assert_eq!(shown[0].lost(), Some(CONTENT_LOST));
        assert_eq!(
            main_content(&TimelineItem::Logged {
                parent: None,
                seq: Seq(1),
                at: Utc::now(),
                context: skywave_core::ContextId::first_of(skywave_core::SessionId::new()),
                entry: Entry::Tool(Box::new(view.clone())),
            }),
            Some(Content::Text("x".into()))
        );

        // A lost input beside a kept body still stands, and says so: the
        // body's blob is not the input.
        let view = call(Some(kept.clone()), Some(FullView::Lost));
        let shown = tool_sections(&view);
        assert!(shown[0].blob().is_some());
        assert_eq!(lost(&shown), Some(INPUT_LOST));

        // Control: kept content is a blob and nothing is said.
        let view = call(Some(kept.clone()), Some(kept));
        let shown = tool_sections(&view);
        assert!(shown[0].blob().is_some());
        assert_eq!(lost(&shown), None);
        assert_eq!(shown[0].note(), None);
    }

    fn device(name: &str, me: bool) -> DeviceRef {
        DeviceRef {
            id: skywave_core::DeviceId::new(),
            name: Some(name.into()),
            me,
        }
    }

    #[test]
    fn deleted_content_says_who_deleted_it_and_has_no_blob() {
        let deleted = FullView::Deleted {
            by: device("laptop", false),
        };
        let view = call(Some(deleted.clone()), None);
        let shown = tool_sections(&view);
        assert_eq!(shown[0].blob(), None, "nothing to download");
        assert_eq!(shown[0].note().as_deref(), Some("Deleted by laptop"));
        // Not the server's loss: an approval does not say it was.
        assert_eq!(shown[0].lost(), None);

        let mine = FullView::Deleted {
            by: device("phone", true),
        };
        let view = call(Some(mine), None);
        assert_eq!(
            tool_sections(&view)[0].note().as_deref(),
            Some("Deleted by you")
        );

        // A deleted input stands, saying so, even beside a kept body.
        let kept = FullView::Blob(BlobView {
            id: BlobId::new(),
            state: BlobState::NotDownloaded { size: 10 },
            change: None,
            pending: None,
        });
        let view = call(Some(kept), Some(deleted));
        let shown = tool_sections(&view);
        assert_eq!(shown.len(), 2);
        assert_eq!(shown[1].note().as_deref(), Some("Deleted by laptop"));
    }

    #[test]
    fn a_cropped_blob_says_what_it_kept_of_the_original() {
        let mut blob = BlobView {
            id: BlobId::new(),
            state: BlobState::NotDownloaded { size: 10 },
            change: None,
            pending: None,
        };
        assert_eq!(change_note(&blob), None);
        blob.change = Some(BlobChange::Cropped {
            kept: 12,
            of: 400,
            by: device("laptop", false),
        });
        assert_eq!(
            change_note(&blob).as_deref(),
            Some("Cropped by laptop: kept 12 of 400 lines")
        );
        // Still a blob to download: the preview is not all there is.
        let full = FullView::Blob(blob);
        let view = call(Some(full), None);
        let shown = tool_sections(&view);
        assert!(shown[0].blob().is_some());
        assert_eq!(shown[0].note(), None);
        // Its whole content is what the crop kept, not the original's 40.
        assert_eq!(shown[0].lines, Some(12));
        // Control: uncropped, the event's own count.
        let view = call(Some(kept_blob()), None);
        assert_eq!(tool_sections(&view)[0].lines, Some(40));
    }

    fn kept_blob() -> FullView {
        FullView::Blob(BlobView {
            id: BlobId::new(),
            state: BlobState::NotDownloaded { size: 10 },
            change: None,
            pending: None,
        })
    }
}
