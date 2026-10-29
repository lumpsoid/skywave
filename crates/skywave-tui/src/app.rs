//! Head-side state: the latest [`ViewState`] from the core plus presentation
//! state the core does not care about (form buffers, focus, the help
//! overlay, list selection). Keys become core [`Action`]s here; nothing in
//! this module decides anything about pairing, sync or delivery (D2).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use skywave_core::{
    Action, BlobPolicy, BlobState, BlobView, CoreError, Decision, Entry, Link, LinkRejection,
    PairingView, Route, Screen, Seq, SessionId, SessionListView, SessionView, TimelineItem,
    ToolOutcome, ToolView, ViewState,
};

use crate::composer::Composer;

/// One of the pairing form's fields, in tab order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Field {
    #[default]
    Address,
    Code,
    Fingerprint,
}

impl Field {
    pub const ALL: [Field; 3] = [Field::Address, Field::Code, Field::Fingerprint];

    fn next(self) -> Field {
        match self {
            Field::Address => Field::Code,
            Field::Code => Field::Fingerprint,
            Field::Fingerprint => Field::Address,
        }
    }

    fn prev(self) -> Field {
        match self {
            Field::Address => Field::Fingerprint,
            Field::Code => Field::Address,
            Field::Fingerprint => Field::Code,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Field::Address => "Address",
            Field::Code => "Code",
            Field::Fingerprint => "Fingerprint",
        }
    }
}

/// What the user typed into the pairing form. Kept across attempts so a
/// failed pairing can be corrected rather than retyped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PairingForm {
    pub address: String,
    pub code: String,
    pub fingerprint: String,
    pub focus: Field,
}

impl PairingForm {
    pub fn value(&self, field: Field) -> &str {
        match field {
            Field::Address => &self.address,
            Field::Code => &self.code,
            Field::Fingerprint => &self.fingerprint,
        }
    }

    fn value_mut(&mut self, field: Field) -> &mut String {
        match field {
            Field::Address => &mut self.address,
            Field::Code => &mut self.code,
            Field::Fingerprint => &mut self.fingerprint,
        }
    }

    fn insert(&mut self, text: &str) {
        let field = self.focus;
        self.value_mut(field)
            .extend(text.chars().filter(|c| !c.is_control()));
    }

    /// Fills every field from a pairing URI and focuses the last, so Enter
    /// submits.
    fn fill(&mut self, uri: PairUri) {
        self.address = uri.address;
        self.code = uri.code;
        self.fingerprint = uri.fingerprint;
        self.focus = Field::Fingerprint;
    }

    fn to_action(&self) -> Action {
        Action::Pair {
            address: self.address.clone(),
            code: self.code.clone(),
            fingerprint: self.fingerprint.clone(),
        }
    }
}

/// The pairing URI `skywaved pair` prints and encodes as a QR code (spec §6):
/// `skywave://pair?addr=<percent-encoded host:port>&code=<code>&fp=<hex>`.
///
/// Parsing only splits the URI into the three form fields; the fingerprint
/// is shown colon-separated like `skywaved pair` prints it. Checking and
/// normalising the values stays with the core.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairUri {
    pub address: String,
    pub code: String,
    pub fingerprint: String,
}

impl PairUri {
    const PREFIX: &'static str = "skywave://pair?";

    pub fn parse(text: &str) -> Option<PairUri> {
        let query = text.trim().strip_prefix(Self::PREFIX)?;
        let (mut address, mut code, mut fingerprint) = (None, None, None);
        for pair in query.split('&') {
            let (key, value) = pair.split_once('=')?;
            let value = percent_decode(value)?;
            match key {
                "addr" => address = Some(value),
                "code" => code = Some(value),
                "fp" => fingerprint = Some(value),
                _ => {}
            }
        }
        let fingerprint = fingerprint?;
        let fingerprint = if fingerprint.len() == 64 && fingerprint.is_ascii() {
            fingerprint
                .as_bytes()
                .chunks(2)
                .map(|pair| std::str::from_utf8(pair).unwrap_or_default())
                .collect::<Vec<_>>()
                .join(":")
        } else {
            fingerprint
        };
        Some(PairUri {
            address: address?,
            code: code?,
            fingerprint,
        })
    }
}

/// Decodes `%XX` escapes (and `+` as a space); `None` on a bad escape.
fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// The "new session" prompt on the session list: the server workdir being
/// typed. Whether the path is acceptable is the server's call (D14).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CreateInput {
    pub workdir: String,
    /// Enter was pressed; the prompt closes once the core takes the action
    /// and stays open with the text if dispatching fails.
    submitted: bool,
}

