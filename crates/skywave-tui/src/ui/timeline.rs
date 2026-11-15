//! Timeline items as screen lines. Text is laid out here rather than by
//! ratatui's wrapping, so a wrapped line hangs under its own column and
//! every item's height is known exactly (scroll anchoring needs it).
//!
//! A tool item is one block: its kind's glyph, the tool name and title, the
//! state right-aligned, an optional detail line, and its content under a
//! gutter: at most [`PREVIEW_LINES`] lines collapsed, the whole blob
//! expanded. Colour carries the state (the glyph, the state and the
//! gutter), never the item's type.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use skywave_core::search::BlobLine;
use skywave_core::{
    Answer, ApprovalStatus, BlobId, BlobState, BodyFormat, Decision, DeviceRef, Entry,
    ImportedEntry, Outgoing, QUESTION_TOOL, Query, RuleSuggestion, SessionView, TimelineItem,
    ToolKind, ToolOutcome, ToolView, read_head,
};
use skywave_present::blob_actions::pending_note;
use skywave_present::format::{
    IMPORTED_MARK, adopted_line, agent_setting_line, agent_version_line, answer_words, asked_by,
    blob_size, bytes, context_moved_line, context_started_line, context_switched_line,
    decision_label, delivery_label, dollars, download_state, fail_reason, import_finished_line,
    moved_from_line, outgoing_label, seconds, subagent_status, took_label, turn_time,
};
use skywave_present::policy::in_conversation;
use skywave_present::sections::{
    ItemKey, PREVIEW_LINES, Section, change_note, collapsed_download, delivery, find, sections,
    selectable, starts_turn, unseen_blob,
};
use skywave_present::text::clean;
use unicode_width::UnicodeWidthChar;

use super::outgoing_badge;
use crate::app::{Search, SessionUi};

/// Most of a blob shown when expanded.
pub const SHOW_BYTES: u64 = 64 * 1024;
/// Most lines of a blob shown when expanded.
pub const SHOW_LINES: usize = 400;
/// Lines shown above a search match deep in a blob, past what shows of
/// its head.
const MATCH_ABOVE: usize = 3;

/// Columns before an item's text: the selection marker and the time.
const LEAD: usize = 8;
/// Columns before a tool item's title: the lead, the glyph and a space.
const TITLE: usize = LEAD + 2;
/// Narrowest a title may get beside its state before it wraps.
const MIN_TITLE: usize = 8;

/// The timeline's items as last laid out, kept from frame to frame so a
/// redraw lays out only what changed: the screen redraws every few hundred
/// milliseconds during a turn, and laying out a long session each time
/// lags typing over a slow link.
///
/// An item's lines are reused while the item is equal to the one they were
/// laid out from and everything else they show is the same ([`Flags`]);
/// a change of width or of the search's query drops them all. The one item
/// laid out against the search's current match in a blob is laid out
/// afresh each frame: the search's text of that blob is not compared.
#[derive(Debug, Clone, Default)]
pub struct TimelineCache {
    /// The width every item was laid out at.
    width: usize,
    /// The search's query every item was marked with.
    query: Option<String>,
    items: HashMap<ItemKey, Cached>,
    heads: Heads,
    /// Counts frames, to drop the items gone from the timeline.
    frame: u64,
    stats: LayoutStats,
}

/// How much laying out the timeline took so far: what a redraw that
/// changed nothing must not add to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LayoutStats {
    /// Items laid out.
    pub items: u64,
    /// Blob heads read from disk.
    pub blob_reads: u64,
}

#[derive(Debug, Clone)]
struct Cached {
    /// The item as laid out.
    item: TimelineItem,
    flags: Flags,
    /// `false` for the item laid out against the search's current match.
    reuse: bool,
    lines: Vec<Line<'static>>,
    /// The blobs whose heads it shows.
    blobs: Vec<BlobId>,
    /// The last frame it was in the timeline.
    frame: u64,
}

/// What an item's lines show beside the item itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Flags {
    above: Above,
    selected: bool,
    expanded: bool,
    /// Its time shows: the last item's was another minute.
    time: bool,
    /// Its matches are marked; `Some(true)` when it is the current match.
    mark: Option<bool>,
    /// The wall clock, for a running call's elapsed time; `None` for the
    /// rest, which do not change with it.
    wall_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Above {
    Nothing,
    /// The `── new ──` rule.
    Rule,
    /// The separator of a new turn.
    Separator,
}

/// The heads of downloaded blobs, as read from disk once: a downloaded
/// blob's file does not change.
#[derive(Debug, Clone, Default)]
struct Heads {
    read: HashMap<BlobId, Head>,
}

#[derive(Debug, Clone)]
struct Head {
    path: PathBuf,
    /// The first [`SHOW_BYTES`] of the file and its whole size, or why it
    /// could not be read.
    head: Result<(String, u64), String>,
}

impl TimelineCache {
    pub fn stats(&self) -> LayoutStats {
        self.stats
    }

