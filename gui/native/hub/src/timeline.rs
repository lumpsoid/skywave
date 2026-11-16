//! The open session's timeline as [`TimelineRow`]s, and the keyed diff
//! between the rows last sent and the rows now. Every label is worded
//! here, from present's sections and formatters, as the TUI words it
//! without its key hints; the diff is transport only.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use skywave_core::attachment::{AttachedImage, attachment_path};
use skywave_core::store::OutboxFile;
use skywave_core::{
    Answer, ApprovalStatus, AttachmentRefusal, BlobId, BlobState, BlobView, BodyFormat, Decision,
    DeviceRef, Entry, FailReason, FullView, ImportedEntry, MessageImage, Outgoing, QUESTION_TOOL,
    RuleSuggestion, Seq, SessionId, SessionView, TimelineItem, ToolKind, ToolOutcome, ToolView,
    read_head,
};
use skywave_present::blob_actions::entry_target;
use skywave_present::format::{
    IMPORTED_MARK, adopted_line, agent_setting_line, agent_version_line, answer_words, asked_by,
    bytes, context_moved_line, context_started_line, context_switched_line, decision_label,
    delivery_label, dollars, download_state, fail_reason, import_finished_line, moved_from_line,
    outgoing_label, safety_approval_label, seconds, subagent_status, took_label, turn_time,
};
use skywave_present::markdown as md;
use skywave_present::sections::{
    ItemKey, PREVIEW_LINES, Section, change_note, delivery, sections, selectable, starts_turn,
    unseen_blob,
};
use skywave_present::text::{clean_line, clean_text};

use crate::crop::target_view;
use crate::signals::{
    BlobFacts, BlobStateKind, LineStyle, MarkdownAlign, MarkdownBlock, MarkdownCell, MarkdownKind,
    MarkdownRow, MarkdownSpan, OutgoingState, OutgoingView, RowFile, RowImage, RowKind,
    SectionFormat, SectionLine, SectionView, TimelineRow, TimelineUpdate, ToolFacts, ToolKindView,
    ToolState,
};

/// Most of a blob an expanded section shows.
pub const SHOW_BYTES: u64 = 64 * 1024;
/// Most lines of a blob an expanded section shows.
pub const SHOW_LINES: usize = 400;

/// `key` as the contract carries it: `s:<seq>` or `p:<command id>`.
pub fn key_string(key: ItemKey) -> String {
    match key {
        ItemKey::Logged(seq) => format!("s:{}", seq.0),
        ItemKey::Pending(id) => format!("p:{}", id.0),
    }
}

/// A key Dart sent back, or why it is not one.
pub fn parse_key(text: &str) -> Result<ItemKey, String> {
    let bad = || format!("not a timeline key: {text:?}");
    match text.split_once(':') {
        Some(("s", seq)) => seq
            .parse()
            .map(|seq| ItemKey::Logged(Seq(seq)))
            .map_err(|_| bad()),
        Some(("p", id)) => uuid::Uuid::parse_str(id)
            .map(|id| ItemKey::Pending(id.into()))
            .map_err(|_| bad()),
        _ => Err(bad()),
    }
}

/// What expanded sections showed of downloaded blobs, by blob, so a file
/// is read once and not on every snapshot. A downloaded blob's file does
/// not change.
#[derive(Debug, Default)]
pub struct BlobHeads {
    read: HashMap<BlobId, Result<Head, String>>,
}

/// The head of a blob's file, as an expanded section shows it.
#[derive(Debug, Clone)]
struct Head {
    lines: Vec<String>,
    /// More lines, or more bytes, than shown.
    cut: Option<String>,
}

impl BlobHeads {
    fn get(&mut self, id: BlobId, path: &Path) -> Result<Head, String> {
        self.read.entry(id).or_insert_with(|| read(path)).clone()
    }
}

fn read(path: &Path) -> Result<Head, String> {
    let (text, size) = read_head(path, SHOW_BYTES)
        .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
    Ok(shown_head(&text, size))
}

/// What an expanded section shows of whole content kept inline: the same
/// head and cut as a downloaded blob's.
fn inline_head(whole: &str) -> Head {
    let mut end = whole.len().min(SHOW_BYTES as usize);
    while !whole.is_char_boundary(end) {
        end -= 1;
    }
    shown_head(&whole[..end], whole.len() as u64)
}

/// The head shown of `text`, the first bytes of content `size` long.
fn shown_head(text: &str, size: u64) -> Head {
    let all = content_lines(text);
    let cut = (all.len() > SHOW_LINES || size > SHOW_BYTES).then(|| {
        let shown = if all.len() > SHOW_LINES {
            format!("{SHOW_LINES} lines")
        } else {
            bytes(SHOW_BYTES)
        };
        format!("cut short: {shown} of {} shown", bytes(size))
    });
    Head {
        lines: all.into_iter().take(SHOW_LINES).map(clean_line).collect(),
        cut,
    }
}

/// A session's rows, and for each the pending key its logged message
/// replaced, if it is one: what a diff's `replaces` names.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Rows {
    pub rows: Vec<TimelineRow>,
    pub was: Vec<Option<String>>,
}

/// Every row of `view`'s timeline at `wall_ms`, the items whose seq is in
/// `expanded` expanded. `data_dir` is where a queued message's images are.
pub fn rows(
    view: &SessionView,
    expanded: &HashSet<Seq>,
    heads: &mut BlobHeads,
    data_dir: &Path,
    wall_ms: i64,
) -> Rows {
    let rule = rule_index(view);
    let mut minute: Option<String> = None;
    let mut out = Rows::default();
    for (index, item) in view.timeline.iter().enumerate() {
        let time_label = match item {
            // Imported history shows when the agent recorded it.
            TimelineItem::Logged {
                entry: Entry::Imported { at, .. },
                ..
            }
            | TimelineItem::Logged { at, .. } => {
                let this = at.format("%Y-%m-%d %H:%M").to_string();
                (minute.as_ref() != Some(&this)).then(|| {
                    minute = Some(this);
                    at.format("%H:%M").to_string()
                })
            }
            TimelineItem::Pending { .. } => None,
        };
        let key = ItemKey::of(item);
        let open = matches!(key, ItemKey::Logged(seq) if expanded.contains(&seq));
        let mut row = row(item, view.id, open, heads, data_dir, wall_ms);
        row.time_label = time_label;
        row.unread_rule = rule == Some(index);
        row.turn_start = !row.unread_rule && index > 0 && starts_turn(item);
        out.was.push(match item {
            TimelineItem::Logged {
                entry: Entry::UserMessage { command_id, .. },
                ..
            } => Some(key_string(ItemKey::Pending(*command_id))),
            _ => None,
        });
        out.rows.push(row);
    }
    out
}

/// The timeline index the "new" rule goes above: the first logged item at
/// or after [`SessionView::unread_from`].
fn rule_index(view: &SessionView) -> Option<usize> {
    let from = view.unread_from?;
    view.timeline
        .iter()
        .position(|item| matches!(item, TimelineItem::Logged { seq, .. } if *seq >= from))
}