impl CreateInput {
    fn insert(&mut self, text: &str) {
        self.workdir
            .extend(text.chars().filter(|c| !c.is_control()));
    }
}

/// Where keys go on the session view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    /// Typing a message.
    #[default]
    Composer,
    /// Scrolling, expanding tool output, answering an approval.
    Timeline,
}

/// The timeline's layout in the last frame, recorded by the renderer so
/// paging and keeping the selection in view know how tall things are. Stale
/// by at most one frame; the renderer clamps whatever the keys make of it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Geometry {
    /// First line of each timeline item, by timeline index.
    pub starts: Vec<usize>,
    /// Lines of the whole timeline.
    pub total: usize,
    /// Lines that fit on screen.
    pub height: usize,
}

impl Geometry {
    /// Top line when following the newest item.
    pub fn bottom(&self) -> usize {
        self.total.saturating_sub(self.height)
    }

    /// The top line shown for `scroll`.
    pub fn top(&self, scroll: Option<usize>) -> usize {
        scroll.map_or(self.bottom(), |top| top.min(self.bottom()))
    }

    fn item(&self, index: usize) -> Option<(usize, usize)> {
        let start = *self.starts.get(index)?;
        let end = self.starts.get(index + 1).copied().unwrap_or(self.total);
        Some((start, end))
    }
}

/// Presentation state of the open session.
#[derive(Debug, Clone)]
pub struct SessionUi {
    pub id: SessionId,
    pub focus: Focus,
    /// Top line of the timeline while the user has scrolled back; `None`
    /// follows the newest item. New items never move a pinned view.
    pub scroll: Option<usize>,
    /// The selected tool call, tool result or approval, by seq so it stays
    /// put as items arrive.
    pub selected: Option<Seq>,
    /// Items whose full content is shown.
    pub expanded: HashSet<Seq>,
    pub geometry: RefCell<Geometry>,
}

impl SessionUi {
    fn new(id: SessionId) -> SessionUi {
        SessionUi {
            id,
            focus: Focus::Composer,
            scroll: None,
            selected: None,
            expanded: HashSet::new(),
            geometry: RefCell::default(),
        }
    }
}

/// The blob behind an expandable item, if its content lives in one.
pub fn item_blob(entry: &Entry) -> Option<&BlobView> {
    match entry {
        Entry::ToolCall { input, .. } | Entry::Approval { input, .. } => input.as_ref(),
        Entry::ToolResult { output, .. } => output.as_ref(),
        Entry::Tool(view) => tool_blob(view),
        _ => None,
    }
}

/// A grouped call's blob: its output once done, else its body, else its
/// input.
fn tool_blob(view: &ToolView) -> Option<&BlobView> {
    let output = match &view.outcome {
        ToolOutcome::Done { output, .. } => output.as_ref(),
        _ => None,
    };
    output
        .or_else(|| view.body.as_ref().and_then(|body| body.full.as_ref()))
        .or(view.input.as_ref())
}

/// Tool calls, tool results and approvals can be selected and expanded.
pub fn selectable(item: &TimelineItem) -> Option<Seq> {
    match item {
        TimelineItem::Logged {
            seq,
            entry:
                Entry::Tool(_)
                | Entry::ToolCall { .. }
                | Entry::ToolResult { .. }
                | Entry::Approval { .. },
            ..
        } => Some(*seq),
        _ => None,
    }
}

/// Presentation state owned by the head.
#[derive(Debug, Clone)]
pub struct UiState {
    pub help: bool,
    pub form: PairingForm,
    /// Selected session on the session list, by id so it stays put when
    /// rows arrive or reorder.
    pub selected: Option<SessionId>,
    /// The open "new session" prompt, if any.
    pub create: Option<CreateInput>,
    /// The open session's focus, scroll and expanded items.
    pub session: Option<SessionUi>,
    /// Unsent messages, per session; drafts are per device (D5).
    pub drafts: HashMap<SessionId, Composer>,
    /// The user chose to browse local data behind the revoked screen.
    pub revoked_dismissed: bool,
    /// When the current [`Link`] value arrived; the retry countdown runs
    /// from here.
    pub link_since: Instant,
    /// The last failed dispatch, shown in the footer until the next key.
    pub error: Option<String>,
    pub quit: bool,
}

