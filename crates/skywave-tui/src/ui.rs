//! Pure rendering: draws an [`App`] into a frame. No state changes, no I/O;
//! `now` is passed in so the retry countdown renders deterministically.

use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use skywave_core::{
    ApprovalStatus, BlobPolicy, Decision, Entry, Link, LinkRejection, Outgoing, PairingStatus,
    PairingView, Screen, SessionListView, SessionState, SessionView, SettingsView, TimelineItem,
};

use crate::app::{App, Field};

/// Draws the whole UI: link banner, current screen, footer, help overlay.
pub fn render(frame: &mut Frame, app: &App, now: Instant) {
    let [banner, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let view = app.view();
    let link_age = now.saturating_duration_since(app.ui().link_since);
    frame.render_widget(
        Paragraph::new(banner_text(&view.link, view.queued, link_age))
            .style(banner_style(&view.link)),
        banner,
    );

    if app.revoked_screen() {
        render_revoked(frame, body);
    } else {
        match &view.screen {
            Screen::Pairing(pairing) => render_pairing(frame, body, app, pairing),
            Screen::SessionList(list) => render_session_list(frame, body, app, list),
            Screen::Session(session) => render_session(frame, body, session),
            Screen::Settings(settings) => render_settings(frame, body, settings),
        }
    }

    let footer_line = match &app.ui().error {
        Some(error) => Line::from(format!("error: {error}")).red(),
        None => Line::from(footer_hint(app)).dark_gray(),
    };
    frame.render_widget(Paragraph::new(footer_line), footer);

    if app.ui().help {
        render_help(frame, app);
    }
}

/// The link banner's text. `link_age` is how long ago the current `Link`
/// arrived; the retry countdown is `next_in` minus that.
pub fn banner_text(link: &Link, queued: usize, link_age: Duration) -> String {
    let queued_part = (queued > 0).then(|| format!(" · {queued} queued"));
    let queued_part = queued_part.as_deref().unwrap_or("");
    match link {
        Link::Connecting => format!("connecting…{queued_part}"),
        Link::Online { latency_ms } => format!("online · {latency_ms} ms{queued_part}"),
        Link::CatchingUp { have, server } => {
            format!("syncing · {}/{}{queued_part}", have.0, server.0)
        }
        Link::Reconnecting {
            attempt, next_in, ..
        } => {
            let left = next_in.saturating_sub(link_age);
            let retry = if left.is_zero() {
                "retrying…".to_owned()
            } else {
                // Round up: "retry in 1s" until the retry actually happens.
                format!("retry in {}s", left.as_millis().div_ceil(1000))
            };
            format!("offline{queued_part} · {retry} (attempt {attempt})")
        }
        Link::Rejected(LinkRejection::Revoked) => {
            format!("device revoked · re-pair to reconnect{queued_part}")
        }
        Link::Rejected(LinkRejection::ProtocolMismatch { client, server }) => format!(
            "protocol mismatch · client v{client}, server v{server} · update to reconnect{queued_part}"
        ),
    }
}

fn banner_style(link: &Link) -> Style {
    let (fg, bg) = match link {
        Link::Online { .. } => (Color::Black, Color::Green),
        Link::Connecting | Link::CatchingUp { .. } => (Color::Black, Color::Cyan),
        Link::Reconnecting { .. } => (Color::Black, Color::Yellow),
        Link::Rejected(_) => (Color::White, Color::Red),
    };
    Style::new().fg(fg).bg(bg)
}

fn render_revoked(frame: &mut Frame, area: Rect) {
    let text = vec![
        Line::from("This device was revoked.").bold(),
        Line::from(""),
        Line::from("The server no longer accepts this device. Local data stays readable."),
        Line::from(""),
        Line::from("p / Enter  re-pair"),
        Line::from("Esc        browse local data"),
        Line::from("q          quit"),
    ];
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .block(Block::bordered().title(" Revoked ").red()),
        area,
    );
}