/// One item of `session`, `expanded` or not, without the facts that
/// depend on its neighbours (time label, separators).
fn row(
    item: &TimelineItem,
    session: SessionId,
    expanded: bool,
    heads: &mut BlobHeads,
    data_dir: &Path,
    wall_ms: i64,
) -> TimelineRow {
    let key = ItemKey::of(item);
    let mut row = TimelineRow {
        key: key_string(key),
        kind: RowKind::PendingMessage,
        author: None,
        own: false,
        text: String::new(),
        markdown: Vec::new(),
        mark: None,
        time_label: None,
        at_ms: None,
        turn_start: false,
        unread_rule: false,
        selectable: selectable(item).is_some(),
        expandable: false,
        expanded: false,
        outgoing: None,
        delivery_label: delivery(item).map(|(o, d)| delivery_label(&d, &o)),
        discardable: false,
        command_id: None,
        tool: None,
        sections: Vec::new(),
        blob_target: None,
        images: Vec::new(),
        files: Vec::new(),
        can_resend_replacing: false,
    };
    let (at, entry) = match item {
        TimelineItem::Pending {
            command_id,
            text,
            images,
            files,
            outgoing,
            discardable,
            ..
        } => {
            row.command_id = Some(command_id.0.to_string());
            row.author = Some("you".to_owned());
            row.own = true;
            row.text = clean_text(text);
            row.outgoing = Some(outgoing_view(outgoing));
            row.discardable = *discardable;
            row.images = images
                .iter()
                .map(|image| pending_image(image, data_dir))
                .collect();
            row.files = files.iter().map(pending_file).collect();
            row.can_resend_replacing = matches!(
                outgoing,
                Outgoing::Failed(FailReason::AttachmentRefused(AttachmentRefusal::FileExists))
            ) && files.iter().any(|file| file.existing.is_some());
            return row;
        }
        TimelineItem::Logged {
            entry: entry @ Entry::Imported { at, .. },
            ..
        }
        | TimelineItem::Logged { at, entry, .. } => (at, entry),
    };
    row.at_ms = Some(at.timestamp_millis());
    match entry {
        Entry::UserMessage {
            text,
            images,
            from,
            outgoing,
            ..
        } => {
            set(&mut row, RowKind::UserMessage, clean_text(text));
            row.own = from.me;
            row.author = Some(if from.me {
                "you".to_owned()
            } else {
                clean_line(&from.label())
            });
            row.outgoing = outgoing.as_ref().map(outgoing_view);
            row.images = images.iter().map(logged_image).collect();
        }
        Entry::AgentMessage { text, .. } => {
            set(&mut row, RowKind::AgentMessage, clean_text(text));
            row.markdown = markdown(text);
            row.author = Some("agent".to_owned());
        }
        Entry::AgentErrored { message } => {
            set(&mut row, RowKind::AgentError, clean_text(message));
            row.author = Some("agent error".to_owned());
        }
        Entry::AgentVersionUnconfirmed { version, confirmed } => set(
            &mut row,
            RowKind::AgentVersion,
            clean_line(&agent_version_line(version, confirmed)),
        ),
        Entry::Interrupted => set(
            &mut row,
            RowKind::Interrupted,
            "interrupted · the agent stopped its turn".to_owned(),
        ),
        Entry::Stopped => set(
            &mut row,
            RowKind::Stopped,
            "stopped · the agent was shut down, a message restarts it".to_owned(),
        ),
        Entry::ContextStarted { name, by } => set(
            &mut row,
            RowKind::ContextDivider,
            clean_line(&context_started_line(name, by)),
        ),
        Entry::ContextSwitched { name, by } => set(
            &mut row,
            RowKind::ContextDivider,
            clean_line(&context_switched_line(name, by)),
        ),
        Entry::ContextMoved { name, to, by } => set(
            &mut row,
            RowKind::ContextDivider,
            clean_line(&context_moved_line(name, to, by)),
        ),
        Entry::MovedFrom { from, name } => set(
            &mut row,
            RowKind::ContextDivider,
            clean_line(&moved_from_line(name, from)),
        ),
        Entry::Adopted { conversation } => set(
            &mut row,
            RowKind::ContextDivider,
            clean_line(&adopted_line(conversation)),
        ),
        Entry::Imported { entry: item, .. } => {
            row.mark = Some(IMPORTED_MARK.to_owned());
            match item {
                ImportedEntry::UserMessage { text, .. } => {
                    set(&mut row, RowKind::ImportedUserMessage, clean_text(text));
                    row.author = Some("you".to_owned());
                }
                ImportedEntry::AgentMessage { text, .. } => {
                    set(&mut row, RowKind::ImportedAgentMessage, clean_text(text));
                    row.markdown = markdown(text);
                    row.author = Some("agent".to_owned());
                }
                ImportedEntry::Tool(view) => {
                    row.kind = RowKind::ImportedTool;
                    row.tool = Some(tool_facts(view, wall_ms, expanded, entry));
                }
                ImportedEntry::ToolResult { .. } => {
                    row.kind = RowKind::ImportedTool;
                    row.tool = Some(plain_facts("result", "", expanded, entry));
                }
            }
        }
        Entry::ImportFinished { outcome } => set(
            &mut row,
            RowKind::ContextDivider,
            clean_line(&import_finished_line(outcome)),
        ),
        Entry::Renamed { name, by } => set(
            &mut row,
            RowKind::Renamed,
            match name {
                Some(name) => format!("renamed to “{}” by {}", clean_line(name), who(by)),
                None => format!("default title restored by {}", who(by)),
            },
        ),
        Entry::Archived { archived, by } => set(
            &mut row,
            RowKind::Archived,
            if *archived {
                format!("archived by {}", who(by))
            } else {
                format!("brought back from the archive by {}", who(by))
            },
        ),
        Entry::WorktreeRemoved { by } => set(
            &mut row,
            RowKind::Archived,
            format!("worktree removed by {}", who(by)),
        ),
        Entry::FileWritten {
            path, replaced, by, ..
        } => set(
            &mut row,
            RowKind::Archived,
            format!(
                "{} {} by {}",
                clean_line(path),
                if *replaced { "replaced" } else { "written" },
                who(by)
            ),
        ),
        Entry::AgentSetting {
            setting,
            by,
            refused,
        } => {
            let mut text = agent_setting_line(setting, refused.as_deref());
            if let Some(by) = asked_by(by.as_ref()) {
                text = format!("{text} {by}");
            }
            let kind = match refused {
                Some(_) => RowKind::SettingRefused,
                None => RowKind::AgentSetting,
            };
            set(&mut row, kind, clean_line(&text));
        }
        Entry::TurnUsage(usage) => {
            let mut text = "turn".to_owned();
            if let Some(ms) = usage.duration_ms {
                text.push_str(&format!(" · {}", turn_time(ms)));
            }
            if let Some(cost) = usage.cost_usd {
                text.push_str(&format!(" · {}", dollars(cost)));
            }
            set(&mut row, RowKind::TurnUsage, text);
        }
        Entry::Tool(view) => {
            row.kind = RowKind::Tool;
            row.tool = Some(tool_facts(view, wall_ms, expanded, entry));
        }
        Entry::ToolCall { name, summary, .. } => {
            row.kind = RowKind::ToolCall;
            row.tool = Some(plain_facts(name, summary, expanded, entry));
        }
        Entry::ToolResult { .. } => {
            row.kind = RowKind::ToolResult;
            row.tool = Some(plain_facts("result", "", expanded, entry));
        }
        Entry::Approval {
            tool,
            summary,
            status,
            answer,
            ..
        } => {
            row.kind = RowKind::Approval;
            let (state, mut label) = approval_state(status);
            if tool == QUESTION_TOOL && *status == ApprovalStatus::Pending {
                label = "needs an answer".to_owned();
            }
            if let Some(note) = unapplied(status, answer.as_ref()) {
                label = format!("{label} · {note}");
            }
            row.tool = Some(ToolFacts {
                state,
                state_label: label,
                ..plain_facts(tool, summary, expanded, entry)
            });
        }
    }
    let all = sections(entry);
    row.expandable = !all.is_empty();
    row.expanded = expanded && row.expandable;
    row.sections = all
        .iter()
        .filter(|s| row.expanded || !s.hidden)
        .filter_map(|s| section_view(s, row.expanded, heads))
        .collect();
    row.blob_target = entry_target(entry, session).as_ref().map(target_view);
    row
}

