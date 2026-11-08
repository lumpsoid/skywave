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

use crate::format::{blob_size, bytes, device_word, download_brief};

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
    /// The whole of a long message, whose head the message itself shows
    /// (`text` is empty): collapsed, it shows no lines, only where the
    /// whole stands; expanded, the whole beneath the head.
    pub message: bool,
    /// The input its approval asked about, which differs from the call's
    /// ([`ToolView::approval_input`]); `input` is set too. Heads head it
    /// with [`Section::label`].
    pub asked: bool,
}

/// What a head heads the input an approval asked about with, when it
/// differs from its call's ([`Section::asked`]).
pub const ASKED_LABEL: &str = "approval input: differs from the call's";

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
            message: false,
            asked: false,
        }
    }

    /// A long message's whole text.
    fn message(full: &'a FullView) -> Section<'a> {
        Section {
            full: Some(full),
            message: true,
            ..Section::text("")
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

    /// The input an approval asked about, differing from its call's: shown
    /// like a whole input, only when expanded.
    fn asked(full: &'a FullView) -> Section<'a> {
        Section {
            asked: true,
            ..Section::input(full)
        }
    }

    /// The line a head shows above this section's content, if it has one:
    /// [`ASKED_LABEL`] on the input an approval asked about.
    pub fn label(&self) -> Option<&'static str> {
        self.asked.then_some(ASKED_LABEL)
    }

    /// The blob holding the whole content, unless there is none or it was
    /// lost.
    pub fn blob(&self) -> Option<&'a BlobView> {
        self.full.and_then(FullView::blob)
    }

    /// The whole content when it came inline in the event: what an
    /// expanded section shows and a copy takes in place of `text`.
    pub fn inline(&self) -> Option<&'a str> {
        self.full.and_then(FullView::inline)
    }

    /// The whole content is somewhere this device has to fetch or cannot
    /// have: a blob, or word it was lost or deleted. Not for inline
    /// content, which is here.
    pub fn elsewhere(&self) -> bool {
        self.full.is_some() && self.inline().is_none()
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

/// While collapsed, the state of an item's blob when no section on screen
/// carries one (a hidden input or output, an empty preview), so a download
/// on it shows what it did: `input 4.2 KiB`, `output 1.0 MiB · requested`;
/// that it was deleted ([`deleted_note`]): `input · Deleted by laptop`; or
/// that the server could not keep it: `input not stored`. `None` once the
/// blob is here or a shown section carries it. Inline content is here
/// already: it is never what this names.
pub fn unseen_blob(entry: &Entry) -> Option<String> {
    let sections = sections(entry);
    let shown = |s: &Section| {
        !s.hidden && (s.message || !s.text.strip_suffix('\n').unwrap_or(s.text).is_empty())
    };
    if sections.iter().any(|s| shown(s) && s.elsewhere()) {
        return None;
    }
    let section = sections.iter().find(|s| s.elsewhere())?;
    let what = match (section.input, section.hidden) {
        (true, _) if section.asked => "approval input",
        (true, _) => "input",
        (false, true) => "output",
        (false, false) => "full",
    };
    let blob = match section.full? {
        FullView::Blob(blob) => blob,
        FullView::Deleted { by } => return Some(format!("{what} · {}", deleted_note(by))),
        FullView::Lost => return Some(format!("{what} not stored")),
        // Not `elsewhere`.
        FullView::Inline(_) => return None,
    };
    let size = bytes(blob_size(&blob.state)?);
    Some(match download_brief(&blob.state) {
        Some(brief) => format!("{what} {size} · {brief}"),
        None => format!("{what} {size}"),
    })
}

/// What an approval panel says of the input the approval asked about, when
/// `sections` (the gated call's) carry one that differs from the call's:
/// [`ASKED_LABEL`], then its size, or that it was deleted or not kept:
/// `approval input: differs from the call's · 4.0 KiB`. A head adds how
/// to see it.
pub fn asked_note(sections: &[Section]) -> Option<String> {
    let section = sections.iter().find(|s| s.asked)?;
    let tail = match section.full? {
        FullView::Blob(blob) => blob_size(&blob.state).map(bytes),
        FullView::Deleted { by } => Some(deleted_note(by)),
        FullView::Lost => Some("not stored".to_owned()),
        FullView::Inline(_) => None,
    };
    Some(match tail {
        Some(tail) => format!("{ASKED_LABEL} · {tail}"),
        None => ASKED_LABEL.to_owned(),
    })
}

/// While collapsed, where the download of the blob behind a shown section
/// stands: `full 4.0 KiB · not downloaded`, `full 4.0 KiB · requested`,
/// `downloading 25%` (the progress is what counts then),
/// `full 4.0 KiB · download failed`, `downloaded`. A head adds its own
/// hint (how to retry).
pub fn collapsed_download(state: &BlobState) -> String {
    match (state, download_brief(state)) {
        (BlobState::Available { .. }, _) => "downloaded".to_owned(),
        (BlobState::Downloading { .. }, Some(progress)) => progress,
        (_, brief) => format!(
            "full {} · {}",
            bytes(blob_size(state).unwrap_or_default()),
            brief.as_deref().unwrap_or("not downloaded")
        ),
    }
}

/// The first of `sections` whose whole content the server could not keep,
/// as [`Section::lost`] words it: what an approval prompt says up front,
/// hidden section or not, since the user decides on what it shows.
pub fn lost(sections: &[Section]) -> Option<&'static str> {
    sections.iter().find_map(Section::lost)
}

/// What an approval prompt says up front when the server could not keep
/// some of what the user decides on: the request's own input first (when
/// it differs from its call's, the gated call's `sections` carry it too),
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
        Entry::AgentMessage {
            full: Some(full), ..
        }
        | Entry::Imported {
            entry:
                ImportedEntry::UserMessage {
                    full: Some(full), ..
                }
                | ImportedEntry::AgentMessage {
                    full: Some(full), ..
                },
            ..
        } => out.push(Section::message(full)),
        _ => {}
    }
    out
}