/// The TUI's state: the core's latest snapshot plus [`UiState`].
#[derive(Debug, Clone)]
pub struct App {
    view: ViewState,
    ui: UiState,
}

impl App {
    pub fn new(view: ViewState, now: Instant) -> App {
        let mut app = App {
            view: view.clone(),
            ui: UiState {
                help: false,
                form: PairingForm::default(),
                selected: None,
                create: None,
                session: None,
                drafts: HashMap::new(),
                revoked_dismissed: false,
                link_since: now,
                error: None,
                quit: false,
            },
        };
        app.prefill(&view);
        app.keep_selection(None);
        app.follow_session();
        app
    }

    pub fn view(&self) -> &ViewState {
        &self.view
    }

    pub fn ui(&self) -> &UiState {
        &self.ui
    }

    /// Takes a new snapshot from the core, received at `now`.
    pub fn set_view(&mut self, view: ViewState, now: Instant) {
        if view.link != self.view.link {
            self.ui.link_since = now;
        }
        if !matches!(view.link, Link::Rejected(LinkRejection::Revoked)) {
            self.ui.revoked_dismissed = false;
        }
        match (
            matches!(self.view.screen, Screen::Pairing(_)),
            matches!(view.screen, Screen::Pairing(_)),
        ) {
            (false, true) => self.prefill(&view),
            // Paired or cancelled; the code was single use anyway.
            (true, false) => self.ui.form = PairingForm::default(),
            _ => {}
        }
        if !matches!(view.screen, Screen::SessionList(_)) {
            self.ui.create = None;
        }
        let old_index = self
            .session_list()
            .and_then(|list| self.selected_index(list));
        self.view = view;
        self.keep_selection(old_index);
        self.follow_session();
    }

    /// Fresh session presentation state when another session opens.
    fn follow_session(&mut self) {
        match &self.view.screen {
            Screen::Session(session) => {
                if self.ui.session.as_ref().is_none_or(|s| s.id != session.id) {
                    self.ui.session = Some(SessionUi::new(session.id));
                }
            }
            _ => self.ui.session = None,
        }
    }

    /// The open session's view and presentation state.
    pub fn session(&self) -> Option<(&SessionView, &SessionUi)> {
        match (&self.view.screen, &self.ui.session) {
            (Screen::Session(view), Some(ui)) if view.id == ui.id => Some((view, ui)),
            _ => None,
        }
    }

    /// The draft for `session`, if one was started.
    pub fn draft(&self, session: SessionId) -> Option<&Composer> {
        self.ui.drafts.get(&session)
    }

    /// Records the outcome of dispatching an action from [`App::on_key`].
    pub fn dispatched(&mut self, result: Result<(), CoreError>) {
        if let Some(create) = &mut self.ui.create
            && create.submitted
        {
            if result.is_ok() {
                self.ui.create = None;
            } else {
                create.submitted = false;
            }
        }
        for draft in self.ui.drafts.values_mut().filter(|d| d.submitted) {
            draft.submitted = false;
            if result.is_ok() {
                draft.clear();
            }
        }
        self.ui.error = result.err().map(|err| err.to_string());
    }

    /// The session list on screen, if that is the current screen.
    pub fn session_list(&self) -> Option<&SessionListView> {
        match &self.view.screen {
            Screen::SessionList(list) => Some(list),
            _ => None,
        }
    }

    /// Position of the selected session in `list`.
    pub fn selected_index(&self, list: &SessionListView) -> Option<usize> {
        let id = self.ui.selected?;
        list.sessions.iter().position(|row| row.id == id)
    }

    /// Keeps the selected session selected. If it is gone (or nothing was
    /// selected), selects the row at `old_index`, clamped, so the cursor
    /// does not jump to the top.
    fn keep_selection(&mut self, old_index: Option<usize>) {
        let Screen::SessionList(list) = &self.view.screen else {
            return;
        };
        if self.selected_index(list).is_some() {
            return;
        }
        let last = list.sessions.len().checked_sub(1);
        self.ui.selected = last.map(|last| list.sessions[old_index.unwrap_or(0).min(last)].id);
    }

    fn move_selection(&mut self, down: bool) {
        let Screen::SessionList(list) = &self.view.screen else {
            return;
        };
        let Some(last) = list.sessions.len().checked_sub(1) else {
            return;
        };
        let index = match (self.selected_index(list), down) {
            (None, _) => 0,
            (Some(i), true) => (i + 1).min(last),
            (Some(i), false) => i.saturating_sub(1),
        };
        self.ui.selected = Some(list.sessions[index].id);
    }

