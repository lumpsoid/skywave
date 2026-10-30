//! The session view: status line, scrolling timeline, approval prompt and
//! composer. The timeline's items are laid out by
//! [`timeline`](super::timeline); expanded blobs are read from the file the
//! core points at, the only I/O in rendering.

use std::time::SystemTime;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use skywave_core::{
    ApprovalPrompt, ApprovalStatus, BlobState, Entry, Link, Outgoing, SessionState, SessionView,
    TimelineItem, ToolView,
};

use super::timeline::{
    approval_state, bytes, detail_lines, glyph, head_lines, item_lines, title_spans,
};
use super::{decision_label, outgoing_badge, outgoing_label, state_badge};
use crate::app::{App, Focus, Geometry, SessionUi};
use crate::timeline::ItemKey;

/// Most composer rows on screen; longer drafts scroll.
const COMPOSER_ROWS: usize = 6;

pub(super) fn render(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    session: &SessionView,
    ui: &SessionUi,
    wall: SystemTime,
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
    let approval = session
        .pending_approval
        .as_ref()
        .map(|p| approval_lines(p, session, ui.focus, inner.width.saturating_sub(2) as usize));
    let approval_height = approval.as_ref().map_or(0, |lines| lines.len() as u16 + 2);
    let [status, timeline, approval_area, composer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(approval_height),
        Constraint::Length(composer_height),
    ])
    .areas(inner);

    frame.render_widget(Paragraph::new(status_line(session, ui)), status);
    render_timeline(frame, timeline, session, ui, wall);
    if let Some(lines) = approval {
        frame.render_widget(
            Paragraph::new(lines).block(Block::bordered().title(" Approval needed ").yellow()),
            approval_area,
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

fn render_timeline(
    frame: &mut Frame,
    area: Rect,
    session: &SessionView,
    ui: &SessionUi,
    wall: SystemTime,
) {
    let focused = ui.focus == Focus::Timeline;
    let items = item_lines(session, ui, focused, area.width as usize, wall);
    let mut starts = Vec::with_capacity(items.len());
    let mut total = 0;
    for lines in &items {
        starts.push(total);
        total += lines.len();
    }
    let mut geometry = Geometry {
        keys: session.timeline.iter().map(ItemKey::of).collect(),
        starts,
        total,
        height: area.height as usize,
        top: ui.geometry.borrow().top,
    };
    let top = geometry.top(ui.scroll);
    geometry.top = top;

    if items.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from("Nothing here yet. Write to the agent below.").dark_gray()),
            area,
        );
    } else {
        let lines: Vec<Line> = items
            .into_iter()
            .flatten()
            .skip(top)
            .take(geometry.height)
            .collect();
        frame.render_widget(Paragraph::new(lines), area);
    }
    *ui.geometry.borrow_mut() = geometry;
}

/// The prompt above the composer while the agent is blocked: the call it
/// gates as the timeline shows it (kind, title, detail), then the keys.
fn approval_lines(
    prompt: &ApprovalPrompt,
    session: &SessionView,
    focus: Focus,
    width: usize,
) -> Vec<Line<'static>> {
    let call = prompt.call.and_then(|call| {
        session.timeline.iter().find_map(|item| match item {
            TimelineItem::Logged {
                entry: Entry::Tool(view),
                ..
            } if view.call == call => Some(view.as_ref()),
            _ => None,
        })
    });
    let mut lines = match call {
        Some(view) => gated_call(view, width),
        None => {
            let (_, style) = approval_state(&ApprovalStatus::Pending);
            let mut lines = head_lines(
                Vec::new(),
                Span::styled("?", style),
                title_spans(&prompt.tool, &prompt.summary),
                None,
                width,
            );
            if let Some(input) = &prompt.input {
                let size = match &input.state {
                    BlobState::NotDownloaded { size } | BlobState::Downloading { size, .. } => {
                        format!("{} · ", bytes(*size))
                    }
                    BlobState::Available { .. } => String::new(),
                };
                lines.push(
                    Line::from(format!(
                        "  full input {size}select it in the timeline and press Enter to see it"
                    ))
                    .dark_gray(),
                );
            }
            lines
        }
    };
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

/// The call an approval gates: glyph, name and title, then its detail.
fn gated_call(view: &ToolView, width: usize) -> Vec<Line<'static>> {
    let (_, style) = approval_state(&ApprovalStatus::Pending);
    let mut lines = head_lines(
        Vec::new(),
        Span::styled(glyph(view.kind), style),
        title_spans(&view.name, &view.title),
        None,
        width,
    );
    if let Some(detail) = &view.detail {
        lines.extend(detail_lines(detail, 2, width));
    }
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