fn set(row: &mut TimelineRow, kind: RowKind, text: String) {
    row.kind = kind;
    row.text = text;
}

/// A queued message's image: its own file, on this device.
fn pending_image(image: &AttachedImage, data_dir: &Path) -> RowImage {
    RowImage {
        blob_id: None,
        path: Some(
            attachment_path(data_dir, image.sha256)
                .display()
                .to_string(),
        ),
        label: bytes(image.size),
        can_download: false,
    }
}

/// A queued message's non-image file: its name and size, and whether the
/// sender found another file at that name.
fn pending_file(file: &OutboxFile) -> RowFile {
    RowFile {
        name: file.file.name.clone(),
        size_label: bytes(file.file.size),
        found: file.existing.is_some(),
    }
}

/// A logged message's image as its content stands. An image is a blob or
/// deleted; inline or lost content is not expected for one, and reads as
/// unavailable.
fn logged_image(image: &MessageImage) -> RowImage {
    match &image.content {
        FullView::Blob(blob) => {
            let facts = blob_facts(blob);
            RowImage {
                blob_id: Some(facts.blob_id),
                path: match &blob.state {
                    BlobState::Available { path } => Some(path.display().to_string()),
                    _ => None,
                },
                label: facts.label,
                can_download: facts.can_download,
            }
        }
        FullView::Deleted { .. } => RowImage {
            blob_id: None,
            path: None,
            label: "image deleted".to_owned(),
            can_download: false,
        },
        FullView::Inline(_) | FullView::Lost => RowImage {
            blob_id: None,
            path: None,
            label: "image unavailable".to_owned(),
            can_download: false,
        },
    }
}

/// Whether the rows show a clock that runs: a tool call's elapsed time.
pub fn live(view: &SessionView) -> bool {
    view.timeline.iter().any(|item| {
        matches!(
            item,
            TimelineItem::Logged { entry: Entry::Tool(tool), .. }
                if matches!(tool.outcome, ToolOutcome::Running { .. })
        )
    })
}

/// The rows Dart holds after applying `updates` to `old`, as the diff
/// promises: removals, then upserts by ascending index.
///
/// `None` when the change is not a keyed diff (rows that stayed changed
/// order): send a snapshot.
pub fn diff(old: &[TimelineRow], new: &Rows) -> Option<Vec<TimelineUpdate>> {
    let new_keys: HashSet<&str> = new.rows.iter().map(|r| r.key.as_str()).collect();
    let old_at: HashMap<&str, usize> = old
        .iter()
        .enumerate()
        .map(|(i, r)| (r.key.as_str(), i))
        .collect();
    let stayed_old = old
        .iter()
        .map(|r| r.key.as_str())
        .filter(|k| new_keys.contains(k));
    let stayed_new = new
        .rows
        .iter()
        .map(|r| r.key.as_str())
        .filter(|k| old_at.contains_key(k));
    if !stayed_old.eq(stayed_new) {
        return None;
    }
    let mut out: Vec<TimelineUpdate> = old
        .iter()
        .filter(|r| !new_keys.contains(r.key.as_str()))
        .map(|r| TimelineUpdate::Removed { key: r.key.clone() })
        .collect();
    for (index, (row, was)) in new.rows.iter().zip(&new.was).enumerate() {
        if old_at
            .get(row.key.as_str())
            .is_some_and(|&i| old[i] == *row)
        {
            continue;
        }
        let replaces = was
            .as_ref()
            .filter(|p| old_at.contains_key(p.as_str()) && !new_keys.contains(p.as_str()))
            .cloned();
        out.push(TimelineUpdate::Upserted {
            key: row.key.clone(),
            item: row.clone(),
            index: crate::link::count(index),
            replaces,
        });
    }
    Some(out)
}

/// A grouped call's head line.
fn tool_facts(view: &ToolView, wall_ms: i64, expanded: bool, entry: &Entry) -> ToolFacts {
    let (state, mut label) = tool_state(view, wall_ms);
    if let Some(note) = view
        .approval
        .as_ref()
        .and_then(|a| unapplied(&a.status, a.answer.as_ref()))
    {
        label = format!("{label} · {note}");
    }
    let (state, status) = match subagent_state(view) {
        Some((state, status)) => (state, Some(status)),
        None => (state, None),
    };
    ToolFacts {
        kind: tool_kind(view.kind),
        name: clean_line(&view.name),
        title: clean_line(&view.title),
        detail: view.detail.as_deref().map(clean_line),
        state,
        state_label: label,
        aside: (!expanded).then(|| unseen_blob(entry)).flatten(),
        opens: view.subagent.as_ref().map(|_| view.call.0.to_string()),
        status,
    }
}

/// How the subagent a call starts stands, where that says more than the
/// call: its own state, not the launch's, as a background subagent's call
/// is done long before it ends. Not while its approval is pending or the
/// call failed: those the call says.
fn subagent_state(view: &ToolView) -> Option<(ToolState, String)> {
    let status = view.subagent.as_ref()?;
    let pending = matches!(
        view.approval.as_ref().map(|a| &a.status),
        Some(ApprovalStatus::Pending)
    );
    let shows = matches!(
        view.outcome,
        ToolOutcome::Running { .. }
            | ToolOutcome::Done {
                is_error: false,
                ..
            }
    );
    (shows && !pending).then(|| {
        let state = if status.live {
            ToolState::Running
        } else {
            ToolState::Done
        };
        (state, clean_line(&subagent_status(status)))
    })
}

/// A legacy call or result: its name and the first line of its summary.
fn plain_facts(name: &str, summary: &str, expanded: bool, entry: &Entry) -> ToolFacts {
    ToolFacts {
        kind: ToolKindView::Other,
        name: clean_line(name),
        title: clean_line(summary),
        detail: None,
        state: ToolState::Plain,
        state_label: String::new(),
        aside: (!expanded).then(|| unseen_blob(entry)).flatten(),
        opens: None,
        status: None,
    }
}

