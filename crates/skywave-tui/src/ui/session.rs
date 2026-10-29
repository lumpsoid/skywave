//! The session view: status line, scrolling timeline, approval prompt and
//! composer. Expanded blobs are read from the file the core points at; that
//! read is the only I/O in rendering.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};
use skywave_core::{
    ApprovalPrompt, ApprovalStatus, BlobState, BlobView, DeviceId, Entry, Link, Outgoing,
    SessionState, SessionView, TimelineItem, ToolOutcome, ToolView,
};

use super::{decision_label, outgoing_badge, outgoing_label, state_badge};
use crate::app::{App, Focus, Geometry, SessionUi, item_blob, selectable};

/// Most of a blob shown when expanded.
pub const SHOW_BYTES: u64 = 64 * 1024;
/// Most lines of a blob shown when expanded.
pub const SHOW_LINES: usize = 400;
/// Most composer rows on screen; longer drafts scroll.
const COMPOSER_ROWS: usize = 6;
/// Width of the selection marker and time column, where item text starts.
const INDENT: &str = "        ";

pub(super) fn render(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    session: &SessionView,
    ui: &SessionUi,
) {
    let title = session
        .workdir
        .clone()
        .unwrap_or_else(|| session.id.0.to_string());
    let block = Block::bordered().title(format!(" {title} · {} ", state_badge(&session.state).0));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let draft = app.draft(session.id);
    let composer_width = inner.width.saturating_sub(2) as usize;
    let composer_rows = draft.map_or(1, |d| d.rows(composer_width).len());
    let composer_height = composer_rows.clamp(1, COMPOSER_ROWS) as u16 + 2;
    let approval_height = session
        .pending_approval
        .as_ref()
        .map_or(0, |p| approval_lines(p, ui.focus).len() as u16 + 2);
    let [status, timeline, approval, composer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(approval_height),
        Constraint::Length(composer_height),
    ])
    .areas(inner);

    frame.render_widget(Paragraph::new(status_line(session, ui)), status);
    render_timeline(frame, timeline, session, ui);
    if let Some(prompt) = &session.pending_approval {
        frame.render_widget(
            Paragraph::new(approval_lines(prompt, ui.focus))
                .block(Block::bordered().title(" Approval needed ").yellow()),
            approval,
        );
    }
    render_composer(frame, composer, app, session, ui);
}

fn status_line(session: &SessionView, ui: &SessionUi) -> Line<'static> {
    let mut spans = vec![match &session.state {
        SessionState::Idle => Span::raw("agent idle").green(),
        SessionState::AgentWorking => Span::raw("agent working · Ctrl-X interrupts").cyan(),
        SessionState::AwaitingApproval(_) => Span::raw("agent waiting for an approval").yellow(),
        SessionState::Errored(info) => Span::raw(format!("agent error: {}", info.message)).red(),
    }];
    if let Some(interrupt) = &session.interrupt {
        spans.push(Span::raw(" · interrupt ").dark_gray());
        spans.push(Span::styled(
            outgoing_label(interrupt),
            outgoing_badge(interrupt).1,
        ));
    }
    if ui.scroll.is_some() {
        spans.push(Span::raw(" · scrolled back, End follows").dark_gray());
    }
    Line::from(spans)
}

fn render_timeline(frame: &mut Frame, area: Rect, session: &SessionView, ui: &SessionUi) {
    let focused = ui.focus == Focus::Timeline;
    let items: Vec<Vec<Line<'static>>> = session
        .timeline
        .iter()
        .map(|item| item_lines(item, ui, focused))
        .collect();
    let mut starts = Vec::with_capacity(items.len());
    let mut total = 0;
    for lines in &items {
        starts.push(total);
        total += Paragraph::new(lines.clone())
            .wrap(Wrap { trim: false })
            .line_count(area.width);
    }
    let geometry = Geometry {
        starts,
        total,
        height: area.height as usize,
    };
    let top = geometry.top(ui.scroll);

    if items.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from("Nothing here yet. Write to the agent below.").dark_gray()),
            area,
        );
    } else {
        // Only the items on screen are laid out again.
        let first = geometry.starts.partition_point(|&start| start <= top) - 1;
        let bottom = top + geometry.height;
        let lines: Vec<Line> = items
            .into_iter()
            .zip(&geometry.starts)
            .skip(first)
            .take_while(|(_, start)| **start < bottom)
            .flat_map(|(lines, _)| lines)
            .collect();
        let offset = (top - geometry.starts[first]).min(u16::MAX as usize) as u16;
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((offset, 0)),
            area,
        );
    }
    *ui.geometry.borrow_mut() = geometry;
}