/// A grouped call: its body (e.g. the file written or the diff), then its
/// output. Output beside a body is shown only when it is an error or the
/// body is a shell command; the rest waits for the item to expand. The
/// whole input is offered when nothing else has a blob or an inline
/// output, and a lost input always stands, so the loss is said. Last, the
/// input its approval asked about when that differs from the call's
/// ([`ToolView::approval_input`]).
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
            message: false,
            asked: false,
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
            message: false,
            asked: false,
        });
    }
    // A lost or deleted input always stands, so the loss is said: no other
    // section's blob holds the input. An inline output counts as the blob
    // it would be were it larger: the input is not offered beside it.
    if let Some(input) = &view.input
        && (input.blob().is_none()
            || out
                .iter()
                .all(|s| s.blob().is_none() && s.inline().is_none()))
    {
        out.push(Section::input(input));
    }
    // What the approval asked, when no section of the call holds it.
    out.extend(view.approval_input().map(Section::asked));
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
        // A long message's head shows whole while collapsed; the rest is
        // only in its blob.
        Entry::AgentMessage {
            text,
            full: Some(_),
        }
        | Entry::Imported {
            entry:
                ImportedEntry::UserMessage {
                    text,
                    full: Some(_),
                }
                | ImportedEntry::AgentMessage {
                    text,
                    full: Some(_),
                },
            ..
        } => text.lines().map(str::to_owned).collect(),
        _ => return false,
    };
    for section in sections(entry).iter().filter(|s| !s.hidden) {
        head.extend(section.text.lines().take(PREVIEW_LINES).map(str::to_owned));
    }
    !head.iter().any(|text| query.matches(text))
}

/// Every blob behind `entry` that nobody asked for yet, in section order:
/// what expanding it fetches. A failed one is not among them, it waits for
/// an explicit retry ([`wanted_blob`]); nor does it hold back the others.
pub fn unasked_blobs(entry: &Entry) -> Vec<&BlobView> {
    sections(entry)
        .into_iter()
        .filter_map(|s| s.blob())
        .filter(|blob| matches!(blob.state, BlobState::NotDownloaded { .. }))
        .collect()
}