    /// Lays out at `width` columns the timeline items not laid out as they
    /// are now, with the search's matches marked. `wall` is what running
    /// calls' elapsed time counts to. Returns each item's height in
    /// timeline order, and where the search's current match is in its
    /// blob, if that line is laid out: the item's timeline index and the
    /// line within the item.
    pub(super) fn lay_out(
        &mut self,
        session: &SessionView,
        ui: &SessionUi,
        focused: bool,
        width: usize,
        wall: SystemTime,
    ) -> (Vec<usize>, Option<(usize, usize)>) {
        let query = ui.shown.search.as_ref().map(|search| search.query.as_str());
        if self.width != width || self.query.as_deref() != query {
            self.width = width;
            self.query = query.map(str::to_owned);
            self.items.clear();
        }
        self.frame += 1;
        let current_line = ui
            .shown
            .search
            .as_ref()
            .and_then(|search| search.current_line(session));
        let layout = Layout {
            ui,
            focused,
            width: width.max(TITLE + MIN_TITLE),
            wall_ms: wall_ms(wall),
            rule: rule_index(session),
            search: ui.shown.search.as_ref(),
            current: ui
                .shown
                .search
                .as_ref()
                .and_then(|search| find(session, search.current?)),
            current_line: current_line.map(|(_, line)| line),
            matched: Cell::new(None),
            heads: RefCell::new(&mut self.heads),
            used: RefCell::new(Vec::new()),
            reads: Cell::new(0),
        };
        let mut minute = None;
        let mut matched = None;
        let mut heights = Vec::with_capacity(session.timeline.len());
        for (index, item) in session.timeline.iter().enumerate() {
            let flags = layout.flags(index, item, &mut minute);
            let key = ItemKey::of(item);
            if let Some(cached) = self
                .items
                .get_mut(&key)
                .filter(|cached| cached.reuse && cached.flags == flags && cached.item == *item)
            {
                cached.frame = self.frame;
                heights.push(cached.lines.len());
                continue;
            }
            let lines = layout.item(item, flags);
            self.stats.items += 1;
            let current = current_line.is_some_and(|(at, _)| at == index);
            if let Some(row) = layout.matched.take()
                && current
            {
                matched = Some((index, row));
            }
            heights.push(lines.len());
            self.items.insert(
                key,
                Cached {
                    item: item.clone(),
                    flags,
                    reuse: !current,
                    lines,
                    blobs: layout.used.take(),
                    frame: self.frame,
                },
            );
        }
        self.stats.blob_reads += layout.reads.get();
        drop(layout);
        if self.items.len() > session.timeline.len() {
            let frame = self.frame;
            self.items.retain(|_, cached| cached.frame == frame);
        }
        if !self.heads.read.is_empty() {
            let shown: HashSet<BlobId> = self
                .items
                .values()
                .flat_map(|cached| cached.blobs.iter().copied())
                .collect();
            self.heads.read.retain(|id, _| shown.contains(id));
        }
        (heights, matched)
    }

    /// At most `height` lines of the timeline as last laid out
    /// ([`TimelineCache::lay_out`]), from line `top`; `starts` is each
    /// item's first line.
    pub(super) fn lines(
        &self,
        session: &SessionView,
        starts: &[usize],
        top: usize,
        height: usize,
    ) -> Vec<Line<'static>> {
        let mut out = Vec::with_capacity(height);
        let first = starts
            .partition_point(|&start| start <= top)
            .saturating_sub(1);
        for (index, item) in session.timeline.iter().enumerate().skip(first) {
            if out.len() >= height {
                break;
            }
            let Some(cached) = self.items.get(&ItemKey::of(item)) else {
                continue;
            };
            let skip = top.saturating_sub(starts[index]);
            let room = height - out.len();
            out.extend(cached.lines.iter().skip(skip).take(room).cloned());
        }
        out
    }
}

/// The timeline item the `── new ──` rule goes above: the first logged one
/// at or after [`SessionView::unread_from`].
pub(super) fn rule_index(session: &SessionView) -> Option<usize> {
    let from = session.unread_from?;
    session
        .timeline
        .iter()
        .position(|item| matches!(item, TimelineItem::Logged { seq, .. } if *seq >= from))
}

struct Layout<'a> {
    ui: &'a SessionUi,
    focused: bool,
    width: usize,
    wall_ms: i64,
    /// Index of the item the `── new ──` rule goes above.
    rule: Option<usize>,
    /// The search: what it marks, and in which items.
    search: Option<&'a Search>,
    /// Index of the search's current match.
    current: Option<usize>,
    /// The line of its blob the current match is on, if it is in one.
    current_line: Option<BlobLine>,
    /// The row of the item being laid out that `current_line` landed on.
    matched: Cell<Option<usize>>,
    heads: RefCell<&'a mut Heads>,
    /// The blobs whose heads the item being laid out shows.
    used: RefCell<Vec<BlobId>>,
    /// Blob heads read from disk.
    reads: Cell<u64>,
}

/// Marks a search's matches in an item's text.
#[derive(Clone, Copy)]
pub(super) struct Mark<'a> {
    query: &'a Query,
    /// The item is the current match: its matches are bold too.
    current: bool,
}

impl Mark<'_> {
    /// `span` split where the query matches, the matches reversed yellow
    /// over the span's own style. Done before wrapping: the style is per
    /// char, so a match broken over two rows stays marked.
    fn spans(self, span: Span<'static>) -> Vec<Span<'static>> {
        let ranges = self.query.ranges(&span.content);
        if ranges.is_empty() {
            return vec![span];
        }
        let mut style = span.style.patch(Style::new().yellow().reversed());
        if self.current {
            style = style.bold();
        }
        split(&span.content, &ranges, span.style, style)
    }
}

/// `text` as spans in `base` style, `ranges` of it in `marked` style.
fn split(text: &str, ranges: &[Range<usize>], base: Style, marked: Style) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut at = 0;
    for range in ranges {
        if range.start > at {
            out.push(Span::styled(text[at..range.start].to_owned(), base));
        }
        out.push(Span::styled(text[range.clone()].to_owned(), marked));
        at = range.end;
    }
    if at < text.len() {
        out.push(Span::styled(text[at..].to_owned(), base));
    }
    out
}

