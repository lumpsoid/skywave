//! Rendering: draws an [`App`] into a frame. `now` is passed in so the
//! retry countdown renders deterministically. The only state written is the
//! timeline geometry the session view records for paging
//! ([`Geometry`](crate::app::Geometry)); the only I/O is reading downloaded
//! blobs to show them.

mod session;

pub use session::{SHOW_BYTES, SHOW_LINES, bytes};

use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Clear, HighlightSpacing, List, ListItem, ListState, Paragraph, Wrap,
};
use skywave_core::{
    BlobPolicy, Decision, FailReason, Link, LinkRejection, Outgoing, PairingStatus, PairingView,
    PendingSession, Screen, SessionListView, SessionRow, SessionState, SettingsView,
};

use crate::app::{App, Field, Focus};

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
            Screen::Session(_) => {
                if let Some((session, ui)) = app.session() {
                    session::render(frame, body, app, session, ui);
                }
            }
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
    let create = app.ui().create.as_ref();
    let creating_height = if list.creating.is_empty() {
        0
    } else {
        list.creating.len() as u16 + 2
    };
    let [sessions_area, creating_area, prompt_area] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Max(creating_height),
        Constraint::Length(if create.is_some() { 3 } else { 0 }),
    ])
    .areas(area);

    let block = Block::bordered().title(format!(" Sessions ({}) ", list.sessions.len()));
    if list.sessions.is_empty() {
        let hint = if list.creating.is_empty() {
            "No sessions yet. Press n to start one."
        } else {
            "No sessions yet."
        };
        frame.render_widget(
            Paragraph::new(Line::from(hint).dark_gray()).block(block),
            sessions_area,
        );
    } else {
        let items: Vec<ListItem> = list.sessions.iter().map(session_row_line).collect();
        let mut state = ListState::default().with_selected(app.selected_index(list));
        frame.render_stateful_widget(
            List::new(items)
                .block(block)
                .highlight_symbol("> ")
                .highlight_spacing(HighlightSpacing::Always)
                .highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
            sessions_area,
            &mut state,
        );
    }

    if !list.creating.is_empty() {
        let lines: Vec<Line> = list.creating.iter().map(pending_session_line).collect();
        frame.render_widget(
            Paragraph::new(lines).block(Block::bordered().title(" Creating ")),
            creating_area,
        );
    }

    if let Some(create) = create {
        let block = Block::bordered()
            .title(" New session · workdir on the server ")
            .cyan();
        let inner = block.inner(prompt_area);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw("> "),
                Span::raw(create.workdir.clone()),
            ]))
            .block(block),
            prompt_area,
        );
        if app.typing() && !app.ui().help {
            let width = create.workdir.chars().count() as u16 + 2;
            let x = (inner.x + width).min(inner.right().saturating_sub(1));
            frame.set_cursor_position(Position::new(x, inner.y));
        }
    }
}

/// Width every badge is padded to, so workdirs line up.
const BADGE_WIDTH: usize = 12;

fn session_row_line(row: &SessionRow) -> ListItem<'static> {
    let name = row.workdir.clone().unwrap_or_else(|| row.id.0.to_string());
    let (badge, style) = state_badge(&row.state);
    let mut spans = vec![
        Span::styled(format!("{:<BADGE_WIDTH$}", format!("[{badge}]")), style),
        Span::raw(" "),
        Span::raw(name),
    ];
    if row.pending_approvals > 0 {
        let noun = if row.pending_approvals == 1 {
            "approval"
        } else {
            "approvals"
        };
        spans.push(
            Span::raw(format!("  ! {} {noun} pending", row.pending_approvals))
                .yellow()
                .bold(),
        );
    }
    if let SessionState::Errored(info) = &row.state {
        spans.push(Span::raw(format!("  {}", info.message)).red());
    }
    spans.push(
        Span::raw(format!(
            "  · {}",
            row.last_activity.format("%Y-%m-%d %H:%M UTC")
        ))
        .dark_gray(),
    );
    ListItem::new(Line::from(spans))
}

fn pending_session_line(pending: &PendingSession) -> Line<'static> {
    let (badge, style) = outgoing_badge(&pending.outgoing);
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(format!("{:<BADGE_WIDTH$}", format!("[{badge}]")), style),
        Span::raw(" "),
        Span::raw(pending.workdir.clone()),
    ];
    match pending.outgoing {
        Outgoing::Queued => spans.push(Span::raw("  · waits for the server").dark_gray()),
        Outgoing::Sending => {}
        Outgoing::Accepted(_) => spans.push(Span::raw("  · created, syncing…").dark_gray()),
        Outgoing::Failed(reason) => {
            spans.push(Span::raw(format!("  {}", fail_reason(reason))).red())
        }
    }
    Line::from(spans)
}

