//! The session view: status line, scrolling timeline, approval prompt and
//! composer. The timeline's items are laid out by
//! [`timeline`](super::timeline); expanded blobs are read from the file the
//! core points at, the only I/O in rendering.

use std::time::SystemTime;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use skywave_core::{
    ApprovalPrompt, ApprovalStatus, BlobView, Entry, Link, Outgoing, PermissionMode,
    RuleSuggestion, SessionState, SessionUsage, SessionView, TimelineItem, ToolView,
};
use skywave_present::format::{
    agent_mode_label, agent_model_label, agent_refusal, answer_words, blob_size, bytes,
    delivery_label, dollars, mode_detail, mode_label, outgoing_label, tokens, turn_time,
};
use skywave_present::sections::{ItemKey, Section, gated_item, sections, tool_sections};

use super::timeline::{
    approval_state, clean, detail_lines, glyph, head_lines, item_lines, preview_lines, rule_index,
    title_spans, wall_ms,
};
use super::{outgoing_badge, rename_badge, render_prompt, rule_text, state_badge};
use crate::app::{App, Focus, Geometry, Picker, SessionUi};

/// Most composer rows on screen; longer drafts scroll.
const COMPOSER_ROWS: usize = 6;
/// Most rows of the gated call's content in the approval prompt; a third of
/// the screen when that is less.
const APPROVAL_ROWS: usize = 10;

pub(super) fn render(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    session: &SessionView,
    ui: &SessionUi,
    wall: SystemTime,
) {
    let mut title = vec![Span::raw(format!(" {}", clean(&session.title))).bold()];
    title.extend(rename_badge(session.rename.as_ref()));
    if let Some(workdir) = &session.workdir {
        title.push(Span::raw(format!(" · {}", clean(workdir))).dark_gray());
    }
    title.push(Span::raw(format!(" · {} ", state_badge(&session.state).0)));
    if session.archived {
        title.push(Span::raw("· archived · a message brings it back ").dark_gray());
    }
    let block = Block::bordered().title(Line::from(title));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let composer_width = inner.width.saturating_sub(2) as usize;
    let composer_rows = ui.composer.rows(composer_width).len();
    let composer_height = composer_rows.clamp(1, COMPOSER_ROWS) as u16 + 2;
    let approval_rows = APPROVAL_ROWS.min(inner.height as usize / 3);
    let approval = session.pending_approval.as_ref().map(|p| {
        approval_lines(
            p,
            session,
            ui.focus,
            ui.rule_pick(session),
            inner.width.saturating_sub(2) as usize,
            approval_rows,
            wall_ms(wall),
        )
    });
    let approval_height = approval.as_ref().map_or(0, |lines| lines.len() as u16 + 2);
    let [status, timeline, approval_area, composer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(approval_height),
        Constraint::Length(composer_height),
    ])
    .areas(inner);

    // The timeline first: the status line reads the scroll state its
    // layout settles.
    render_timeline(frame, timeline, session, ui, wall);
    frame.render_widget(
        Paragraph::new(status_line(session, ui, status.width as usize)),
        status,
    );
    if let Some(lines) = approval {
        frame.render_widget(
            Paragraph::new(lines).block(Block::bordered().title(" Approval needed ").yellow()),
            approval_area,
        );
    }
    // A rename prompt stands in for the composer while it is open.
    match &app.ui().prompt {
        Some(prompt) => render_prompt(frame, composer, app, prompt),
        None => render_composer(frame, composer, app, ui),
    }
    if let Some(picker) = &ui.picker {
        render_picker(frame, inner, composer, app, session, picker);
    }
}