/// `spans` with `mark`'s matches marked in each.
fn marked(spans: Vec<Span<'static>>, mark: Option<Mark>) -> Vec<Span<'static>> {
    match mark {
        Some(mark) => spans
            .into_iter()
            .flat_map(|span| mark.spans(span))
            .collect(),
        None => spans,
    }
}

impl Layout<'_> {
    /// What item `index` shows beside the item itself. The time shows only
    /// when its minute differs from the last one shown (`minute`).
    fn flags(&self, index: usize, item: &TimelineItem, minute: &mut Option<i64>) -> Flags {
        let above = if self.rule == Some(index) {
            Above::Rule
        } else if index > 0 && starts_turn(item) {
            Above::Separator
        } else {
            Above::Nothing
        };
        let key = selectable(item);
        let time = match item {
            // Imported history shows when the agent had it, not when the
            // server logged it.
            TimelineItem::Logged {
                entry: Entry::Imported { at, .. },
                ..
            }
            | TimelineItem::Logged { at, .. } => {
                let this = at.timestamp().div_euclid(60);
                minute.replace(this) != Some(this)
            }
            TimelineItem::Pending { .. } => false,
        };
        Flags {
            above,
            selected: self.focused && key.is_some() && key == self.ui.shown.selected,
            expanded: matches!(key, Some(ItemKey::Logged(seq)) if self.ui.shown.expanded.contains(&seq)),
            time,
            mark: self
                .search
                .and_then(|search| search.hit(index))
                .map(|_| self.current == Some(index)),
            wall_ms: ticks(item).then_some(self.wall_ms),
        }
    }

    /// One item: the `── new ──` rule if the news starts here, else a
    /// separator if it starts a turn, then its lines.
    fn item(&self, item: &TimelineItem, flags: Flags) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        match flags.above {
            Above::Rule => {
                let rest = self.width.saturating_sub(7);
                out.push(Line::from(vec![
                    Span::raw("── new ").magenta().bold(),
                    Span::raw("─".repeat(rest)).magenta(),
                ]));
            }
            Above::Separator => out.push(Line::from("─".repeat(self.width)).dark_gray()),
            Above::Nothing => {}
        }
        let selected = flags.selected;
        let expanded = flags.expanded;
        let mark = self.search.zip(flags.mark).map(|(search, current)| Mark {
            query: &search.query,
            current,
        });
        let marker = if selected {
            Span::raw("> ").bold().reversed()
        } else {
            Span::raw("  ")
        };
        let time = match item {
            TimelineItem::Logged {
                entry: Entry::Imported { at, .. },
                ..
            }
            | TimelineItem::Logged { at, .. }
                if flags.time =>
            {
                Span::raw(at.format("%H:%M ").to_string()).dark_gray()
            }
            _ => Span::raw("      "),
        };
        let lead = vec![marker, time];
        let above = out.len();
        out.extend(match item {
            TimelineItem::Pending { text, outgoing, .. } => {
                self.message(lead, you(Some(outgoing)), Color::Blue, text, mark)
            }
            // Dimmed: it happened outside Skywave.
            TimelineItem::Logged {
                entry: entry @ Entry::Imported { .. },
                ..
            } => self
                .entry(lead, entry, expanded, mark)
                .into_iter()
                .map(|line| line.patch_style(Modifier::DIM))
                .collect(),
            // Dimmed while out of the way, so the messages stand out to
            // someone skimming; at full strength once selected or open.
            TimelineItem::Logged { entry, .. }
                if !in_conversation(item) && !selected && !expanded =>
            {
                self.entry(lead, entry, expanded, mark)
                    .into_iter()
                    .map(|line| line.patch_style(Modifier::DIM))
                    .collect()
            }
            TimelineItem::Logged { entry, .. } => self.entry(lead, entry, expanded, mark),
        });
        if let Some(row) = self.matched.get() {
            self.matched.set(Some(above + row));
        }
        if selected && let Some((outgoing, delivery)) = delivery(item) {
            out.extend(wrap(
                vec![indent(LEAD)],
                vec![indent(LEAD)],
                vec![Span::raw(delivery_label(&delivery, &outgoing)).dark_gray()],
                self.width,
            ));
        }
        out
    }

    fn entry(
        &self,
        lead: Vec<Span<'static>>,
        entry: &Entry,
        expanded: bool,
        mark: Option<Mark>,
    ) -> Vec<Line<'static>> {
        let width = self.width;
        let aside = if expanded { None } else { unseen_blob(entry) };
        let titled = |mut content: Vec<Span<'static>>| {
            if let Some(aside) = &aside {
                content.push(Span::raw(format!(" · {aside}")).dark_gray());
            }
            content
        };
        // A grouped call's head, live or imported.
        let tool_head = |lead: Vec<Span<'static>>, view: &ToolView| {
            let (mut state, style) = tool_state(view, self.wall_ms);
            if let Some(note) = view
                .approval
                .as_ref()
                .and_then(|a| unapplied(&a.status, a.answer.as_ref()))
            {
                state = format!("{state} · {note}");
            }
            let mut out = head_lines(
                lead,
                Span::styled(glyph(view.kind), style),
                titled(marked(title_spans(&view.name, &view.title), mark)),
                marked(vec![Span::styled(state, style)], mark),
                width,
            );
            if let Some(detail) = &view.detail {
                out.extend(detail_lines(detail, TITLE, width, mark));
            }
            (out, style)
        };
        let result_head = |lead: Vec<Span<'static>>| {
            (
                head_lines(
                    lead,
                    Span::raw("↳"),
                    titled(vec![Span::raw("result").bold()]),
                    Vec::new(),
                    width,
                ),
                Style::new().dark_gray(),
            )
        };
        // Imported rows say so between the time and what they are.
        let imported = |mut lead: Vec<Span<'static>>| {
            lead.push(Span::raw(format!("{IMPORTED_MARK} ")).italic());
            lead
        };
        let (mut out, gutter) = match entry {
            Entry::UserMessage {
                text,
                from,
                outgoing,
                ..
            } => {
                let (label, bar) = if from.me {
                    (you(outgoing.as_ref()), Color::Blue)
                } else {
                    let label = vec![
                        Span::raw(device_label(from)).magenta().bold(),
                        Span::raw(": "),
                    ];
                    (label, Color::Magenta)
                };
                return self.message(lead, label, bar, text, mark);
            }
            // A long message's whole is a section beneath it.
            Entry::AgentMessage { text, .. } => {
                let label = vec![Span::raw("agent: ").green().bold()];
                (
                    self.message(lead, label, Color::Green, text, mark),
                    Style::new().green(),
                )
            }
            Entry::Interrupted => {
                let mut spans = lead;
                spans.push(Span::raw("interrupted · the agent stopped its turn").yellow());
                return vec![Line::from(spans)];
            }
            Entry::Stopped => {
                let mut spans = lead;
                spans.push(
                    Span::raw("stopped · the agent was shut down, a message restarts it").yellow(),
                );
                return vec![Line::from(spans)];
            }
            Entry::ContextStarted { name, by } => {
                return context_divider(lead, &context_started_line(name, by));
            }
            Entry::ContextSwitched { name, by } => {
                return context_divider(lead, &context_switched_line(name, by));
            }
            Entry::ContextMoved { name, to, by } => {
                return context_divider(lead, &context_moved_line(name, to, by));
            }
            Entry::MovedFrom { from, name } => {
                return context_divider(lead, &moved_from_line(name, from));
            }
            Entry::Adopted { conversation } => {
                return context_divider(lead, &adopted_line(conversation));
            }
            // The outside conversation's user wrote it, not a device.
            Entry::Imported {
                entry: ImportedEntry::UserMessage { text, .. },
                ..
            } => {
                let label = vec![Span::raw("you").blue().bold(), Span::raw(": ")];
                (
                    self.message(imported(lead), label, Color::Blue, text, mark),
                    Style::new().blue(),
                )
            }
            Entry::Imported {
                entry: ImportedEntry::AgentMessage { text, .. },
                ..
            } => {
                let label = vec![Span::raw("agent: ").green().bold()];
                (
                    self.message(imported(lead), label, Color::Green, text, mark),
                    Style::new().green(),
                )
            }
            Entry::Imported {
                entry: ImportedEntry::Tool(view),
                ..
            } => tool_head(imported(lead), view),
            Entry::Imported {
                entry: ImportedEntry::ToolResult { .. },
                ..
            } => result_head(imported(lead)),
            Entry::ImportFinished { outcome } => {
                return context_divider(lead, &import_finished_line(outcome));
            }
            Entry::Renamed { name, by } => {
                let who = if by.me {
                    "you".to_owned()
                } else {
                    device_label(by)
                };
                let what = match name {
                    Some(name) => format!("renamed to “{}” by {who}", clean(name)),
                    None => format!("default title restored by {who}"),
                };
                let mut spans = lead;
                spans.push(Span::raw(what).dark_gray());
                return vec![Line::from(spans)];
            }
            Entry::Archived { archived, by } => {
                let who = if by.me {
                    "you".to_owned()
                } else {
                    device_label(by)
                };
                let what = if *archived {
                    format!("archived by {who}")
                } else {
                    format!("brought back from the archive by {who}")
                };
                let mut spans = lead;
                spans.push(Span::raw(what).dark_gray());
                return vec![Line::from(spans)];
            }
            Entry::WorktreeRemoved { by } => {
                let who = if by.me {
                    "you".to_owned()
                } else {
                    device_label(by)
                };
                let mut spans = lead;
                spans.push(Span::raw(format!("worktree removed by {who}")).dark_gray());
                return vec![Line::from(spans)];
            }
            Entry::FileWritten {
                path, replaced, by, ..
            } => {
                let who = if by.me {
                    "you".to_owned()
                } else {
                    device_label(by)
                };
                let verb = if *replaced { "replaced" } else { "written" };
                let mut spans = lead;
                spans.push(Span::raw(format!("{} {verb} by {who}", clean(path))).dark_gray());
                return vec![Line::from(spans)];
            }
            Entry::AgentSetting {
                setting,
                by,
                refused,
            } => {
                let mut what = agent_setting_line(setting, refused.as_deref());
                if let Some(by) = asked_by(by.as_ref()) {
                    what = format!("{what} {by}");
                }
                let what = clean(&what);
                let mut spans = lead;
                // A refusal kept the old value: worth noticing.
                spans.push(match refused {
                    Some(_) => Span::raw(what).yellow(),
                    None => Span::raw(what).dark_gray(),
                });
                return vec![Line::from(spans)];
            }
            Entry::TurnUsage(usage) => {
                // What the turn took, after its last item; the status line
                // has tokens and the model.
                let mut text = "turn".to_owned();
                if let Some(ms) = usage.duration_ms {
                    text.push_str(&format!(" · {}", turn_time(ms)));
                }
                if let Some(cost) = usage.cost_usd {
                    text.push_str(&format!(" · {}", dollars(cost)));
                }
                let mut spans = lead;
                spans.push(Span::raw(text).dark_gray());
                return vec![Line::from(spans)];
            }
            Entry::AgentErrored { message } => {
                return self.message(
                    lead,
                    vec![Span::raw("agent error: ").red().bold()],
                    Color::Red,
                    message,
                    mark,
                );
            }
            Entry::AgentVersionUnconfirmed { version, confirmed } => {
                let mut spans = lead;
                spans.push(Span::raw(clean(&agent_version_line(version, confirmed))).yellow());
                return vec![Line::from(spans)];
            }
            Entry::Tool(view) => tool_head(lead, view),
            Entry::ToolCall { name, summary, .. } => (
                head_lines(
                    lead,
                    Span::raw("•"),
                    titled(marked(title_spans(name, summary), mark)),
                    Vec::new(),
                    width,
                ),
                Style::new().dark_gray(),
            ),
            Entry::ToolResult { .. } => result_head(lead),
            Entry::Approval {
                tool,
                summary,
                status,
                answer,
                ..
            } => {
                let (mut state, style) = approval_state(status);
                if tool == QUESTION_TOOL && *status == ApprovalStatus::Pending {
                    state = "needs an answer".to_owned();
                }
                if let Some(note) = unapplied(status, answer.as_ref()) {
                    state = format!("{state} · {note}");
                }
                let out = head_lines(
                    lead,
                    Span::styled("?", style),
                    titled(marked(title_spans(tool, summary), mark)),
                    vec![Span::styled(state, style)],
                    width,
                );
                (out, style)
            }
        };
        for section in sections(entry) {
            if expanded || !section.hidden {
                let (lines, matched) = self.section(&section, expanded, gutter, mark);
                if let Some(row) = matched {
                    self.matched.set(Some(out.len() + row));
                }
                out.extend(lines);
            }
        }
        out
    }

    /// A message: a bar in its author's colour down its every row, where a
    /// tool item has its glyph, so messages stand out among the calls;
    /// then `label` and the first line of `text`, every further or wrapped
    /// line under the label.
    fn message(
        &self,
        mut lead: Vec<Span<'static>>,
        label: Vec<Span<'static>>,
        bar: Color,
        text: &str,
        mark: Option<Mark>,
    ) -> Vec<Line<'static>> {
        let bar = Span::styled("▌ ", Style::new().fg(bar));
        let rest = vec![indent(LEAD), bar.clone()];
        lead.push(bar);
        let mut lines = text.split('\n');
        let mut content = label;
        content.extend(marked(
            vec![Span::raw(clean(lines.next().unwrap_or_default()))],
            mark,
        ));
        let mut out = wrap(lead, rest.clone(), content, self.width);
        for line in lines {
            out.extend(wrap(
                rest.clone(),
                rest.clone(),
                marked(vec![Span::raw(clean(line))], mark),
                self.width,
            ));
        }
        out
    }

    /// One section under the gutter: its preview's head and what more there
    /// is while collapsed; all of it (the blob once here) while expanded.
    /// Also the row the search's current match landed on, if it is in this
    /// section's blob.
    fn section(
        &self,
        section: &Section,
        expanded: bool,
        gutter: Style,
        mark: Option<Mark>,
    ) -> (Vec<Line<'static>>, Option<usize>) {
        let preview = content_lines(section.text);
        let note: Option<Span<'static>>;
        // Lines of the blob left out above what is shown.
        let mut skipped = 0;
        // Which content line the current match is on.
        let mut matched = None;
        let content: Vec<String> = if expanded {
            match section.blob().map(|blob| (blob.id, &blob.state)) {
                Some((id, BlobState::Available { path })) => {
                    let line = self
                        .current_line
                        .filter(|line| line.id == id)
                        .map(|line| line.line);
                    match self.blob_part(path, id, line) {
                        Ok(part) => {
                            note = part.cut.map(|cut| Span::raw(cut).dark_gray());
                            skipped = part.from;
                            matched = line
                                .and_then(|line| line.checked_sub(part.from))
                                .filter(|&at| at < part.lines.len());
                            part.lines
                        }
                        Err(err) => {
                            note = Some(
                                Span::raw(format!("cannot read {}: {err}", path.display())).red(),
                            );
                            preview.into_iter().map(clean).collect()
                        }
                    }
                }
                Some((_, state)) => {
                    note = blob_note(state);
                    preview.into_iter().map(clean).collect()
                }
                None => match section.inline() {
                    // Capped like a downloaded blob's head.
                    Some(whole) => {
                        let shown = head_of(whole, SHOW_BYTES as usize);
                        let lines = content_lines(shown);
                        note = (lines.len() > SHOW_LINES || shown.len() < whole.len()).then(|| {
                            Span::raw(format!(
                                "… cut short: {} of {} shown",
                                if lines.len() > SHOW_LINES {
                                    format!("{SHOW_LINES} lines")
                                } else {
                                    bytes(SHOW_BYTES)
                                },
                                bytes(whole.len() as u64)
                            ))
                            .dark_gray()
                        });
                        lines.into_iter().take(SHOW_LINES).map(clean).collect()
                    }
                    None => {
                        note = section.note().map(|note| Span::raw(note).yellow());
                        preview.into_iter().map(clean).collect()
                    }
                },
            }
        } else if preview.is_empty() && !section.message {
            // Nothing worth a line, e.g. a write's output beside its status;
            // `lines` then counts what the agent was told, not content. A
            // long message's whole still says where it stands, under the
            // head the message shows.
            return (Vec::new(), None);
        } else {
            let shown: Vec<String> = preview
                .iter()
                .take(PREVIEW_LINES)
                .map(|line| clean(line))
                .collect();
            let whole = section.lines.map_or(preview.len(), |lines| lines as usize);
            let mut more = Vec::new();
            if whole > shown.len() {
                let rest = whole - shown.len();
                more.push(format!(
                    "+{rest} {}",
                    if rest == 1 { "line" } else { "lines" }
                ));
            }
            if let Some(blob) = section.blob() {
                let state = collapsed_download(&blob.state);
                more.push(match blob.state {
                    BlobState::Failed { .. } => format!("{state} · d retries"),
                    _ => state,
                });
                more.extend(change_note(blob));
                more.extend(blob.pending.as_ref().map(pending_note));
            }
            note = match section.note() {
                // Said in full and in colour: the preview is all there is.
                Some(lost) => {
                    more.push(lost.into_owned());
                    Some(Span::raw(format!("… {}", more.join(" · "))).yellow())
                }
                None if !more.is_empty() => {
                    Some(Span::raw(format!("… {}", more.join(" · "))).dark_gray())
                }
                None => None,
            };
            shown
        };

        let bar = || vec![indent(LEAD), Span::styled("│ ", gutter)];
        let mut out: Vec<Line<'static>> = Vec::new();
        if let Some(label) = section.label() {
            out.extend(wrap(
                bar(),
                bar(),
                vec![Span::raw(label).yellow()],
                self.width,
            ));
        }
        if skipped > 0 {
            let above = format!(
                "… {skipped} {} above",
                if skipped == 1 { "line" } else { "lines" }
            );
            out.extend(wrap(
                bar(),
                bar(),
                vec![Span::raw(above).dark_gray()],
                self.width,
            ));
        }
        let mut row = None;
        for (i, rows) in content_rows(section, content, skipped, &bar, self.width, mark)
            .into_iter()
            .enumerate()
        {
            if matched == Some(i) {
                row = Some(out.len());
            }
            out.extend(rows);
        }
        // Expanded, what the server did to the blob and a crop or delete
        // still pending stay said, as they are collapsed, after the note.
        let facts: Vec<String> = section
            .blob()
            .filter(|_| expanded)
            .map(|blob| {
                change_note(blob)
                    .into_iter()
                    .chain(blob.pending.as_ref().map(pending_note))
                    .collect()
            })
            .unwrap_or_default();
        let note: Vec<Span<'static>> = match (note, facts.is_empty()) {
            (note, true) => note.into_iter().collect(),
            (Some(note), false) => {
                vec![
                    note,
                    Span::raw(format!(" · {}", facts.join(" · "))).dark_gray(),
                ]
            }
            (None, false) => vec![Span::raw(format!("… {}", facts.join(" · "))).dark_gray()],
        };
        if !note.is_empty() {
            out.extend(wrap(bar(), bar(), note, self.width));
        }
        (out, row)
    }

    /// What an expanded section shows of its downloaded blob: its head, or,
    /// when the search's current match is on a `line` past that, the part
    /// from a few lines above it, out of what the search read.
    fn blob_part(&self, path: &Path, id: BlobId, line: Option<usize>) -> Result<BlobPart, String> {
        let searched = line
            .zip(self.search.and_then(|search| search.blob_text(id)))
            .filter(|(line, searched)| {
                let head = content_lines(head_of(&searched.text, SHOW_BYTES as usize));
                *line >= head.len().min(SHOW_LINES)
            });
        let Some((line, searched)) = searched else {
            self.used.borrow_mut().push(id);
            let mut heads = self.heads.borrow_mut();
            if heads.read.get(&id).is_none_or(|head| head.path != path) {
                self.reads.set(self.reads.get() + 1);
                let head = Head {
                    path: path.to_owned(),
                    head: read_head(path, SHOW_BYTES).map_err(|err| err.to_string()),
                };
                heads.read.insert(id, head);
            }
            let (text, size) = heads.read[&id].head.as_ref().map_err(String::clone)?;
            let size = *size;
            let lines = content_lines(text);
            let cut = (lines.len() > SHOW_LINES || size > SHOW_BYTES).then(|| {
                format!(
                    "… cut short: {} of {} shown; the whole file is {}",
                    if lines.len() > SHOW_LINES {
                        format!("{SHOW_LINES} lines")
                    } else {
                        bytes(SHOW_BYTES)
                    },
                    bytes(size),
                    path.display()
                )
            });
            return Ok(BlobPart {
                lines: lines.into_iter().take(SHOW_LINES).map(clean).collect(),
                from: 0,
                cut,
            });
        };
        let text: &str = &searched.text;
        let starts: Vec<usize> = std::iter::once(0)
            .chain(text.match_indices('\n').map(|(at, _)| at + 1))
            .collect();
        // A few lines above the match, as long as it still fits.
        let mut from = line.saturating_sub(MATCH_ABOVE);
        while from < line && starts[line] - starts[from] > SHOW_BYTES as usize / 2 {
            from += 1;
        }
        let rest = &text[starts[from]..];
        let shown = head_of(rest, SHOW_BYTES as usize);
        let lines = content_lines(shown);
        let cut =
            (lines.len() > SHOW_LINES || shown.len() < rest.len() || searched.cut).then(|| {
                let size = std::fs::metadata(path).map_or(String::new(), |m| bytes(m.len()));
                format!(
                    "… cut short: lines {}–{} of {size} shown; the whole file is {}",
                    from + 1,
                    from + lines.len().min(SHOW_LINES),
                    path.display()
                )
            });
        Ok(BlobPart {
            lines: lines.into_iter().take(SHOW_LINES).map(clean).collect(),
            from,
            cut,
        })
    }
}