/// One timeline item: its head line, any further text lines, and the full
/// content when expanded.
fn item_lines(item: &TimelineItem, ui: &SessionUi, focused: bool) -> Vec<Line<'static>> {
    let seq = selectable(item);
    let selected = focused && seq.is_some() && seq == ui.selected;
    let expanded = seq.is_some_and(|seq| ui.expanded.contains(&seq));
    let mut head = vec![
        if selected {
            Span::raw("> ").bold().reversed()
        } else {
            Span::raw("  ")
        },
        match item {
            TimelineItem::Logged { at, .. } => {
                Span::raw(at.format("%H:%M ").to_string()).dark_gray()
            }
            TimelineItem::Pending { .. } => Span::raw("      "),
        },
    ];
    let entry = match item {
        TimelineItem::Pending { text, outgoing, .. } => {
            head.extend(you(Some(outgoing)));
            return text_lines(head, text);
        }
        TimelineItem::Logged { entry, .. } => entry,
    };
    match entry {
        Entry::UserMessage {
            text,
            from,
            own,
            outgoing,
        } => {
            if *own {
                head.extend(you(outgoing.as_ref()));
            } else {
                head.push(Span::raw(device_label(*from)).magenta().bold());
                head.push(Span::raw(": "));
            }
            text_lines(head, text)
        }
        Entry::AgentMessage { text } => {
            head.push(Span::raw("agent: ").green().bold());
            text_lines(head, text)
        }
        Entry::Tool(view) => {
            head.push(expander(expanded));
            head.push(Span::raw(format!("tool {}: ", view.name)).cyan().bold());
            let mut lines = tool_lines(head, &view.title, entry, expanded);
            lines.splice(1..1, tool_status_lines(view));
            lines
        }
        Entry::ToolCall { name, summary, .. } => {
            head.push(expander(expanded));
            head.push(Span::raw(format!("tool {name}: ")).cyan().bold());
            tool_lines(head, summary, entry, expanded)
        }
        Entry::ToolResult { summary, .. } => {
            head.push(expander(expanded));
            head.push(Span::raw("result: ").cyan().bold());
            tool_lines(head, summary, entry, expanded)
        }
        Entry::Approval {
            tool,
            summary,
            status,
            ..
        } => {
            head.push(expander(expanded));
            head.push(Span::raw(format!("approval {tool} ")).yellow().bold());
            let (status, style) = approval_status(status);
            head.push(Span::styled(format!("[{status}]"), style));
            head.push(Span::raw(": "));
            tool_lines(head, summary, entry, expanded)
        }
        Entry::Interrupted => {
            head.push(Span::raw("interrupted · the agent stopped its turn").yellow());
            vec![Line::from(head)]
        }
        Entry::AgentErrored { message } => {
            head.push(Span::raw("agent error: ").red().bold());
            text_lines(head, message)
        }
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

fn expander(expanded: bool) -> Span<'static> {
    Span::raw(if expanded { "▾ " } else { "▸ " }).dark_gray()
}