/// The agent's state and what is on its way, then what the session takes,
/// right-aligned in `width` columns.
fn status_line(session: &SessionView, ui: &SessionUi, width: usize) -> Line<'static> {
    let mut spans = vec![match &session.state {
        SessionState::Idle => Span::raw("agent idle").green(),
        SessionState::AgentWorking => Span::raw("agent working · Ctrl-X interrupts").cyan(),
        SessionState::AwaitingApproval(_) => Span::raw("agent waiting for an approval").yellow(),
        SessionState::Errored(info) => Span::raw(format!("agent error: {}", info.message)).red(),
        SessionState::Stopped => Span::raw("agent stopped · a message restarts it").dark_gray(),
    }];
    // The mode and model once the agent said them or one was asked for,
    // then what it refused, until the next switch.
    let agent = &session.agent;
    if agent.mode.is_some() || agent.mode_change.is_some() {
        spans.push(Span::raw(format!(" · mode {}", agent_mode_label(agent))));
    }
    if let Some(model) = agent_model_label(agent) {
        // A known model names itself; `unknown → opus` alone would not say
        // what is unknown.
        let what = if agent.model.is_none() { "model " } else { "" };
        spans.push(Span::raw(format!(" · {what}{}", clean(&model))));
    }
    for refusal in agent_refusal(agent) {
        spans.push(Span::raw(format!(" · {}", clean(&refusal))).yellow());
    }
    if let Some(interrupt) = &session.interrupt {
        spans.push(Span::raw(" · interrupt ").dark_gray());
        spans.push(Span::styled(
            outgoing_label(interrupt),
            outgoing_badge(interrupt).1,
        ));
    }
    if let Some(stop) = &session.stop {
        spans.push(Span::raw(" · stop ").dark_gray());
        spans.push(Span::styled(outgoing_label(stop), outgoing_badge(stop).1));
    }
    // Read-only: only what was granted here; `?` lists them.
    match session.rules.len() {
        0 => {}
        1 => spans.push(Span::raw(" · 1 rule").dark_gray()),
        n => spans.push(Span::raw(format!(" · {n} rules")).dark_gray()),
    }
    if ui.shown_scroll().is_some() {
        spans.push(Span::raw(" · scrolled back, End follows").dark_gray());
    }
    // While the prompt is open, the footer shows it instead.
    if let Some(search) = ui.search.as_ref().filter(|s| !s.editing) {
        spans.push(Span::raw(format!(" · {}", search.status(session))).yellow());
        if search.wrapped {
            spans.push(Span::raw(" · search wrapped").yellow().bold());
        }
        // So a partial search is not taken for a whole one.
        if let Some(partial) = search.partial() {
            spans.push(Span::raw(format!(" · {partial}")).dark_gray());
        }
    }
    // What the session takes, right-aligned in what the rest leaves.
    let used = Line::from(spans.clone()).width();
    // The model is on the left once the agent named it.
    let model = session.agent.model.is_none();
    if let Some(usage) = &session.usage
        && let Some(summary) = usage_summary(usage, model, width.saturating_sub(used + 2))
    {
        let gap = width - used - summary.chars().count();
        spans.push(Span::raw(" ".repeat(gap)));
        spans.push(Span::raw(summary).dark_gray());
    }
    Line::from(spans)
}

/// `last turn 12.4s · 3.1k in / 812 out · $0.04 · session $1.27 · <model>`,
/// with the figures the agent did not report left out (and the model unless
/// `model`), and fields dropped from the left until it fits in `room`
/// columns; `None` if nothing does.
fn usage_summary(usage: &SessionUsage, model: bool, room: usize) -> Option<String> {
    let turn = &usage.last_turn;
    let mut fields = Vec::new();
    if let Some(ms) = turn.duration_ms {
        fields.push(turn_time(ms));
    }
    match (turn.tokens_in, turn.tokens_out) {
        (Some(read), Some(written)) => {
            fields.push(format!("{} in / {} out", tokens(read), tokens(written)));
        }
        (Some(read), None) => fields.push(format!("{} in", tokens(read))),
        (None, Some(written)) => fields.push(format!("{} out", tokens(written))),
        (None, None) => {}
    }
    if let Some(cost) = turn.cost_usd {
        fields.push(dollars(cost));
    }
    if let Some(first) = fields.first_mut() {
        *first = format!("last turn {first}");
    }
    if let Some(cost) = usage.total.cost_usd {
        fields.push(format!("session {}", dollars(cost)));
    }
    if let Some(model) = usage.total.model.as_ref().filter(|_| model) {
        fields.push(clean(model));
    }
    (0..fields.len())
        .map(|first| fields[first..].join(" · "))
        .find(|summary| summary.chars().count() <= room)
}