/// The lines an expanded section shows of a blob.
struct BlobPart {
    lines: Vec<String>,
    /// Index of the first line shown; lines above it are left out.
    from: usize,
    /// What was left out below, if anything.
    cut: Option<String>,
}

/// Where a blob that is not here yet stands, in place of its content:
/// not downloaded (`d` downloads it), waiting, how far its download got,
/// or why it failed (`d` retries). `None` once it is here.
pub(super) fn blob_note(state: &BlobState) -> Option<Span<'static>> {
    let words = download_state(state);
    Some(match state {
        BlobState::Available { .. } => return None,
        BlobState::NotDownloaded { .. } => {
            Span::raw(format!("{words} · press d to download")).dark_gray()
        }
        BlobState::Requested { .. } | BlobState::Downloading { paused: None, .. } => {
            Span::raw(words).cyan()
        }
        BlobState::Downloading {
            paused: Some(_), ..
        } => Span::raw(words).yellow(),
        BlobState::Failed { .. } => Span::raw(format!("{words} · d retries")).red(),
    })
}

/// The first `max` bytes of `text`, cut back to a char boundary.
fn head_of(text: &str, max: usize) -> &str {
    let mut end = max.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// A section's head as the approval panel shows it: its preview's lines
/// under a gutter, at most `max_rows` rows of them (at least one), then
/// what was left out and that `v` shows all.
pub(super) fn preview_lines(
    section: &Section,
    width: usize,
    max_rows: usize,
) -> Vec<Line<'static>> {
    let preview = content_lines(section.text);
    let bar = || vec![Span::styled("│ ", Style::new().yellow())];
    let content = preview.iter().map(|line| clean(line)).collect();
    let max_rows = max_rows.max(1);
    let mut out = Vec::new();
    let mut shown = 0;
    for rows in content_rows(section, content, 0, &bar, width, None) {
        if out.len() + rows.len() > max_rows {
            // A line taller than the cap shows its head.
            if shown == 0 {
                out.extend(rows.into_iter().take(max_rows));
            }
            break;
        }
        out.extend(rows);
        shown += 1;
    }
    let whole = section
        .lines
        .map_or(preview.len(), |lines| lines as usize)
        .max(preview.len());
    let rest = whole - shown;
    if rest > 0 || section.blob().is_some() {
        let mut more = match rest {
            0 => String::new(),
            1 => "+1 line · ".to_owned(),
            _ => format!("+{rest} lines · "),
        };
        if let Some(size) = section.blob().and_then(|blob| blob_size(&blob.state)) {
            more.push_str(&format!("full {} · ", bytes(size)));
        }
        out.extend(wrap(
            bar(),
            bar(),
            vec![Span::raw(format!("… {more}v shows all")).dark_gray()],
            width,
        ));
    }
    out
}

