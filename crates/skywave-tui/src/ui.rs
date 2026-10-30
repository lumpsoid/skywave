//! Rendering: draws an [`App`] into a frame. `now` (and, for [`render_at`],
//! the wall clock) is passed in so the retry countdown and running tools'
//! clocks render deterministically. The only state written is the timeline
//! geometry the session view records for paging
//! ([`Geometry`](crate::app::Geometry)); the only I/O is reading downloaded
//! blobs to show them.

mod session;
mod timeline;

pub use timeline::{SHOW_BYTES, SHOW_LINES, bytes};

use std::borrow::Cow;
use std::time::{Duration, Instant, SystemTime};

use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Clear, HighlightSpacing, List, ListItem, ListState, Paragraph, Wrap,
};
use skywave_core::{
    BlobPolicy, CandidateSource, Completion, CompletionStatus, Decision, FailReason, Link,
    LinkRejection, ListKind, Outgoing, PairingStatus, PairingView, PendingSession, RuleScope,
    RuleSuggestion, Screen, SessionListView, SessionRow, SessionState, SettingsView,
};

use crate::app::{App, Field, Focus, POPUP_ROWS, Prompt, PromptKind, dismissable};
use crate::notify;
use crate::timeline::{discardable, wanted_blob};

/// Draws the whole UI: link banner, current screen, footer, help overlay.
pub fn render(frame: &mut Frame, app: &App, now: Instant) {
    render_at(frame, app, now, SystemTime::now());
}

/// [`render`] with the wall clock given: `wall` is what a running tool
/// call's elapsed time counts to.
pub fn render_at(frame: &mut Frame, app: &App, now: Instant, wall: SystemTime) {
    let [banner, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let view = app.view();
    let link_age = now.saturating_duration_since(app.ui().link_since);
    frame.render_widget(
        Paragraph::new(banner_text(
            &view.link,
            view.queued,
            view.attention.elsewhere,
            link_age,
        ))
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
                    session::render(frame, body, app, session, ui, wall);
                }
            }
            Screen::Settings(settings) => render_settings(frame, body, settings),
        }
    }

    let footer_line = match (&app.ui().error, app.delete_row()) {
        (Some(error), _) => Line::from(format!("error: {error}")).red(),
        (None, Some(row)) => Line::from(format!(
            "Delete {} from this device for good? It stays on the server and other devices. y/N",
            notify::sanitise(&row.title)
        ))
        .yellow(),
        (None, None) => match &app.ui().notice {
            Some(notice) => Line::from(notice.as_str()).yellow(),
            None => Line::from(footer_hint(app)).dark_gray(),
        },
    };
    frame.render_widget(Paragraph::new(footer_line), footer);

    if app.ui().help {
        render_help(frame, app);
    }
}

/// The link banner's text, then how many approvals wait outside the open
/// session (`elsewhere`). `link_age` is how long ago the current `Link`
/// arrived; the retry countdown is `next_in` minus that.
pub fn banner_text(link: &Link, queued: usize, elsewhere: usize, link_age: Duration) -> String {
    let mut text = link_text(link, queued, link_age);
    match elsewhere {
        0 => {}
        1 => text.push_str(" · 1 approval elsewhere (^G)"),
        n => text.push_str(&format!(" · {n} approvals elsewhere (^G)")),
    }
    text
}