/// What one download request on `entry` fetches: the first blob nobody
/// asked for yet, else the first whose download failed (a retry). Blobs
/// nobody asked for go first, so a blob that keeps failing never keeps
/// another from being fetched.
pub fn wanted_blob(entry: &Entry) -> Option<&BlobView> {
    unasked_blobs(entry).into_iter().next().or_else(|| {
        sections(entry)
            .into_iter()
            .filter_map(|s| s.blob())
            .find(|blob| matches!(blob.state, BlobState::Failed { .. }))
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
            None => Content::Text(section.inline().unwrap_or(section.text).to_owned()),
        }
    }
}

/// What a shell call's title, and so its body, starts with (see
/// `ToolDisplay::title`); a copied command goes without it.
const PROMPT: &str = "$ ";

/// A message's text, logged, pending or imported: the blob holding a long
/// one whole, else its text (all there is once the blob was lost or
/// deleted).
fn message(item: &TimelineItem) -> Option<Content> {
    let (text, full) = match item {
        TimelineItem::Pending { text, .. }
        | TimelineItem::Logged {
            entry: Entry::UserMessage { text, .. },
            ..
        } => (text, None),
        TimelineItem::Logged {
            entry:
                Entry::AgentMessage { text, full }
                | Entry::Imported {
                    entry:
                        ImportedEntry::UserMessage { text, full }
                        | ImportedEntry::AgentMessage { text, full },
                    ..
                },
            ..
        } => (text, full.as_ref()),
        _ => return None,
    };
    Some(match full.and_then(FullView::blob) {
        Some(blob) => Content::Blob {
            blob: blob.clone(),
            strip: None,
        },
        None => Content::Text(text.clone()),
    })
}