/// Each content line's rows after `bar`: numbered (from `skipped` + 1) if
/// the section is, coloured by [`line_style`], search matches marked,
/// wrapped to `width`.
fn content_rows(
    section: &Section,
    content: Vec<String>,
    skipped: usize,
    bar: &dyn Fn() -> Vec<Span<'static>>,
    width: usize,
    mark: Option<Mark>,
) -> Vec<Vec<Line<'static>>> {
    let digits = if section.numbered {
        let last = section
            .lines
            .map_or(0, |lines| lines as usize)
            .max(skipped + content.len());
        last.max(1).to_string().len()
    } else {
        0
    };
    content
        .into_iter()
        .enumerate()
        .map(|(i, line)| {
            let mut first = bar();
            let mut rest = bar();
            if section.numbered {
                first.push(Span::raw(format!("{:>digits$} ", skipped + i + 1)).dark_gray());
                rest.push(indent(digits + 1));
            }
            let style = line_style(section, &line);
            wrap(
                first,
                rest,
                marked(vec![Span::styled(line, style)], mark),
                width,
            )
        })
        .collect()
}

/// A content line's colour: diff lines by their sign, an error's in red.
fn line_style(section: &Section, line: &str) -> Style {
    if section.error {
        return Style::new().red();
    }
    if section.format != BodyFormat::Diff {
        return Style::new();
    }
    if line.starts_with("+++") || line.starts_with("---") {
        Style::new().bold()
    } else if line.starts_with('+') {
        Style::new().green()
    } else if line.starts_with('-') {
        Style::new().red()
    } else if line.starts_with("@@") {
        Style::new().cyan()
    } else {
        Style::new()
    }
}