pub fn tool_kind(kind: ToolKind) -> ToolKindView {
    match kind {
        ToolKind::FileWrite => ToolKindView::FileWrite,
        ToolKind::FileEdit => ToolKindView::FileEdit,
        ToolKind::FileRead => ToolKindView::FileRead,
        ToolKind::Shell => ToolKindView::Shell,
        ToolKind::Search => ToolKindView::Search,
        ToolKind::Fetch => ToolKindView::Fetch,
        ToolKind::Subagent => ToolKindView::Subagent,
        ToolKind::Other => ToolKindView::Other,
    }
}

/// How a grouped call stands. A denial or cancellation is said once, by
/// the outcome.
fn tool_state(view: &ToolView, wall_ms: i64) -> (ToolState, String) {
    let approval = view.approval.as_ref().map(|a| &a.status);
    let answered = match approval {
        Some(ApprovalStatus::Resolved { decision, by, rule }) => {
            Some(resolved_label(*decision, by, rule.as_ref()))
        }
        Some(ApprovalStatus::Answered { by }) => Some(format!("answered {}", where_(by))),
        Some(ApprovalStatus::Cancelled { .. }) => Some("approval cancelled".to_owned()),
        Some(ApprovalStatus::Pending) | None => None,
    };
    // Skywave's own check approved it, when no human did: the same words'
    // place as a human's answer, and a human's answer wins if both are set.
    let answered = answered.or_else(|| {
        view.safety_check
            .as_deref()
            .map(|reason| safety_approval_label(&clean_line(reason)))
    });
    let asks = view.approval.as_ref().is_some_and(|a| a.question);
    match &view.outcome {
        ToolOutcome::Running { .. } if approval == Some(&ApprovalStatus::Pending) => {
            let words = if asks {
                "needs an answer"
            } else {
                "needs approval"
            };
            (ToolState::NeedsApproval, words.to_owned())
        }
        ToolOutcome::Running { since } => {
            let elapsed = u64::try_from(wall_ms - since.timestamp_millis()).unwrap_or(0);
            let parts: Vec<String> = answered
                .into_iter()
                .chain([format!(
                    "running {}",
                    seconds(Duration::from_millis(elapsed))
                )])
                .collect();
            (ToolState::Running, parts.join(" · "))
        }
        ToolOutcome::Done {
            is_error,
            status,
            took,
            ..
        } => {
            let status = status
                .clone()
                .or_else(|| is_error.then(|| "error".to_owned()));
            let parts: Vec<String> = status
                .into_iter()
                .chain(answered)
                .chain([took_label(*took)])
                .collect();
            let state = if *is_error {
                ToolState::Failed
            } else {
                ToolState::Done
            };
            (state, parts.join(" · "))
        }
        ToolOutcome::Denied { by } => (
            ToolState::Denied,
            format!("{} {}", decision_label(Decision::Deny), where_(by)),
        ),
        ToolOutcome::Cancelled { reason } => (
            ToolState::Cancelled,
            format!("cancelled: {}", clean_line(reason)),
        ),
        ToolOutcome::NoResult => {
            let parts: Vec<String> = answered
                .into_iter()
                .chain(["no result".to_owned()])
                .collect();
            (ToolState::NoResult, parts.join(" · "))
        }
    }
}

/// A standalone approval's state.
fn approval_state(status: &ApprovalStatus) -> (ToolState, String) {
    match status {
        ApprovalStatus::Pending => (ToolState::NeedsApproval, "needs approval".to_owned()),
        ApprovalStatus::Resolved { decision, by, rule } => (
            match decision {
                Decision::Approve => ToolState::Done,
                Decision::Deny => ToolState::Denied,
            },
            resolved_label(*decision, by, rule.as_ref()),
        ),
        ApprovalStatus::Answered { by } => (ToolState::Done, format!("answered {}", where_(by))),
        ApprovalStatus::Cancelled { reason } => (
            ToolState::Cancelled,
            format!("cancelled: {}", clean_line(reason)),
        ),
    }
}

/// This device's answer to a settled approval, when it did not apply:
/// `you approved · not applied: the approval was cancelled first`.
fn unapplied(status: &ApprovalStatus, answer: Option<&Answer>) -> Option<String> {
    let answer = answer?;
    match (status, answer.outgoing) {
        (
            ApprovalStatus::Resolved { .. }
            | ApprovalStatus::Answered { .. }
            | ApprovalStatus::Cancelled { .. },
            Outgoing::Failed(reason),
        ) => Some(format!(
            "you {} · not applied: {}",
            answer_words(answer.decision, answer.rule.as_ref()),
            fail_reason(reason)
        )),
        _ => None,
    }
}

/// `approved always on <device> · <rule>`.
fn resolved_label(decision: Decision, by: &DeviceRef, rule: Option<&RuleSuggestion>) -> String {
    let words = answer_words(decision, rule);
    let rule = rule.map_or_else(String::new, |rule| {
        format!(" · {}", clean_line(&rule.label))
    });
    format!("{words} {}{rule}", where_(by))
}

/// Who answered: `here`, or `on <device>`.
fn where_(by: &DeviceRef) -> String {
    if by.me {
        "here".to_owned()
    } else {
        format!("on {}", clean_line(&by.label()))
    }
}

/// Who did something: `you`, or the other device.
fn who(by: &DeviceRef) -> String {
    if by.me {
        "you".to_owned()
    } else {
        clean_line(&by.label())
    }
}

pub fn outgoing_view(outgoing: &Outgoing) -> OutgoingView {
    OutgoingView {
        state: match outgoing {
            Outgoing::Queued => OutgoingState::Queued,
            Outgoing::Sending => OutgoingState::Sending,
            Outgoing::Accepted(_) => OutgoingState::Accepted,
            Outgoing::Failed(_) => OutgoingState::Failed,
        },
        label: outgoing_label(outgoing),
    }
}

/// One section as shown: collapsed, its preview's head and what more
/// there is; expanded, all of it (the blob's head once here). `None` for
/// a collapsed section with nothing to show.
fn section_view(section: &Section, expanded: bool, heads: &mut BlobHeads) -> Option<SectionView> {
    let preview = content_lines(section.text);
    let mut note = None;
    let mut cut_short = false;
    let lines: Vec<String> = if expanded {
        match section.blob().map(|blob| (blob.id, &blob.state)) {
            Some((id, BlobState::Available { path })) => match heads.get(id, path) {
                Ok(head) => {
                    cut_short = head.cut.is_some();
                    note = head.cut;
                    head.lines
                }
                Err(err) => {
                    note = Some(err);
                    preview.into_iter().map(clean_line).collect()
                }
            },
            // Where its download stands is the blob's label.
            Some(_) => preview.into_iter().map(clean_line).collect(),
            None => match section.inline() {
                Some(whole) => {
                    let head = inline_head(whole);
                    cut_short = head.cut.is_some();
                    note = head.cut;
                    head.lines
                }
                None => {
                    note = section.note().map(Cow::into_owned);
                    preview.into_iter().map(clean_line).collect()
                }
            },
        }
    } else {
        if preview.is_empty() && !section.message {
            // Nothing worth a line, e.g. a write's output beside its status.
            // A long message's whole still says where it stands, under the
            // head the message shows.
            return None;
        }
        let shown: Vec<String> = preview
            .iter()
            .take(PREVIEW_LINES)
            .map(|l| clean_line(l))
            .collect();
        note = match (
            more_note(section, preview.len(), shown.len()),
            section.note(),
        ) {
            (Some(more), Some(lost)) => Some(format!("{more} · {lost}")),
            (None, Some(lost)) => Some(format!("… {lost}")),
            (more, None) => more,
        };
        shown
    };
    let whole = (expanded && section.message).then(|| markdown(&lines.join("\n")));
    let mut view = view_of(section, lines, note, cut_short);
    view.markdown = whole.unwrap_or_default();
    Some(view)
}