/// The main copy target: a message's text; else the last section shown
/// while collapsed: a call's output, the diff or file an edit or write
/// shows in its stead, its body while no result is here; else the call, as
/// [`call_content`].
pub fn main_content(item: &TimelineItem) -> Option<Content> {
    if let Some(content) = message(item) {
        return Some(content);
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
    if let Some(content) = message(item) {
        return Some(content);
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
    use skywave_core::{BlobId, CallId, Pause};

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

    #[test]
    fn an_approvals_own_input_is_offered_only_when_it_differs() {
        let blob = |size| {
            FullView::Blob(BlobView {
                id: BlobId::new(),
                state: BlobState::NotDownloaded { size },
                change: None,
                pending: None,
            })
        };
        let gated = |view: ToolView, asked: Option<FullView>| ToolView {
            approval: Some(skywave_core::ToolApproval {
                id: skywave_core::ApprovalId::new(),
                status: skywave_core::ApprovalStatus::Pending,
                answer: None,
                input: asked,
            }),
            ..view
        };
        let asked = |sections: &[Section]| sections.iter().filter(|s| s.asked).count();

        // The call has no input blob (its body holds it); the approval
        // asked something else: offered last, hidden, labelled, fetched on
        // expanding, and named while collapsed.
        let own = blob(4096);
        let view = gated(call(None, None), Some(own.clone()));
        let shown = tool_sections(&view);
        let last = shown.last().unwrap();
        assert!(last.asked && last.input && last.hidden);
        assert_eq!(last.full, Some(&own));
        assert_eq!(last.label(), Some(ASKED_LABEL));
        assert_eq!(shown[0].label(), None);
        assert_eq!(
            asked_note(&shown).as_deref(),
            Some("approval input: differs from the call's · 4.0 KiB")
        );
        let entry = Entry::Tool(Box::new(view));
        assert_eq!(unasked_blobs(&entry), vec![own.blob().unwrap()]);
        assert_eq!(
            unseen_blob(&entry).as_deref(),
            Some("approval input 4.0 KiB")
        );

        // Beside the call's own input blob, a different one still stands.
        let input = blob(10);
        let view = gated(call(None, Some(input.clone())), Some(own));
        assert_eq!(asked(&tool_sections(&view)), 1);

        // The same blob as the call's input, or none asked: nothing extra.
        let view = gated(call(None, Some(input.clone())), Some(input));
        assert_eq!(asked(&tool_sections(&view)), 0);
        let view = gated(call(None, None), None);
        assert_eq!(asked(&tool_sections(&view)), 0);
        assert_eq!(asked_note(&tool_sections(&view)), None);

        // One the server could not keep says so, as an input.
        let view = gated(call(None, None), Some(FullView::Lost));
        assert_eq!(lost(&tool_sections(&view)), Some(INPUT_LOST));
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
    fn a_collapsed_unseen_input_says_deleted_apart_from_lost() {
        // A call with no body: its input is the only section, hidden.
        let entry = |input: FullView| {
            let mut view = call(None, Some(input));
            view.body = None;
            Entry::Tool(Box::new(view))
        };
        let deleted = FullView::Deleted {
            by: device("laptop", false),
        };
        assert_eq!(
            unseen_blob(&entry(deleted)).as_deref(),
            Some("input · Deleted by laptop")
        );
        assert_eq!(
            unseen_blob(&entry(FullView::Lost)).as_deref(),
            Some("input not stored")
        );
        // Control: a kept input says its download state.
        assert_eq!(
            unseen_blob(&entry(kept_blob())).as_deref(),
            Some("input 10 B")
        );
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

    #[test]
    fn an_inline_output_is_content_at_hand_not_a_blob() {
        let read = |output: FullView| {
            let mut view = call(None, None);
            view.name = "Read".into();
            view.kind = ToolKind::FileRead;
            view.body = None;
            view.outcome = ToolOutcome::Done {
                is_error: false,
                status: Some("2 lines".into()),
                preview: "alpha\n".into(),
                lines: 2,
                output: Some(output),
                took: std::time::Duration::ZERO,
            };
            Entry::Tool(Box::new(view))
        };
        let item = |entry: Entry| TimelineItem::Logged {
            parent: None,
            seq: Seq(1),
            at: Utc::now(),
            context: skywave_core::ContextId::first_of(skywave_core::SessionId::new()),
            entry,
        };

        let entry = read(FullView::Inline("1\talpha\n2\t".into()));
        let shown = sections(&entry);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].text, "alpha\n");
        assert_eq!(shown[0].inline(), Some("1\talpha\n2\t"));
        assert_eq!((shown[0].blob(), shown[0].note()), (None, None));
        assert!(!shown[0].elsewhere(), "nothing to fetch or say");
        assert_eq!(wanted_blob(&entry), None);
        assert_eq!(unseen_blob(&entry), None, "no `not stored` label");
        // A copy takes the whole output, not the preview.
        assert_eq!(
            main_content(&item(entry.clone())),
            Some(Content::Text("1\talpha\n2\t".into()))
        );
        // Text only in the whole output shows once expanded.
        assert!(match_hidden(&entry, &Query::new("1\talpha")));
        assert!(!match_hidden(&entry, &Query::new("alpha")));

        // Control: a blob output is fetched, and a copy takes the blob.
        let entry = read(kept_blob());
        let shown = sections(&entry);
        assert!(shown[0].elsewhere());
        assert_eq!(shown[0].inline(), None);
        assert!(matches!(
            main_content(&item(entry.clone())),
            Some(Content::Blob { .. })
        ));
    }

    #[test]
    fn a_long_message_is_its_head_with_its_whole_as_a_blob() {
        let item = |entry: Entry| TimelineItem::Logged {
            parent: None,
            seq: Seq(1),
            at: Utc::now(),
            context: skywave_core::ContextId::first_of(skywave_core::SessionId::new()),
            entry,
        };
        let agent = |full| Entry::AgentMessage {
            text: "# Report\nfirst part\n".into(),
            full,
        };
        let imported = |full| Entry::Imported {
            at: Utc::now(),
            entry: ImportedEntry::UserMessage {
                text: "# Report\nfirst part\n".into(),
                full,
            },
        };
        for entry in [agent(Some(kept_blob())), imported(Some(kept_blob()))] {
            let shown = sections(&entry);
            assert_eq!(shown.len(), 1, "{entry:?}");
            assert!(shown[0].message && !shown[0].hidden);
            assert_eq!(shown[0].text, "", "the message shows its head");
            assert!(shown[0].blob().is_some());
            assert!(wanted_blob(&entry).is_some(), "a download fetches it");
            // Its section says where the blob stands: no aside.
            assert_eq!(unseen_blob(&entry), None);
            // A copy takes the whole text.
            assert!(matches!(
                main_content(&item(entry.clone())),
                Some(Content::Blob { strip: None, .. })
            ));
            assert!(matches!(
                call_content(&item(entry.clone())),
                Some(Content::Blob { .. })
            ));
            // A match past the head shows once expanded.
            assert!(match_hidden(&entry, &Query::new("second part")));
            assert!(!match_hidden(&entry, &Query::new("first part")));
        }

        // Deleted or lost: the head is all a copy takes, and it says why.
        let deleted = agent(Some(FullView::Deleted {
            by: device("laptop", false),
        }));
        assert_eq!(
            sections(&deleted)[0].note().as_deref(),
            Some("Deleted by laptop")
        );
        assert_eq!(
            main_content(&item(deleted)),
            Some(Content::Text("# Report\nfirst part\n".into()))
        );
        assert_eq!(
            sections(&agent(Some(FullView::Lost)))[0].lost(),
            Some(CONTENT_LOST)
        );

        // Control: a whole message has no section, and a copy takes it.
        let whole = agent(None);
        assert!(sections(&whole).is_empty());
        assert!(!match_hidden(&whole, &Query::new("second part")));
        assert_eq!(
            main_content(&item(whole)),
            Some(Content::Text("# Report\nfirst part\n".into()))
        );
    }

    fn kept_blob() -> FullView {
        FullView::Blob(BlobView {
            id: BlobId::new(),
            state: BlobState::NotDownloaded { size: 10 },
            change: None,
            pending: None,
        })
    }

    /// One blob in each download state, with the progress `got` of 4 KiB.
    fn states() -> [BlobState; 6] {
        let size = 4096;
        [
            BlobState::NotDownloaded { size },
            BlobState::Requested { size },
            BlobState::Downloading {
                got: 1024,
                size,
                paused: None,
            },
            BlobState::Downloading {
                got: 1024,
                size,
                paused: Some(Pause::Offline),
            },
            BlobState::Failed {
                size,
                reason: "gone".into(),
            },
            BlobState::Available {
                path: "/blobs/a".into(),
            },
        ]
    }

    #[test]
    fn a_collapsed_unseen_blob_names_its_size_and_download() {
        let entry = |state: BlobState| {
            let mut view = call(
                None,
                Some(FullView::Blob(BlobView {
                    id: BlobId::new(),
                    state,
                    change: None,
                    pending: None,
                })),
            );
            view.body = None;
            Entry::Tool(Box::new(view))
        };
        let said: Vec<_> = states()
            .into_iter()
            .map(|state| unseen_blob(&entry(state)))
            .collect();
        assert_eq!(
            said,
            [
                Some("input 4.0 KiB".to_owned()),
                Some("input 4.0 KiB · requested".to_owned()),
                Some("input 4.0 KiB · downloading 25%".to_owned()),
                Some("input 4.0 KiB · downloading 25% · paused, offline".to_owned()),
                Some("input 4.0 KiB · download failed".to_owned()),
                // Here: the expanded item shows it.
                None,
            ]
        );
    }

    #[test]
    fn a_collapsed_section_says_where_its_download_stands() {
        let said: Vec<_> = states().iter().map(collapsed_download).collect();
        assert_eq!(
            said,
            [
                "full 4.0 KiB · not downloaded",
                "full 4.0 KiB · requested",
                // The progress is what counts while it downloads.
                "downloading 25%",
                "downloading 25% · paused, offline",
                "full 4.0 KiB · download failed",
                "downloaded",
            ]
        );
        let full = BlobState::Downloading {
            got: 1024,
            size: 4096,
            paused: Some(Pause::DiskFull),
        };
        assert_eq!(
            collapsed_download(&full),
            "downloading 25% · paused, device disk full"
        );
    }

    #[test]
    fn an_expanded_section_says_where_its_download_stands_in_full() {
        let mut states = states().to_vec();
        // A reason is shown as its first line, control characters dropped.
        states.push(BlobState::Failed {
            size: 10,
            reason: "bad\u{7} hash\nmore".into(),
        });
        let said: Vec<_> = states.iter().map(crate::format::download_state).collect();
        assert_eq!(
            said,
            [
                "not downloaded · 4.0 KiB",
                "requested · waiting for a slot · 4.0 KiB",
                "downloading · 1.0 KiB of 4.0 KiB (25%)",
                "downloading · 1.0 KiB of 4.0 KiB (25%) · paused, offline",
                "download failed: gone · 4.0 KiB",
                "downloaded",
                "download failed: bad hash · 10 B",
            ]
        );
    }
}