/// `head` followed by the first line of `text`, then the other lines
/// indented under it.
fn text_lines(mut head: Vec<Span<'static>>, text: &str) -> Vec<Line<'static>> {
    let mut lines = text.split('\n').map(clean);
    head.push(Span::raw(lines.next().unwrap_or_default()));
    let mut out = vec![Line::from(head)];
    out.extend(lines.map(|line| Line::from(format!("{INDENT}{line}"))));
    out
}

/// A tool call, result or approval: the summary's first line and the blob
/// state when collapsed; the whole summary or the blob's content when
/// expanded.
fn tool_lines(
    mut head: Vec<Span<'static>>,
    summary: &str,
    entry: &Entry,
    expanded: bool,
) -> Vec<Line<'static>> {
    let blob = item_blob(entry);
    let mut lines = summary.split('\n').map(clean);
    head.push(Span::raw(lines.next().unwrap_or_default()));
    let rest: Vec<String> = lines.collect();
    if !expanded && !rest.is_empty() {
        head.push(Span::raw(format!(" (+{} lines)", rest.len())).dark_gray());
    }
    if let Some(blob) = blob {
        let (badge, style) = blob_badge(&blob.state);
        head.push(Span::raw(" "));
        head.push(Span::styled(badge, style));
    }
    let mut out = vec![Line::from(head)];
    if !expanded {
        return out;
    }
    let indent = format!("{INDENT}  ");
    if blob.is_none() {
        out.extend(
            rest.into_iter()
                .map(|line| Line::from(format!("{indent}{line}"))),
        );
    }
    if let Some(blob) = blob {
        out.extend(blob_lines(blob).into_iter().map(|line| {
            let mut spans = vec![Span::raw(indent.clone())];
            spans.extend(line.spans);
            Line::from(spans)
        }));
    }
    out
}

/// Short blob state for a collapsed item.
fn blob_badge(state: &BlobState) -> (String, Style) {
    match state {
        BlobState::NotDownloaded { size } => (
            format!("[full {} · not downloaded]", bytes(*size)),
            Style::new().dark_gray(),
        ),
        BlobState::Downloading { got, size } => (
            format!("[downloading {}%]", percent(*got, *size)),
            Style::new().cyan(),
        ),
        BlobState::Available { .. } => ("[downloaded]".to_owned(), Style::new().green()),
    }
}