fn render_timeline(
    frame: &mut Frame,
    area: Rect,
    session: &SessionView,
    ui: &SessionUi,
    wall: SystemTime,
) {
    let focused = ui.focus == Focus::Timeline;
    let (items, matched) = item_lines(session, ui, focused, area.width as usize, wall);
    let mut starts = Vec::with_capacity(items.len());
    let mut total = 0;
    for lines in &items {
        starts.push(total);
        total += lines.len();
    }
    let rule = rule_index(session).and_then(|index| starts.get(index).copied());
    let keys: Vec<ItemKey> = session.timeline.iter().map(ItemKey::of).collect();
    let matched = matched.map(|(index, row)| (keys[index], starts[index] + row));
    let mut geometry = Geometry {
        keys,
        starts,
        total,
        height: area.height as usize,
        top: ui.geometry.borrow().top,
        rule,
        matched,
        reveal: ui.reveal,
    };
    // While the opening is not settled, show the scroll state it will
    // settle on, from this frame's layout; likewise the one that shows a
    // search match in a blob, while that waits; and following, while the
    // item `v` revealed waits, if it fits at the bottom.
    let pinned = ui
        .search
        .as_ref()
        .filter(|search| search.pin)
        .and_then(|search| geometry.match_scroll(search.current?));
    let scroll = if ui.opening {
        geometry.opening()
    } else if ui.reveal.is_some() {
        pinned.unwrap_or(geometry.follow_at_bottom(ui.scroll))
    } else {
        pinned.unwrap_or(ui.scroll)
    };
    let top = geometry.top(scroll);
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
/// gates as the timeline shows it (kind, title, detail), what it will do
/// (at most `max_rows` rows of it), the lasting rule `A` grants, then the
/// keys. While the rules offered are listed (`pick`, the highlighted one),
/// the list takes the place of what the call will do, under the same cap.
fn approval_lines(
    prompt: &ApprovalPrompt,
    session: &SessionView,
    focus: Focus,
    pick: Option<usize>,
    width: usize,
    max_rows: usize,
    wall_ms: i64,
) -> Vec<Line<'static>> {
    let (mut lines, content) = match gated_item(session) {
        Some((
            _,
            TimelineItem::Logged {
                entry: Entry::Tool(view),
                ..
            },
        )) => (
            gated_call(view, width),
            gated_content(&tool_sections(view), width, max_rows),
        ),
        _ => {
            let (_, style) = approval_state(&ApprovalStatus::Pending);
            let lines = head_lines(
                Vec::new(),
                Span::styled("?", style),
                title_spans(&prompt.tool, &prompt.summary),
                Vec::new(),
                width,
            );
            // An approval with no logged call (a v1 one): the rest of its
            // summary, else its input.
            let entry = Entry::Approval {
                id: prompt.id,
                tool: prompt.tool.clone(),
                summary: prompt.summary.clone(),
                input: prompt.input.clone(),
                status: ApprovalStatus::Pending,
                answer: None,
            };
            let content = gated_content(&sections(&entry), width, max_rows);
            (lines, content)
        }
    };
    if let Some(selected) = pick {
        lines.extend(rule_list(&prompt.suggestions, selected, max_rows));
        lines.push(
            Line::from("↑↓ choose · Enter approve and always allow · Esc back to a/x")
                .yellow()
                .bold(),
        );
        return lines;
    }
    lines.extend(content);
    let answered = prompt.answer.as_ref().is_some_and(|a| !a.retryable);
    if !answered {
        lines.extend(always_line(&prompt.suggestions));
    }
    let keys = match focus {
        Focus::Composer => "Esc, then a approve · x deny",
        Focus::Timeline => "a approve · x deny",
    };
    lines.push(match &prompt.answer {
        None => Line::from(keys).yellow().bold(),
        Some(answer) => {
            // On its way: how long it has waited says more than "queued".
            let label = match (&answer.outgoing, &answer.delivery) {
                (Outgoing::Queued | Outgoing::Sending, Some(delivery)) => {
                    delivery_label(delivery, &answer.outgoing, wall_ms)
                }
                _ => outgoing_label(&answer.outgoing),
            };
            let rule = answer
                .rule
                .as_ref()
                .map_or_else(String::new, |rule| format!(" · {}", rule_text(rule)));
            let mut spans = vec![
                Span::raw(format!(
                    "you {}{rule} · ",
                    answer_words(answer.decision, answer.rule.as_ref())
                )),
                Span::styled(label, outgoing_badge(&answer.outgoing).1),
            ];
            if answer.retryable {
                spans.push(Span::raw(format!(" · {keys} to answer again")).dark_gray());
            }
            Line::from(spans)
        }
    });
    lines
}

