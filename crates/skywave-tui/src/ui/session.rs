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
    ApprovalPrompt, ApprovalStatus, BlobView, Entry, Link, Outgoing, QuestionPrompt,
    RuleSuggestion, RunWhen, SessionState, SessionUsage, SessionView, TimelineItem, ToolView,
};
use skywave_present::format::{
    AMBIENT_WORD, BackgroundRow, LOOPS_NOTE, NOTHING_RUNNING, SAVED_IN_FOLDER, StopAction,
    TRAIL_SEPARATOR, agent_effort_label, agent_mode_label, agent_model_label, agent_refusal,
    answer_words, approval_origin, background_rows, blob_size, bytes, context_moved_label,
    context_row, delivery_label, effort_label, mode_detail, mode_label, offered_modes,
    outgoing_label, repeat_word, session_usage_fields, shell_prompt, stop_heading, stop_warning,
    subagent_status, task_kind_word, trail, viewing_hint, work_summary,
};
use skywave_present::question::{QuestionDraft, question_label, reply_text};
use skywave_present::sections::{
    ItemKey, Section, approval_lost, asked_note, gated_item, sections, tool_sections,
};
use skywave_present::text::clean;

use super::timeline::{
    approval_state, detail_lines, glyph, head_lines, preview_lines, rule_index, title_spans, wrap,
};
use super::{outgoing_badge, rename_badge, render_prompt, rule_text, shown_title, state_badge};
use crate::app::{App, Focus, Geometry, Picker, SessionUi, StopKind};

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
    let mut title =
        vec![Span::raw(format!(" {}", shown_title(&session.title, session.named))).bold()];
    if let Some(subagent) = &session.subagent {
        let path = [trail(&subagent.trail), subagent.title.clone()]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(TRAIL_SEPARATOR);
        title.push(
            Span::raw(format!("{TRAIL_SEPARATOR}{} ", clean(&path)))
                .cyan()
                .bold(),
        );
    }
    title.extend(rename_badge(session.rename.as_ref()));
    if let Some(workdir) = &session.workdir {
        title.push(Span::raw(format!(" · {}", clean(workdir))).dark_gray());
    }
    title.push(Span::raw(format!(" · {} ", state_badge(&session.state).0)));
    if session.archived {
        title.push(Span::raw("· archived · a message brings it back ").dark_gray());
    }
    if app.ui().conversation_only.on() {
        title.push(Span::raw("· tool calls hidden · t shows them ").yellow());
    }
    let block = Block::bordered().title(Line::from(title));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let composer_width = inner.width.saturating_sub(2) as usize;
    let composer_rows = ui.composer.rows(composer_width).len();
    let composer_height = composer_rows.clamp(1, COMPOSER_ROWS) as u16 + 2;
    let approval_rows = APPROVAL_ROWS.min(inner.height as usize / 3);
    let panel_width = inner.width.saturating_sub(2) as usize;
    let approval = session
        .pending_approval
        .as_ref()
        .map(|p| {
            let lines = approval_lines(
                p,
                session,
                ui.focus,
                ui.rule_pick(session),
                panel_width,
                approval_rows,
            );
            (" Approval needed ", lines)
        })
        .or_else(|| {
            let prompt = session.pending_question.as_ref()?;
            let lines = question_lines(prompt, ui.focus, panel_width, approval_rows);
            Some((" Question ", lines))
        });
    let approval_height = approval
        .as_ref()
        .map_or(0, |(_, lines)| lines.len() as u16 + 2);
    let [status, timeline, approval_area, composer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(approval_height),
        Constraint::Length(composer_height),
    ])
    .areas(inner);

    // The timeline first: the status line reads the scroll state its
    // layout settles. Writing shell commands, the runs stand in for it.
    if ui.shell() {
        super::shell::render(frame, timeline, session, ui, wall);
    } else {
        render_timeline(frame, timeline, session, ui, wall);
    }
    frame.render_widget(
        Paragraph::new(status_line(session, ui, status.width as usize)),
        status,
    );
    if let Some((title, lines)) = approval {
        frame.render_widget(
            Paragraph::new(lines).block(Block::bordered().title(title).yellow()),
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
    let mut spans = Vec::new();
    // In a subagent's view, how it stands comes first, then what its keys do.
    if let Some(subagent) = &session.subagent {
        spans.push(Span::raw(clean(&subagent_status(&subagent.status))).cyan());
        let stop = if subagent.status.task.is_some() && subagent.status.live {
            " · S stops it"
        } else {
            ""
        };
        spans.push(Span::raw(format!("{stop} · Esc leaves · ")).dark_gray());
    }
    spans.push(match &session.state {
        SessionState::Idle => Span::raw("agent idle").green(),
        // Ctrl-X stops a shell run in shell mode.
        SessionState::AgentWorking if ui.shell() => Span::raw("agent working").cyan(),
        SessionState::AgentWorking => Span::raw("agent working · Ctrl-X interrupts").cyan(),
        SessionState::BackgroundWork => Span::raw("background work running").cyan(),
        SessionState::AwaitingApproval(_) if session.pending_question.is_some() => {
            Span::raw("agent waiting for an answer").yellow()
        }
        SessionState::AwaitingApproval(_) => Span::raw("agent waiting for an approval").yellow(),
        SessionState::Errored(info) => Span::raw(format!("agent error: {}", info.message)).red(),
        SessionState::Stopped => Span::raw("agent stopped · a message restarts it").dark_gray(),
    });
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
    if let Some(effort) = agent_effort_label(agent) {
        // As the model: `high → max` alone would not say what changes.
        let what = if effort.starts_with("effort ") || effort.starts_with("no effort") {
            ""
        } else {
            "effort "
        };
        spans.push(Span::raw(format!(" · {what}{}", clean(&effort))));
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
    for (what, outgoing) in [("new context", &session.clear), ("switch", &session.switch)] {
        if let Some(outgoing) = outgoing {
            spans.push(Span::raw(format!(" · {what} ")).dark_gray());
            spans.push(Span::styled(
                outgoing_label(outgoing),
                outgoing_badge(outgoing).1,
            ));
        }
    }
    if let Some(hint) = viewing_hint(session) {
        spans.push(Span::raw(format!(" · {}", clean(&hint))).yellow());
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
    if let Some(search) = ui.shown.search.as_ref().filter(|s| !s.editing) {
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

/// The footer's usage fields (`session_usage_fields`), `last turn 12.4s ·
/// 3.1k in / 812 out · $0.04 · ctx 19k/1M · 2% · session $1.27 · <model>`
/// (the model unless `model`), with fields dropped from the left until it
/// fits in `room` columns; `None` if nothing does.
fn usage_summary(usage: &SessionUsage, model: bool, room: usize) -> Option<String> {
    let fields = session_usage_fields(usage, model);
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
    let mut cache = ui.shown.layout.borrow_mut();
    let (heights, matched) = cache.lay_out(session, ui, focused, area.width as usize, wall);
    let mut starts = Vec::with_capacity(heights.len());
    let mut total = 0;
    for height in &heights {
        starts.push(total);
        total += height;
    }
    let rule = rule_index(session).and_then(|index| starts.get(index).copied());
    let keys: Vec<ItemKey> = session.timeline.iter().map(ItemKey::of).collect();
    let matched = matched.map(|(index, row)| (keys[index], starts[index] + row));
    let mut geometry = Geometry {
        keys,
        starts,
        total,
        height: area.height as usize,
        top: ui.shown.geometry.borrow().top,
        rule,
        matched,
        reveal: ui.shown.reveal,
    };
    // While the opening is not settled, show the scroll state it will
    // settle on, from this frame's layout; likewise the one that shows a
    // search match in a blob, while that waits; and following, while the
    // item `v` revealed waits, if it fits at the bottom.
    let pinned = ui
        .shown
        .search
        .as_ref()
        .filter(|search| search.pin)
        .and_then(|search| geometry.match_scroll(search.current?));
    let scroll = if ui.shown.opening {
        geometry.opening()
    } else if ui.shown.reveal.is_some() {
        pinned.unwrap_or(geometry.follow_at_bottom(ui.shown.scroll))
    } else {
        pinned.unwrap_or(ui.shown.scroll)
    };
    let top = geometry.top(scroll);
    geometry.top = top;

    if heights.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from("Nothing here yet. Write to the agent below.").dark_gray()),
            area,
        );
    } else {
        // Only what is on screen is copied out of the cache.
        let lines = cache.lines(session, &geometry.starts, top, geometry.height);
        frame.render_widget(Paragraph::new(lines), area);
    }
    *ui.shown.geometry.borrow_mut() = geometry;
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
) -> Vec<Line<'static>> {
    let shown = gated_item(session).is_some();
    let (mut lines, content) = match gated_item(session) {
        Some((
            _,
            TimelineItem::Logged {
                entry: Entry::Tool(view),
                ..
            },
        )) => (
            gated_call(view, width),
            gated_content(prompt, &tool_sections(view), width, max_rows),
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
            let content = gated_content(prompt, &sections(&entry), width, max_rows);
            (lines, content)
        }
    };
    if let Some(from) = &prompt.from {
        lines.insert(0, Line::from(clean(&approval_origin(from, shown))).yellow());
    }
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
                    delivery_label(delivery, &answer.outgoing)
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

/// The question prompt above the composer: who asks, each question and
/// its options in short, at most `max_rows` of them, then the keys or this
/// device's answer.
fn question_lines(
    prompt: &QuestionPrompt,
    focus: Focus,
    width: usize,
    max_rows: usize,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(from) = &prompt.from {
        lines.push(Line::from(clean(&format!("from subagent: {from}"))).yellow());
    }
    let mut body = Vec::new();
    for question in &prompt.questions {
        let mut head = Vec::new();
        if !question.header.trim().is_empty() {
            head.push(Span::raw(format!("{} · ", clean(&question.header))).bold());
        }
        head.push(Span::raw(clean(&question.text)).bold());
        body.extend(wrap(
            vec![Span::raw("? ")],
            vec![Span::raw("  ")],
            head,
            width,
        ));
        let pick = if question.multi_select {
            "any of"
        } else {
            "one of"
        };
        let labels: Vec<String> = question.options.iter().map(|o| clean(&o.label)).collect();
        let options =
            Span::raw(format!("{pick}: {} · or your own", labels.join(" / "))).dark_gray();
        body.extend(wrap(
            vec![Span::raw("  ")],
            vec![Span::raw("  ")],
            vec![options],
            width,
        ));
    }
    body.truncate(max_rows.max(2));
    lines.extend(body);
    let keys = match focus {
        Focus::Composer => "Esc, then a answer · x decline",
        Focus::Timeline => "a answer · x decline",
    };
    lines.push(match &prompt.answer {
        None => Line::from(keys).yellow().bold(),
        Some(answer) => {
            let label = match (&answer.outgoing, &answer.delivery) {
                (Outgoing::Queued | Outgoing::Sending, Some(delivery)) => {
                    delivery_label(delivery, &answer.outgoing)
                }
                _ => outgoing_label(&answer.outgoing),
            };
            let words = clean(&reply_text(prompt).unwrap_or_default());
            let mut spans = vec![
                Span::raw(format!("{words} · ")),
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

/// The question picker's title, lines and typing position: the question
/// in view, its options with their descriptions and the row for the
/// user's own text, the highlighted option's preview, then the keys.
fn question_picker(
    prompt: &QuestionPrompt,
    draft: &QuestionDraft,
    selected: usize,
    width: usize,
    rows: usize,
) -> (String, Vec<Line<'static>>, Option<(usize, usize)>) {
    let index = draft.current();
    let Some(question) = prompt.questions.get(index) else {
        return (" Question ".to_owned(), Vec::new(), None);
    };
    let answer = draft.answer(index);
    let picked =
        |i: usize| answer.is_some_and(|a| u8::try_from(i).is_ok_and(|i| a.picked.contains(&i)));
    let mark = |on: bool| match (question.multi_select, on) {
        (true, true) => "[x] ",
        (true, false) => "[ ] ",
        (false, true) => "(•) ",
        (false, false) => "( ) ",
    };
    let mut lines = wrap(
        Vec::new(),
        Vec::new(),
        vec![Span::raw(clean(&question.text)).bold()],
        width,
    );
    let pick = if question.multi_select {
        "pick any, Space toggles"
    } else {
        "pick one"
    };
    lines.push(Line::from(pick).dark_gray());
    let mut items: Vec<Vec<Span<'static>>> = question
        .options
        .iter()
        .enumerate()
        .map(|(i, option)| {
            let mut spans = vec![Span::raw(mark(picked(i))), Span::raw(clean(&option.label))];
            if !option.description.is_empty() {
                spans.push(Span::raw(format!("  {}", clean(&option.description))).dark_gray());
            }
            spans
        })
        .collect();
    let own = answer.and_then(|a| a.other.as_deref());
    items.push(match own {
        Some(text) => vec![
            Span::raw(mark(true)),
            Span::raw(format!("Other: {}", clean(text))),
        ],
        None => vec![
            Span::raw(mark(false)),
            Span::raw("Other: "),
            Span::raw("type your own").dark_gray(),
        ],
    });
    let preview: Vec<Line<'static>> = question
        .options
        .get(selected)
        .and_then(|o| o.preview.as_deref())
        .map(|preview| {
            preview
                .lines()
                .take(rows / 2)
                .map(|line| Line::from(format!("  {}", clean(line))).dark_gray())
                .collect()
        })
        .unwrap_or_default();
    let room = rows.saturating_sub(lines.len() + preview.len() + 1).max(1);
    let list = list_lines(items, selected, room);
    let typing = selected == question.options.len();
    let input = typing.then(|| {
        let row = lines.len() + list.len().saturating_sub(1);
        let column = 2 + 4 + "Other: ".len() + own.map_or(0, |t| clean(t).chars().count());
        (row, column)
    });
    lines.extend(list);
    lines.extend(preview);
    let next = if draft.first_unanswered(&prompt.questions).is_none() {
        "Enter send"
    } else {
        "Enter next"
    };
    let steps = if prompt.questions.len() > 1 {
        " · ←→ question"
    } else {
        ""
    };
    lines.push(
        Line::from(format!("↑↓ move · {next}{steps} · Esc close"))
            .yellow()
            .bold(),
    );
    let of = if prompt.questions.len() > 1 {
        format!(" {}/{}", index + 1, prompt.questions.len())
    } else {
        String::new()
    };
    let title = format!(" Question{of} · {} ", clean(question_label(question)));
    (title, lines, input)
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

/// First, that the server could not keep some of what the call will do, if
/// so: the user decides on what is shown. Then the first section shown
/// while collapsed, capped to `max_rows`; with none, where to see the whole
/// input if there is one. Last, that the approval asked about an input
/// other than the call's, and where to see it.
fn gated_content(
    prompt: &ApprovalPrompt,
    sections: &[Section],
    width: usize,
    max_rows: usize,
) -> Vec<Line<'static>> {
    let mut out = call_content(prompt, sections, width, max_rows);
    if let Some(note) = asked_note(sections) {
        let hint = Span::raw(format!("{note} · v to see it")).yellow();
        out.extend(wrap(
            vec![Span::raw("  ")],
            vec![Span::raw("  ")],
            vec![hint],
            width,
        ));
    }
    out
}

/// [`gated_content`] up to what the approval asked about.
fn call_content(
    prompt: &ApprovalPrompt,
    sections: &[Section],
    width: usize,
    max_rows: usize,
) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = approval_lost(prompt, sections)
        .map(|lost| {
            let warning = Span::raw(lost).yellow().bold();
            wrap(
                vec![Span::raw("! ").yellow().bold()],
                vec![Span::raw("  ")],
                vec![warning],
                width,
            )
        })
        .unwrap_or_default();
    if let Some(section) = sections.iter().find(|s| !s.hidden) {
        out.extend(preview_lines(section, width, max_rows));
        return out;
    }
    out.extend(
        sections
            .iter()
            .filter(|s| !s.asked)
            .find_map(Section::blob)
            .map(input_line),
    );
    out
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
            let modes = offered_modes(agent.bypass_allowed);
            let mode = modes[(*selected).min(modes.len() - 1)];
            let lines = vec![
                Line::from(format!("Switch to {}?", mode_label(mode))).bold(),
                Line::from(format!("{}.", capitalised(mode_detail(mode)))).yellow(),
                Line::from("y switch · any other key back").yellow().bold(),
            ];
            (" Permission mode ".to_owned(), lines, None)
        }
        Picker::Mode { selected, .. } => {
            let modes = offered_modes(agent.bypass_allowed).into_iter().map(|mode| {
                let mut spans = vec![
                    Span::raw(format!("{:<14}", mode_label(mode))),
                    Span::raw(mode_detail(mode)).dark_gray(),
                ];
                if agent.mode == Some(mode) {
                    spans.push(Span::raw(" · now").green());
                }
                spans
            });
            let lines = list_lines(modes.collect(), *selected, rows);
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
        Picker::Agent { selected } => {
            let model = agent_model_label(agent).unwrap_or_else(|| "unknown".to_owned());
            let effort = agent_effort_label(agent).unwrap_or_else(|| "unknown".to_owned());
            let items = vec![
                vec![Span::raw(format!("{:<8}{}", "model", clean(&model)))],
                vec![Span::raw(format!(
                    "{:<8}{}",
                    "effort",
                    clean(effort.strip_prefix("effort ").unwrap_or(&effort))
                ))],
            ];
            (
                " Agent ".to_owned(),
                list_lines(items, *selected, rows),
                None,
            )
        }
        Picker::Contexts {
            moving: Some(id), ..
        } => {
            let name = session
                .contexts
                .iter()
                .find(|c| c.id == *id)
                .map(|c| clean(&context_row(c)))
                .unwrap_or_default();
            let lines = vec![
                Line::from(format!("Move {name} to a new session?")).bold(),
                Line::from("It goes on there; here it stays as it was.").yellow(),
                Line::from("y move · any other key back").yellow().bold(),
            ];
            (" Contexts ".to_owned(), lines, None)
        }
        Picker::Contexts { selected, note, .. } => {
            let new = if session.can_clear {
                Span::raw("New context")
            } else {
                Span::raw("New context (this one is empty)").dark_gray()
            };
            let mut items = vec![vec![new]];
            items.extend(session.contexts.iter().map(|context| {
                let mut spans = vec![Span::raw(clean(&context_row(context)))];
                if context.current {
                    spans.push(Span::raw(" · current").green());
                }
                if let Some(outgoing) = &context.outgoing {
                    spans.push(Span::raw(" "));
                    spans.push(Span::styled(
                        outgoing_label(outgoing),
                        outgoing_badge(outgoing).1,
                    ));
                }
                if let Some(moved) = context_moved_label(context) {
                    spans.push(Span::raw(format!(" · {}", clean(&moved))).dark_gray());
                }
                spans
            }));
            let selected = (*selected).min(items.len() - 1);
            // The reason `M` was refused takes the last row.
            let rows = if note.is_some() {
                rows.saturating_sub(1)
            } else {
                rows
            };
            let mut lines = list_lines(items, selected, rows.max(1));
            if let Some(note) = note {
                lines.push(Line::from(clean(note)).yellow());
            }
            (
                " Contexts · Enter switches · v views · M moves ".to_owned(),
                lines,
                None,
            )
        }
        Picker::Stop(StopKind::Task(id)) => {
            let title = session
                .subagent
                .as_ref()
                .filter(|s| s.status.task.as_ref() == Some(id))
                .map(|s| clean(&s.title))
                .unwrap_or_default();
            let lines = vec![
                Line::from(format!("{} {title}", stop_heading(StopAction::Task))).bold(),
                Line::from("Its work ends; the main agent goes on.").yellow(),
                Line::from("y stops it · any other key back")
                    .yellow()
                    .bold(),
            ];
            (" Subagent ".to_owned(), lines, None)
        }
        Picker::Stop(kind) => {
            let mut lines = vec![Line::from(stop_heading(kind.stop_action())).bold()];
            if let Some(warning) = stop_warning(&session.work) {
                lines.push(Line::from(clean(&warning)).yellow());
            }
            lines.push(Line::from("y go on · any other key back").yellow().bold());
            (" Background work ".to_owned(), lines, None)
        }
        Picker::Background {
            stopping: Some(id), ..
        } => {
            let title = session
                .work
                .tasks
                .iter()
                .find(|t| t.id == *id)
                .map(|t| clean(&t.title))
                .unwrap_or_default();
            let lines = vec![
                Line::from(format!("Stop {title}?")).bold(),
                Line::from("y stops it · any other key back")
                    .yellow()
                    .bold(),
            ];
            (" Background work ".to_owned(), lines, None)
        }
        Picker::Background { selected, .. } => {
            let work_rows = background_rows(&session.work);
            let mut items: Vec<Vec<Span<'static>>> = Vec::new();
            for row in &work_rows {
                items.push(match row {
                    BackgroundRow::Task(task, depth) => {
                        let kind = task_kind_word(task.kind);
                        let mut spans = vec![
                            Span::raw("  ".repeat(*depth)),
                            Span::raw(format!("{kind:<9}")).dark_gray(),
                            Span::raw(clean(&task.title)),
                        ];
                        if task.ambient {
                            spans.push(Span::raw(format!(" · {AMBIENT_WORD}")).dark_gray());
                        }
                        if let Some((_, outgoing)) =
                            session.task_stops.iter().find(|(id, _)| *id == task.id)
                        {
                            spans.push(Span::raw(" stop "));
                            spans.push(Span::styled(
                                outgoing_label(outgoing),
                                outgoing_badge(outgoing).1,
                            ));
                        }
                        spans
                    }
                    BackgroundRow::Loop(schedule) => {
                        let mut spans = vec![
                            Span::raw(format!("loop {} ", clean(&schedule.when))),
                            Span::raw(repeat_word(schedule.recurring)).dark_gray(),
                            Span::raw(format!(" {}", clean(&schedule.prompt))),
                        ];
                        if schedule.outlives_agent == Some(true) {
                            spans.push(Span::raw(format!(" · {SAVED_IN_FOLDER}")).dark_gray());
                        }
                        spans
                    }
                });
            }
            let title = format!(
                " Background work · {} ",
                work_summary(&session.work).unwrap_or_else(|| "nothing running".to_owned())
            );
            let lines = if items.is_empty() {
                vec![Line::from(NOTHING_RUNNING).dark_gray()]
            } else {
                let selected = (*selected).min(items.len() - 1);
                let mut lines = list_lines(items, selected, rows.saturating_sub(1).max(1));
                if !session.work.schedules.is_empty() {
                    lines.push(Line::from(LOOPS_NOTE).dark_gray());
                }
                lines
            };
            (title, lines, None)
        }
        Picker::Question { draft, selected } => match session
            .pending_question
            .as_ref()
            .filter(|p| p.id == draft.id())
        {
            Some(prompt) => question_picker(prompt, draft, *selected, below.width as usize, rows),
            // Answered or withdrawn: the next key closes it.
            None => return,
        },
        Picker::Effort { selected } => {
            let efforts = agent.efforts();
            let lines = if efforts.is_empty() {
                let model = agent_model_label(agent).unwrap_or_else(|| "this model".to_owned());
                vec![Line::from(format!("{} takes no effort level", clean(&model))).dark_gray()]
            } else {
                let items = efforts
                    .iter()
                    .map(|effort| {
                        let mut spans = vec![Span::raw(effort_label(*effort))];
                        if agent.effort == Some(Some(*effort)) {
                            spans.push(Span::raw(" · now").green());
                        }
                        spans
                    })
                    .collect();
                list_lines(items, (*selected).min(efforts.len() - 1), rows)
            };
            let now = agent_effort_label(agent).unwrap_or_else(|| "unknown".to_owned());
            (format!(" Effort · now {} ", clean(&now)), lines, None)
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

/// The shell composer's prompt, in its border: the cwd's last part, the
/// condition picked with Ctrl-O, then `$`, e.g. `skywave [if ok ✓] $`.
/// The condition is never part of the text written.
fn shell_prompt_line(session: Option<&SessionView>, when: RunWhen) -> String {
    clean(&shell_prompt(session.and_then(|s| s.shell.cwd.as_deref()), when).to_string())
}

fn render_composer(frame: &mut Frame, area: Rect, app: &App, ui: &SessionUi) {
    let focused = ui.focus == Focus::Composer;
    let session = app.session().map(|(view, _)| view);
    let shell = ui.shell();
    let mut title = if shell {
        format!(" {} ", shell_prompt_line(session, ui.when))
    } else {
        " Message ".to_owned()
    };
    if !shell && session.is_some_and(|view| view.subagent.is_some()) {
        title.push_str("· to the main agent ");
    }
    if matches!(app.view().link, Link::Reconnecting { .. }) {
        title.push_str("· offline, sends when back online ");
    }
    // Shell mode in its own colour, so a command is not sent as a message.
    let border = match (shell, focused) {
        (true, true) => Style::new().magenta(),
        (true, false) => Style::new().magenta().dim(),
        (false, true) => Style::new().cyan(),
        (false, false) => Style::new().dark_gray(),
    };
    let block = Block::bordered().title(title).border_style(border);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let draft = &ui.composer;
    if draft.text().is_empty() {
        let placeholder = if shell {
            "Type a shell command for the server…"
        } else {
            "Write to the agent…"
        };
        frame.render_widget(Paragraph::new(Line::from(placeholder).dark_gray()), inner);
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
