//! Timeline items as screen lines. Text is laid out here rather than by
//! ratatui's wrapping, so a wrapped line hangs under its own column and
//! every item's height is known exactly (scroll anchoring needs it).
//!
//! A tool item is one block: its kind's glyph, the tool name and title, the
//! state right-aligned, an optional detail line, and its content under a
//! gutter: at most [`PREVIEW_LINES`] lines collapsed, the whole blob
//! expanded. Colour carries the state (the glyph, the state and the
//! gutter), never the item's type.

use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use skywave_core::{
    ApprovalStatus, BlobState, BodyFormat, Decision, DeviceId, Entry, Outgoing, SessionView,
    TimelineItem, ToolKind, ToolOutcome, ToolView,
};
use unicode_width::UnicodeWidthChar;

use super::{decision_label, outgoing_badge, outgoing_label};
use crate::app::SessionUi;
use crate::timeline::{ItemKey, PREVIEW_LINES, Section, sections, selectable, starts_turn};

/// Most of a blob shown when expanded.
pub const SHOW_BYTES: u64 = 64 * 1024;
/// Most lines of a blob shown when expanded.
pub const SHOW_LINES: usize = 400;

/// Columns before an item's text: the selection marker and the time.
const LEAD: usize = 8;
/// Columns before a tool item's title: the lead, the glyph and a space.
const TITLE: usize = LEAD + 2;
/// Narrowest a title may get beside its state before it wraps.
const MIN_TITLE: usize = 8;

/// Every timeline item's lines at `width` columns, in timeline order.
/// `wall` is what running calls' elapsed time counts to.
pub(super) fn item_lines(
    session: &SessionView,
    ui: &SessionUi,
    focused: bool,
    width: usize,
    wall: SystemTime,
) -> Vec<Vec<Line<'static>>> {
    let layout = Layout {
        ui,
        focused,
        width: width.max(TITLE + MIN_TITLE),
        wall_ms: wall
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX)),
    };
    let mut minute = None;
    session
        .timeline
        .iter()
        .enumerate()
        .map(|(index, item)| layout.item(index, item, &mut minute))
        .collect()
}

struct Layout<'a> {
    ui: &'a SessionUi,
    focused: bool,
    width: usize,
    wall_ms: i64,
}