/// A section's head as the approval panel shows it: at most `max` lines of
/// its preview, then what was left out.
pub fn preview_view(section: &Section, max: usize) -> SectionView {
    let preview = content_lines(section.text);
    let shown: Vec<String> = preview.iter().take(max).map(|l| clean_line(l)).collect();
    let note = more_note(section, preview.len(), shown.len());
    view_of(section, shown, note, false)
}

fn view_of(
    section: &Section,
    lines: Vec<String>,
    note: Option<String>,
    cut_short: bool,
) -> SectionView {
    SectionView {
        format: match section.format {
            BodyFormat::Text => SectionFormat::Text,
            BodyFormat::Diff => SectionFormat::Diff,
        },
        numbered: section.numbered,
        first_line: 1,
        input: section.input,
        label: section.label().map(str::to_owned),
        lines: lines
            .into_iter()
            .map(|text| SectionLine {
                style: line_style(section, &text),
                text,
            })
            .collect(),
        markdown: Vec::new(),
        note,
        blob: section.blob().map(blob_facts),
        cut_short,
    }
}

/// An agent message's `text` as the GUI's markdown blocks.
fn markdown(text: &str) -> Vec<MarkdownBlock> {
    let spans = |spans: Vec<md::Span>| spans.into_iter().map(span).collect();
    md::blocks(text)
        .into_iter()
        .map(|block| MarkdownBlock {
            kind: match block.kind {
                md::BlockKind::Paragraph(s) => MarkdownKind::Paragraph { spans: spans(s) },
                md::BlockKind::Heading { level, spans: s } => MarkdownKind::Heading {
                    level,
                    spans: spans(s),
                },
                md::BlockKind::Code { lang, text } => MarkdownKind::Code { lang, text },
                md::BlockKind::Rule => MarkdownKind::Rule,
                md::BlockKind::Table { align, head, rows } => {
                    let cells = |cells: Vec<Vec<md::Span>>| {
                        cells
                            .into_iter()
                            .map(|s| MarkdownCell { spans: spans(s) })
                            .collect()
                    };
                    MarkdownKind::Table {
                        align: align
                            .into_iter()
                            .map(|a| match a {
                                md::Align::Left => MarkdownAlign::Left,
                                md::Align::Center => MarkdownAlign::Center,
                                md::Align::Right => MarkdownAlign::Right,
                            })
                            .collect(),
                        head: cells(head),
                        rows: rows
                            .into_iter()
                            .map(|row| MarkdownRow { cells: cells(row) })
                            .collect(),
                    }
                }
            },
            depth: block.depth,
            quote: block.quote,
            marker: block.marker,
        })
        .collect()
}

fn span(span: md::Span) -> MarkdownSpan {
    MarkdownSpan {
        text: span.text,
        bold: span.style.bold,
        italic: span.style.italic,
        code: span.style.code,
        strike: span.style.strike,
        link: span.link,
    }
}

/// While collapsed: how many more lines there are, `… +12 lines`. Where
/// the blob behind them stands is its label, not the note.
fn more_note(section: &Section, preview: usize, shown: usize) -> Option<String> {
    let whole = section
        .lines
        .map_or(preview, |lines| lines as usize)
        .max(preview);
    let rest = whole.checked_sub(shown).filter(|&rest| rest > 0)?;
    Some(format!(
        "… +{rest} {}",
        if rest == 1 { "line" } else { "lines" }
    ))
}

/// A section's blob with where its download stands ([`download_state`]):
/// `not downloaded · 4.0 KiB`, `downloading · 1.0 KiB of 4.0 KiB (25%)`.
pub fn blob_facts(blob: &BlobView) -> BlobFacts {
    let state = match &blob.state {
        BlobState::NotDownloaded { .. } => BlobStateKind::NotDownloaded,
        BlobState::Requested { .. } => BlobStateKind::Requested,
        BlobState::Downloading {
            paused: Some(_), ..
        } => BlobStateKind::Paused,
        BlobState::Downloading { paused: None, .. } => BlobStateKind::Downloading,
        BlobState::Failed { .. } => BlobStateKind::Failed,
        BlobState::Available { .. } => BlobStateKind::Available,
    };
    let label = download_state(&blob.state);
    // What the server did to it is said beside where it stands.
    let label = match change_note(blob) {
        Some(note) => format!("{label} · {note}"),
        None => label,
    };
    BlobFacts {
        blob_id: blob.id.0.to_string(),
        state,
        label,
        can_download: matches!(
            blob.state,
            BlobState::NotDownloaded { .. } | BlobState::Failed { .. }
        ),
    }
}

/// A content line's colour: diff lines by their sign, an error's all red.
fn line_style(section: &Section, line: &str) -> LineStyle {
    if section.error {
        return LineStyle::Error;
    }
    if section.format != BodyFormat::Diff {
        return LineStyle::Plain;
    }
    if line.starts_with("+++") || line.starts_with("---") {
        LineStyle::FileHeader
    } else if line.starts_with('+') {
        LineStyle::Added
    } else if line.starts_with('-') {
        LineStyle::Removed
    } else if line.starts_with("@@") {
        LineStyle::Hunk
    } else {
        LineStyle::Plain
    }
}

