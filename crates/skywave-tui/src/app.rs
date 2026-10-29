//! Head-side state: the latest [`ViewState`] from the core plus presentation
//! state the core does not care about (form buffers, focus, the help
//! overlay, list selection). Keys become core [`Action`]s here; nothing in
//! this module decides anything about pairing, sync or delivery (D2).

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use skywave_core::{
    Action, BlobPolicy, CoreError, Link, LinkRejection, PairingView, Route, Screen, ViewState,
};

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

/// Presentation state owned by the head.
#[derive(Debug, Clone)]
pub struct UiState {
    pub help: bool,
    pub form: PairingForm,
    /// Selected row on the session list.
    pub selected: usize,
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
                selected: 0,
                revoked_dismissed: false,
                link_since: now,
                error: None,
                quit: false,
            },
        };
        app.prefill(&view);
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
        if let Screen::SessionList(list) = &view.screen {
            self.ui.selected = self.ui.selected.min(list.sessions.len().saturating_sub(1));
        }
        self.view = view;
    }

    /// Records the outcome of dispatching an action from [`App::on_key`].
    pub fn dispatched(&mut self, result: Result<(), CoreError>) {
        self.ui.error = result.err().map(|err| err.to_string());
    }

    /// The revoked full-screen state covers the current screen.
    pub fn revoked_screen(&self) -> bool {
        matches!(self.view.link, Link::Rejected(LinkRejection::Revoked))
            && !matches!(self.view.screen, Screen::Pairing(_))
            && !self.ui.revoked_dismissed
    }

    /// The focused element takes text, so letters are input, not commands.
    pub fn typing(&self) -> bool {
        matches!(self.view.screen, Screen::Pairing(_)) && !self.revoked_screen()
    }

    /// Bracketed paste: a pairing URI fills the whole form, anything else
    /// goes into the focused field.
    pub fn on_paste(&mut self, text: &str) {
        if !self.typing() || self.ui.help {
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
            Screen::SessionList(list) => match key.code {
                KeyCode::Char('q') => self.quit(),
                KeyCode::Up | KeyCode::Char('k') => {
                    self.ui.selected = self.ui.selected.saturating_sub(1);
                    None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let last = list.sessions.len().saturating_sub(1);
                    self.ui.selected = (self.ui.selected + 1).min(last);
                    None
                }
                KeyCode::Enter => list
                    .sessions
                    .get(self.ui.selected)
                    .map(|row| Action::OpenSession(row.id)),
                KeyCode::Char('s') => Some(Action::Navigate(Route::Settings)),
                _ => None,
            },
            Screen::Session(_) => match key.code {
                KeyCode::Char('q') => self.quit(),
                KeyCode::Esc => Some(Action::Navigate(Route::SessionList)),
                _ => None,
            },
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