/// The lines of some content; a final line break ends the last line, it
/// does not start one. Nothing for empty content.
pub(super) fn content_lines(text: &str) -> Vec<&str> {
    let text = text.strip_suffix('\n').unwrap_or(text);
    if text.is_empty() {
        return Vec::new();
    }
    text.split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect()
}

/// A tool kind's glyph.
pub(super) fn glyph(kind: ToolKind) -> &'static str {
    match kind {
        ToolKind::FileWrite => "✎",
        ToolKind::FileEdit => "±",
        ToolKind::FileRead => "≡",
        // Not `$`: shell titles already start with one.
        ToolKind::Shell => "❯",
        ToolKind::Search => "⌕",
        ToolKind::Fetch => "⇣",
        ToolKind::Subagent => "»",
        ToolKind::Other => "•",
    }
}

/// The tool's name, then the first line of its title.
pub(super) fn title_spans(name: &str, title: &str) -> Vec<Span<'static>> {
    vec![
        Span::raw(clean(name)).bold(),
        Span::raw(" "),
        Span::raw(clean(title.split('\n').next().unwrap_or_default())),
    ]
}

/// A head line: `lead`, `glyph`, then `content` with `state` (may be
/// empty) right-aligned on the first row; wrapped rows hang under the
/// content.
pub(super) fn head_lines(
    lead: Vec<Span<'static>>,
    glyph: Span<'static>,
    content: Vec<Span<'static>>,
    state: Vec<Span<'static>>,
    width: usize,
) -> Vec<Line<'static>> {
    let mut first = lead;
    first.push(glyph);
    first.push(Span::raw(" "));
    let column = spans_width(&first);
    let state_width = spans_width(&state);
    let reserve = if state_width > 0 { state_width + 2 } else { 0 };
    let rows = rows(
        &content,
        width.saturating_sub(column + reserve).max(MIN_TITLE),
        width.saturating_sub(column).max(1),
    );
    let mut out = Vec::with_capacity(rows.len());
    for (i, row) in rows.into_iter().enumerate() {
        let used = column + spans_width(&row);
        let mut spans = if i == 0 {
            first.clone()
        } else {
            vec![indent(column)]
        };
        spans.extend(row);
        if i == 0 && state_width > 0 {
            let pad = width.saturating_sub(used + state_width).max(2);
            spans.push(Span::raw(" ".repeat(pad)));
            spans.extend(state.iter().cloned());
        }
        out.push(Line::from(spans));
    }
    out
}