    /// The revoked full-screen state covers the current screen.
    pub fn revoked_screen(&self) -> bool {
        matches!(self.view.link, Link::Rejected(LinkRejection::Revoked))
            && !matches!(self.view.screen, Screen::Pairing(_))
            && !self.ui.revoked_dismissed
    }

    /// The focused element takes text, so letters are input, not commands.
    pub fn typing(&self) -> bool {
        !self.revoked_screen()
            && match self.view.screen {
                Screen::Pairing(_) => true,
                Screen::SessionList(_) => self.ui.create.is_some(),
                Screen::Session(_) => self
                    .ui
                    .session
                    .as_ref()
                    .is_some_and(|s| s.focus == Focus::Composer),
                _ => false,
            }
    }

    /// Bracketed paste: on the pairing screen a pairing URI fills the whole
    /// form, anything else goes into the focused field; in the new-session
    /// prompt it goes into the workdir; on a session it goes into the
    /// composer, line breaks included.
    pub fn on_paste(&mut self, text: &str) {
        if self.ui.help || self.revoked_screen() {
            return;
        }
        if let Some(session) = &mut self.ui.session {
            session.focus = Focus::Composer;
            self.ui.drafts.entry(session.id).or_default().insert(text);
            return;
        }
        if !self.typing() {
            return;
        }
        if let Some(create) = &mut self.ui.create {
            create.insert(text.trim());
            return;
        }
        match PairUri::parse(text) {
            Some(uri) => self.ui.form.fill(uri),
            None => self.ui.form.insert(text.trim()),
        }
    }

