//! Rendering: draws an [`App`] into a frame. [`render_at`] takes the wall
//! clock, so the retry countdown and running tools' clocks render
//! deterministically. The only state written is the timeline
//! geometry the session view records for paging
//! ([`Geometry`](crate::app::Geometry)) and how far the help overlay can
//! scroll ([`UiState::help_end`](crate::app::UiState::help_end)); the only
//! I/O is reading downloaded blobs to show them.

mod crop;
mod limits;
mod session;
mod shell;
mod storage;
mod timeline;
mod usage;
mod workdir;

use timeline::wall_ms;
pub use timeline::{LayoutStats, SHOW_BYTES, SHOW_LINES, TimelineCache};
pub use workdir::WORKDIR_SHOW_BYTES;

use std::borrow::Cow;
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Clear, HighlightSpacing, List, ListItem, ListState, Paragraph, Wrap,
};
use skywave_core::{
    BlobPolicy, BlobState, CandidateSource, Completion, CompletionStatus, ConversationPicker,
    ConversationsStatus, CropContent, FullView, Link, LinkRejection, ListKind, Outgoing,
    PairingStatus, PairingView, PendingSession, RuleSuggestion, Screen, SessionId, SessionListView,
    SessionRow, SessionState, SettingsView, WorkdirPane,
};
use skywave_present::format::{
    IMPORTING_HISTORY, adopting_note, blob_size, bytes, conversation_detail, conversation_link,
    conversation_title, conversations_empty, conversations_status, dollars, fail_reason,
    scope_label,
};
use skywave_present::link::{self, RepairNotice};
use skywave_present::sections::{discardable, wanted_blob};
use skywave_present::text::clean;

use crate::app::{App, Field, Focus, POPUP_ROWS, Picker, Prompt, PromptKind, dismissable};
use crate::notify;

/// Draws the whole UI: link banner, current screen, footer, help overlay.
pub fn render(frame: &mut Frame, app: &App) {
    render_at(frame, app, SystemTime::now());
}