/// A tool call's detail line, dim, at `column`, search matches marked.
pub(super) fn detail_lines(
    detail: &str,
    column: usize,
    width: usize,
    mark: Option<Mark>,
) -> Vec<Line<'static>> {
    wrap(
        vec![indent(column)],
        vec![indent(column)],
        marked(
            vec![Span::raw(clean(detail.split('\n').next().unwrap_or_default())).dark_gray()],
            mark,
        ),
        width,
    )
}

/// How a grouped call stands, in words and its state's colour. A denial or
/// cancellation is said once, by the outcome.
fn tool_state(view: &ToolView, wall_ms: i64) -> (String, Style) {
    let approval = view.approval.as_ref().map(|a| &a.status);
    let answered = match approval {
        Some(ApprovalStatus::Resolved { decision, by, rule }) => {
            Some(resolved_label(*decision, by, rule.as_ref()))
        }
        Some(ApprovalStatus::Answered { by }) => Some(format!("answered {}", where_(by))),
        Some(ApprovalStatus::Cancelled { .. }) => Some("approval cancelled".to_owned()),
        Some(ApprovalStatus::Pending) | None => None,
    };
    let asks = view.approval.as_ref().is_some_and(|a| a.question);
    // The subagent's own state, not the launch's: a background subagent's
    // call is done long before it ends.
    if let Some(status) = &view.subagent
        && matches!(
            view.outcome,
            ToolOutcome::Running { .. }
                | ToolOutcome::Done {
                    is_error: false,
                    ..
                }
        )
        && approval != Some(&ApprovalStatus::Pending)
    {
        let style = if status.live {
            Style::new().cyan()
        } else {
            Style::new().green()
        };
        return (subagent_status(status), style);
    }
    match &view.outcome {
        ToolOutcome::Running { .. } if approval == Some(&ApprovalStatus::Pending) => {
            let words = if asks {
                "needs an answer"
            } else {
                "needs approval"
            };
            (words.to_owned(), Style::new().yellow().bold())
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
            (parts.join(" · "), Style::new().cyan())
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
            let style = if *is_error {
                Style::new().red()
            } else {
                Style::new().green()
            };
            (parts.join(" · "), style)
        }
        ToolOutcome::Denied { by } => (
            format!("{} {}", decision_label(Decision::Deny), where_(by)),
            Style::new().magenta(),
        ),
        ToolOutcome::Cancelled { reason } => {
            (format!("cancelled: {reason}"), Style::new().dark_gray())
        }
        ToolOutcome::NoResult => {
            let parts: Vec<String> = answered
                .into_iter()
                .chain(["no result".to_owned()])
                .collect();
            (parts.join(" · "), Style::new().dark_gray())
        }
    }
}