    /// Handles a key press; returns the action to dispatch, if any.
    pub fn on_key(&mut self, key: KeyEvent) -> Option<Action> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        self.ui.error = None;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            self.ui.quit = true;
            return None;
        }
        if self.ui.help {
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::F(1) | KeyCode::Char('?') | KeyCode::Char('q')
            ) {
                self.ui.help = false;
            }
            return None;
        }
        if key.code == KeyCode::F(1) || (!self.typing() && key.code == KeyCode::Char('?')) {
            self.ui.help = true;
            return None;
        }
        if self.revoked_screen() {
            return match key.code {
                KeyCode::Char('p') | KeyCode::Enter => Some(Action::Navigate(Route::Pairing)),
                KeyCode::Esc => {
                    self.ui.revoked_dismissed = true;
                    None
                }
                KeyCode::Char('q') => {
                    self.ui.quit = true;
                    None
                }
                _ => None,
            };
        }
        match &self.view.screen {
            Screen::Pairing(pairing) => {
                let paired = pairing.paired.is_some();
                self.pairing_key(key, ctrl, paired)
            }
            Screen::SessionList(_) if self.ui.create.is_some() => self.create_key(key, ctrl),
            Screen::SessionList(list) => match key.code {
                KeyCode::Char('q') => self.quit(),
                KeyCode::Up | KeyCode::Char('k') => {
                    self.move_selection(false);
                    None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.move_selection(true);
                    None
                }
                KeyCode::Enter => self
                    .selected_index(list)
                    .map(|i| Action::OpenSession(list.sessions[i].id)),
                KeyCode::Char('n') => {
                    self.ui.create = Some(CreateInput::default());
                    None
                }
                KeyCode::Char('s') => Some(Action::Navigate(Route::Settings)),
                _ => None,
            },
            Screen::Session(_) => self.session_key(key, ctrl),
            Screen::Settings(settings) => match key.code {
                KeyCode::Char('q') => self.quit(),
                KeyCode::Esc => Some(Action::Navigate(Route::SessionList)),
                KeyCode::Char('b') => Some(Action::SetBlobPolicy(match settings.blob_policy {
                    BlobPolicy::Eager => BlobPolicy::OnDemand,
                    BlobPolicy::OnDemand => BlobPolicy::Eager,
                })),
                KeyCode::Char('p') => Some(Action::Navigate(Route::Pairing)),
                _ => None,
            },
        }
    }

    fn pairing_key(&mut self, key: KeyEvent, ctrl: bool, paired: bool) -> Option<Action> {
        let form = &mut self.ui.form;
        // A URI typed or pasted without bracketed paste lands in one field.
        if key.code == KeyCode::Enter
            && let Some(uri) = PairUri::parse(form.value(form.focus))
        {
            form.fill(uri);
            return None;
        }
        match key.code {
            KeyCode::Tab | KeyCode::Down => form.focus = form.focus.next(),
            KeyCode::BackTab | KeyCode::Up => form.focus = form.focus.prev(),
            // Enter moves through the form; on the last field it submits.
            // Validation is the core's: an incomplete form fails there.
            KeyCode::Enter if form.focus == Field::Fingerprint => {
                return Some(form.to_action());
            }
            KeyCode::Enter => form.focus = form.focus.next(),
            KeyCode::Esc if paired => return Some(Action::Navigate(Route::SessionList)),
            KeyCode::Backspace => {
                let field = form.focus;
                form.value_mut(field).pop();
            }
            KeyCode::Char('u') if ctrl => {
                let field = form.focus;
                form.value_mut(field).clear();
            }
            KeyCode::Char(c) if !ctrl => form.insert(c.encode_utf8(&mut [0; 4])),
            _ => {}
        }
        None
    }

    /// Keys while the new-session prompt is open.
    fn create_key(&mut self, key: KeyEvent, ctrl: bool) -> Option<Action> {
        let create = self.ui.create.as_mut()?;
        match key.code {
            KeyCode::Esc => self.ui.create = None,
            // Blank or unacceptable paths are refused by the core or the
            // server, not here.
            KeyCode::Enter => {
                create.submitted = true;
                return Some(Action::CreateSession {
                    workdir: create.workdir.clone(),
                });
            }
            KeyCode::Backspace => {
                create.workdir.pop();
            }
            KeyCode::Char('u') if ctrl => create.workdir.clear(),
            KeyCode::Char(c) if !ctrl => create.insert(c.encode_utf8(&mut [0; 4])),
            _ => {}
        }
        None
    }

    /// Keys on the session view. Ctrl-X, PgUp/PgDn and Tab work in both
    /// focuses; the rest depends on [`Focus`].
    fn session_key(&mut self, key: KeyEvent, ctrl: bool) -> Option<Action> {
        let Screen::Session(view) = &self.view.screen else {
            return None;
        };
        let session = self.ui.session.as_mut()?;
        match key.code {
            KeyCode::Char('x') if ctrl => {
                return Some(Action::Interrupt {
                    session_id: view.id,
                });
            }
            KeyCode::PageUp => {
                session.page(true);
                return None;
            }
            KeyCode::PageDown => {
                session.page(false);
                return None;
            }
            KeyCode::Tab | KeyCode::BackTab => {
                session.focus = match session.focus {
                    Focus::Composer => Focus::Timeline,
                    Focus::Timeline => Focus::Composer,
                };
                return None;
            }
            _ => {}
        }
        match session.focus {
            Focus::Composer => {
                let alt = key.modifiers.contains(KeyModifiers::ALT);
                let shift = key.modifiers.contains(KeyModifiers::SHIFT);
                let draft = self.ui.drafts.entry(view.id).or_default();
                match key.code {
                    KeyCode::Esc => session.focus = Focus::Timeline,
                    KeyCode::Enter if alt || shift => draft.newline(),
                    KeyCode::Char('j') if ctrl => draft.newline(),
                    // A blank draft is refused by the core.
                    KeyCode::Enter => {
                        draft.submitted = true;
                        return Some(Action::SendMessage {
                            session_id: view.id,
                            text: draft.text().to_owned(),
                        });
                    }
                    KeyCode::Backspace => draft.backspace(),
                    KeyCode::Delete => draft.delete(),
                    KeyCode::Left => draft.left(),
                    KeyCode::Right => draft.right(),
                    KeyCode::Up => draft.vertical(true),
                    KeyCode::Down => draft.vertical(false),
                    KeyCode::Home => draft.home(),
                    KeyCode::End => draft.end(),
                    KeyCode::Char('u') if ctrl => draft.clear(),
                    KeyCode::Char(c) if !ctrl => draft.insert(c.encode_utf8(&mut [0; 4])),
                    _ => {}
                }
                None
            }
            Focus::Timeline => match key.code {
                KeyCode::Esc => Some(Action::Navigate(Route::SessionList)),
                KeyCode::Char('q') => {
                    self.ui.quit = true;
                    None
                }
                KeyCode::Char('i') | KeyCode::Char('c') => {
                    session.focus = Focus::Composer;
                    None
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    session.select(view, true);
                    None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    session.select(view, false);
                    None
                }
                KeyCode::Home | KeyCode::Char('g') => {
                    session.scroll = Some(0);
                    None
                }
                KeyCode::End | KeyCode::Char('G') => {
                    session.scroll = None;
                    None
                }
                KeyCode::Enter | KeyCode::Char(' ') => session.toggle(view),
                KeyCode::Char('d') => session
                    .selected_blob(view)
                    .filter(|blob| !matches!(blob.state, BlobState::Available { .. }))
                    .map(|blob| Action::DownloadBlob(blob.id)),
                KeyCode::Char('a') => answer(view, Decision::Approve),
                KeyCode::Char('x') => answer(view, Decision::Deny),
                _ => None,
            },
        }
    }

    fn quit(&mut self) -> Option<Action> {
        self.ui.quit = true;
        None
    }

    /// Re-pairing starts from the address the device is paired with.
    fn prefill(&mut self, view: &ViewState) {
        if let Screen::Pairing(PairingView {
            paired: Some(info), ..
        }) = &view.screen
            && let Some(address) = &info.server_address
            && self.ui.form.address.is_empty()
        {
            self.ui.form.address = address.clone();
            self.ui.form.focus = Field::Code;
        }
    }
}