/// What `A` does, when the agent offers lasting rules: grant the one it
/// offers, or list them to pick one.
fn always_line(offered: &[RuleSuggestion]) -> Option<Line<'static>> {
    let what = match offered {
        [] => return None,
        [rule] => rule_text(rule),
        rules => format!("choose one of {} rules", rules.len()),
    };
    Some(Line::from(format!("A  always allow · {what}")).yellow())
}

/// The rules offered, the `selected` one marked, at most `max_rows` of them
/// with the selected one in view.
fn rule_list(offered: &[RuleSuggestion], selected: usize, max_rows: usize) -> Vec<Line<'static>> {
    let rows = max_rows.max(1);
    let first = (selected + 1).saturating_sub(rows);
    offered
        .iter()
        .enumerate()
        .skip(first)
        .take(rows)
        .map(|(i, rule)| {
            if i == selected {
                Line::from(format!("› {}", rule_text(rule))).reversed()
            } else {
                Line::from(format!("  {}", rule_text(rule)))
            }
        })
        .collect()
}

/// The call an approval gates: glyph, name and title, then its detail.
fn gated_call(view: &ToolView, width: usize) -> Vec<Line<'static>> {
    let (_, style) = approval_state(&ApprovalStatus::Pending);
    let mut lines = head_lines(
        Vec::new(),
        Span::styled(glyph(view.kind), style),
        title_spans(&view.name, &view.title),
        Vec::new(),
        width,
    );
    if let Some(detail) = &view.detail {
        lines.extend(detail_lines(detail, 2, width, None));
    }
    lines
}

/// The first section shown while collapsed, capped to `max_rows`; with none,
/// where to see the whole input if there is one.
fn gated_content(sections: &[Section], width: usize, max_rows: usize) -> Vec<Line<'static>> {
    if let Some(section) = sections.iter().find(|s| !s.hidden) {
        return preview_lines(section, width, max_rows);
    }
    sections
        .iter()
        .find_map(|s| s.blob)
        .map(input_line)
        .into_iter()
        .collect()
}

/// Where the whole input of a gated call is, and its size if it is not here.
fn input_line(input: &BlobView) -> Line<'static> {
    let size = blob_size(&input.state).map_or_else(String::new, |size| format!(" {}", bytes(size)));
    Line::from(format!("  full input{size} · v to see it")).dark_gray()
}