fn link_text(link: &Link, queued: usize, link_age: Duration) -> String {
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
                // Round up: "retry in 01s" until the retry actually happens. Two
                // digits (the cap is 30s) keep the banner from shifting.
                format!(
                    "retry in {:02}s · Ctrl-R now",
                    left.as_millis().div_ceil(1000)
                )
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
    let prompt = app.ui().prompt.as_ref();
    let creating_height = if list.creating.is_empty() {
        0
    } else {
        list.creating.len() as u16 + 2
    };
    let [sessions_area, creating_area, prompt_area] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Max(creating_height),
        Constraint::Length(if prompt.is_some() { 3 } else { 0 }),
    ])
    .areas(area);

    let count = list.sessions.len();
    let title = match (list.kind, list.other_count) {
        (ListKind::Active, 0) => format!(" Sessions ({count}) "),
        (ListKind::Active, archived) => format!(" Sessions ({count}) · {archived} archived "),
        (ListKind::Archived, active) => format!(" Archived ({count}) · {active} active "),
    };
    let block = Block::bordered().title(title);
    if list.sessions.is_empty() {
        let hint = match list.kind {
            ListKind::Archived => "No archived sessions. Press A or Esc for the active ones.",
            ListKind::Active if !list.creating.is_empty() => "No sessions yet.",
            ListKind::Active if list.other_count > 0 => {
                "No active sessions. Press n to start one, A for the archived ones."
            }
            ListKind::Active => "No sessions yet. Press n to start one.",
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
        let title = if dismissable(list).is_some() {
            " Creating · Del dismiss "
        } else {
            " Creating "
        };
        frame.render_widget(
            Paragraph::new(lines).block(Block::bordered().title(title)),
            creating_area,
        );
    }

    if let Some(prompt) = prompt {
        render_prompt(frame, prompt_area, app, prompt);
        if let Some((completion, selected)) = app.completion_popup() {
            render_completion(frame, area, prompt_area, app, completion, selected);
        }
    }
}

/// An open [`Prompt`] in `area`, three rows high, with the cursor at the end
/// of its text.
pub(crate) fn render_prompt(frame: &mut Frame, area: Rect, app: &App, prompt: &Prompt) {
    let title = match prompt.kind {
        PromptKind::Create => " New session · workdir on the server ",
        PromptKind::Rename(_) => " Rename session · empty restores the default title ",
    };
    let block = Block::bordered().title(title).cyan();
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw("> "),
            Span::raw(prompt.text.clone()),
        ]))
        .block(block),
        area,
    );
    if app.typing() && !app.ui().help {
        let width = prompt.text.chars().count() as u16 + 2;
        let x = (inner.x + width).min(inner.right().saturating_sub(1));
        frame.set_cursor_position(Position::new(x, inner.y));
    }
}