/// Whether an item's lines change with the wall clock: those of a call
/// [`tool_state`] says is running, with its elapsed time.
fn ticks(item: &TimelineItem) -> bool {
    match item {
        TimelineItem::Logged {
            entry:
                Entry::Tool(view)
                | Entry::Imported {
                    entry: ImportedEntry::Tool(view),
                    ..
                },
            ..
        } => {
            matches!(view.outcome, ToolOutcome::Running { .. })
                && view.subagent.is_none()
                && !matches!(
                    view.approval.as_ref().map(|a| &a.status),
                    Some(ApprovalStatus::Pending)
                )
        }
        _ => false,
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

/// A standalone approval's state, in words and colour.
pub(super) fn approval_state(status: &ApprovalStatus) -> (String, Style) {
    match status {
        ApprovalStatus::Pending => ("needs approval".to_owned(), Style::new().yellow().bold()),
        ApprovalStatus::Resolved { decision, by, rule } => (
            resolved_label(*decision, by, rule.as_ref()),
            match decision {
                Decision::Approve => Style::new().green(),
                Decision::Deny => Style::new().magenta(),
            },
        ),
        ApprovalStatus::Answered { by } => {
            (format!("answered {}", where_(by)), Style::new().green())
        }
        ApprovalStatus::Cancelled { reason } => {
            (format!("cancelled: {reason}"), Style::new().dark_gray())
        }
    }
}

/// How an approval was answered and where, with the rule it granted:
/// `approved always on <device> · <rule>`.
fn resolved_label(decision: Decision, by: &DeviceRef, rule: Option<&RuleSuggestion>) -> String {
    let words = answer_words(decision, rule);
    let rule = rule.map_or_else(String::new, |rule| format!(" · {}", clean(&rule.label)));
    format!("{words} {}{rule}", where_(by))
}

/// Who answered an approval: "here" or the other device.
fn where_(by: &DeviceRef) -> String {
    if by.me {
        "here".to_owned()
    } else {
        format!("on {}", device_label(by))
    }
}

/// Milliseconds since the Unix epoch.
pub(super) fn wall_ms(wall: SystemTime) -> i64 {
    wall.duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// "you [state]: " for this device's messages.
fn you(outgoing: Option<&Outgoing>) -> Vec<Span<'static>> {
    let mut spans = vec![Span::raw("you").blue().bold()];
    if let Some(outgoing) = outgoing {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("[{}]", outgoing_label(outgoing)),
            outgoing_badge(outgoing).1,
        ));
    }
    spans.push(Span::raw(": "));
    spans
}

fn indent(columns: usize) -> Span<'static> {
    Span::raw(" ".repeat(columns))
}

fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(Span::width).sum()
}

/// `content` in rows of at most `width` columns, each after its prefix:
/// `first` on the first row, `rest` on the others.
pub(super) fn wrap(
    first: Vec<Span<'static>>,
    rest: Vec<Span<'static>>,
    content: Vec<Span<'static>>,
    width: usize,
) -> Vec<Line<'static>> {
    let rows = rows(
        &content,
        width.saturating_sub(spans_width(&first)).max(1),
        width.saturating_sub(spans_width(&rest)).max(1),
    );
    rows.into_iter()
        .enumerate()
        .map(|(i, row)| {
            let mut spans = if i == 0 { first.clone() } else { rest.clone() };
            spans.extend(row);
            Line::from(spans)
        })
        .collect()
}

/// Breaks `content` into rows: `first` columns wide, then `rest`. Breaks
/// after the last space that fits, else mid-word; the space at a break goes.
/// Only the row being filled is kept char by char.
fn rows(content: &[Span<'static>], first: usize, rest: usize) -> Vec<Vec<Span<'static>>> {
    let mut rows: Vec<Vec<Span<'static>>> = Vec::new();
    let mut row: Vec<(char, Style)> = Vec::new();
    let mut used = 0;
    let mut space: Option<usize> = None;
    let cells = content
        .iter()
        .flat_map(|span| span.content.chars().map(move |c| (c, span.style)));
    for (c, style) in cells {
        let w = c.width().unwrap_or(0);
        let limit = if rows.is_empty() { first } else { rest };
        if used + w > limit && !row.is_empty() {
            let carry = match space {
                Some(at) if at > 0 => {
                    let carry = row.split_off(at + 1);
                    row.pop();
                    carry
                }
                _ => Vec::new(),
            };
            rows.push(group(std::mem::replace(&mut row, carry)));
            used = row.iter().map(|(c, _)| c.width().unwrap_or(0)).sum();
            space = row.iter().rposition(|(c, _)| *c == ' ');
            if c == ' ' && row.is_empty() {
                continue;
            }
        }
        if c == ' ' {
            space = Some(row.len());
        }
        row.push((c, style));
        used += w;
    }
    rows.push(group(row));
    rows
}

/// Runs of one style as spans.
fn group(cells: Vec<(char, Style)>) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut text = String::new();
    let mut current: Option<Style> = None;
    for (c, style) in cells {
        if current.is_some_and(|s| s != style) {
            spans.push(Span::styled(
                std::mem::take(&mut text),
                current.unwrap_or_default(),
            ));
        }
        current = Some(style);
        text.push(c);
    }
    if let Some(style) = current {
        spans.push(Span::styled(text, style));
    }
    spans
}

/// Another device, by its name or the end of its id, safe for the grid.
/// A context divider: what follows went to another context.
fn context_divider(mut lead: Vec<Span<'static>>, what: &str) -> Vec<Line<'static>> {
    lead.push(Span::raw(format!("── {} ──", clean(what))).cyan());
    vec![Line::from(lead)]
}

pub(super) fn device_label(device: &DeviceRef) -> String {
    clean(&device.label())
}