/// [`render`] with the wall clock given: `wall` is what the retry
/// countdown and a running tool call's elapsed time count to.
pub fn render_at(frame: &mut Frame, app: &App, wall: SystemTime) {
    let [banner, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let view = app.view();
    frame.render_widget(
        Paragraph::new(banner_text(
            &view.link,
            view.queued,
            view.attention.elsewhere,
            wall_ms(wall),
        ))
        .style(banner_style(&view.link)),
        banner,
    );

    if let Some(notice) = app
        .repair_screen()
        .then(|| repair_notice_of(&view.link))
        .flatten()
    {
        render_repair(frame, body, notice, can_retry(&view.link));
    } else {
        match &view.screen {
            Screen::Pairing(pairing) => render_pairing(frame, body, app, pairing),
            Screen::SessionList(list) => render_session_list(frame, body, app, list, wall),
            Screen::Session(_) => {
                if let Some((session, ui)) = app.session() {
                    session::render(frame, body, app, session, ui, wall);
                }
            }
            Screen::Settings(settings) => render_settings(frame, body, app, settings),
            Screen::Usage(usage) => usage::render(frame, body, app, usage, wall),
            Screen::Storage(view) => storage::render(frame, body, app, view),
            Screen::UsageLimits(view) => limits::render(frame, body, view, DateTime::from(wall)),
            Screen::Workdir(workdir) => workdir::render(frame, body, app, workdir, wall),
            Screen::Crop(view) => crop::render(frame, body, app, view),
        }
    }

    let footer_line = match (&app.ui().error, app.delete_row(), &app.ui().confirm) {
        (Some(error), _, _) => Line::from(format!("error: {}", clean(error))).red(),
        (None, Some(row), _) => Line::from(format!(
            "Delete {} from this device for good? It stays on the server and other devices. y/N",
            notify::sanitise(&row.title)
        ))
        .yellow(),
        (None, None, Some(confirm)) => Line::from(format!("{} y/N", confirm.question())).yellow(),
        (None, None, None) => match &app.ui().notice {
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
/// session (`elsewhere`). `wall_ms` is the wall clock in unix-epoch millis;
/// the retry countdown runs from it to the link's `retry_at`.
pub fn banner_text(link: &Link, queued: usize, elsewhere: usize, wall_ms: i64) -> String {
    let mut text = link_text(link, queued, wall_ms);
    match elsewhere {
        0 => {}
        1 => text.push_str(" · 1 approval elsewhere (^G)"),
        n => text.push_str(&format!(" · {n} approvals elsewhere (^G)")),
    }
    text
}

fn link_text(link: &Link, queued: usize, wall_ms: i64) -> String {
    let queued_part = (queued > 0).then(|| format!(" · {queued} queued"));
    let queued_part = queued_part.as_deref().unwrap_or("");
    match link {
        // Nothing to connect to: the pairing screen says what to do.
        Link::Unpaired => String::new(),
        Link::Connecting => format!("connecting…{queued_part}"),
        Link::Online { latency_ms } => format!("online · {latency_ms} ms{queued_part}"),
        Link::CatchingUp { have, server } => {
            format!("syncing · {}/{}{queued_part}", have.0, server.0)
        }
        Link::Reconnecting {
            attempt,
            retry_at,
            reason,
            ..
        } => {
            let left_ms = retry_at.timestamp_millis().saturating_sub(wall_ms);
            let retry = if left_ms <= 0 {
                "retrying…".to_owned()
            } else {
                // Round up: "retry in 01s" until the retry actually happens. Two
                // digits (the cap is 30s) keep the banner from shifting.
                format!(
                    "retry in {:02}s · Ctrl-R now",
                    left_ms.unsigned_abs().div_ceil(1000)
                )
            };
            let reason = link::failure(*reason);
            format!("offline · {reason}{queued_part} · {retry} (attempt {attempt})")
        }
        Link::Rejected(
            rejection @ (LinkRejection::Revoked
            | LinkRejection::ServerChanged
            | LinkRejection::NameMismatch
            | LinkRejection::BadIdentity),
        ) => {
            let short = link::repair_notice(*rejection).map_or("", |n| n.short);
            let retry = if rejection.can_retry() {
                " · Ctrl-R retry"
            } else {
                ""
            };
            format!("{short} · re-pair to reconnect{retry}{queued_part}")
        }
        Link::Rejected(LinkRejection::ProtocolMismatch { client, server }) => format!(
            "protocol mismatch · client v{client}, server v{server} · update, then Ctrl-R{queued_part}"
        ),
        // The replica is the only copy of what the server lost: say so,
        // and how to start over once the user has what they need.
        Link::Rejected(LinkRejection::HistoryLost { have, server }) => format!(
            "server lost history (had {}, server at {}) · sync stopped · restart with --reset-replica to start over{queued_part}",
            have.0, server.0
        ),
    }
}

fn banner_style(link: &Link) -> Style {
    let (fg, bg) = match link {
        Link::Unpaired => return Style::new(),
        Link::Online { .. } => (Color::Black, Color::Green),
        Link::Connecting | Link::CatchingUp { .. } => (Color::Black, Color::Cyan),
        Link::Reconnecting { .. } => (Color::Black, Color::Yellow),
        Link::Rejected(_) => (Color::White, Color::Red),
    };
    Style::new().fg(fg).bg(bg)
}

/// The re-pair notice for `link`, if it stopped for a reason only pairing
/// again fixes.
fn repair_notice_of(link: &Link) -> Option<RepairNotice> {
    match link {
        Link::Rejected(rejection) => link::repair_notice(*rejection),
        _ => None,
    }
}

fn can_retry(link: &Link) -> bool {
    matches!(link, Link::Rejected(rejection) if rejection.can_retry())
}

fn render_repair(frame: &mut Frame, area: Rect, notice: RepairNotice, can_retry: bool) {
    let mut text = vec![
        Line::from(notice.title).bold(),
        Line::from(""),
        Line::from(notice.body),
        Line::from(""),
        Line::from("p / Enter  re-pair"),
    ];
    if can_retry {
        text.push(Line::from("Ctrl-R     try again"));
    }
    text.extend([
        Line::from("Esc        browse local data"),
        Line::from("q          quit"),
    ]);
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .block(Block::bordered().title(" Re-pair ").red()),
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

fn render_session_list(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    list: &SessionListView,
    wall: SystemTime,
) {
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

    if let Some(picker) = &list.conversations {
        render_conversations(frame, sessions_area, app, list, picker, wall);
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

/// The conversation picker over the session list: each conversation the
/// server's agent keeps, newest first, two rows each (what it is, then
/// where and how long), with where Skywave already has it.
fn render_conversations(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    list: &SessionListView,
    picker: &ConversationPicker,
    wall: SystemTime,
) {
    let mut block = Block::bordered()
        .title(" Continue a conversation from outside Skywave ")
        .cyan();
    if let Some(status) = conversations_status(&picker.status) {
        block = block.title_bottom(Line::from(format!(" {status} ")).dark_gray());
    }
    frame.render_widget(Clear, area);
    if let Some(empty) = conversations_empty(picker) {
        let empty = Line::from(clean(&empty));
        let lines = match &picker.status {
            ConversationsStatus::Loading => vec![empty.dark_gray()],
            ConversationsStatus::Unavailable(_) => vec![
                empty.yellow(),
                Line::from("Press p to paste a conversation id instead.").dark_gray(),
            ],
            ConversationsStatus::Ready { .. } => vec![
                empty.dark_gray(),
                Line::from("Press p to paste a conversation id instead.").dark_gray(),
            ],
        };
        frame.render_widget(Paragraph::new(lines).block(block), area);
        return;
    }
    let now = DateTime::<Utc>::from(wall);
    let title = |id: SessionId| {
        list.sessions
            .iter()
            .find(|row| row.id == id)
            .map(|row| row.title.clone())
    };
    let items: Vec<ListItem> = picker
        .items
        .iter()
        .map(|item| {
            let mut head = vec![Span::raw(clean(conversation_title(item))).bold()];
            if let Some(link) = conversation_link(item, title) {
                head.push(Span::raw(format!("  · {}", clean(&link))).green());
            }
            let detail =
                Span::raw(format!("  {}", clean(&conversation_detail(item, now)))).dark_gray();
            ListItem::new(vec![Line::from(head), Line::from(detail)])
        })
        .collect();
    let mut state = ListState::default().with_selected(app.conversation_index(picker));
    frame.render_stateful_widget(
        List::new(items)
            .block(block)
            .highlight_symbol("> ")
            .highlight_spacing(HighlightSpacing::Always)
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
        area,
        &mut state,
    );
}

/// An open [`Prompt`] in `area`, three rows high, with the cursor at the end
/// of its text.
pub(crate) fn render_prompt(frame: &mut Frame, area: Rect, app: &App, prompt: &Prompt) {
    let title = match prompt.kind {
        PromptKind::Create => " New session · workdir on the server ",
        PromptKind::Adopt if prompt.asks_workdir() => {
            " Continue an outside conversation · workdir on the server "
        }
        PromptKind::Adopt => " Continue an outside conversation · agent session id (paste it) ",
        PromptKind::Rename(_) => " Rename session · empty restores the default title ",
        PromptKind::ServerAddress => " Server address · host:port, same server ",
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
        Span::raw(shown_title(&row.title, row.named)).bold(),
    ];
    spans.extend(rename_badge(row.rename.as_ref()));
    if let Some(workdir) = &row.workdir {
        spans.push(Span::raw(format!("  {}", clean(workdir))).dark_gray());
    }
    if row.importing {
        spans.push(Span::raw(format!("  {IMPORTING_HISTORY}")).cyan());
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
        spans.push(Span::raw(format!("  {}", clean(&info.message))).red());
    }
    if row.has_draft {
        spans.push(Span::raw("  ✎ draft").dark_gray());
    }
    // What the session has cost, when its agent says.
    let cost = row
        .cost_usd
        .map_or_else(String::new, |cost| format!("{} · ", dollars(cost)));
    spans.push(
        Span::raw(format!(
            "  · {cost}{}",
            row.last_activity.format("%Y-%m-%d %H:%M UTC")
        ))
        .dark_gray(),
    );
    ListItem::new(Line::from(spans))
}

/// Longest unnamed session's title a line shows, in chars, before an
/// ellipsis: a first prompt can run to 200, and the row's markers follow
/// it on the same line. A name is capped where it is set.
const DEFAULT_TITLE_MAX_CHARS: usize = 48;

/// A session's title as one line shows it: an unnamed session's, its
/// first prompt, cut to [`DEFAULT_TITLE_MAX_CHARS`] with an ellipsis.
pub(crate) fn shown_title(title: &str, named: bool) -> String {
    if named || title.chars().count() <= DEFAULT_TITLE_MAX_CHARS {
        return clean(title);
    }
    let head: String = title.chars().take(DEFAULT_TITLE_MAX_CHARS - 1).collect();
    clean(&format!("{}…", head.trim_end()))
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
        Span::raw(clean(&pending.workdir)),
    ];
    if let Some(note) = adopting_note(pending) {
        spans.push(Span::raw(format!("  · {note}")).cyan());
        return Line::from(spans);
    }
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

fn render_settings(frame: &mut Frame, area: Rect, app: &App, settings: &SettingsView) {
    let prompt = app.ui().prompt.as_ref();
    let [area, prompt_area] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(if prompt.is_some() { 3 } else { 0 }),
    ])
    .areas(area);
    let mark = |policy: BlobPolicy| {
        if settings.blob_policy == policy {
            "(•)"
        } else {
            "( )"
        }
    };
    let mut lines = vec![
        Line::from("Blob policy"),
        Line::from("  Blobs hold full tool inputs and outputs. Sessions sync without them.")
            .dark_gray(),
        Line::from(format!(
            "  {} On demand  a tool's blob downloads when you expand it",
            mark(BlobPolicy::OnDemand)
        )),
        Line::from(format!(
            "  {} Eager      every tool blob downloads as it syncs (more data and disk)",
            mark(BlobPolicy::Eager)
        )),
        Line::from("  Press b to switch. Workdir diffs and files always download when opened.")
            .dark_gray(),
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
    if let Some(fingerprint) = settings.server_fingerprint {
        // 95 characters: two rows of 16 pairs, to compare with what
        // `skywaved` prints without wrapping mid-pair.
        let shown = fingerprint.to_string();
        let (first, second) = shown.split_at(shown.len().min(16 * 3 - 1));
        lines.push(Line::from(format!("Fingerprint: {first}")));
        lines.push(Line::from(format!(
            "             {}",
            second.trim_start_matches(':')
        )));
    }
    if settings.device_id.is_some() {
        lines.push(Line::from(""));
        lines.push(
            Line::from("  Press a to change the server address (same server, no re-pairing).")
                .dark_gray(),
        );
    }
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" Settings ")),
        area,
    );
    if let Some(prompt) = prompt {
        render_prompt(frame, prompt_area, app, prompt);
    }
}

/// Key hints for the footer, the search prompt while it is open, and where
/// the composer is while it shows an earlier prompt.
fn footer_hint(app: &App) -> Cow<'static, str> {
    if !app.ui().help
        && let Some((view, ui)) = app.session()
        && !ui.shell()
        && let Some(search) = &ui.shown.search
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
        let enter = if ui.shell() { "run" } else { "send" };
        return format!("history {at}/{of} · Ctrl-P/N older/newer · edit to keep · Enter {enter}")
            .into();
    }
    let keys = footer_keys(app);
    // `d` says how much it would fetch.
    if !app.ui().help
        && let Some((view, ui)) = app.session()
        && ui.focus == Focus::Timeline
        && let Some(size) = if ui.shell() {
            ui.selected_run(view)
                .and_then(|run| run.output.as_ref().and_then(FullView::blob))
                .filter(|blob| !matches!(blob.state, BlobState::Available { .. }))
                .and_then(|blob| blob_size(&blob.state))
        } else {
            ui.selected_entry(view)
                .and_then(wanted_blob)
                .and_then(|blob| blob_size(&blob.state))
        }
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
    if app.repair_screen() {
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
        Screen::SessionList(_)
            if app
                .ui()
                .prompt
                .as_ref()
                .is_some_and(|p| p.kind == PromptKind::Adopt && !p.asks_workdir()) =>
        {
            "Enter create (the server checks the id) · Esc cancel · Ctrl-U clear · F1 help · Ctrl-C quit"
        }
        Screen::SessionList(_)
            if app
                .ui()
                .prompt
                .as_ref()
                .is_some_and(|p| p.kind == PromptKind::Adopt) =>
        {
            "Tab complete · Enter next: the id · Esc cancel · Ctrl-U clear · F1 help · Ctrl-C quit"
        }
        Screen::SessionList(_) if app.ui().prompt.is_some() => {
            "Tab complete · Enter create · Esc cancel · Ctrl-U clear · F1 help · Ctrl-C quit"
        }
        Screen::SessionList(_)
            if app
                .conversation_item()
                .is_some_and(|item| item.session.is_some()) =>
        {
            "↑↓ choose · Enter continue in a new session · o open its session · p paste an id · r refresh · Esc close · ? help"
        }
        Screen::SessionList(_) if app.conversation_picker().is_some() => {
            "↑↓ choose · Enter continue in a new session · p paste an id · r refresh · Esc close · ? help"
        }
        Screen::SessionList(list) if list.kind == ListKind::Archived => {
            // `↑↓ select` makes way for `D` in 100 columns.
            "Enter open · a unarchive · D delete here · r rename · A/Esc active sessions · ? help · q quit"
        }
        Screen::SessionList(_) => {
            // `↑↓ select` makes way for the archive and continue keys in 100
            // columns.
            "Enter open · n new · c continue · r rename · a archive · A archived · s settings · ? help · q quit"
        }
        Screen::Session(_) => match app.session().map(|(view, ui)| (view, ui, ui.focus)) {
            Some((_, ui, _)) if ui.picker.is_some() => match ui.picker {
                Some(Picker::Mode { confirm: true, .. }) => {
                    "y switch · any other key back to the list · ? help"
                }
                Some(Picker::Mode { .. }) => "↑↓ choose · Enter switch · Esc cancel · ? help",
                Some(Picker::Agent { .. }) => {
                    "↑↓ choose · Enter open · m model · e effort · Esc cancel · ? help"
                }
                Some(Picker::Contexts {
                    moving: Some(_), ..
                }) => "y move to a new session · any other key back to the list · ? help",
                Some(Picker::Contexts { .. }) => {
                    "↑↓ choose · Enter switch / new · v view · M move · x drop a refused move · Esc cancel · ? help"
                }
                Some(Picker::Stop(_)) => "y go on · any other key back · ? help",
                Some(Picker::Background {
                    stopping: Some(_), ..
                }) => "y stops the task · any other key back · ? help",
                Some(Picker::Background { .. }) => {
                    "↑↓ choose · Enter on a task stops it · o open a subagent · Esc close · ? help"
                }
                Some(Picker::Effort { .. }) => "↑↓ choose · Enter switch · Esc back · ? help",
                Some(Picker::Question { .. }) => {
                    "↑↓ move · Space pick · Enter next / send · ←→ question · type on Other · Esc close · F1 help"
                }
                _ => {
                    "↑↓ choose · type a name instead · Enter switch · Ctrl-U clear · Esc back · F1 help"
                }
            },
            // A pending message only takes these keys while the core says it
            // can be taken back.
            Some((view, ui, Focus::Timeline)) if ui.rule_pick(view).is_some() => {
                "↑↓ choose · Enter approve and always allow · Esc back to a/x · ? help"
            }
            Some((view, ui, Focus::Timeline)) if ui.shell() => {
                match ui.selected_run(view).filter(|run| run.discardable) {
                    Some(_) => {
                        "e edit · Del take back · ↑↓ select · Enter expand · y copy · PgUp/PgDn · Ctrl-X stop · Tab write · Esc back · ? help"
                    }
                    None => {
                        "↑↓ select · Enter expand · PgUp/PgDn · d download · y copy · Ctrl-X stop · Ctrl-T agent · Tab write · ? help"
                    }
                }
            }
            Some((_, ui, _)) if ui.shell() => {
                "Enter run · Ctrl-O condition · Ctrl-P/N history · Ctrl-X stop · Ctrl-T agent · Esc runs · F1 help"
            }
            Some((view, ui, Focus::Timeline)) => match discardable(view, ui.shown.selected) {
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
                    "a/x approve/deny · A always allow · v show the call · ↑↓ select · Enter expand · d download · y/Y copy · / search · [ ] turns · PgUp/PgDn · p mode · m model/effort · C contexts · R rename · S stop · Tab write · Esc back · ? help"
                }
                None if view.pending_question.is_some() => {
                    "a answer · x decline · ↑↓ select · Enter expand · d download · y/Y copy · / search · [ ] turns · PgUp/PgDn · p mode · m model/effort · C contexts · R rename · S stop · Tab write · Esc back · ? help"
                }
                None if view.pending_approval.is_some() => {
                    "a/x approve/deny · v show the call · ↑↓ select · Enter expand · d download · y/Y copy · / search · [ ] turns · PgUp/PgDn · p mode · m model/effort · C contexts · R rename · S stop · Tab write · Esc back · ? help"
                }
                None if view.subagent.is_some() => {
                    "↑↓ select · Enter expand · o open · d download · y/Y copy · / search · a/x approve/deny · [ ] turns · t tools · S stop subagent · Tab write · Esc back out · ? help"
                }
                None if ui.open_subagent(view).is_some() => {
                    "↑↓ select · Enter expand · o open subagent · d download · y/Y copy · / search · a/x approve/deny · [ ] turns · t tools · PgUp/PgDn · S stop · Tab write · Esc back · ? help"
                }
                None => {
                    "↑↓ select · Enter expand · d download · y/Y copy · / search · a/x approve/deny · [ ] turns · t tools · PgUp/PgDn · w workdir · p mode · m model/effort · C contexts · R rename · S stop · Tab write · Esc back · ? help"
                }
            },
            // The toggle only where the server runs shell commands.
            Some((view, _, _)) if view.shell.enabled => {
                "Enter send · Ctrl-J newline · Ctrl-X interrupt · Ctrl-T shell · Esc timeline · F1 help"
            }
            _ => {
                "Enter send · Ctrl-J newline · Ctrl-X interrupt · PgUp/PgDn · Esc timeline · F1 help"
            }
        },
        Screen::Settings(_) if app.ui().prompt.is_some() => {
            "Enter reconnect there · Esc cancel · Ctrl-U clear · F1 help · Ctrl-C quit"
        }
        Screen::Settings(_) => {
            "b toggle blob policy · a server address · p re-pair · Esc back · ? help · q quit"
        }
        Screen::Usage(_) => {
            "r range · Tab group · ↑↓ scroll · L limits · Esc back · ? help · q quit"
        }
        Screen::UsageLimits(_) => "r refresh · Esc back · ? help · q quit",
        Screen::Storage(_) => {
            "↑↓ select · D delete · K crop · r refresh · Tab list · Esc back · ? help · q quit"
        }
        Screen::Workdir(view) => match view.pane {
            WorkdirPane::Changes | WorkdirPane::Dir { .. } => {
                "↑↓ select · Enter open · Tab changes/files · r refresh · y copy · Esc back · ? help · q quit"
            }
            WorkdirPane::Diff { .. } | WorkdirPane::File { .. } => {
                "↑↓ PgUp/PgDn scroll · d download · Tab changes/files · r refresh · y copy · Esc back · ? help · q quit"
            }
        },
        Screen::Crop(view) => match view.content {
            CropContent::NotHere(_) => "d download · Esc back · ? help · q quit",
            CropContent::Ready { .. } if app.ui().crop.anchor.is_some() => {
                "↑↓ PgUp/PgDn extend · v keep or drop the range · Esc cancel the range · ? help"
            }
            CropContent::Ready { .. } => {
                "↑↓ PgUp/PgDn move · 1-9 preset · v range · Enter crop · Esc back · ? help · q quit"
            }
            _ => "Esc back · ? help · q quit",
        },
    }
}

