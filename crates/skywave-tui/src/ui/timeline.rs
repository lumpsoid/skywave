//! Timeline items as screen lines. Text is laid out here rather than by
//! ratatui's wrapping, so a wrapped line hangs under its own column and
//! every item's height is known exactly (scroll anchoring needs it).
//!
//! A tool item is one block: its kind's glyph, the tool name and title, the
//! state right-aligned, an optional detail line, and its content under a
//! gutter: at most [`PREVIEW_LINES`] lines collapsed, the whole blob
//! expanded. Colour carries the state (the glyph, the state and the
//! gutter), never the item's type.

use std::cell::Cell;
use std::io;
use std::ops::Range;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use skywave_core::search::BlobLine;
use skywave_core::{
    ApprovalStatus, BlobId, BlobState, BodyFormat, Decision, Delivery, DeviceId, Entry, Outgoing,
    Query, RuleSuggestion, SessionView, TimelineItem, ToolKind, ToolOutcome, ToolView, read_head,
};
use unicode_width::UnicodeWidthChar;

use super::{answer_words, decision_label, outgoing_badge, outgoing_label};
use crate::app::{Search, SessionUi};
use crate::timeline::{
    ItemKey, PREVIEW_LINES, Section, delivery, find, sections, selectable, starts_turn,
};

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