fn render_pairing(frame: &mut Frame, area: Rect, app: &App, pairing: &PairingView) {
    let block = Block::bordered().title(" Pair this device ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [intro, fields, status] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(Field::ALL.len() as u16),
        Constraint::Min(0),
    ])
    .areas(inner);

    let mut intro_lines = vec![Line::from(
        "Enter what `skywaved pair` printed on the server.",
    )];
    if let Some(info) = &pairing.paired {
        let server = info
            .server_address
            .as_deref()
            .unwrap_or("an unknown server");
        intro_lines.push(Line::from(format!(
            "Currently paired as {} with {server}; pairing again replaces this identity.",
            info.device_id.0
        )));
    }
    frame.render_widget(
        Paragraph::new(intro_lines).wrap(Wrap { trim: false }),
        intro,
    );

    let form = &app.ui().form;
    const LABEL_WIDTH: u16 = 13;
    let rows = Layout::vertical([Constraint::Length(1); 3]).split(fields);
    for (field, row) in Field::ALL.into_iter().zip(rows.iter()) {
        let focused = field == form.focus;
        let marker = if focused { "> " } else { "  " };
        let label = format!(
            "{marker}{:<w$}",
            format!("{}:", field.label()),
            w = (LABEL_WIDTH - 2) as usize
        );
        let value_style = if focused {
            Style::new().add_modifier(Modifier::UNDERLINED)
        } else {
            Style::new()
        };
        let line = Line::from(vec![
            Span::raw(label),
            Span::styled(form.value(field).to_owned(), value_style),
        ]);
        frame.render_widget(Paragraph::new(line), *row);
        if focused && app.typing() && !app.ui().help {
            let width = form.value(field).chars().count() as u16;
            let x = (row.x + LABEL_WIDTH + width).min(row.right().saturating_sub(1));
            frame.set_cursor_position(Position::new(x, row.y));
        }
    }

    let status_line = match &pairing.status {
        PairingStatus::Idle => Line::from(""),
        PairingStatus::InProgress { address } => {
            Line::from(format!("Pairing with {address}…")).cyan()
        }
        PairingStatus::Failed { address, reason } => {
            Line::from(format!("Pairing with {address} failed: {reason}")).red()
        }
    };
    frame.render_widget(
        Paragraph::new(vec![Line::from(""), status_line]).wrap(Wrap { trim: false }),
        status,
    );
}

fn render_session_list(frame: &mut Frame, area: Rect, app: &App, list: &SessionListView) {
    let mut lines = Vec::new();
    if list.sessions.is_empty() && list.creating.is_empty() {
        lines.push(Line::from("No sessions yet.").dark_gray());
    }
    for (i, row) in list.sessions.iter().enumerate() {
        let name = row.workdir.clone().unwrap_or_else(|| row.id.0.to_string());
        let mut text = format!(
            "{name}  [{}]  {}",
            state_badge(&row.state),
            row.last_activity.format("%Y-%m-%d %H:%M")
        );
        if row.pending_approvals > 0 {
            text.push_str(&format!("  {} approval(s) pending", row.pending_approvals));
        }
        let line = if i == app.ui().selected {
            Line::from(format!("> {text}")).reversed()
        } else {
            Line::from(format!("  {text}"))
        };
        lines.push(line);
    }
    for pending in &list.creating {
        lines.push(
            Line::from(format!(
                "  {}  [{}]",
                pending.workdir,
                outgoing_label(&pending.outgoing)
            ))
            .dark_gray(),
        );
    }
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" Sessions ")),
        area,
    );
}

fn render_session(frame: &mut Frame, area: Rect, session: &SessionView) {
    let title = session
        .workdir
        .clone()
        .unwrap_or_else(|| session.id.0.to_string());
    let block = Block::bordered().title(format!(" {title} · {} ", state_badge(&session.state)));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line> = session.timeline.iter().flat_map(timeline_lines).collect();
    if let Some(prompt) = &session.pending_approval {
        let answer = match &prompt.answer {
            Some(answer) => format!(
                " · answered {} ({})",
                decision_label(answer.decision),
                outgoing_label(&answer.outgoing)
            ),
            None => String::new(),
        };
        lines.push(
            Line::from(format!(
                "awaiting approval: {} — {}{answer}",
                prompt.tool, prompt.summary
            ))
            .yellow(),
        );
    }
    if lines.is_empty() {
        lines.push(Line::from("Nothing here yet.").dark_gray());
    }
    // Newest at the bottom: show the tail that fits.
    let skip = lines.len().saturating_sub(inner.height as usize);
    frame.render_widget(Paragraph::new(lines.split_off(skip)), inner);
}

fn timeline_lines(item: &TimelineItem) -> Vec<Line<'static>> {
    let (prefix, text) = match item {
        TimelineItem::Pending { text, outgoing, .. } => (
            format!("you [{}]: ", outgoing_label(outgoing)),
            text.clone(),
        ),
        TimelineItem::Logged { entry, .. } => match entry {
            Entry::UserMessage {
                text,
                from,
                own,
                outgoing,
            } => {
                let who = if *own {
                    "you".to_owned()
                } else {
                    format!("device {}", short_id(&from.0.to_string()))
                };
                let state = outgoing
                    .as_ref()
                    .map(|o| format!(" [{}]", outgoing_label(o)))
                    .unwrap_or_default();
                (format!("{who}{state}: "), text.clone())
            }
            Entry::AgentMessage { text } => ("agent: ".to_owned(), text.clone()),
            Entry::ToolCall { name, summary, .. } => (format!("tool {name}: "), summary.clone()),
            Entry::ToolResult { summary, .. } => ("result: ".to_owned(), summary.clone()),
            Entry::Approval {
                tool,
                summary,
                status,
                ..
            } => {
                let status = match status {
                    ApprovalStatus::Pending => "pending".to_owned(),
                    ApprovalStatus::Resolved {
                        decision, by_me, ..
                    } => {
                        let who = if *by_me { "here" } else { "elsewhere" };
                        format!("{} {who}", decision_label(*decision))
                    }
                    ApprovalStatus::Cancelled { reason } => format!("cancelled: {reason}"),
                };
                (format!("approval {tool} [{status}]: "), summary.clone())
            }
            Entry::Interrupted => ("interrupted".to_owned(), String::new()),
            Entry::AgentErrored { message } => ("error: ".to_owned(), message.clone()),
        },
    };
    let mut text_lines = text.lines();
    let first = text_lines.next().unwrap_or("").to_owned();
    let mut lines = vec![Line::from(vec![Span::raw(prefix).bold(), Span::raw(first)])];
    lines.extend(text_lines.map(|l| Line::from(format!("  {l}"))));
    lines
}