/// An expanded blob: what to do while it is not here, its content once it
/// is.
fn blob_lines(blob: &BlobView) -> Vec<Line<'static>> {
    match &blob.state {
        BlobState::NotDownloaded { size } => vec![
            Line::from(format!(
                "not downloaded · {} · press d to download",
                bytes(*size)
            ))
            .dark_gray(),
        ],
        BlobState::Downloading { got, size } => vec![
            Line::from(format!(
                "downloading · {} of {} ({}%)",
                bytes(*got),
                bytes(*size),
                percent(*got, *size)
            ))
            .cyan(),
        ],
        BlobState::Available { path } => match read_head(path) {
            Ok((content, size)) => {
                // A final line break ends the last line; it does not start one.
                let content = content.strip_suffix('\n').unwrap_or(&content);
                let mut lines: Vec<Line> = content
                    .split('\n')
                    .take(SHOW_LINES)
                    .map(|line| Line::from(clean(line)))
                    .collect();
                let line_count = content.split('\n').count();
                if line_count > SHOW_LINES || size > SHOW_BYTES {
                    lines.push(
                        Line::from(format!(
                            "… cut short: {} of {} shown; the whole file is {}",
                            if line_count > SHOW_LINES {
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
                lines
            }
            Err(err) => vec![Line::from(format!("cannot read {}: {err}", path.display())).red()],
        },
    }
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

/// A grouped call's approval and result, one line each, under its title.
fn tool_status_lines(view: &ToolView) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(approval) = &view.approval {
        let (status, style) = approval_status(&approval.status);
        lines.push(Line::from(vec![
            Span::raw(format!("{INDENT}  ")),
            Span::raw(format!("approval {} ", view.name))
                .yellow()
                .bold(),
            Span::styled(format!("[{status}]"), style),
        ]));
    }
    let outcome = match &view.outcome {
        ToolOutcome::Done {
            is_error, preview, ..
        } => {
            let label = if *is_error { "error: " } else { "result: " };
            Some(vec![
                Span::raw(label).cyan().bold(),
                Span::raw(clean(preview.split('\n').next().unwrap_or_default())),
            ])
        }
        ToolOutcome::NoResult => Some(vec![Span::raw("no result").dark_gray()]),
        // The approval line says why it stopped.
        ToolOutcome::Running { .. }
        | ToolOutcome::Denied { .. }
        | ToolOutcome::Cancelled { .. } => None,
    };
    if let Some(spans) = outcome {
        let mut line = vec![Span::raw(format!("{INDENT}  "))];
        line.extend(spans);
        lines.push(Line::from(line));
    }
    lines
}

fn approval_status(status: &ApprovalStatus) -> (String, Style) {
    match status {
        ApprovalStatus::Pending => ("waiting for an answer".to_owned(), Style::new().yellow()),
        ApprovalStatus::Resolved {
            decision,
            by,
            by_me,
        } => {
            let where_ = if *by_me {
                "here".to_owned()
            } else {
                format!("on {}", device_label(*by))
            };
            (
                format!("{} {where_}", decision_label(*decision)),
                Style::new().green(),
            )
        }
        ApprovalStatus::Cancelled { reason } => {
            (format!("cancelled: {reason}"), Style::new().dark_gray())
        }
    }
}

/// The prompt above the composer while the agent is blocked.
fn approval_lines(prompt: &ApprovalPrompt, focus: Focus) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(vec![
            Span::raw("The agent wants to use "),
            Span::raw(prompt.tool.clone()).bold(),
        ]),
        Line::from(clean(prompt.summary.split('\n').next().unwrap_or_default())),
    ];
    if let Some(input) = &prompt.input {
        let (badge, style) = blob_badge(&input.state);
        lines.push(Line::from(vec![
            Span::styled(badge, style),
            Span::raw(" select the approval in the timeline and press Enter to see it all")
                .dark_gray(),
        ]));
    }
    let keys = match focus {
        Focus::Composer => "Esc, then a approve · x deny",
        Focus::Timeline => "a approve · x deny",
    };
    lines.push(match &prompt.answer {
        None => Line::from(keys).yellow().bold(),
        Some(answer) => {
            let mut spans = vec![
                Span::raw(format!("you {} · ", decision_label(answer.decision))),
                Span::styled(
                    outgoing_label(&answer.outgoing),
                    outgoing_badge(&answer.outgoing).1,
                ),
            ];
            if matches!(answer.outgoing, Outgoing::Failed(_)) {
                spans.push(Span::raw(format!(" · {keys} to answer again")).dark_gray());
            }
            Line::from(spans)
        }
    });
    lines
}

fn render_composer(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    session: &SessionView,
    ui: &SessionUi,
) {
    let focused = ui.focus == Focus::Composer;
    let mut title = " Message ".to_owned();
    if matches!(app.view().link, Link::Reconnecting { .. }) {
        title.push_str("· offline, sends when back online ");
    }
    let block = Block::bordered().title(title).border_style(if focused {
        Style::new().cyan()
    } else {
        Style::new().dark_gray()
    });
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let draft = app.draft(session.id);
    let Some(draft) = draft.filter(|d| !d.text().is_empty()) else {
        frame.render_widget(
            Paragraph::new(Line::from("Write to the agent…").dark_gray()),
            inner,
        );
        if focused && !app.ui().help {
            frame.set_cursor_position(Position::new(inner.x, inner.y));
        }
        return;
    };
    let width = inner.width as usize;
    let rows = draft.rows(width);
    let (row, column) = draft.cursor_position(width);
    let height = inner.height as usize;
    let skip = (row + 1).saturating_sub(height);
    let lines: Vec<Line> = rows
        .iter()
        .skip(skip)
        .take(height)
        .map(|r| Line::from(draft.text()[r.start..r.end].to_owned()))
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
    if focused && !app.ui().help {
        let x = inner.x + (column as u16).min(inner.width.saturating_sub(1));
        let y = inner.y + (row - skip) as u16;
        frame.set_cursor_position(Position::new(x, y));
    }
}

/// Another device, by the end of its id: the start of a UUIDv7 is a
/// timestamp, the end is random.
fn device_label(id: DeviceId) -> String {
    let id = id.0.simple().to_string();
    format!("device …{}", &id[id.len() - 8..])
}

/// Text made safe for a terminal cell grid: tabs become spaces, other
/// control characters go.
fn clean(line: &str) -> String {
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

fn percent(got: u64, size: u64) -> u64 {
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