/// Every timeline item's lines at `width` columns, in timeline order, with
/// the search's matches marked. `wall` is what running calls' elapsed time
/// and a selected own message's wait count to. Also where the search's
/// current match is in its blob, if that line is laid out: the item's
/// timeline index and the line within the item.
pub(super) fn item_lines(
    session: &SessionView,
    ui: &SessionUi,
    focused: bool,
    width: usize,
    wall: SystemTime,
) -> (Vec<Vec<Line<'static>>>, Option<(usize, usize)>) {
    let current_line = ui
        .search
        .as_ref()
        .and_then(|search| search.current_line(session));
    let layout = Layout {
        ui,
        focused,
        width: width.max(TITLE + MIN_TITLE),
        wall_ms: wall_ms(wall),
        rule: rule_index(session),
        search: ui.search.as_ref(),
        current: ui
            .search
            .as_ref()
            .and_then(|search| find(session, search.current?)),
        current_line: current_line.map(|(_, line)| line),
        matched: Cell::new(None),
    };
    let mut minute = None;
    let mut matched = None;
    let items = session
        .timeline
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let lines = layout.item(index, item, &mut minute);
            if let Some(row) = layout.matched.take()
                && current_line.is_some_and(|(at, _)| at == index)
            {
                matched = Some((index, row));
            }
            lines
        })
        .collect();
    (items, matched)
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
    /// One item: the `── new ──` rule if the news starts here, else a
    /// separator if it starts a turn, then its lines. The time shows only
    /// when it differs from the last one shown (`minute`).
    fn item(
        &self,
        index: usize,
        item: &TimelineItem,
        minute: &mut Option<String>,
    ) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        if self.rule == Some(index) {
            let rest = self.width.saturating_sub(7);
            out.push(Line::from(vec![
                Span::raw("── new ").magenta().bold(),
                Span::raw("─".repeat(rest)).magenta(),
            ]));
        } else if index > 0 && starts_turn(item) {
            out.push(Line::from("─".repeat(self.width)).dark_gray());
        }
        let key = selectable(item);
        let selected = self.focused && key.is_some() && key == self.ui.selected;
        let mark = self.search.and_then(|search| {
            search.hit(index).map(|_| Mark {
                query: &search.query,
                current: self.current == Some(index),
            })
        });
        let expanded = matches!(key, Some(ItemKey::Logged(seq)) if self.ui.expanded.contains(&seq));
        let marker = if selected {
            Span::raw("> ").bold().reversed()
        } else {
            Span::raw("  ")
        };
        let time = match item {
            TimelineItem::Logged { at, .. } => {
                let this = at.format("%Y-%m-%d %H:%M").to_string();
                if minute.as_ref() == Some(&this) {
                    Span::raw("      ")
                } else {
                    *minute = Some(this);
                    Span::raw(at.format("%H:%M ").to_string()).dark_gray()
                }
            }
            TimelineItem::Pending { .. } => Span::raw("      "),
        };
        let lead = vec![marker, time];
        let above = out.len();
        out.extend(match item {
            TimelineItem::Pending { text, outgoing, .. } => {
                self.message(lead, you(Some(outgoing)), text, mark)
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
                vec![Span::raw(delivery_label(&delivery, &outgoing, self.wall_ms)).dark_gray()],
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
        let (mut out, gutter) = match entry {
            Entry::UserMessage {
                text,
                from,
                own,
                outgoing,
                ..
            } => {
                let label = if *own {
                    you(outgoing.as_ref())
                } else {
                    vec![
                        Span::raw(device_label(*from)).magenta().bold(),
                        Span::raw(": "),
                    ]
                };
                return self.message(lead, label, text, mark);
            }
            Entry::AgentMessage { text } => {
                return self.message(lead, vec![Span::raw("agent: ").green().bold()], text, mark);
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
            Entry::Renamed { name, by, own } => {
                let who = if *own {
                    "you".to_owned()
                } else {
                    device_label(*by)
                };
                let what = match name {
                    Some(name) => format!("renamed to “{}” by {who}", clean(name)),
                    None => format!("default title restored by {who}"),
                };
                let mut spans = lead;
                spans.push(Span::raw(what).dark_gray());
                return vec![Line::from(spans)];
            }
            Entry::Archived { archived, by, own } => {
                let who = if *own {
                    "you".to_owned()
                } else {
                    device_label(*by)
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
                    message,
                    mark,
                );
            }
            Entry::Tool(view) => {
                let (state, style) = tool_state(view, self.wall_ms);
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
            }
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
            Entry::ToolResult { .. } => (
                head_lines(
                    lead,
                    Span::raw("↳"),
                    titled(vec![Span::raw("result").bold()]),
                    Vec::new(),
                    width,
                ),
                Style::new().dark_gray(),
            ),
            Entry::Approval {
                tool,
                summary,
                status,
                ..
            } => {
                let (state, style) = approval_state(status);
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

    /// A message: `label` and the first line of `text` after the lead, every
    /// further or wrapped line under the label.
    fn message(
        &self,
        lead: Vec<Span<'static>>,
        label: Vec<Span<'static>>,
        text: &str,
        mark: Option<Mark>,
    ) -> Vec<Line<'static>> {
        let mut lines = text.split('\n');
        let mut content = label;
        content.extend(marked(
            vec![Span::raw(clean(lines.next().unwrap_or_default()))],
            mark,
        ));
        let mut out = wrap(lead, vec![indent(LEAD)], content, self.width);
        for line in lines {
            out.extend(wrap(
                vec![indent(LEAD)],
                vec![indent(LEAD)],
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
        let mut note: Option<Span<'static>> = None;
        // Lines of the blob left out above what is shown.
        let mut skipped = 0;
        // Which content line the current match is on.
        let mut matched = None;
        let content: Vec<String> = if expanded {
            match section.blob.map(|blob| (blob.id, &blob.state)) {
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
                Some((_, BlobState::NotDownloaded { size })) => {
                    note = Some(
                        Span::raw(format!(
                            "not downloaded · {} · press d to download",
                            bytes(*size)
                        ))
                        .dark_gray(),
                    );
                    preview.into_iter().map(clean).collect()
                }
                Some((_, BlobState::Requested { size })) => {
                    note = Some(
                        Span::raw(format!("requested · waiting for a slot · {}", bytes(*size)))
                            .cyan(),
                    );
                    preview.into_iter().map(clean).collect()
                }
                Some((_, BlobState::Downloading { got, size, paused })) => {
                    let progress = format!(
                        "downloading · {} of {} ({}%)",
                        bytes(*got),
                        bytes(*size),
                        percent(*got, *size)
                    );
                    note = Some(if *paused {
                        Span::raw(format!("{progress} · paused, offline")).yellow()
                    } else {
                        Span::raw(progress).cyan()
                    });
                    preview.into_iter().map(clean).collect()
                }
                Some((_, BlobState::Failed { size, reason })) => {
                    note = Some(
                        Span::raw(format!(
                            "download failed: {reason} · {} · d retries",
                            bytes(*size)
                        ))
                        .red(),
                    );
                    preview.into_iter().map(clean).collect()
                }
                None => preview.into_iter().map(clean).collect(),
            }
        } else if preview.is_empty() {
            // Nothing worth a line, e.g. a write's output beside its status;
            // `lines` then counts what the agent was told, not content.
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
            if let Some(blob) = section.blob {
                more.push(match &blob.state {
                    BlobState::NotDownloaded { size } => {
                        format!("full {} · not downloaded", bytes(*size))
                    }
                    BlobState::Requested { size } => {
                        format!("full {} · requested", bytes(*size))
                    }
                    BlobState::Downloading { got, size, paused } => {
                        let progress = format!("downloading {}%", percent(*got, *size));
                        if *paused {
                            format!("{progress} · paused, offline")
                        } else {
                            progress
                        }
                    }
                    BlobState::Failed { size, .. } => {
                        format!("full {} · download failed · d retries", bytes(*size))
                    }
                    BlobState::Available { .. } => "downloaded".to_owned(),
                });
            }
            if !more.is_empty() {
                note = Some(Span::raw(format!("… {}", more.join(" · "))).dark_gray());
            }
            shown
        };

        let bar = || vec![indent(LEAD), Span::styled("│ ", gutter)];
        let mut out: Vec<Line<'static>> = Vec::new();
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
        if let Some(note) = note {
            out.extend(wrap(bar(), bar(), vec![note], self.width));
        }
        (out, row)
    }

    /// What an expanded section shows of its downloaded blob: its head, or,
    /// when the search's current match is on a `line` past that, the part
    /// from a few lines above it, out of what the search read.
    fn blob_part(&self, path: &Path, id: BlobId, line: Option<usize>) -> io::Result<BlobPart> {
        let searched = line
            .zip(self.search.and_then(|search| search.blob_text(id)))
            .filter(|(line, searched)| {
                let head = content_lines(head_of(&searched.text, SHOW_BYTES as usize));
                *line >= head.len().min(SHOW_LINES)
            });
        let Some((line, searched)) = searched else {
            let (text, size) = read_head(path, SHOW_BYTES)?;
            let lines = content_lines(&text);
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

/// The first `max` bytes of `text`, cut back to a char boundary.
fn head_of(text: &str, max: usize) -> &str {
    let mut end = max.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// While collapsed, the state of an item's blob when no section on screen
/// carries one (a hidden input or output, an empty preview), so `d` on it
/// shows what it did: `input 4.2 KiB`, `output 4.2 KiB · requested`.
fn unseen_blob(entry: &Entry) -> Option<String> {
    let sections = sections(entry);
    let shown = |s: &Section| !s.hidden && !content_lines(s.text).is_empty();
    if sections.iter().any(|s| shown(s) && s.blob.is_some()) {
        return None;
    }
    let section = sections.iter().find(|s| s.blob.is_some())?;
    let what = match (section.input, section.hidden) {
        (true, _) => "input",
        (false, true) => "output",
        (false, false) => "full",
    };
    let state = match &section.blob?.state {
        BlobState::NotDownloaded { size } => bytes(*size),
        BlobState::Requested { size } => format!("{} · requested", bytes(*size)),
        BlobState::Downloading { got, size, paused } => format!(
            "{} · downloading {}%{}",
            bytes(*size),
            percent(*got, *size),
            if *paused { " · paused, offline" } else { "" }
        ),
        BlobState::Failed { size, .. } => format!("{} · download failed", bytes(*size)),
        BlobState::Available { .. } => return None,
    };
    Some(format!("{what} {state}"))
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
    if rest > 0 || section.blob.is_some() {
        let mut more = match rest {
            0 => String::new(),
            1 => "+1 line · ".to_owned(),
            _ => format!("+{rest} lines · "),
        };
        if let Some(size) = section.blob.and_then(|blob| blob_size(&blob.state)) {
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
fn content_lines(text: &str) -> Vec<&str> {
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
        Some(ApprovalStatus::Resolved {
            decision,
            by,
            by_me,
            rule,
        }) => Some(resolved_label(*decision, *by, *by_me, rule.as_ref())),
        Some(ApprovalStatus::Cancelled { .. }) => Some("approval cancelled".to_owned()),
        Some(ApprovalStatus::Pending) | None => None,
    };
    match &view.outcome {
        ToolOutcome::Running { .. } if approval == Some(&ApprovalStatus::Pending) => {
            ("needs approval".to_owned(), Style::new().yellow().bold())
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
        ToolOutcome::Denied { by, by_me } => (
            format!("{} {}", decision_label(Decision::Deny), where_(*by, *by_me)),
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

/// A standalone approval's state, in words and colour.
pub(super) fn approval_state(status: &ApprovalStatus) -> (String, Style) {
    match status {
        ApprovalStatus::Pending => ("needs approval".to_owned(), Style::new().yellow().bold()),
        ApprovalStatus::Resolved {
            decision,
            by,
            by_me,
            rule,
        } => (
            resolved_label(*decision, *by, *by_me, rule.as_ref()),
            match decision {
                Decision::Approve => Style::new().green(),
                Decision::Deny => Style::new().magenta(),
            },
        ),
        ApprovalStatus::Cancelled { reason } => {
            (format!("cancelled: {reason}"), Style::new().dark_gray())
        }
    }
}

/// How an approval was answered and where, with the rule it granted:
/// `approved always on <device> · <rule>`.
fn resolved_label(
    decision: Decision,
    by: DeviceId,
    by_me: bool,
    rule: Option<&RuleSuggestion>,
) -> String {
    let words = answer_words(decision, rule);
    let rule = rule.map_or_else(String::new, |rule| format!(" · {}", clean(&rule.label)));
    format!("{words} {}{rule}", where_(by, by_me))
}

/// Who answered an approval: "here" or the other device.
fn where_(by: DeviceId, by_me: bool) -> String {
    if by_me {
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

/// How an own command went out, all by this device's clock. Once answered,
/// what each step took from the queueing: `queued 14:02:11 · sent +3.2s ·
/// 2 tries · answered +8.1s`. Until then, how long it has waited and where
/// it is: `queued 2m10s ago · sending, try 3`, or `… · sent once, waiting
/// for the server` (sent, so it can no longer be taken back).
pub(super) fn delivery_label(delivery: &Delivery, outgoing: &Outgoing, wall_ms: i64) -> String {
    let queued = delivery.queued_at;
    // From the queueing to `ms`; never negative, should the clock step back.
    let since =
        |ms: i64| Duration::from_millis(u64::try_from(ms - queued.timestamp_millis()).unwrap_or(0));
    let tries = delivery.tries;
    let mut parts = Vec::new();
    match delivery.answered_at {
        Some(answered) => {
            parts.push(format!("queued {}", queued.format("%H:%M:%S")));
            match delivery.first_sent_at {
                Some(sent) => {
                    parts.push(format!(
                        "sent +{}",
                        took_label(since(sent.timestamp_millis()))
                    ));
                }
                // Sent before send times were recorded.
                None if tries > 0 => parts.push("sent".to_owned()),
                None => {}
            }
            if tries > 1 {
                parts.push(format!("{tries} tries"));
            }
            let verb = match outgoing {
                Outgoing::Failed(_) => "refused",
                _ => "answered",
            };
            parts.push(format!(
                "{verb} +{}",
                took_label(since(answered.timestamp_millis()))
            ));
        }
        None => {
            parts.push(format!("queued {} ago", seconds(since(wall_ms))));
            match (outgoing, tries) {
                (Outgoing::Sending, 0 | 1) => parts.push("sending".to_owned()),
                (Outgoing::Sending, n) => parts.push(format!("sending, try {n}")),
                (Outgoing::Queued, 0) => {}
                (Outgoing::Queued, 1) => parts.push("sent once, waiting for the server".to_owned()),
                (Outgoing::Queued, n) => {
                    parts.push(format!("sent {n} times, waiting for the server"));
                }
                (Outgoing::Accepted(_) | Outgoing::Failed(_), _) => {}
            }
        }
    }
    parts.join(" · ")
}

/// A call's duration: tenths below ten seconds.
fn took_label(took: Duration) -> String {
    if took < Duration::from_secs(10) {
        format!("{:.1}s", took.as_secs_f64())
    } else {
        seconds(took)
    }
}

/// Whole seconds, minutes or hours: "7s", "2m05s", "1h02m".
fn seconds(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
    }
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
fn wrap(
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
fn rows(content: &[Span<'static>], first: usize, rest: usize) -> Vec<Vec<Span<'static>>> {
    let mut rows: Vec<Vec<(char, Style)>> = Vec::new();
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
            rows.push(std::mem::replace(&mut row, carry));
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
    rows.push(row);
    rows.into_iter().map(group).collect()
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

/// Another device, by the end of its id: the start of a UUIDv7 is a
/// timestamp, the end is random.
pub(super) fn device_label(id: DeviceId) -> String {
    let id = id.0.simple().to_string();
    format!("device …{}", &id[id.len() - 8..])
}

/// Text made safe for a terminal cell grid: tabs become spaces, other
/// control characters go.
pub(super) fn clean(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    for c in line.chars() {
        match c {
            '\t' => out.push_str("    "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// A blob's size while it is not here; a downloaded blob is its file.
pub(super) fn blob_size(state: &BlobState) -> Option<u64> {
    match state {
        BlobState::NotDownloaded { size }
        | BlobState::Requested { size }
        | BlobState::Downloading { size, .. }
        | BlobState::Failed { size, .. } => Some(*size),
        BlobState::Available { .. } => None,
    }
}

pub(super) fn percent(got: u64, size: u64) -> u64 {
    (got.saturating_mul(100))
        .checked_div(size)
        .unwrap_or(100)
        .min(100)
}

/// A byte count in words: "812 B", "12.3 KiB", "4.0 MiB".
pub fn bytes(n: u64) -> String {
    const KIB: f64 = 1024.0;
    match n {
        0..1024 => format!("{n} B"),
        1024..1_048_576 => format!("{:.1} KiB", n as f64 / KIB),
        _ => format!("{:.1} MiB", n as f64 / (KIB * KIB)),
    }
}

/// A turn's length: "0.8s", "12.4s", "2m 05s", "1h 02m".
pub fn turn_time(ms: u64) -> String {
    let secs = ms / 1000;
    match secs {
        0..60 => format!("{:.1}s", ms as f64 / 1000.0),
        60..3600 => format!("{}m {:02}s", secs / 60, secs % 60),
        _ => format!("{}h {:02}m", secs / 3600, secs % 3600 / 60),
    }
}

/// A token count, compact: "812", "3.1k", "153k", "1.2M".
pub fn tokens(n: u64) -> String {
    let (value, unit) = match n {
        0..1000 => return n.to_string(),
        1000..999_500 => (n as f64 / 1e3, "k"),
        _ => (n as f64 / 1e6, "M"),
    };
    if value < 9.95 {
        format!("{value:.1}{unit}")
    } else {
        format!("{value:.0}{unit}")
    }
}

/// A cost in US dollars to 2–3 significant digits: "$0.0014", "$0.04",
/// "$0.87", "$1.27", "$12.3", "$140".
pub fn dollars(usd: f64) -> String {
    if usd.is_nan() || usd <= 0.0 {
        return "$0".to_owned();
    }
    if usd >= 1.0 {
        let decimals = if usd < 9.995 {
            2
        } else if usd < 99.95 {
            1
        } else {
            0
        };
        return format!("${usd:.decimals$}");
    }
    // Two significant digits, at least cents, without trailing zeros.
    let decimals = (1 - usd.log10().floor() as i32).max(2) as usize;
    let text = format!("{usd:.decimals$}");
    let (whole, fraction) = text.split_once('.').unwrap_or((&text, ""));
    let fraction = fraction.trim_end_matches('0');
    format!("${whole}.{fraction:0<2}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_figures_are_compact() {
        let times = [(800, "0.8s"), (12_400, "12.4s"), (125_000, "2m 05s")];
        for (ms, text) in times.into_iter().chain([(3_720_000, "1h 02m")]) {
            assert_eq!(turn_time(ms), text);
        }
        for (n, text) in [
            (812, "812"),
            (3_100, "3.1k"),
            (19_337, "19k"),
            (153_400, "153k"),
            (999_999, "1.0M"),
            (1_234_567, "1.2M"),
            (45_000_000, "45M"),
        ] {
            assert_eq!(tokens(n), text, "{n}");
        }
        for (usd, text) in [
            (0.0, "$0"),
            (0.00142969, "$0.0014"),
            (0.04, "$0.04"),
            (0.1, "$0.10"),
            (0.86724, "$0.87"),
            (1.27, "$1.27"),
            (12.34, "$12.3"),
            (140.2, "$140"),
        ] {
            assert_eq!(dollars(usd), text, "{usd}");
        }
    }
}