/// The lines of some content; a final line break ends the last line, it
/// does not start one. Nothing for empty content.
pub fn content_lines(text: &str) -> Vec<&str> {
    let text = text.strip_suffix('\n').unwrap_or(text);
    if text.is_empty() {
        return Vec::new();
    }
    text.split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use chrono::{TimeZone, Utc};
    use skywave_core::attachment::AttachedFile;
    use skywave_core::{
        AgentSetting, AgentView, CommandId, ContentHash, ContextId, DeviceId, FullView, ImageType,
        Pause, PermissionMode, SessionId, SessionState,
    };
    use skywave_present::sections::CONTENT_LOST;

    use super::*;

    fn session(timeline: Vec<TimelineItem>) -> SessionView {
        SessionView {
            subagent: None,
            id: SessionId::new(),
            title: "x".into(),
            named: false,
            rename: None,
            archived: false,
            workdir: None,
            state: SessionState::Idle,
            timeline,
            pending_approval: None,
            pending_question: None,
            rules: Vec::new(),
            usage: None,
            agent: AgentView::default(),
            safety_check: false,
            safety_check_change: None,
            contexts: Vec::new(),
            context: ContextId::new(),
            viewing: None,
            can_clear: false,
            clear: None,
            switch: None,
            interrupt: None,
            stop: None,
            work: Default::default(),
            task_stops: Vec::new(),
            mode: skywave_core::ComposerMode::Agent,
            draft: None,
            draft_images: Vec::new(),
            draft_files: Vec::new(),
            shell_draft: None,
            draft_rev: 0,
            shell: Default::default(),
            last_seq: Seq(0),
            unread_from: None,
        }
    }

    fn agent(seq: u64, minute: u32, text: &str) -> TimelineItem {
        TimelineItem::Logged {
            parent: None,
            seq: Seq(seq),
            at: Utc.with_ymd_and_hms(2026, 10, 31, 12, minute, 0).unwrap(),
            context: ContextId::new(),
            entry: Entry::AgentMessage {
                text: text.into(),
                full: None,
            },
        }
    }

    fn pending(id: CommandId, text: &str) -> TimelineItem {
        TimelineItem::Pending {
            command_id: id,
            context: ContextId::new(),
            text: text.into(),
            images: Vec::new(),
            files: Vec::new(),
            outgoing: Outgoing::Queued,
            discardable: true,
            delivery: None,
        }
    }

    fn logged_message(seq: u64, id: CommandId, text: &str) -> TimelineItem {
        TimelineItem::Logged {
            parent: None,
            seq: Seq(seq),
            at: Utc.with_ymd_and_hms(2026, 10, 31, 12, 5, 0).unwrap(),
            context: ContextId::new(),
            entry: Entry::UserMessage {
                command_id: id,
                text: text.into(),
                images: Vec::new(),
                from: DeviceRef {
                    id: DeviceId::new(),
                    name: None,
                    me: true,
                },
                outgoing: Some(Outgoing::Accepted(Seq(seq))),
                delivery: None,
            },
        }
    }

    fn rows_of(timeline: Vec<TimelineItem>) -> Rows {
        rows(
            &session(timeline),
            &HashSet::new(),
            &mut BlobHeads::default(),
            Path::new("/data"),
            0,
        )
    }

    /// Applies `updates` to `rows` as the Dart holder does.
    fn apply(rows: &mut Vec<TimelineRow>, updates: Vec<TimelineUpdate>) {
        for update in updates {
            match update {
                TimelineUpdate::Snapshot { items, .. } => *rows = items,
                TimelineUpdate::Upserted {
                    key,
                    item,
                    index,
                    replaces,
                } => {
                    rows.retain(|r| r.key != key && Some(&r.key) != replaces.as_ref());
                    rows.insert(index as usize, item);
                }
                TimelineUpdate::Removed { key } => rows.retain(|r| r.key != key),
            }
        }
    }

    fn keys(updates: &[TimelineUpdate]) -> Vec<String> {
        updates
            .iter()
            .map(|u| match u {
                TimelineUpdate::Snapshot { .. } => "snapshot".to_owned(),
                TimelineUpdate::Upserted {
                    key,
                    index,
                    replaces,
                    ..
                } => format!("upsert {key}@{index} replaces {replaces:?}"),
                TimelineUpdate::Removed { key } => format!("remove {key}"),
            })
            .collect()
    }

    #[test]
    fn keys_round_trip_and_bad_ones_are_errors() {
        let pending = ItemKey::Pending(CommandId::new());
        for key in [ItemKey::Logged(Seq(42)), pending] {
            assert_eq!(parse_key(&key_string(key)), Ok(key));
        }
        for bad in ["", "s:", "s:x", "p:nope", "q:1", "42"] {
            assert!(parse_key(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_changed_row_and_a_new_one_are_upserted() {
        let old = rows_of(vec![agent(1, 0, "A"), agent(2, 0, "B")]);
        let new = rows_of(vec![agent(1, 0, "A"), agent(2, 0, "B'"), agent(3, 0, "C")]);
        let updates = diff(&old.rows, &new).unwrap();
        assert_eq!(
            keys(&updates),
            ["upsert s:2@1 replaces None", "upsert s:3@2 replaces None"]
        );
        let mut held = old.rows;
        apply(&mut held, updates);
        assert_eq!(held, new.rows);
    }

    #[test]
    fn a_logged_message_replaces_its_pending_one() {
        let id = CommandId::new();
        let old = rows_of(vec![agent(1, 0, "A"), pending(id, "hi")]);
        let new = rows_of(vec![agent(1, 0, "A"), logged_message(2, id, "hi")]);
        let updates = diff(&old.rows, &new).unwrap();
        let p = key_string(ItemKey::Pending(id));
        assert_eq!(
            keys(&updates),
            [
                format!("remove {p}"),
                format!("upsert s:2@1 replaces Some({p:?})"),
            ]
        );
        let mut held = old.rows;
        apply(&mut held, updates);
        assert_eq!(held, new.rows);
    }

    #[test]
    fn a_pending_message_carries_the_command_id_discard_takes() {
        let id = CommandId::new();
        let pending = rows_of(vec![pending(id, "hi")]);
        assert_eq!(pending.rows[0].command_id, Some(id.0.to_string()));
        assert!(pending.rows[0].discardable);
        let logged = rows_of(vec![logged_message(2, id, "hi")]);
        assert_eq!(logged.rows[0].command_id, None);
    }

    #[test]
    fn a_conflated_snapshot_diffs_from_what_was_sent() {
        let (p, q) = (CommandId::new(), CommandId::new());
        let sent = rows_of(vec![agent(1, 0, "A"), agent(2, 0, "B"), pending(p, "one")]);
        // Skipped: [A, B, logged one, C]. Now B is gone (deleted
        // locally? edited), one is logged, C and D arrived, and another
        // message is pending.
        let now = rows_of(vec![
            agent(1, 0, "A"),
            logged_message(3, p, "one"),
            agent(4, 1, "C"),
            agent(5, 1, "D"),
            pending(q, "two"),
        ]);
        let updates = diff(&sent.rows, &now).unwrap();
        let mut held = sent.rows.clone();
        apply(&mut held, updates);
        assert_eq!(held, now.rows);
    }

    #[test]
    fn rows_that_change_order_are_a_snapshot() {
        let old = rows_of(vec![agent(1, 0, "A"), agent(2, 0, "B")]);
        let mut new = old.clone();
        new.rows.swap(0, 1);
        new.was.swap(0, 1);
        assert_eq!(diff(&old.rows, &new), None);
    }

    #[test]
    fn a_setting_change_is_a_note_and_a_refusal_a_warning() {
        let logged = |seq, setting, by, refused: Option<&str>| TimelineItem::Logged {
            parent: None,
            seq: Seq(seq),
            at: Utc.with_ymd_and_hms(2026, 10, 31, 12, 0, 0).unwrap(),
            context: ContextId::new(),
            entry: Entry::AgentSetting {
                setting,
                by,
                refused: refused.map(str::to_owned),
            },
        };
        let laptop = DeviceRef {
            id: DeviceId::new(),
            name: Some("laptop".into()),
            me: false,
        };
        let rows = rows_of(vec![
            logged(1, AgentSetting::Mode(PermissionMode::Plan), None, None),
            logged(
                2,
                AgentSetting::Model("big".into()),
                Some(laptop),
                Some("no model named"),
            ),
        ]);
        let got: Vec<_> = rows
            .rows
            .iter()
            .map(|r| (r.kind, r.text.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                (RowKind::AgentSetting, "mode → plan"),
                (
                    RowKind::SettingRefused,
                    "model big refused: no model named by laptop"
                ),
            ]
        );
    }

    #[test]
    fn the_time_shows_when_the_minute_changes() {
        let rows = rows_of(vec![agent(1, 0, "A"), agent(2, 0, "B"), agent(3, 1, "C")]);
        let labels: Vec<_> = rows.rows.iter().map(|r| r.time_label.clone()).collect();
        assert_eq!(
            labels,
            [Some("12:00".to_owned()), None, Some("12:01".to_owned())]
        );
    }

    /// The input an approval asked about, other than its call's, shows
    /// labelled once the call is expanded, and not before.
    #[test]
    fn an_approvals_own_input_shows_labelled_when_expanded() {
        let asked = FullView::Blob(BlobView {
            id: BlobId::new(),
            state: BlobState::NotDownloaded { size: 2048 },
            change: None,
            pending: None,
        });
        let view = session(vec![TimelineItem::Logged {
            parent: None,
            seq: Seq(1),
            at: Utc.with_ymd_and_hms(2026, 10, 31, 12, 0, 0).unwrap(),
            context: ContextId::new(),
            entry: Entry::Tool(Box::new(ToolView {
                call: skywave_core::CallId::new(),
                name: "Bash".into(),
                kind: skywave_core::ToolKind::Shell,
                title: "$ make".into(),
                detail: None,
                body: None,
                input: None,
                approval: Some(skywave_core::ToolApproval {
                    id: skywave_core::ApprovalId::new(),
                    question: false,
                    status: skywave_core::ApprovalStatus::Pending,
                    answer: None,
                    input: Some(asked),
                }),
                safety_check: None,
                outcome: ToolOutcome::Running { since: Utc::now() },
                subagent: None,
            })),
        }]);
        let labels = |expanded: &HashSet<Seq>| {
            let rows = rows(
                &view,
                expanded,
                &mut BlobHeads::default(),
                Path::new("/data"),
                0,
            )
            .rows;
            rows[0]
                .sections
                .iter()
                .map(|s| (s.label.clone(), s.blob.is_some()))
                .collect::<Vec<_>>()
        };
        assert_eq!(labels(&HashSet::new()), []);
        assert_eq!(
            labels(&HashSet::from([Seq(1)])),
            [(
                Some("approval input: differs from the call's".to_owned()),
                true
            )]
        );
    }

    #[test]
    fn the_unread_rule_stands_in_for_the_turn_separator() {
        let mut view = session(vec![
            agent(1, 0, "A"),
            logged_message(2, CommandId::new(), "go"),
            agent(3, 0, "B"),
            logged_message(4, CommandId::new(), "again"),
        ]);
        view.unread_from = Some(Seq(2));
        let rows = rows(
            &view,
            &HashSet::new(),
            &mut BlobHeads::default(),
            Path::new("/data"),
            0,
        )
        .rows;
        let facts: Vec<_> = rows.iter().map(|r| (r.unread_rule, r.turn_start)).collect();
        assert_eq!(
            facts,
            [(false, false), (true, false), (false, false), (false, true)]
        );
    }

    #[test]
    fn a_note_says_what_is_left_out_and_the_label_where_the_blob_stands() {
        let blob = |state| BlobView {
            id: BlobId::new(),
            state,
            change: None,
            pending: None,
        };
        let text = (1..=10).map(|i| format!("line {i}\n")).collect::<String>();
        let shown = |state, expanded| {
            let blob = FullView::Blob(blob(state));
            let section = Section {
                text: &text,
                lines: Some(40),
                format: BodyFormat::Text,
                numbered: false,
                error: false,
                full: Some(&blob),
                hidden: false,
                input: false,
                message: false,
                asked: false,
            };
            let view = section_view(&section, expanded, &mut BlobHeads::default()).unwrap();
            (view.note, view.blob.unwrap().label)
        };
        let size = 70 * 1024;
        assert_eq!(
            shown(BlobState::NotDownloaded { size }, false),
            (
                Some("… +32 lines".to_owned()),
                "not downloaded · 70.0 KiB".to_owned()
            )
        );
        for (state, label) in [
            (
                BlobState::NotDownloaded { size },
                "not downloaded · 70.0 KiB",
            ),
            (
                BlobState::Requested { size },
                "requested · waiting for a slot · 70.0 KiB",
            ),
            (
                BlobState::Downloading {
                    got: size / 2,
                    size,
                    paused: Some(Pause::Offline),
                },
                "downloading · 35.0 KiB of 70.0 KiB (50%) · paused, offline",
            ),
            (
                BlobState::Downloading {
                    got: size / 2,
                    size,
                    paused: Some(Pause::DiskFull),
                },
                "downloading · 35.0 KiB of 70.0 KiB (50%) · paused, device disk full",
            ),
            (
                BlobState::Failed {
                    size,
                    reason: "gone".into(),
                },
                "download failed: gone · 70.0 KiB",
            ),
        ] {
            assert_eq!(shown(state, true), (None, label.to_owned()));
        }
    }

    #[test]
    fn an_agent_message_comes_as_markdown_and_others_do_not() {
        let id = CommandId::new();
        let rows = rows_of(vec![
            agent(1, 0, "**Done**: see `a.rs`"),
            logged_message(2, id, "**not** markdown"),
        ])
        .rows;
        let span = |text: &str, bold, code| MarkdownSpan {
            text: text.to_owned(),
            bold,
            italic: false,
            code,
            strike: false,
            link: None,
        };
        assert_eq!(
            rows[0].markdown,
            [MarkdownBlock {
                kind: MarkdownKind::Paragraph {
                    spans: vec![
                        span("Done", true, false),
                        span(": see ", false, false),
                        span("a.rs", false, true),
                    ]
                },
                depth: 0,
                quote: 0,
                marker: None,
            }]
        );
        // The raw text stays, for a copy and for a search's marks.
        assert_eq!(rows[0].text, "**Done**: see `a.rs`");
        assert_eq!(rows[1].markdown, []);
    }

    #[test]
    fn a_long_messages_whole_is_markdown_only_when_expanded() {
        let whole = FullView::Inline("# Title\n\nbody".to_owned());
        let section = Section {
            text: "",
            lines: None,
            format: BodyFormat::Text,
            numbered: false,
            error: false,
            full: Some(&whole),
            hidden: false,
            input: false,
            message: true,
            asked: false,
        };
        let expanded = section_view(&section, true, &mut BlobHeads::default()).unwrap();
        assert_eq!(
            expanded
                .markdown
                .iter()
                .map(|b| &b.kind)
                .collect::<Vec<_>>(),
            [
                &MarkdownKind::Heading {
                    level: 1,
                    spans: vec![MarkdownSpan {
                        text: "Title".to_owned(),
                        bold: false,
                        italic: false,
                        code: false,
                        strike: false,
                        link: None,
                    }]
                },
                &MarkdownKind::Paragraph {
                    spans: vec![MarkdownSpan {
                        text: "body".to_owned(),
                        bold: false,
                        italic: false,
                        code: false,
                        strike: false,
                        link: None,
                    }]
                },
            ]
        );
        let collapsed = section_view(&section, false, &mut BlobHeads::default()).unwrap();
        assert_eq!(collapsed.markdown, []);
    }

    #[test]
    fn a_section_the_server_could_not_keep_says_so_and_has_no_blob() {
        let text = (1..=10).map(|i| format!("line {i}\n")).collect::<String>();
        let lost = FullView::Lost;
        let section = |full| Section {
            text: &text,
            lines: Some(40),
            format: BodyFormat::Text,
            numbered: false,
            error: false,
            full,
            hidden: false,
            input: false,
            message: false,
            asked: false,
        };
        let view = |full, expanded| {
            section_view(&section(full), expanded, &mut BlobHeads::default()).unwrap()
        };
        let collapsed = view(Some(&lost), false);
        assert_eq!(
            collapsed.note.as_deref(),
            Some(format!("… +32 lines · {CONTENT_LOST}").as_str())
        );
        assert_eq!(collapsed.blob, None);
        assert_eq!(view(Some(&lost), true).note.as_deref(), Some(CONTENT_LOST));
        // Control: a preview that holds it all says nothing of the kind.
        assert_eq!(view(None, true).note, None);
    }

    /// A logged message carrying `images`.
    fn logged_images(images: Vec<MessageImage>) -> TimelineItem {
        TimelineItem::Logged {
            parent: None,
            seq: Seq(2),
            at: Utc.with_ymd_and_hms(2026, 10, 31, 12, 5, 0).unwrap(),
            context: ContextId::new(),
            entry: Entry::UserMessage {
                command_id: CommandId::new(),
                text: "look".into(),
                images,
                from: DeviceRef {
                    id: DeviceId::new(),
                    name: None,
                    me: true,
                },
                outgoing: None,
                delivery: None,
            },
        }
    }

    #[test]
    fn a_queued_message_image_is_its_own_file_on_this_device() {
        let sha256 = ContentHash::of(b"png");
        let item = TimelineItem::Pending {
            command_id: CommandId::new(),
            context: ContextId::new(),
            text: "look".into(),
            images: vec![AttachedImage {
                sha256,
                media_type: ImageType::Png,
                size: 348_160,
                width: 1,
                height: 1,
            }],
            files: Vec::new(),
            outgoing: Outgoing::Queued,
            discardable: true,
            delivery: None,
        };
        let rows = rows(
            &session(vec![item]),
            &HashSet::new(),
            &mut BlobHeads::default(),
            Path::new("/data"),
            0,
        )
        .rows;
        assert_eq!(
            rows[0].images,
            [RowImage {
                blob_id: None,
                path: Some(
                    attachment_path(Path::new("/data"), sha256)
                        .display()
                        .to_string()
                ),
                label: bytes(348_160),
                can_download: false,
            }]
        );
    }

    /// A queued message of `files`, in `outgoing` state.
    fn queued_files(files: Vec<OutboxFile>, outgoing: Outgoing) -> TimelineItem {
        TimelineItem::Pending {
            command_id: CommandId::new(),
            context: ContextId::new(),
            text: "files".into(),
            images: Vec::new(),
            files,
            outgoing,
            discardable: true,
            delivery: None,
        }
    }

    /// A file of a queued message called `name` of `size` bytes, with the
    /// sender's `existing` file at that name when it found one.
    fn file_named(name: &str, size: u64, existing: Option<ContentHash>) -> OutboxFile {
        OutboxFile {
            file: AttachedFile {
                sha256: ContentHash::of(name.as_bytes()),
                name: name.into(),
                size,
                replace: None,
            },
            existing,
        }
    }

    #[test]
    fn a_queued_message_lists_its_files() {
        let item = queued_files(
            vec![
                file_named("notes.pdf", 2_048, None),
                file_named("data.csv", 512, None),
            ],
            Outgoing::Queued,
        );
        let row = rows_of(vec![item]).rows.remove(0);
        assert_eq!(
            row.files,
            [
                RowFile {
                    name: "notes.pdf".into(),
                    size_label: bytes(2_048),
                    found: false,
                },
                RowFile {
                    name: "data.csv".into(),
                    size_label: bytes(512),
                    found: false,
                },
            ]
        );
        assert!(!row.can_resend_replacing);
    }

    #[test]
    fn a_message_refused_for_a_found_file_can_be_resent_replacing() {
        let file_exists =
            Outgoing::Failed(FailReason::AttachmentRefused(AttachmentRefusal::FileExists));
        let old = ContentHash::of(b"old");
        let found = rows_of(vec![queued_files(
            vec![file_named("notes.pdf", 4_096, Some(old))],
            file_exists,
        )])
        .rows
        .remove(0);
        assert!(found.files[0].found);
        assert!(found.can_resend_replacing);

        // Control: the same refusal, with nothing found at the name.
        let nothing_found = rows_of(vec![queued_files(
            vec![file_named("notes.pdf", 4_096, None)],
            Outgoing::Failed(FailReason::AttachmentRefused(AttachmentRefusal::FileExists)),
        )])
        .rows
        .remove(0);
        assert!(!nothing_found.files[0].found);
        assert!(!nothing_found.can_resend_replacing);

        // Control: a file found, but refused for another reason.
        let missing = rows_of(vec![queued_files(
            vec![file_named("notes.pdf", 4_096, Some(old))],
            Outgoing::Failed(FailReason::AttachmentRefused(AttachmentRefusal::Missing)),
        )])
        .rows
        .remove(0);
        assert!(missing.files[0].found);
        assert!(!missing.can_resend_replacing);
    }

    #[test]
    fn a_logged_message_image_shows_its_blob_as_it_stands() {
        let id = BlobId::new();
        let blob = |state| BlobView {
            id,
            state,
            change: None,
            pending: None,
        };
        let not_downloaded = blob(BlobState::NotDownloaded { size: 70 * 1024 });
        let facts = blob_facts(&not_downloaded);
        let rows = rows_of(vec![logged_images(vec![
            MessageImage {
                media_type: ImageType::Png,
                content: FullView::Blob(not_downloaded),
            },
            MessageImage {
                media_type: ImageType::Png,
                content: FullView::Blob(blob(BlobState::Available {
                    path: std::path::PathBuf::from("/data/image"),
                })),
            },
            MessageImage {
                media_type: ImageType::Png,
                content: FullView::Deleted {
                    by: DeviceRef {
                        id: DeviceId::new(),
                        name: Some("laptop".into()),
                        me: false,
                    },
                },
            },
        ])])
        .rows;
        let blob_id = Some(id.0.to_string());
        assert_eq!(rows[0].images[0].label, facts.label);
        assert_eq!(
            rows[0].images,
            [
                RowImage {
                    blob_id: blob_id.clone(),
                    path: None,
                    label: "not downloaded · 70.0 KiB".to_owned(),
                    can_download: true,
                },
                RowImage {
                    blob_id,
                    path: Some("/data/image".to_owned()),
                    label: "downloaded".to_owned(),
                    can_download: false,
                },
                RowImage {
                    blob_id: None,
                    path: None,
                    label: "image deleted".to_owned(),
                    can_download: false,
                },
            ]
        );
        // Control: a message with no images has none.
        assert_eq!(
            rows_of(vec![logged_message(3, CommandId::new(), "hi")]).rows[0].images,
            []
        );
    }
}