/// Every key binding, grouped by where it applies.
fn help_lines(app: &App) -> Vec<Line<'static>> {
    let section = |title: &'static str| Line::from(title).bold();
    // Two spans, so a long description wraps under itself ([`fit`]).
    let key = |keys: &'static str, what: &'static str| {
        Line::from(vec![Span::raw(format!("  {keys:<14}")), Span::raw(what)])
    };
    let mut lines = vec![
        section("Everywhere"),
        key("F1 / ?", "toggle this help (? only outside text fields)"),
        key("q / Ctrl-C", "quit (q only outside text fields)"),
        key("Ctrl-R", "retry now while offline"),
        key("Ctrl-G", "open the next session waiting for an approval"),
        Line::from(""),
    ];
    if app.repair_screen() {
        lines.extend([
            section("Re-pair"),
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
        Screen::SessionList(_)
            if app
                .ui()
                .prompt
                .as_ref()
                .is_some_and(|p| p.kind == PromptKind::Adopt && !p.asks_workdir()) =>
        {
            lines.extend([
                section("Continue an outside conversation"),
                key("type / paste", "the agent's id of the conversation"),
                key("Enter", "create the session; the server checks the id"),
                key("Backspace", "delete a character"),
                key("Ctrl-U", "clear"),
                key("Esc", "cancel"),
            ])
        }
        Screen::SessionList(_) if app.ui().prompt.is_some() => lines.extend([
            section(
                if app
                    .ui()
                    .prompt
                    .as_ref()
                    .is_some_and(|p| p.kind == PromptKind::Adopt)
                {
                    "Continue an outside conversation: workdir"
                } else {
                    "New session"
                },
            ),
            key("type / paste", "the session's directory on the server"),
            key("Tab", "complete the directory under the roots"),
            key("↑↓ Tab/Enter", "pick from the list"),
            if app
                .ui()
                .prompt
                .as_ref()
                .is_some_and(|p| p.kind == PromptKind::Adopt)
            {
                key("Enter", "next: the conversation id")
            } else {
                key("Enter", "create the session")
            },
            key("Backspace", "delete a character"),
            key("Ctrl-U", "clear"),
            key("Esc", "close the list, then cancel"),
        ]),
        Screen::SessionList(_) if app.conversation_picker().is_some() => lines.extend([
            section("Continue an outside conversation"),
            key("↑↓ / k j", "choose a conversation the server's agent keeps"),
            key(
                "Enter",
                "continue it in a new session; its history is imported",
            ),
            key("o", "open the session already running it"),
            key("p", "paste a conversation id instead"),
            key("r", "ask the server again"),
            key("Esc", "close"),
            Line::from(""),
            Line::from("  Continuing one again makes another session; the row says"),
            Line::from("  where it was continued before."),
        ]),
        Screen::SessionList(_) => lines.extend([
            section("Sessions"),
            key("↑↓ / k j", "select"),
            key("Enter", "open session"),
            key("n", "new session in a server directory"),
            key(
                "c",
                "continue an agent conversation started outside Skywave",
            ),
            key("r", "rename the selected session"),
            key("a", "archive it (on the archived list: bring it back)"),
            key("A", "switch between active and archived sessions"),
            key("Esc", "from the archived list: back to the active one"),
            key("D", "archived list: delete here only (y/N)"),
            key("Del", "dismiss the oldest refused new session"),
            key("s", "settings"),
            key("u", "usage: cost and tokens across sessions"),
            key("S", "storage: what the server's data takes on its disk"),
            key("L", "usage limits of the agent's account"),
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
            let (shell, enabled) = app.session().map_or((false, false), |(view, ui)| {
                (ui.shell(), view.shell.enabled)
            });
            lines.push(section("Session"));
            if shell {
                lines.extend([
                    key("Ctrl-X", "stop the running shell command, else the newest"),
                    key("", "waiting one (yours never sent: taken back)"),
                ]);
            } else {
                lines.push(key("Ctrl-X", "interrupt the agent's turn"));
            }
            // Only where the server runs shell commands.
            if shell || enabled {
                lines.push(key(
                    "Ctrl-T",
                    "switch between writing to the agent and shell commands",
                ));
            }
            if shell {
                lines.push(key("Tab", "switch between command and runs"));
            } else {
                lines.extend([
                    key("Ctrl-F", "search the timeline (also / on it)"),
                    key("PgUp / PgDn", "scroll the timeline"),
                    key("Tab", "switch between message and timeline"),
                ]);
            }
            lines.push(Line::from(""));
            lines.extend(rules_lines(app));
            // One focus at a time, so the overlay fits a small terminal.
            match app.session().map(|(_, ui)| ui.focus) {
                Some(Focus::Timeline) if shell => lines.extend([
                    section("Shell runs (Tab or i: command keys)"),
                    key("↑↓ / k j", "select a run"),
                    key("Home g/End G", "oldest / newest run"),
                    key(
                        "Enter / Space",
                        "show its whole output; downloads it if needed",
                    ),
                    key("PgUp / PgDn", "page through a run taller than the list"),
                    key("d", "download its whole output"),
                    key("y", "copy its output, colours stripped (the whole"),
                    key("", "output once downloaded, else what shows)"),
                    key("e", "edit your never-sent or refused command"),
                    key("Del", "take back your never-sent command, or dismiss"),
                    key("", "a refused one; else your delete or crop of its output"),
                    key(
                        "D",
                        "delete its output on the server and every device (y/N)",
                    ),
                    key("K", "crop its output: choose the lines every device keeps"),
                    key("a / x", "approve / deny the pending approval"),
                    key("w", "the session's workdir: git changes and files"),
                    key("L", "usage limits of the agent's account"),
                    key("Esc", "back to sessions"),
                ]),
                _ if shell => lines.extend([
                    section("Shell command (Esc or Tab: run keys)"),
                    key("Enter", "run it on the server, in the session's directory"),
                    key("", "(in send order with your messages; queued offline)"),
                    key("Ctrl-O", "run it only if the previous run succeeded,"),
                    key("", "then only if it failed, then always"),
                    key("Ctrl-J", "new line (also Alt-Enter, Shift-Enter)"),
                    key("←→↑↓ Home End", "move the cursor"),
                    key(
                        "Ctrl-P / N",
                        "earlier / later command (also ↑ on the first line, ↓ on the last)",
                    ),
                    key("Ctrl-U", "clear the command"),
                    key("Esc", "go to the runs"),
                ]),
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
                    key(
                        "",
                        "(on a call: take back your delete or crop of its output)",
                    ),
                    key(
                        "D",
                        "delete a call's output on the server and every device (y/N)",
                    ),
                    key(
                        "K",
                        "crop a call's output: choose the lines every device keeps",
                    ),
                    key("a / x", "approve / deny the pending approval"),
                    key("A", "approve, always allowing a rule the agent offers"),
                    key("", "(several: ↑↓ Enter pick one, Esc back to a / x)"),
                    key("v", "select and expand the call it gates"),
                    key("a / x", "a question: pick an answer / decline it"),
                    key(
                        "",
                        "(↑↓ move, Space picks, Enter next or send, type on Other, Esc closes)",
                    ),
                    key(
                        "t",
                        "conversation only: hide tool calls, or show them again",
                    ),
                    key("S", "stop the agent; a message restarts it"),
                    key("", "(with background work, y confirms)"),
                    key("", "(in a subagent's view: stop its task, y confirms)"),
                    key("B", "list background work; Enter on a task stops it"),
                    key("o", "open the selected subagent (also o in B's list)"),
                    key("", "(Esc leaves it; messages still go to the main agent)"),
                    key("p", "switch the agent's permission mode"),
                    key("", "(bypass lets every tool run: y confirms it)"),
                    key("m", "switch the agent's model or effort: m or e"),
                    key("C", "new context; switch to or view one"),
                    key("", "(Enter switches, v views; on this one: all)"),
                    key("", "(M moves it to a new session, y confirms;"),
                    key("", "x drops a refused move; Enter on a moved one opens it)"),
                    key("R", "rename the session"),
                    key("w", "the session's workdir: git changes and files"),
                    key("L", "usage limits of the agent's account"),
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
                Line::from("  Status line, left: the agent's state, mode, model, a refusal"),
                Line::from(
                    "  Status line, right: last turn's time, tokens, cost; context fill; session cost; model",
                ),
            ]);
        }
        Screen::Settings(_) => lines.extend([
            section("Settings"),
            key("b", "toggle blob policy: on demand / eager"),
            key(
                "a",
                "change the server address, e.g. after its IP changed (no re-pairing)",
            ),
            key("p", "re-pair"),
            key("Esc", "back to sessions"),
        ]),
        Screen::Usage(_) => lines.extend([
            section("Usage"),
            key("r", "range: today, 7 days, 30 days, all"),
            key("Tab", "group by day, session, directory or model"),
            key("↑↓ / k j", "scroll"),
            key("L", "usage limits of the agent's account"),
            key("Esc", "back to sessions"),
            Line::from(""),
            Line::from("  Totals come from this device's replica, so they work offline."),
            Line::from("  A turn whose agent reported no cost is unpriced, not free."),
            Line::from("  Archived sessions count; sessions deleted here do not."),
        ]),
        Screen::UsageLimits(_) => lines.extend([
            section("Usage limits"),
            key("r", "ask the agent again"),
            key("Esc", "back where you opened it"),
            Line::from(""),
            Line::from("  The last limits read are kept on this device, so they show offline."),
            Line::from("  Opening the page asks only when none were read yet."),
        ]),
        Screen::Storage(_) => lines.extend([
            section("Storage"),
            key("r", "ask the server again"),
            key("Tab", "list sessions, the largest blobs or shell runs"),
            key("↑↓ / k j", "select a row"),
            key("Home g/End G", "first / last row"),
            key("D", "delete its blob (a run's output) everywhere (y/N)"),
            key("K", "crop a tool or run output: choose the lines to keep"),
            key("", "(only for a session on this device)"),
            key("Del", "take back your delete or crop of it"),
            key("Esc", "back to sessions"),
            Line::from(""),
            Line::from("  Read from the server when the screen opens, so only online."),
            Line::from("  A session's size is its events in the log and their blobs."),
        ]),
        Screen::Workdir(_) => lines.extend([
            section("Workdir (w on a session's timeline)"),
            key("Tab", "switch between changes and files"),
            key("↑↓ / k j", "select a row; in a diff or file, scroll"),
            key("PgUp / PgDn", "a page up / down"),
            key("Home g/End G", "first / last"),
            key(
                "Enter",
                "open the changed file's diff, the directory or file",
            ),
            key("", "(.. goes up; on a diff or file: download it)"),
            key("d", "download the diff or file; retries a failed one"),
            key("r", "read it again from the server"),
            key("y", "copy the diff or file, else the list"),
            key("Esc", "back: up a directory, to the changes, the session"),
            Line::from(""),
            Line::from("  Read on request only and kept here, so what was read shows"),
            Line::from("  offline, with when it was taken. Nothing here changes files."),
        ]),
        Screen::Crop(_) => lines.extend([
            section("Crop (K on an output)"),
            key("↑↓ / k j", "move the cursor"),
            key("PgUp / PgDn", "a page up / down"),
            key("Home g/End G", "first / last line"),
            key("1-9", "keep a ready-made selection, as listed"),
            key("v", "start a range; v again keeps it, or drops it if"),
            key("", "every line in it is kept"),
            key("Enter", "crop to the lines marked + (y/N)"),
            key("d", "download it, when it isn't on this device"),
            key("Esc", "cancel the range, else back"),
            Line::from(""),
            Line::from("  A crop keeps only the marked lines, on the server and on"),
            Line::from("  every device. It can't be undone."),
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

/// Widest the help overlay gets, borders included.
const HELP_WIDTH: u16 = 62;

/// The help overlay, centred. Long lines wrap; when the lines do not all
/// fit, it scrolls from [`UiState::help_top`](crate::app::UiState::help_top)
/// and its bottom border says which way there is more.
fn render_help(frame: &mut Frame, app: &App) {
    let width = HELP_WIDTH.min(frame.area().width);
    let lines: Vec<Line> = help_lines(app)
        .into_iter()
        .flat_map(|line| fit(line, usize::from(width.saturating_sub(2))))
        .collect();
    let height = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .saturating_add(2)
        .min(frame.area().height);
    let [area] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(frame.area());
    let [area] = Layout::horizontal([Constraint::Length(width)])
        .flex(Flex::Center)
        .areas(area);
    let end = lines
        .len()
        .saturating_sub(usize::from(height.saturating_sub(2)));
    app.ui().help_end.set(end);
    let top = app.ui().help_top.min(end);
    let more = match (top > 0, top < end) {
        (false, false) => None,
        (false, true) => Some(" ↓ more below · j/k scroll "),
        (true, true) => Some(" ↑↓ more above and below · j/k scroll "),
        (true, false) => Some(" ↑ more above · j/k scroll "),
    };
    let mut block = Block::bordered().title(" Keys ");
    if let Some(more) = more {
        block = block.title_bottom(Line::from(more).right_aligned());
    }
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .scroll((u16::try_from(top).unwrap_or(u16::MAX), 0)),
        area,
    );
}

/// `line` in rows of at most `width` columns: a key's description wraps
/// under itself, other text under its own indent.
fn fit(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    if line.width() <= width {
        return vec![line];
    }
    let style = line.style;
    let mut spans = line.spans;
    let (first, rest) = if spans.len() > 1 {
        let head = spans.remove(0);
        let hang = Span::raw(" ".repeat(head.width()));
        (vec![head], vec![hang])
    } else {
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        let indent = text.len() - text.trim_start_matches(' ').len();
        (Vec::new(), vec![Span::raw(" ".repeat(indent))])
    };
    timeline::wrap(first, rest, spans, width)
        .into_iter()
        .map(|row| row.style(style))
        .collect()
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

/// A rule as listed: its label, then how far it reaches.
pub(crate) fn rule_text(rule: &RuleSuggestion) -> String {
    format!("{} ({})", clean(&rule.label), scope_label(rule.scope))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unnamed_sessions_long_title_is_cut_and_a_name_is_not() {
        let long = format!("{} tail", "x".repeat(60));
        let shown = shown_title(&long, false);
        assert_eq!(shown, format!("{}…", "x".repeat(47)));
        assert_eq!(shown.chars().count(), DEFAULT_TITLE_MAX_CHARS);
        assert_eq!(
            shown_title("fix the flaky test", false),
            "fix the flaky test"
        );
        assert_eq!(shown_title(&long, true), long);
    }
}