fn render_settings(frame: &mut Frame, area: Rect, settings: &SettingsView) {
    let policy = match settings.blob_policy {
        BlobPolicy::Eager => "eager (download every blob)",
        BlobPolicy::OnDemand => "on demand (download when opened)",
    };
    let mark = |policy: BlobPolicy| {
        if settings.blob_policy == policy {
            "(•)"
        } else {
            "( )"
        }
    };
    let lines = vec![
        Line::from(format!("Blob policy: {policy}")),
        Line::from(""),
        Line::from("  Blobs hold full tool inputs and outputs; sessions sync without them.")
            .dark_gray(),
        Line::from(format!(
            "  {} On demand  a blob downloads when you expand its tool output",
            mark(BlobPolicy::OnDemand)
        )),
        Line::from(format!(
            "  {} Eager      every blob downloads as soon as it is synced",
            mark(BlobPolicy::Eager)
        )),
        Line::from("  Press b to switch. Eager uses more data and disk.").dark_gray(),
        Line::from(""),
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
        Screen::SessionList(_) if app.ui().create.is_some() => {
            "Enter create · Esc cancel · Ctrl-U clear · F1 help · Ctrl-C quit"
        }
        Screen::SessionList(_) => {
            "↑↓ select · Enter open · n new session · s settings · ? help · q quit"
        }
        Screen::Session(_) => match app.session().map(|(_, ui)| ui.focus) {
            Some(Focus::Timeline) => {
                "↑↓ select · Enter expand · d download · a/x approve/deny · PgUp/PgDn · Tab write · Esc back · ? help"
            }
            _ => {
                "Enter send · Ctrl-J newline · Ctrl-X interrupt · PgUp/PgDn · Esc timeline · F1 help"
            }
        },
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
        Screen::SessionList(_) if app.ui().create.is_some() => lines.extend([
            section("New session"),
            key("type / paste", "the session's directory on the server"),
            key("Enter", "create the session"),
            key("Backspace", "delete a character"),
            key("Ctrl-U", "clear"),
            key("Esc", "cancel"),
        ]),
        Screen::SessionList(_) => lines.extend([
            section("Sessions"),
            key("↑↓ / k j", "select"),
            key("Enter", "open session"),
            key("n", "new session in a server directory"),
            key("s", "settings"),
            Line::from(""),
            section("Badges"),
            key("[idle]", "agent waiting for a message"),
            key("[working]", "agent busy with a turn"),
            key("[approval]", "agent blocked on an approval"),
            key("[error]", "agent reported an error"),
            key("! N pending", "approvals waiting for an answer"),
        ]),
        Screen::Session(_) => {
            lines.extend([
                section("Session"),
                key("Ctrl-X", "interrupt the agent's turn"),
                key("PgUp / PgDn", "scroll the timeline"),
                key("Tab", "switch between message and timeline"),
                Line::from(""),
            ]);
            // One focus at a time, so the overlay fits a small terminal.
            match app.session().map(|(_, ui)| ui.focus) {
                Some(Focus::Timeline) => lines.extend([
                    section("Timeline (Tab or i: message keys)"),
                    key("↑↓ / k j", "select a tool call, result or approval"),
                    key("Enter / Space", "expand it; downloads its blob if needed"),
                    key("d", "download the selected blob"),
                    key("a / x", "approve / deny the pending approval"),
                    key("Home g/End G", "oldest / newest (End follows new items)"),
                    key("i", "write a message"),
                    key("Esc", "back to sessions"),
                ]),
                _ => lines.extend([
                    section("Message (Esc or Tab: timeline keys)"),
                    key("Enter", "send (queued while offline)"),
                    key("Ctrl-J", "new line (also Alt-Enter, Shift-Enter)"),
                    key("paste", "inserts text, line breaks included"),
                    key("←→↑↓ Home End", "move the cursor"),
                    key("Ctrl-U", "clear the message"),
                    key("Esc", "go to the timeline"),
                ]),
            }
        }
        Screen::Settings(_) => lines.extend([
            section("Settings"),
            key("b", "toggle blob policy: on demand / eager"),
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

/// Short text and colour for a session state; the text alone tells them
/// apart.
pub(crate) fn state_badge(state: &SessionState) -> (&'static str, Style) {
    match state {
        SessionState::Idle => ("idle", Style::new().green()),
        SessionState::AgentWorking => ("working", Style::new().cyan().bold()),
        SessionState::AwaitingApproval(_) => ("approval", Style::new().yellow().bold()),
        SessionState::Errored(_) => ("error", Style::new().red().bold()),
    }
}

/// Short text and colour for a command's delivery state.
pub(crate) fn outgoing_badge(outgoing: &Outgoing) -> (&'static str, Style) {
    match outgoing {
        Outgoing::Queued => ("queued", Style::new().dark_gray()),
        Outgoing::Sending => ("sending", Style::new().cyan()),
        Outgoing::Accepted(_) => ("accepted", Style::new().green()),
        Outgoing::Failed(_) => ("failed", Style::new().red().bold()),
    }
}

pub(crate) fn outgoing_label(outgoing: &Outgoing) -> String {
    match outgoing {
        Outgoing::Queued => "queued".to_owned(),
        Outgoing::Sending => "sending".to_owned(),
        Outgoing::Accepted(_) => "accepted".to_owned(),
        Outgoing::Failed(reason) => format!("failed: {}", fail_reason(*reason)),
    }
}

/// Why the server refused a command, in words.
fn fail_reason(reason: FailReason) -> &'static str {
    match reason {
        FailReason::WorkdirNotAllowed => "not under a configured root on the server",
        FailReason::WorkdirNotFound => "no such directory on the server",
        FailReason::UnknownSession => "the server does not know this session",
        FailReason::UnknownApproval => "the server does not know this approval",
        FailReason::ApprovalAlreadyResolved => "already answered on another device",
        FailReason::NoActiveTurn => "the agent was not working",
    }
}

pub(crate) fn decision_label(decision: Decision) -> &'static str {
    match decision {
        Decision::Approve => "approved",
        Decision::Deny => "denied",
    }
}