/// The workdir candidates, in a popup just above the new-session prompt.
fn render_completion(
    frame: &mut Frame,
    area: Rect,
    prompt: Rect,
    app: &App,
    completion: &Completion,
    selected: usize,
) {
    let rows = completion.candidates.len().clamp(1, POPUP_ROWS) as u16 + 2;
    let top = prompt.y.saturating_sub(rows).max(area.y);
    let popup = Rect::new(prompt.x, top, prompt.width, prompt.y - top);
    let offline = matches!(
        app.view().link,
        Link::Reconnecting { .. } | Link::Rejected(_)
    );
    let status = match completion.status {
        CompletionStatus::Loading => Some("listing…"),
        CompletionStatus::Unavailable if offline => Some("offline · recent only"),
        CompletionStatus::Unavailable => Some("unavailable · recent only"),
        CompletionStatus::Ready { truncated: true } => Some("more not shown · keep typing"),
        CompletionStatus::Ready { truncated: false } => None,
    };
    let mut block = Block::bordered().title(" Directories ").cyan();
    if let Some(status) = status {
        block = block.title_bottom(Line::from(format!(" {status} ")).dark_gray());
    }
    frame.render_widget(Clear, popup);
    if completion.candidates.is_empty() {
        let note = if completion.status == CompletionStatus::Loading {
            ""
        } else {
            "no matching directories"
        };
        frame.render_widget(
            Paragraph::new(Line::from(note).dark_gray()).block(block),
            popup,
        );
        return;
    }
    let items: Vec<ListItem> = completion
        .candidates
        .iter()
        .map(|candidate| {
            let path: String = candidate.path.chars().filter(|c| !c.is_control()).collect();
            let mut spans = vec![Span::raw(path).reset()];
            if candidate.source == CandidateSource::Recent {
                spans.push(Span::raw("  recent").dark_gray());
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(
        List::new(items)
            .block(block)
            .highlight_symbol("> ")
            .highlight_spacing(HighlightSpacing::Always)
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
        popup,
        &mut state,
    );
}

/// Width every badge is padded to, so workdirs line up.
const BADGE_WIDTH: usize = 12;

fn session_row_line(row: &SessionRow) -> ListItem<'static> {
    let (badge, style) = state_badge(&row.state);
    let mut spans = vec![
        Span::styled(format!("{:<BADGE_WIDTH$}", format!("[{badge}]")), style),
        Span::raw(" "),
        Span::raw(timeline::clean(&row.title)).bold(),
    ];
    spans.extend(rename_badge(row.rename.as_ref()));
    if let Some(workdir) = &row.workdir {
        spans.push(Span::raw(format!("  {}", timeline::clean(workdir))).dark_gray());
    }
    // News since this device last looked.
    if row.unread > 0 {
        spans.push(Span::raw(format!("  ● {}", row.unread)).magenta().bold());
    }
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
    if row.has_draft {
        spans.push(Span::raw("  ✎ draft").dark_gray());
    }
    // What the session has cost, when its agent says.
    let cost = row.cost_usd.map_or_else(String::new, |cost| {
        format!("{} · ", timeline::dollars(cost))
    });
    spans.push(
        Span::raw(format!(
            "  · {cost}{}",
            row.last_activity.format("%Y-%m-%d %H:%M UTC")
        ))
        .dark_gray(),
    );
    ListItem::new(Line::from(spans))
}

/// " queued" after a title this device's rename has not settled yet; why a
/// rename failed.
pub(crate) fn rename_badge(rename: Option<&Outgoing>) -> Option<Span<'static>> {
    match rename? {
        Outgoing::Accepted(_) => None,
        Outgoing::Failed(reason) => {
            Some(Span::raw(format!("  rename failed: {}", fail_reason(*reason))).red())
        }
        pending => {
            let (label, style) = outgoing_badge(pending);
            Some(Span::styled(format!(" {label}"), style))
        }
    }
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

/// Key hints for the footer, the search prompt while it is open, and where
/// the composer is while it shows an earlier prompt.
fn footer_hint(app: &App) -> Cow<'static, str> {
    if !app.ui().help
        && let Some((view, ui)) = app.session()
        && let Some(search) = &ui.search
    {
        if search.editing {
            let partial = search
                .partial()
                .map_or(String::new(), |partial| format!(" · {partial}"));
            return format!(
                "/{}▏ · {}{partial} · Enter jump · Esc cancel",
                search.query.as_str(),
                search.count(view)
            )
            .into();
        }
        if ui.focus == Focus::Timeline {
            return "n/N older/newer match · / new search · Esc clear search · ↑↓ select · Enter expand · Tab write · ? help".into();
        }
    }
    if !app.ui().help
        && let Some((_, ui)) = app.session()
        && ui.focus == Focus::Composer
        && let Some((at, of)) = ui.history_position()
    {
        return format!("history {at}/{of} · Ctrl-P/N older/newer · edit to keep · Enter send")
            .into();
    }
    let keys = footer_keys(app);
    // `d` says how much it would fetch.
    if !app.ui().help
        && let Some((view, ui)) = app.session()
        && ui.focus == Focus::Timeline
        && let Some(size) = ui
            .selected_entry(view)
            .and_then(wanted_blob)
            .and_then(|blob| timeline::blob_size(&blob.state))
    {
        return keys
            .replacen("d download", &format!("d download ({})", bytes(size)), 1)
            .into();
    }
    keys.into()
}

fn footer_keys(app: &App) -> &'static str {
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
        Screen::SessionList(_) if app.completion_popup().is_some() => {
            "↑↓ select · Tab/Enter accept · Esc close list · F1 help · Ctrl-C quit"
        }
        Screen::SessionList(_) | Screen::Session(_)
            if app
                .ui()
                .prompt
                .as_ref()
                .is_some_and(|p| matches!(p.kind, PromptKind::Rename(_))) =>
        {
            "Enter rename (empty: default title) · Esc cancel · Ctrl-U clear · F1 help · Ctrl-C quit"
        }
        Screen::SessionList(_) if app.ui().prompt.is_some() => {
            "Tab complete · Enter create · Esc cancel · Ctrl-U clear · F1 help · Ctrl-C quit"
        }
        Screen::SessionList(list) if list.kind == ListKind::Archived => {
            // `↑↓ select` makes way for `D` in 100 columns.
            "Enter open · a unarchive · D delete here · r rename · A/Esc active sessions · ? help · q quit"
        }
        Screen::SessionList(_) => {
            // `↑↓ select` makes way for the archive keys in 100 columns.
            "Enter open · n new session · r rename · a archive · A archived · s settings · ? help · q quit"
        }
        Screen::Session(_) => match app.session().map(|(view, ui)| (view, ui, ui.focus)) {
            // A pending message only takes these keys while the core says it
            // can be taken back.
            Some((view, ui, Focus::Timeline)) if ui.rule_pick(view).is_some() => {
                "↑↓ choose · Enter approve and always allow · Esc back to a/x · ? help"
            }
            Some((view, ui, Focus::Timeline)) => match discardable(view, ui.selected) {
                Some((_, Outgoing::Failed(_))) => {
                    "e edit · Del dismiss · ↑↓ select · a/x approve/deny · [ ] turns · PgUp/PgDn · S stop · Tab write · Esc back · ? help"
                }
                Some(_) => {
                    "e edit · Del cancel · ↑↓ select · a/x approve/deny · [ ] turns · PgUp/PgDn · S stop · Tab write · Esc back · ? help"
                }
                None if view
                    .pending_approval
                    .as_ref()
                    .is_some_and(|p| !p.suggestions.is_empty()) =>
                {
                    "a/x approve/deny · A always allow · v show the call · ↑↓ select · Enter expand · d download · y/Y copy · / search · [ ] turns · PgUp/PgDn · R rename · S stop · Tab write · Esc back · ? help"
                }
                None if view.pending_approval.is_some() => {
                    "a/x approve/deny · v show the call · ↑↓ select · Enter expand · d download · y/Y copy · / search · [ ] turns · PgUp/PgDn · R rename · S stop · Tab write · Esc back · ? help"
                }
                None => {
                    "↑↓ select · Enter expand · d download · y/Y copy · / search · a/x approve/deny · [ ] turns · PgUp/PgDn · R rename · S stop · Tab write · Esc back · ? help"
                }
            },
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
        key("q / Ctrl-C", "quit (q only outside text fields)"),
        key("Ctrl-R", "retry now while offline"),
        key("Ctrl-G", "open the next session waiting for an approval"),
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
        Screen::SessionList(_) | Screen::Session(_)
            if app
                .ui()
                .prompt
                .as_ref()
                .is_some_and(|p| matches!(p.kind, PromptKind::Rename(_))) =>
        {
            lines.extend([
                section("Rename session"),
                key("type / paste", "the session's new name"),
                key("Enter", "rename; empty restores the default title"),
                key("Backspace", "delete a character"),
                key("Ctrl-U", "clear"),
                key("Esc", "cancel"),
            ])
        }
        Screen::SessionList(_) if app.ui().prompt.is_some() => lines.extend([
            section("New session"),
            key("type / paste", "the session's directory on the server"),
            key("Tab", "complete the directory under the roots"),
            key("↑↓ Tab/Enter", "pick from the list"),
            key("Enter", "create the session"),
            key("Backspace", "delete a character"),
            key("Ctrl-U", "clear"),
            key("Esc", "close the list, then cancel"),
        ]),
        Screen::SessionList(_) => lines.extend([
            section("Sessions"),
            key("↑↓ / k j", "select"),
            key("Enter", "open session"),
            key("n", "new session in a server directory"),
            key("r", "rename the selected session"),
            key("a", "archive it (on the archived list: bring it back)"),
            key("A", "switch between active and archived sessions"),
            key("Esc", "from the archived list: back to the active one"),
            key("D", "archived list: delete here only (y/N)"),
            key("Del", "dismiss the oldest refused new session"),
            key("s", "settings"),
            Line::from(""),
            section("Badges"),
            key("[idle]", "agent waiting for a message"),
            key("[working]", "agent busy with a turn"),
            key("[approval]", "agent blocked on an approval"),
            key("[error]", "agent reported an error"),
            key("[stopped]", "agent stopped; a message restarts it"),
            key("! N pending", "approvals waiting for an answer"),
            Line::from(""),
            section("Archive"),
            Line::from("  Archived sessions open and work as usual; a message from any"),
            Line::from("  device brings one back. Archiving does not stop the agent."),
            Line::from("  D deletes one from this device for good; it stays on the"),
            Line::from("  server and other devices, and later use elsewhere is not"),
            Line::from("  shown here. Not while it waits on an approval, a turn or a"),
            Line::from("  command from this device: answer or cancel first."),
        ]),
        Screen::Session(_) => {
            lines.extend([
                section("Session"),
                key("Ctrl-X", "interrupt the agent's turn"),
                key("Ctrl-F", "search the timeline (also / on it)"),
                key("PgUp / PgDn", "scroll the timeline"),
                key("Tab", "switch between message and timeline"),
                Line::from(""),
            ]);
            lines.extend(rules_lines(app));
            // One focus at a time, so the overlay fits a small terminal.
            match app.session().map(|(_, ui)| ui.focus) {
                Some(Focus::Timeline) => lines.extend([
                    section("Timeline (Tab or i: message keys)"),
                    key("↑↓ / k j", "select a tool call or agent message"),
                    key("[ / ]", "previous / next turn"),
                    key("Enter / Space", "expand it; downloads its blob if needed"),
                    key(
                        "d",
                        "download the selected blob; a long command's text before its output",
                    ),
                    key("y / Y", "copy its output or text / its command"),
                    key("", "(in tmux: set -g set-clipboard on)"),
                    key("e", "edit your never-sent or refused message"),
                    key(
                        "Del",
                        "cancel your never-sent message, or dismiss a refused one",
                    ),
                    key("a / x", "approve / deny the pending approval"),
                    key("A", "approve, always allowing a rule the agent offers"),
                    key("", "(several: ↑↓ Enter pick one, Esc back to a / x)"),
                    key("v", "select and expand the call it gates"),
                    key("S", "stop the agent; a message restarts it"),
                    key("R", "rename the session"),
                    key("Home g/End G", "oldest / newest (End follows new items)"),
                    key("/", "search; an upper-case letter matches case"),
                    key("n / N", "older / newer match, round the ends"),
                    key("i", "write a message"),
                    key("Esc", "clear the search, else back to sessions"),
                ]),
                _ => lines.extend([
                    section("Message (Esc or Tab: timeline keys)"),
                    key("Enter", "send (queued while offline)"),
                    key("Ctrl-J", "new line (also Alt-Enter, Shift-Enter)"),
                    key("paste", "inserts text, line breaks included"),
                    key("←→↑↓ Home End", "move the cursor"),
                    key(
                        "Ctrl-P / N",
                        "earlier / later sent message (also ↑ on the first line, ↓ on the last)",
                    ),
                    key("Ctrl-U", "clear the message"),
                    key("Esc", "go to the timeline"),
                ]),
            }
            lines.extend([
                Line::from(""),
                Line::from(
                    "  Status line, right: last turn's time, tokens, cost; session cost; model",
                ),
            ]);
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

/// The rules granted in the open session, read-only: Skywave sees only
/// those granted through it, and cannot take them back.
fn rules_lines(app: &App) -> Vec<Line<'static>> {
    let Some((view, _)) = app.session() else {
        return Vec::new();
    };
    if view.rules.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![Line::from("Rules granted in this session").bold()];
    lines.extend(
        view.rules
            .iter()
            .map(|rule| Line::from(format!("  {}", rule_text(rule)))),
    );
    lines.extend([
        Line::from("  Granted here only; the agent's settings may hold more.").dark_gray(),
        Line::from(""),
    ]);
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
        SessionState::Stopped => ("stopped", Style::new().dark_gray()),
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
        FailReason::UnknownRule => "the agent did not offer that rule",
    }
}

pub(crate) fn decision_label(decision: Decision) -> &'static str {
    match decision {
        Decision::Approve => "approved",
        Decision::Deny => "denied",
    }
}

/// An answer in words: `approved always` when it granted a rule.
pub(crate) fn answer_words(decision: Decision, rule: Option<&RuleSuggestion>) -> &'static str {
    match rule {
        Some(_) => "approved always",
        None => decision_label(decision),
    }
}

/// How far a rule reaches, in words.
fn scope_label(scope: RuleScope) -> &'static str {
    match scope {
        RuleScope::Session => "session",
        RuleScope::Project => "project",
        RuleScope::User => "user",
    }
}

/// A rule as listed: its label, then how far it reaches.
pub(crate) fn rule_text(rule: &RuleSuggestion) -> String {
    format!(
        "{} ({})",
        timeline::clean(&rule.label),
        scope_label(rule.scope)
    )
}