impl Layout<'_> {
    /// One item: a separator if it starts a turn, then its lines. The time
    /// shows only when it differs from the last one shown (`minute`).
    fn item(
        &self,
        index: usize,
        item: &TimelineItem,
        minute: &mut Option<String>,
    ) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        if index > 0 && starts_turn(item) {
            out.push(Line::from("─".repeat(self.width)).dark_gray());
        }
        let key = selectable(item);
        let selected = self.focused && key.is_some() && key == self.ui.selected;
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
        out.extend(match item {
            TimelineItem::Pending { text, outgoing, .. } => {
                self.message(lead, you(Some(outgoing)), text)
            }
            TimelineItem::Logged { entry, .. } => self.entry(lead, entry, expanded),
        });
        out
    }

    fn entry(&self, lead: Vec<Span<'static>>, entry: &Entry, expanded: bool) -> Vec<Line<'static>> {
        let width = self.width;
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
                return self.message(lead, label, text);
            }
            Entry::AgentMessage { text } => {
                return self.message(lead, vec![Span::raw("agent: ").green().bold()], text);
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
            Entry::AgentErrored { message } => {
                return self.message(lead, vec![Span::raw("agent error: ").red().bold()], message);
            }
            Entry::Tool(view) => {
                let (state, style) = tool_state(view, self.wall_ms);
                let mut out = head_lines(
                    lead,
                    Span::styled(glyph(view.kind), style),
                    title_spans(&view.name, &view.title),
                    Some(Span::styled(state, style)),
                    width,
                );
                if let Some(detail) = &view.detail {
                    out.extend(detail_lines(detail, TITLE, width));
                }
                (out, style)
            }
            Entry::ToolCall { name, summary, .. } => (
                head_lines(
                    lead,
                    Span::raw("•"),
                    title_spans(name, summary),
                    None,
                    width,
                ),
                Style::new().dark_gray(),
            ),
            Entry::ToolResult { .. } => (
                head_lines(
                    lead,
                    Span::raw("↳"),
                    vec![Span::raw("result").bold()],
                    None,
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
                    title_spans(tool, summary),
                    Some(Span::styled(state, style)),
                    width,
                );
                (out, style)
            }
        };
        for section in sections(entry) {
            if expanded || !section.hidden {
                out.extend(self.section(&section, expanded, gutter));
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
    ) -> Vec<Line<'static>> {
        let mut lines = text.split('\n');
        let mut content = label;
        content.push(Span::raw(clean(lines.next().unwrap_or_default())));
        let mut out = wrap(lead, vec![indent(LEAD)], content, self.width);
        for line in lines {
            out.extend(wrap(
                vec![indent(LEAD)],
                vec![indent(LEAD)],
                vec![Span::raw(clean(line))],
                self.width,
            ));
        }
        out
    }

    /// One section under the gutter: its preview's head and what more there
    /// is while collapsed; all of it (the blob once here) while expanded.
    fn section(&self, section: &Section, expanded: bool, gutter: Style) -> Vec<Line<'static>> {
        let preview = content_lines(section.text);
        let mut note: Option<Span<'static>> = None;
        let content: Vec<String> = if expanded {
            match section.blob.map(|blob| &blob.state) {
                Some(BlobState::Available { path }) => match read_head(path) {
                    Ok((text, size)) => {
                        let lines = content_lines(&text);
                        if lines.len() > SHOW_LINES || size > SHOW_BYTES {
                            note = Some(
                                Span::raw(format!(
                                    "… cut short: {} of {} shown; the whole file is {}",
                                    if lines.len() > SHOW_LINES {
                                        format!("{SHOW_LINES} lines")
                                    } else {
                                        bytes(SHOW_BYTES)
                                    },
                                    bytes(size),
                                    path.display()
                                ))
                                .dark_gray(),
                            );
                        }
                        lines.into_iter().take(SHOW_LINES).map(clean).collect()
                    }
                    Err(err) => {
                        note =
                            Some(Span::raw(format!("cannot read {}: {err}", path.display())).red());
                        preview.into_iter().map(clean).collect()
                    }
                },
                Some(BlobState::NotDownloaded { size }) => {
                    note = Some(
                        Span::raw(format!(
                            "not downloaded · {} · press d to download",
                            bytes(*size)
                        ))
                        .dark_gray(),
                    );
                    preview.into_iter().map(clean).collect()
                }
                Some(BlobState::Downloading { got, size }) => {
                    note = Some(
                        Span::raw(format!(
                            "downloading · {} of {} ({}%)",
                            bytes(*got),
                            bytes(*size),
                            percent(*got, *size)
                        ))
                        .cyan(),
                    );
                    preview.into_iter().map(clean).collect()
                }
                None => preview.into_iter().map(clean).collect(),
            }
        } else if preview.is_empty() {
            // Nothing worth a line, e.g. a write's output beside its status;
            // `lines` then counts what the agent was told, not content.
            return Vec::new();
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
                more.push(match blob.state {
                    BlobState::NotDownloaded { size } => {
                        format!("full {} · not downloaded", bytes(size))
                    }
                    BlobState::Downloading { got, size } => {
                        format!("downloading {}%", percent(got, size))
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
        let mut out: Vec<Line<'static>> = content_rows(section, content, &bar, self.width)
            .into_iter()
            .flatten()
            .collect();
        if let Some(note) = note {
            out.extend(wrap(bar(), bar(), vec![note], self.width));
        }
        out
    }
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
    for rows in content_rows(section, content, &bar, width) {
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
        let more = match rest {
            0 => String::new(),
            1 => "+1 line · ".to_owned(),
            _ => format!("+{rest} lines · "),
        };
        out.extend(wrap(
            bar(),
            bar(),
            vec![Span::raw(format!("… {more}v shows all")).dark_gray()],
            width,
        ));
    }
    out
}

/// Each content line's rows after `bar`: numbered if the section is,
/// coloured by [`line_style`], wrapped to `width`.
fn content_rows(
    section: &Section,
    content: Vec<String>,
    bar: &dyn Fn() -> Vec<Span<'static>>,
    width: usize,
) -> Vec<Vec<Line<'static>>> {
    let digits = if section.numbered {
        let last = section
            .lines
            .map_or(0, |lines| lines as usize)
            .max(content.len());
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
                first.push(Span::raw(format!("{:>digits$} ", i + 1)).dark_gray());
                rest.push(indent(digits + 1));
            }
            let style = line_style(section, &line);
            wrap(first, rest, vec![Span::styled(line, style)], width)
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

/// A head line: `lead`, `glyph`, then `content` with `state` right-aligned
/// on the first row; wrapped rows hang under the content.
pub(super) fn head_lines(
    lead: Vec<Span<'static>>,
    glyph: Span<'static>,
    content: Vec<Span<'static>>,
    state: Option<Span<'static>>,
    width: usize,
) -> Vec<Line<'static>> {
    let mut first = lead;
    first.push(glyph);
    first.push(Span::raw(" "));
    let column = spans_width(&first);
    let state_width = state.as_ref().map_or(0, Span::width);
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
        if i == 0
            && let Some(state) = &state
        {
            let pad = width.saturating_sub(used + state_width).max(2);
            spans.push(Span::raw(" ".repeat(pad)));
            spans.push(state.clone());
        }
        out.push(Line::from(spans));
    }
    out
}

/// A tool call's detail line, dim, at `column`.
pub(super) fn detail_lines(detail: &str, column: usize, width: usize) -> Vec<Line<'static>> {
    wrap(
        vec![indent(column)],
        vec![indent(column)],
        vec![Span::raw(clean(detail.split('\n').next().unwrap_or_default())).dark_gray()],
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
        }) => Some(format!(
            "{} {}",
            decision_label(*decision),
            where_(*by, *by_me)
        )),
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
        } => (
            format!("{} {}", decision_label(*decision), where_(*by, *by_me)),
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

/// Who answered an approval: "here" or the other device.
fn where_(by: DeviceId, by_me: bool) -> String {
    if by_me {
        "here".to_owned()
    } else {
        format!("on {}", device_label(by))
    }
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

/// The first [`SHOW_BYTES`] of the file as text (invalid UTF-8 replaced),
/// and the file's size.
fn read_head(path: &Path) -> std::io::Result<(String, u64)> {
    let file = File::open(path)?;
    let size = file.metadata()?.len();
    let mut head = Vec::new();
    file.take(SHOW_BYTES).read_to_end(&mut head)?;
    Ok((String::from_utf8_lossy(&head).into_owned(), size))
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