fn render_settings(frame: &mut Frame, area: Rect, settings: &SettingsView) {
    let policy = match settings.blob_policy {
        BlobPolicy::Eager => "eager (download every blob)",
        BlobPolicy::OnDemand => "on demand (download when opened)",
    };
    let lines = vec![
        Line::from(format!("Blob policy: {policy}")),
        Line::from(format!(
            "Server:      {}",
            settings.server_address.as_deref().unwrap_or("not paired")
        )),
        Line::from(format!(
            "Device:      {}",
            settings
                .device_id
                .map(|id| id.0.to_string())
                .unwrap_or_else(|| "not paired".to_owned())
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" Settings ")),
        area,
    );
}

/// Key hints for the footer.
fn footer_hint(app: &App) -> &'static str {
    if app.ui().help {
        return "Esc close help";
    }
    if app.revoked_screen() {
        return "p re-pair · Esc browse local data · q quit · ? help";
    }
    match &app.view().screen {
        Screen::Pairing(PairingView {
            paired: Some(_), ..
        }) => "Tab next field · Enter next/submit · Esc cancel · F1 help · Ctrl-C quit",
        Screen::Pairing(_) => "Tab next field · Enter next/submit · F1 help · Ctrl-C quit",
        Screen::SessionList(_) => "↑↓ select · Enter open · s settings · ? help · q quit",
        Screen::Session(_) => "Esc back · ? help · q quit",
        Screen::Settings(_) => "b toggle blob policy · p re-pair · Esc back · ? help · q quit",
    }
}

/// Every key binding, grouped by where it applies.
fn help_lines(app: &App) -> Vec<Line<'static>> {
    let section = |title: &'static str| Line::from(title).bold();
    let key = |keys: &'static str, what: &'static str| Line::from(format!("  {keys:<14}{what}"));
    let mut lines = vec![
        section("Everywhere"),
        key("F1 / ?", "toggle this help (? only outside text fields)"),
        key("q", "quit (outside text fields)"),
        key("Ctrl-C", "quit"),
        Line::from(""),
    ];
    if app.revoked_screen() {
        lines.extend([
            section("Revoked"),
            key("p / Enter", "re-pair"),
            key("Esc", "browse local data"),
        ]);
        return lines;
    }
    match &app.view().screen {
        Screen::Pairing(_) => lines.extend([
            section("Pairing"),
            key("Tab / ↓", "next field"),
            key("Shift-Tab / ↑", "previous field"),
            key("Enter", "next field; submit on the last"),
            key("Backspace", "delete a character"),
            key("Ctrl-U", "clear the field"),
            key("paste", "a skywave://pair?… URI fills all fields"),
            key("Esc", "cancel re-pairing"),
        ]),
        Screen::SessionList(_) => lines.extend([
            section("Sessions"),
            key("↑↓ / k j", "select"),
            key("Enter", "open session"),
            key("s", "settings"),
        ]),
        Screen::Session(_) => lines.extend([section("Session"), key("Esc", "back to sessions")]),
        Screen::Settings(_) => lines.extend([
            section("Settings"),
            key("b", "toggle blob policy"),
            key("p", "re-pair"),
            key("Esc", "back to sessions"),
        ]),
    }
    lines
}

fn render_help(frame: &mut Frame, app: &App) {
    let lines = help_lines(app);
    let height = (lines.len() as u16 + 2).min(frame.area().height);
    let [area] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(frame.area());
    let [area] = Layout::horizontal([Constraint::Length(62)])
        .flex(Flex::Center)
        .areas(area);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" Keys ")),
        area,
    );
}

fn state_badge(state: &SessionState) -> &'static str {
    match state {
        SessionState::Idle => "idle",
        SessionState::AgentWorking => "working",
        SessionState::AwaitingApproval(_) => "needs approval",
        SessionState::Errored(_) => "error",
    }
}

fn outgoing_label(outgoing: &Outgoing) -> String {
    match outgoing {
        Outgoing::Queued => "queued".to_owned(),
        Outgoing::Sending => "sending".to_owned(),
        Outgoing::Accepted(_) => "sent".to_owned(),
        Outgoing::Failed(reason) => format!("failed: {reason:?}"),
    }
}

fn decision_label(decision: Decision) -> &'static str {
    match decision {
        Decision::Approve => "approved",
        Decision::Deny => "denied",
    }
}

fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}