/// The mode or model list, just above the composer (`below`) and as wide,
/// over the timeline; at most as tall as the session view (`area`) leaves.
fn render_picker(
    frame: &mut Frame,
    area: Rect,
    below: Rect,
    app: &App,
    session: &SessionView,
    picker: &Picker,
) {
    let room = below.y.saturating_sub(area.y);
    let rows = usize::from(room.saturating_sub(2)).max(1);
    let agent = &session.agent;
    let (title, lines, input) = match picker {
        Picker::Mode {
            selected,
            confirm: true,
        } => {
            let mode = PermissionMode::ALL[*selected];
            let lines = vec![
                Line::from(format!("Switch to {}?", mode_label(mode))).bold(),
                Line::from(format!("{}.", capitalised(mode_detail(mode)))).yellow(),
                Line::from("y switch · any other key back").yellow().bold(),
            ];
            (" Permission mode ".to_owned(), lines, None)
        }
        Picker::Mode { selected, .. } => {
            let modes = PermissionMode::ALL.map(|mode| {
                let mut spans = vec![
                    Span::raw(format!("{:<14}", mode_label(mode))),
                    Span::raw(mode_detail(mode)).dark_gray(),
                ];
                if agent.mode == Some(mode) {
                    spans.push(Span::raw(" · now").green());
                }
                spans
            });
            let lines = list_lines(modes.into(), *selected, rows);
            (
                format!(" Permission mode · now {} ", agent_mode_label(agent)),
                lines,
                None,
            )
        }
        Picker::Model { selected, name } => {
            let current = agent.model.as_deref().and_then(|m| agent.choice(m));
            let models: Vec<Vec<Span<'static>>> = agent
                .models
                .iter()
                .map(|choice| {
                    let mut spans = vec![Span::raw(clean(&choice.label))];
                    if let Some(detail) = &choice.detail {
                        spans.push(Span::raw(format!("  {}", clean(detail))).dark_gray());
                    }
                    if current == Some(choice) {
                        spans.push(Span::raw(" · now").green());
                    }
                    spans
                })
                .collect();
            let mut lines = if models.is_empty() {
                vec![Line::from("No list yet: it comes once the agent has run.").dark_gray()]
            } else if name.trim().is_empty() {
                let selected = (*selected).min(models.len() - 1);
                list_lines(models, selected, rows.saturating_sub(1))
            } else {
                // Typing: Enter sends the name, not the highlighted model.
                let shown = rows.saturating_sub(1);
                models
                    .into_iter()
                    .take(shown)
                    .map(|spans| {
                        let mut row = vec![Span::raw("  ")];
                        row.extend(spans);
                        Line::from(row).dark_gray()
                    })
                    .collect()
            };
            let typed = if name.is_empty() {
                Line::from(vec![
                    Span::raw("> "),
                    Span::raw("or type a model name").dark_gray(),
                ])
            } else {
                Line::from(format!("> {}", clean(name)))
            };
            lines.push(typed);
            let now = agent_model_label(agent).unwrap_or_else(|| "unknown".to_owned());
            let input = (lines.len() - 1, clean(name).chars().count() + 2);
            (format!(" Model · now {} ", clean(&now)), lines, Some(input))
        }
    };
    let height = (lines.len() as u16 + 2).min(room);
    if height < 3 {
        return;
    }
    let popup = Rect::new(below.x, below.y - height, below.width, height);
    let block = Block::bordered().title(title).cyan();
    let inner = block.inner(popup);
    frame.render_widget(Clear, popup);
    frame.render_widget(Paragraph::new(lines).block(block), popup);
    if let Some((row, column)) = input
        && app.typing()
        && !app.ui().help
        && (row as u16) < inner.height
    {
        let x = inner.x + (column as u16).min(inner.width.saturating_sub(1));
        frame.set_cursor_position(Position::new(x, inner.y + row as u16));
    }
}

/// Rows of a list, `selected` marked, at most `max_rows` of them with the
/// selected one in view.
fn list_lines(
    rows: Vec<Vec<Span<'static>>>,
    selected: usize,
    max_rows: usize,
) -> Vec<Line<'static>> {
    let shown = max_rows.max(1);
    let first = (selected + 1).saturating_sub(shown);
    rows.into_iter()
        .enumerate()
        .skip(first)
        .take(shown)
        .map(|(i, spans)| {
            let mut row = vec![Span::raw(if i == selected { "› " } else { "  " })];
            row.extend(spans);
            if i == selected {
                Line::from(row).reversed()
            } else {
                Line::from(row)
            }
        })
        .collect()
}

/// `text` with its first letter upper-case.
fn capitalised(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

fn render_composer(frame: &mut Frame, area: Rect, app: &App, ui: &SessionUi) {
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

    let draft = &ui.composer;
    if draft.text().is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from("Write to the agent…").dark_gray()),
            inner,
        );
        if focused && !app.ui().help {
            frame.set_cursor_position(Position::new(inner.x, inner.y));
        }
        return;
    }
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