/// Answers the approval the session is blocked on, if any. Whether it may
/// still be answered is the core's call.
fn answer(view: &SessionView, decision: Decision) -> Option<Action> {
    let prompt = view.pending_approval.as_ref()?;
    Some(Action::ResolveApproval {
        id: prompt.id,
        decision,
    })
}

impl SessionUi {
    /// Scrolls a page back (`up`) or forward; reaching the bottom follows
    /// again.
    fn page(&mut self, up: bool) {
        let geometry = self.geometry.borrow();
        let page = geometry.height.saturating_sub(1).max(1);
        let top = geometry.top(self.scroll);
        let top = if up {
            top.saturating_sub(page)
        } else {
            top + page
        };
        let scroll = (top < geometry.bottom()).then_some(top);
        drop(geometry);
        self.scroll = scroll;
    }

    /// Moves the selection to the previous (`up`) or next selectable item
    /// and scrolls it into view. Nothing selected starts from the newest.
    fn select(&mut self, view: &SessionView, up: bool) {
        let items: Vec<(usize, Seq)> = view
            .timeline
            .iter()
            .enumerate()
            .filter_map(|(i, item)| Some((i, selectable(item)?)))
            .collect();
        let Some(&last) = items.last() else {
            return;
        };
        let current = self
            .selected
            .and_then(|seq| items.iter().position(|(_, s)| *s == seq));
        let (index, seq) = match (current, up) {
            (None, _) => last,
            (Some(i), true) => items[i.saturating_sub(1)],
            (Some(i), false) => items[(i + 1).min(items.len() - 1)],
        };
        self.selected = Some(seq);
        self.reveal(index);
    }

    /// Scrolls so the timeline item at `index` is on screen, its top first
    /// if it does not fit.
    fn reveal(&mut self, index: usize) {
        let geometry = self.geometry.borrow();
        let Some((start, end)) = geometry.item(index) else {
            return;
        };
        let top = geometry.top(self.scroll);
        let top = if start < top || end - start > geometry.height {
            start
        } else if end > top + geometry.height {
            end - geometry.height
        } else {
            return;
        };
        let scroll = (top < geometry.bottom()).then_some(top);
        drop(geometry);
        self.scroll = scroll;
    }

    fn selected_entry<'a>(&self, view: &'a SessionView) -> Option<&'a Entry> {
        let seq = self.selected?;
        view.timeline.iter().find_map(|item| match item {
            TimelineItem::Logged { seq: s, entry, .. } if *s == seq => Some(entry),
            _ => None,
        })
    }

    fn selected_blob<'a>(&self, view: &'a SessionView) -> Option<&'a BlobView> {
        item_blob(self.selected_entry(view)?)
    }

    /// Expands or collapses the selected item. Expanding a blob that is not
    /// here yet asks for it (spec §8: expanding downloads if needed).
    fn toggle(&mut self, view: &SessionView) -> Option<Action> {
        let seq = self.selected?;
        if self.expanded.remove(&seq) {
            return None;
        }
        self.expanded.insert(seq);
        let blob = self.selected_blob(view)?;
        matches!(blob.state, BlobState::NotDownloaded { .. })
            .then_some(Action::DownloadBlob(blob.id))
    }
}
