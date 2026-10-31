//! The head: core's latest snapshot plus what the GUI head keeps itself
//! (present's policies, the composer, the open prompts, what was sent to
//! Dart), driven the way the TUI drives them. Snapshots, intents, dispatch
//! results and wake-ups go in; signals for Dart and actions for the
//! dispatcher come out, in order. No rinf and no clock here: the loop in
//! `run` sends what comes out, and time comes in as a [`Clock`].

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use skywave_core::search::BlobText;
use skywave_core::{
    Action, BlobId, BlobState, Entry, PairUri, PairingView, Screen, Seq, SessionId, SessionView,
    SnippetId, TimelineItem, ViewState, parse_pair_uri,
};
use skywave_present::policy::{
    AlertWatcher, CompletionSettle, DraftSaver, PromptHistory, ReadMarker, Recall, TimelineSearch,
    looks_like_pair_uri,
};
use skywave_present::sections::{ItemKey, call_content, find, main_content, sections, wanted_blob};
use skywave_present::utf16::{chars_to_utf16, utf16_to_chars};

use crate::clip::{COPY_MAX, Copied, copy};
use crate::dispatcher::{Dispatched, Tag};
use crate::lifecycle::{self, Call, Moved, Target};
use crate::screen::{CreateUi, Ui, rename_initial, revoked};
use crate::signals::{
    ComposerView, CopyWhat, GuiIntent, LinkUpdate, PairForm, PlatformEffect, RenamePrompt,
    ScreenUpdate, SearchUpdate, TimelineRow, TimelineUpdate,
};
use crate::timeline::{BlobHeads, Rows, parse_key};
use crate::{intents, link, screen, search, timeline};

/// How often running clocks on screen (a tool's elapsed time, how long a
/// message has waited) are brought up to date.
pub const TICK: Duration = Duration::from_secs(1);

/// The time a step happens at: monotonic for the policies, wall-clock
/// (ms since the Unix epoch) for the labels counting against core's
/// wall-clock times.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clock {
    pub now: Instant,
    pub wall_ms: i64,
}

impl Clock {
    /// Now.
    pub fn read() -> Clock {
        let wall_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
        Clock {
            now: Instant::now(),
            wall_ms,
        }
    }

    /// `d` later.
    #[cfg(test)]
    pub fn after(self, d: Duration) -> Clock {
        Clock {
            now: self.now + d,
            wall_ms: self
                .wall_ms
                .saturating_add(i64::try_from(d.as_millis()).unwrap_or(i64::MAX)),
        }
    }
}

/// Something a step asks for, in the order it must happen.
#[derive(Debug, Clone, PartialEq)]
#[expect(
    clippy::large_enum_variant,
    reason = "a step's few outputs are sent at once and dropped"
)]
pub enum Out {
    Link(LinkUpdate),
    Screen(ScreenUpdate),
    Timeline(TimelineUpdate),
    Search(SearchUpdate),
    Effect(PlatformEffect),
    /// Hand `action` to the dispatcher with `tag`.
    Dispatch(Tag, Action),
    /// Pause the engine's network loops (`Core::suspend`).
    Suspend,
    /// Start them again (`Core::resume`).
    Resume,
}

/// What a dispatched action was for, so its result reaches its owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Purpose {
    /// A message from the composer of this session.
    Send(SessionId),
    /// A draft save of the open session.
    Draft,
    /// The new-session prompt's workdir.
    Create,
    /// The rename prompt's name.
    Rename,
    /// Anything else; only a failure matters.
    Plain,
}

/// The composer's text as the hub knows it: what Dart reported last, and
/// what the hub last set Dart's field to.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Composer {
    text: String,
    /// In `char`s, as the core counts.
    cursor: usize,
    /// What Dart's field was last set to; changes only with `rev`.
    shown: ComposerView,
}

impl Composer {
    /// Sets Dart's field to `text` with the cursor `cursor` chars in.
    fn new(text: String, cursor: usize, rev: u32) -> Composer {
        Composer {
            shown: ComposerView {
                cursor_utf16: u32::try_from(chars_to_utf16(&text, cursor)).unwrap_or(u32::MAX),
                text: text.clone(),
                rev,
                history_label: None,
                offline_label: None,
            },
            text,
            cursor,
        }
    }

    fn of(view: &SessionView, rev: u32) -> Composer {
        match &view.draft {
            Some(draft) => Composer::new(draft.text.clone(), draft.cursor, rev),
            None => Composer::new(String::new(), 0, rev),
        }
    }
}

/// The open session's head-side state.
#[derive(Debug)]
struct SessionHead {
    id: SessionId,
    composer: Composer,
    saver: DraftSaver,
    history: PromptHistory<(String, usize)>,
    marker: ReadMarker,
    /// The newest row is on screen (Dart says so).
    following: bool,
    /// Items whose content shows whole; only logged items expand.
    expanded: HashSet<Seq>,
    /// Blobs fetched because their item was expanded, so each is asked
    /// for once and not on every snapshot while its request is in flight.
    fetched: HashSet<BlobId>,
    search: Option<TimelineSearch>,
    heads: BlobHeads,
    /// The composer's message waiting for its result.
    sending: Option<Tag>,
}

impl SessionHead {
    fn new(view: &SessionView, rev: u32) -> SessionHead {
        SessionHead {
            id: view.id,
            composer: Composer::of(view, rev),
            saver: DraftSaver::new(view),
            history: PromptHistory::new(),
            marker: ReadMarker::new(view.id),
            following: false,
            expanded: HashSet::new(),
            fetched: HashSet::new(),
            search: None,
            heads: BlobHeads::default(),
            sending: None,
        }
    }
}

/// What Dart was sent last, so only changes go out.
#[derive(Debug, Default)]
struct Sent {
    link: Option<LinkUpdate>,
    screen: Option<ScreenUpdate>,
    /// The session the rows are of, and the rows.
    timeline: Option<(Option<SessionId>, Vec<TimelineRow>)>,
    search: Option<SearchUpdate>,
}

/// The GUI head's state. See the module docs.
#[derive(Debug, Default)]
pub struct Head {
    view: Option<ViewState>,
    alerts: Option<AlertWatcher>,
    next_tag: Tag,
    pending: HashMap<Tag, Purpose>,
    /// Bumped for every value the hub writes into a Dart field.
    revs: u32,
    form: PairForm,
    create: Option<CreateUi>,
    create_tag: Option<Tag>,
    rename: Option<RenamePrompt>,
    rename_tag: Option<Tag>,
    revoked_dismissed: bool,
    session: Option<SessionHead>,
    sent: Sent,
}

impl Head {
    pub fn new() -> Head {
        Head::default()
    }

    /// A snapshot from the core.
    pub fn on_view(&mut self, view: ViewState, clock: Clock) -> Vec<Out> {
        let mut out = Vec::new();
        match &mut self.alerts {
            // The first snapshot's approvals need no alert.
            None => self.alerts = Some(AlertWatcher::new(&view.attention)),
            Some(alerts) => {
                if let Some(alert) = alerts.observe(&view.attention) {
                    out.push(Out::Effect(PlatformEffect::Notify {
                        body: alert.notice(),
                        title: alert.title.unwrap_or_default(),
                        session_id: view
                            .attention
                            .approvals
                            .last()
                            .map(|a| a.session_id.0.to_string()),
                    }));
                }
            }
        }
        if !revoked(&view) {
            self.revoked_dismissed = false;
        }
        let was_pairing = self
            .view
            .as_ref()
            .is_some_and(|v| matches!(v.screen, Screen::Pairing(_)));
        match (was_pairing, matches!(view.screen, Screen::Pairing(_))) {
            (false, true) => self.prefill(&view),
            // Paired or cancelled; the code was single use anyway.
            (true, false) => {
                self.revs += 1;
                self.form = PairForm {
                    rev: self.revs,
                    ..PairForm::default()
                };
            }
            _ => {}
        }
        // A prompt goes with its screen.
        if !matches!(view.screen, Screen::SessionList(_)) {
            self.create = None;
        }
        if let Some(rename) = &self.rename {
            let fits = match &view.screen {
                Screen::SessionList(_) => true,
                Screen::Session(session) => session.id.0.to_string() == rename.session_id,
                _ => false,
            };
            if !fits {
                self.rename = None;
            }
        }
        let old_match = open_session(&self.view)
            .zip(self.session.as_ref())
            .and_then(|(view, session)| find(view, session.search.as_ref()?.current?));
        self.view = Some(view);
        if let (Some(create), Some(Screen::SessionList(list))) =
            (&mut self.create, self.view.as_ref().map(|v| &v.screen))
            && let Some(completion) = &list.completion
        {
            let before = create.input.clone();
            create.settle.settle(&mut create.input, completion);
            if create.input != before {
                self.revs += 1;
                create.rev = self.revs;
            }
        }
        self.follow_session();
        if let (Some(view), Some(session)) = (open_session(&self.view), &mut self.session) {
            if session.saver.refill(view) {
                self.revs += 1;
                session.composer = Composer::of(view, self.revs);
                session.history.end();
            }
            if let Some(search) = &mut session.search {
                search.refresh(view, old_match, BlobText::read);
            }
        }
        self.fetch_expanded(clock, &mut out);
        self.after(clock, &mut out);
        out
    }

    /// A GUI intent.
    pub fn on_intent(&mut self, intent: GuiIntent, clock: Clock) -> Vec<Out> {
        let mut out = Vec::new();
        // A keystroke changes no row: the timeline is not mapped again.
        let rows = !matches!(intent, GuiIntent::DraftEdited { .. });
        if let Err(text) = self.intent(intent, clock, &mut out) {
            out.push(Out::Effect(PlatformEffect::Error { text }));
        }
        self.save_draft(clock, false, &mut out);
        self.mark_read(clock, false, &mut out);
        self.publish(clock, rows, &mut out);
        out
    }

    /// The result of a dispatched action.
    pub fn on_dispatched(&mut self, done: Dispatched, clock: Clock) -> Vec<Out> {
        let mut out = Vec::new();
        let purpose = self.pending.remove(&done.tag).unwrap_or(Purpose::Plain);
        match purpose {
            Purpose::Send(id) => {
                if let Some(session) = self
                    .session
                    .as_mut()
                    .filter(|s| s.id == id && s.sending == Some(done.tag))
                {
                    session.sending = None;
                    if done.result.is_ok() {
                        // The enqueue dropped the core's draft. Clearing
                        // counts as an edit: if the core refused a blank
                        // message instead, the next save drops its draft.
                        self.revs += 1;
                        session.composer = Composer::new(String::new(), 0, self.revs);
                        session.saver.edited(clock.now);
                    }
                }
            }
            Purpose::Draft => {
                if done.result.is_err()
                    && let Some(session) = &mut self.session
                {
                    session.saver.failed(clock.now);
                }
            }
            Purpose::Create => {
                if done.result.is_ok() && self.create_tag == Some(done.tag) {
                    self.create = None;
                }
            }
            Purpose::Rename => {
                if done.result.is_ok() && self.rename_tag == Some(done.tag) {
                    self.rename = None;
                }
            }
            Purpose::Plain => {}
        }
        if let Err(err) = done.result {
            out.push(Out::Effect(PlatformEffect::Error {
                text: err.to_string(),
            }));
        }
        self.after(clock, &mut out);
        out
    }

    /// The wake-up [`Head::next_wake`] asked for (or any other).
    pub fn on_wake(&mut self, clock: Clock) -> Vec<Out> {
        let mut out = Vec::new();
        self.after(clock, &mut out);
        out
    }

    /// When something is due without input: a draft save, a read mark, the
    /// retry countdown, a running clock on screen. `None`: nothing is.
    pub fn next_wake(&self, clock: Clock) -> Option<Instant> {
        let view = self.view.as_ref()?;
        let open = open_session(&self.view);
        let session = self.session.as_ref();
        [
            session.and_then(|s| s.saver.next_wake()),
            session
                .zip(open)
                .and_then(|(s, v)| s.marker.next_wake(clock.now, v.last_seq, s.following)),
            link::next_change(&view.link, clock.wall_ms).map(|d| clock.now + d),
            open.filter(|v| timeline::live(v)).map(|_| clock.now + TICK),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    fn intent(
        &mut self,
        intent: GuiIntent,
        clock: Clock,
        out: &mut Vec<Out>,
    ) -> Result<(), String> {
        match intent {
            GuiIntent::AppReady => self.sent = Sent::default(),
            GuiIntent::Act { action } => {
                let action = intents::action(action)?;
                self.dispatch_user(action, Purpose::Plain, clock, out);
            }
            GuiIntent::DraftEdited {
                session_id,
                text,
                cursor_utf16,
            } => {
                let Some(session) = self.open_head(&session_id) else {
                    return Ok(());
                };
                let cursor = utf16_to_chars(&text, cursor_utf16.into());
                let composer = &mut session.composer;
                // Setting the field from `rev` may echo back: no edit.
                if composer.text != text || composer.cursor != cursor {
                    composer.text = text;
                    composer.cursor = cursor;
                    session.saver.edited(clock.now);
                    session.history.end();
                }
            }
            GuiIntent::ComposerSubmit { session_id } => {
                let Some(session) = self.open_head(&session_id) else {
                    return Ok(());
                };
                session.history.end();
                let id = session.id;
                // A blank message is refused by the core.
                let action = Action::SendMessage {
                    session_id: id,
                    text: session.composer.text.clone(),
                };
                let tag = self.dispatch_user(action, Purpose::Send(id), clock, out);
                if let Some(session) = &mut self.session {
                    session.sending = Some(tag);
                }
            }
            GuiIntent::HistoryOlder => self.recall(true, clock),
            GuiIntent::HistoryNewer => self.recall(false, clock),
            GuiIntent::SaveSnippet { key, name } => {
                let key = parse_key(&key)?;
                let text = open_session(&self.view)
                    .and_then(|view| prompt(&view.timeline[find(view, key)?]))
                    .map(str::to_owned);
                if let Some(text) = text {
                    self.save_snippet(name, text, clock, out);
                }
            }
            GuiIntent::SaveComposerSnippet { session_id, name } => {
                let Some(session) = self.open_head(&session_id) else {
                    return Ok(());
                };
                let text = session.composer.text.clone();
                self.save_snippet(name, text, clock, out);
            }
            GuiIntent::InsertSnippet {
                session_id,
                snippet_id,
            } => {
                let id: SnippetId = intents::id("snippet", &snippet_id)?;
                let Some(snippet) = self
                    .view
                    .as_ref()
                    .and_then(|view| view.snippets.iter().find(|s| s.id == id))
                    .map(|s| s.text.clone())
                else {
                    return Ok(());
                };
                self.revs += 1;
                let rev = self.revs;
                let Some(session) = self.open_head(&session_id) else {
                    return Ok(());
                };
                let composer = &session.composer;
                let at = composer
                    .text
                    .char_indices()
                    .nth(composer.cursor)
                    .map_or(composer.text.len(), |(at, _)| at);
                let mut text = composer.text.clone();
                text.insert_str(at, &snippet);
                let cursor = composer.cursor + snippet.chars().count();
                session.composer = Composer::new(text, cursor, rev);
                session.saver.edited(clock.now);
                session.history.end();
                // Saving the draft first keeps the inserted text too.
                self.dispatch_user(Action::UseSnippet(id), Purpose::Plain, clock, out);
            }
            GuiIntent::FollowingChanged { following } => {
                if let Some(session) = &mut self.session {
                    session.following = following;
                }
            }
            GuiIntent::ExpandItem { key } => {
                let Some(seq) = logged(parse_key(&key)?) else {
                    return Ok(());
                };
                let Some(entry) = self.entry(seq) else {
                    return Ok(());
                };
                if sections(entry).is_empty() {
                    return Ok(());
                }
                let wanted = expand_fetch(entry);
                if let Some(session) = &mut self.session {
                    session.expanded.insert(seq);
                    // Expanding again asks again.
                    if let Some(id) = wanted {
                        session.fetched.remove(&id);
                    }
                }
                self.fetch_expanded(clock, out);
            }
            GuiIntent::CollapseItem { key } => {
                if let (Some(seq), Some(session)) = (logged(parse_key(&key)?), &mut self.session) {
                    session.expanded.remove(&seq);
                }
            }
            GuiIntent::DownloadItem { key } => {
                let fetch = logged(parse_key(&key)?)
                    .and_then(|seq| self.entry(seq))
                    .and_then(wanted_blob)
                    .map(|b| Action::DownloadBlob(b.id));
                if let Some(fetch) = fetch {
                    self.dispatch_user(fetch, Purpose::Plain, clock, out);
                }
            }
            GuiIntent::OpenFull { blob_id } => {
                let id = intents::id("blob", &blob_id)?;
                let state = open_session(&self.view).and_then(|view| {
                    view.timeline.iter().find_map(|item| match item {
                        TimelineItem::Logged { entry, .. } => sections(entry)
                            .into_iter()
                            .filter_map(|s| s.blob)
                            .find(|b| b.id == id)
                            .map(|b| b.state.clone()),
                        TimelineItem::Pending { .. } => None,
                    })
                });
                out.push(Out::Effect(match state {
                    Some(BlobState::Available { path }) => PlatformEffect::OpenFile {
                        path: path.display().to_string(),
                    },
                    _ => PlatformEffect::Notice {
                        text: "not downloaded yet".to_owned(),
                    },
                }));
            }
            GuiIntent::Copy { key, what } => {
                let key = parse_key(&key)?;
                let content = open_session(&self.view).and_then(|view| {
                    let item = &view.timeline[find(view, key)?];
                    match what {
                        CopyWhat::Main => main_content(item),
                        CopyWhat::Call => call_content(item),
                    }
                });
                let Some(content) = content else {
                    return Ok(());
                };
                match copy(content, COPY_MAX) {
                    Copied::Text { text, notice } => {
                        out.push(Out::Effect(PlatformEffect::CopyToClipboard { text }));
                        out.push(Out::Effect(PlatformEffect::Notice { text: notice }));
                    }
                    Copied::Fetch { fetch, notice } => {
                        if let Some(id) = fetch {
                            self.dispatch_user(
                                Action::DownloadBlob(id),
                                Purpose::Plain,
                                clock,
                                out,
                            );
                        }
                        out.push(Out::Effect(PlatformEffect::Notice { text: notice }));
                    }
                    Copied::Refused(text) => out.push(Out::Effect(PlatformEffect::Notice { text })),
                    Copied::Failed(text) => return Err(text),
                }
            }
            GuiIntent::SearchTyped { query } => {
                let Some((view, session)) = open_session(&self.view).zip(self.session.as_mut())
                else {
                    return Ok(());
                };
                let query: String = query
                    .chars()
                    .map(|c| if c == '\n' { ' ' } else { c })
                    .filter(|c| !c.is_control())
                    .collect();
                let search = session.search.get_or_insert_with(TimelineSearch::new);
                search.wrapped = false;
                search.edit(view, |text| *text = query, BlobText::read);
            }
            GuiIntent::SearchStep { older, from_key } => {
                let from = from_key.as_deref().map(parse_key).transpose()?;
                let Some((view, session)) = open_session(&self.view).zip(self.session.as_mut())
                else {
                    return Ok(());
                };
                let Some(search) = &mut session.search else {
                    return Ok(());
                };
                let from = from
                    .and_then(|key| find(view, key))
                    .unwrap_or(view.timeline.len().saturating_sub(1));
                search.wrapped = false;
                if let Some(jump) = search.jump(view, older, from)
                    && let Some(seq) = jump.expand
                {
                    session.expanded.insert(seq);
                }
            }
            GuiIntent::SearchClose => {
                if let Some(session) = &mut self.session {
                    session.search = None;
                }
            }
            GuiIntent::CreateOpen => {
                if let Some(Screen::SessionList(list)) = self.view.as_ref().map(|v| &v.screen)
                    && list.kind == skywave_core::ListKind::Active
                {
                    self.create = Some(CreateUi::default());
                }
            }
            GuiIntent::CompletionTyped { input } => {
                let Some(create) = &mut self.create else {
                    return Ok(());
                };
                create.settle.input();
                create.input = input.chars().filter(|c| !c.is_control()).collect();
                if let Some(action) = create.settle.edited(&create.input) {
                    self.dispatch_user(action, Purpose::Plain, clock, out);
                }
            }
            GuiIntent::CompletionComplete => {
                let candidates = self.candidates();
                let Some(create) = &mut self.create else {
                    return Ok(());
                };
                create.settle.input();
                if candidates.is_empty() {
                    let action = create.settle.complete(&create.input);
                    self.dispatch_user(action, Purpose::Plain, clock, out);
                } else if let Some(chosen) = create.settle.choose(&candidates) {
                    self.revs += 1;
                    create.input = chosen;
                    create.rev = self.revs;
                }
            }
            GuiIntent::CompletionMove { up } => {
                let len = self.candidates().len();
                if let Some(create) = self.create.as_mut().filter(|_| len > 0) {
                    create.settle.input();
                    if up {
                        create.settle.up();
                    } else {
                        create.settle.down(len);
                    }
                }
            }
            GuiIntent::CompletionAccept => {
                let candidates = self.candidates();
                let Some(create) = &mut self.create else {
                    return Ok(());
                };
                create.settle.input();
                if candidates.is_empty() {
                    // Blank or unacceptable paths are refused by the core
                    // or the server, not here.
                    let action = Action::CreateSession {
                        workdir: create.input.clone(),
                    };
                    let tag = self.dispatch_user(action, Purpose::Create, clock, out);
                    self.create_tag = Some(tag);
                } else if let Some(chosen) = create.settle.choose(&candidates) {
                    self.revs += 1;
                    create.input = chosen;
                    create.rev = self.revs;
                }
            }
            GuiIntent::CompletionPick { index } => {
                let candidates = self.candidates();
                let Some(create) = &mut self.create else {
                    return Ok(());
                };
                let Some(index) = usize::try_from(index)
                    .ok()
                    .filter(|&index| index < candidates.len())
                else {
                    return Ok(());
                };
                create.settle.input();
                // Highlight it, then take it as Enter would.
                for _ in 0..create.settle.selected() {
                    create.settle.up();
                }
                for _ in 0..index {
                    create.settle.down(candidates.len());
                }
                if let Some(chosen) = create.settle.choose(&candidates) {
                    self.revs += 1;
                    create.input = chosen;
                    create.rev = self.revs;
                }
            }
            GuiIntent::CompletionCancel => {
                let completing = matches!(
                    self.view.as_ref().map(|v| &v.screen),
                    Some(Screen::SessionList(list)) if list.completion.is_some()
                );
                let Some(create) = &mut self.create else {
                    return Ok(());
                };
                create.settle.input();
                if create.settle.open() {
                    create.settle.close();
                } else {
                    let end = create.settle.cancel(completing);
                    self.create = None;
                    if let Some(end) = end {
                        self.dispatch_user(end, Purpose::Plain, clock, out);
                    }
                }
            }
            GuiIntent::RenameOpen { session_id } => {
                let id: SessionId = intents::id("session", &session_id)?;
                let initial = match self.view.as_ref().map(|v| &v.screen) {
                    Some(Screen::SessionList(list)) => list
                        .sessions
                        .iter()
                        .find(|row| row.id == id)
                        .map(|row| rename_initial(row.named, &row.title)),
                    Some(Screen::Session(view)) if view.id == id => {
                        Some(rename_initial(view.named, &view.title))
                    }
                    _ => None,
                };
                self.rename = initial.map(|initial| RenamePrompt {
                    session_id,
                    initial,
                });
            }
            GuiIntent::RenameSubmit { name } => {
                let Some(rename) = &self.rename else {
                    return Ok(());
                };
                let action = Action::RenameSession {
                    session_id: intents::id("session", &rename.session_id)?,
                    name,
                };
                let tag = self.dispatch_user(action, Purpose::Rename, clock, out);
                self.rename_tag = Some(tag);
            }
            GuiIntent::RenameCancel => self.rename = None,
            GuiIntent::PairSubmit {
                address,
                code,
                fingerprint,
            } => {
                // A URI typed or pasted into one field fills the form.
                if let Some(uri) = [&address, &code, &fingerprint]
                    .into_iter()
                    .find(|field| looks_like_pair_uri(field))
                {
                    let uri = parse_pair_uri(uri).map_err(|err| err.to_string())?;
                    self.fill(uri);
                    return Ok(());
                }
                // Validation is the core's: an incomplete form fails there.
                let action = Action::Pair {
                    address,
                    code,
                    fingerprint,
                };
                self.dispatch_user(action, Purpose::Plain, clock, out);
            }
            GuiIntent::PairPasted { text } => match parse_pair_uri(&text) {
                Ok(uri) => self.fill(uri),
                Err(err) if looks_like_pair_uri(&text) => return Err(err.to_string()),
                Err(_) => {}
            },
            GuiIntent::PairScanned { text } => {
                let uri = parse_pair_uri(&text).map_err(|err| err.to_string())?;
                self.fill(uri);
            }
            GuiIntent::RevokedDismiss => self.revoked_dismissed = true,
            GuiIntent::AppBackgrounded => self.moved(Moved::Backgrounded, clock, out),
            GuiIntent::AppForegrounded => self.moved(Moved::Foregrounded, clock, out),
        }
        Ok(())
    }

    /// Dispatches a user's `action`: the open session's draft is saved
    /// first, unless it is a message, whose enqueue drops the draft anyway;
    /// leaving the session marks what was seen read first.
    fn dispatch_user(
        &mut self,
        action: Action,
        purpose: Purpose,
        clock: Clock,
        out: &mut Vec<Out>,
    ) -> Tag {
        if DraftSaver::flushes_before(&action) {
            self.save_draft(clock, true, out);
        }
        if ReadMarker::marks_before(&action) {
            self.mark_read(clock, true, out);
        }
        self.dispatch(action, purpose, out)
    }

    /// The app left or came back to the screen: what that does to the core
    /// on this platform is [`lifecycle::calls`]'s to say. The calls go out
    /// in order, so a save is queued on the dispatcher before the suspend.
    fn moved(&mut self, moved: Moved, clock: Clock, out: &mut Vec<Out>) {
        for call in lifecycle::calls(moved, Target::BUILT) {
            match call {
                Call::SaveDraft => self.save_draft(clock, true, out),
                Call::MarkRead => self.mark_read(clock, true, out),
                Call::Suspend => out.push(Out::Suspend),
                Call::Resume => out.push(Out::Resume),
                Call::Dispatch(action) => {
                    self.dispatch(action, Purpose::Plain, out);
                }
            }
        }
    }

    fn dispatch(&mut self, action: Action, purpose: Purpose, out: &mut Vec<Out>) -> Tag {
        self.next_tag += 1;
        let tag = self.next_tag;
        self.pending.insert(tag, purpose);
        out.push(Out::Dispatch(tag, action));
        tag
    }

    /// Keeps `text` as a snippet named `name` and says so; blank text is
    /// the core's to refuse, and not worth a word.
    fn save_snippet(&mut self, name: String, text: String, clock: Clock, out: &mut Vec<Out>) {
        if text.trim().is_empty() {
            return;
        }
        self.dispatch_user(
            Action::SaveSnippet { name, text },
            Purpose::Plain,
            clock,
            out,
        );
        out.push(Out::Effect(PlatformEffect::Notice {
            text: "saved as a snippet".to_owned(),
        }));
    }

    /// Saves the open session's draft when [`DraftSaver::save`] says so.
    fn save_draft(&mut self, clock: Clock, force: bool, out: &mut Vec<Out>) {
        let Some(session) = &mut self.session else {
            return;
        };
        let composer = &session.composer;
        if let Some(action) = session
            .saver
            .save(clock.now, force, &composer.text, composer.cursor)
        {
            self.dispatch(action, Purpose::Draft, out);
        }
    }

    /// Marks the open session read when [`ReadMarker::mark`] says so.
    fn mark_read(&mut self, clock: Clock, force: bool, out: &mut Vec<Out>) {
        let Some((view, session)) = open_session(&self.view).zip(self.session.as_mut()) else {
            return;
        };
        if let Some(action) =
            session
                .marker
                .mark(clock.now, view.last_seq, session.following, force)
        {
            self.dispatch(action, Purpose::Plain, out);
        }
    }

    /// The end of every step: what is due unforced, then what changed for
    /// Dart.
    /// Expanding downloads what is not here yet (spec §8), including what
    /// arrives while the item is open: an edit expanded from the approval
    /// panel before its result logged gets the result's output once it does.
    /// Each blob is asked for once per expansion.
    fn fetch_expanded(&mut self, clock: Clock, out: &mut Vec<Out>) {
        let Some((view, session)) = open_session(&self.view).zip(self.session.as_mut()) else {
            return;
        };
        let fetches: Vec<BlobId> = view
            .timeline
            .iter()
            .filter_map(|item| match item {
                TimelineItem::Logged { seq, entry, .. } if session.expanded.contains(seq) => {
                    expand_fetch(entry)
                }
                _ => None,
            })
            .filter(|id| session.fetched.insert(*id))
            .collect();
        for id in fetches {
            self.dispatch_user(Action::DownloadBlob(id), Purpose::Plain, clock, out);
        }
    }

    fn after(&mut self, clock: Clock, out: &mut Vec<Out>) {
        self.save_draft(clock, false, out);
        self.mark_read(clock, false, out);
        self.publish(clock, true, out);
    }

    /// Sends Dart what changed since it was last sent; the timeline and the
    /// search only if `rows` may have changed.
    fn publish(&mut self, clock: Clock, rows: bool, out: &mut Vec<Out>) {
        let Some(view) = &self.view else {
            return;
        };
        let banner = link::update(
            &view.link,
            view.queued,
            view.attention.elsewhere,
            clock.wall_ms,
        );
        if self.sent.link.as_ref() != Some(&banner) {
            self.sent.link = Some(banner.clone());
            out.push(Out::Link(banner));
        }

        let composer = self.session.as_ref().map(|s| ComposerView {
            history_label: s.history.position().map(|(at, of)| format!("{at} of {of}")),
            ..s.composer.shown.clone()
        });
        let ui = Ui {
            form: &self.form,
            create: self.create.as_ref(),
            rename: self.rename.as_ref(),
            composer: composer.as_ref(),
            revoked_dismissed: self.revoked_dismissed,
        };
        let shown = screen::update(view, ui);
        if self.sent.screen.as_ref() != Some(&shown) {
            self.sent.screen = Some(shown.clone());
            out.push(Out::Screen(shown));
        }
        if !rows {
            return;
        }

        let open = open_session(&self.view);
        let (id, rows) = match (open, &mut self.session) {
            (Some(view), Some(session)) => (
                Some(view.id),
                timeline::rows(view, &session.expanded, &mut session.heads, clock.wall_ms),
            ),
            _ => (None, Rows::default()),
        };
        let updates = match &self.sent.timeline {
            Some((sent_id, old)) if *sent_id == id => timeline::diff(old, &rows),
            _ => None,
        };
        match updates {
            Some(updates) => out.extend(updates.into_iter().map(Out::Timeline)),
            None => out.push(Out::Timeline(TimelineUpdate::Snapshot {
                session_id: id.map(|id| id.0.to_string()),
                items: rows.rows.clone(),
            })),
        }
        let found = match (open, self.session.as_ref().and_then(|s| s.search.as_ref())) {
            (Some(view), Some(search)) => search::update(search, view, &rows.rows),
            _ => search::closed(),
        };
        self.sent.timeline = Some((id, rows.rows));
        if self.sent.search.as_ref() != Some(&found) {
            self.sent.search = Some(found.clone());
            out.push(Out::Search(found));
        }
    }

    /// A fresh session head when another session opens; none off a session.
    fn follow_session(&mut self) {
        match open_session(&self.view) {
            Some(view) if self.session.as_ref().is_none_or(|s| s.id != view.id) => {
                self.revs += 1;
                self.session = Some(SessionHead::new(view, self.revs));
            }
            Some(_) => {}
            None => self.session = None,
        }
    }

    /// The open session's head, if `session_id` names it.
    fn open_head(&mut self, session_id: &str) -> Option<&mut SessionHead> {
        self.session
            .as_mut()
            .filter(|s| s.id.0.to_string() == session_id)
    }

    /// Shows the next older (`older`) or newer earlier prompt; newer than
    /// the newest is the text that was being written. The shown text is
    /// the draft: it is saved like typed text.
    fn recall(&mut self, older: bool, clock: Clock) {
        let Some((view, session)) = open_session(&self.view).zip(self.session.as_mut()) else {
            return;
        };
        let composer = &session.composer;
        let stash = (composer.text.clone(), composer.cursor);
        let next = if older {
            session
                .history
                .older(view, || stash)
                .map(|prompt| (prompt.to_owned(), prompt.chars().count()))
        } else {
            session.history.newer().map(|recall| match recall {
                Recall::Prompt(prompt) => (prompt.to_owned(), prompt.chars().count()),
                Recall::Stash(stash) => stash,
            })
        };
        let Some((text, cursor)) = next else {
            return;
        };
        self.revs += 1;
        session.composer = Composer::new(text, cursor, self.revs);
        session.saver.edited(clock.now);
    }

    /// The open session's logged entry at `seq`.
    fn entry(&self, seq: Seq) -> Option<&Entry> {
        open_session(&self.view)?
            .timeline
            .iter()
            .find_map(|item| match item {
                TimelineItem::Logged { seq: s, entry, .. } if *s == seq => Some(entry),
                _ => None,
            })
    }

    /// The candidates the new-session prompt lists, while its list is open.
    fn candidates(&self) -> Vec<skywave_core::Candidate> {
        let open = self
            .create
            .as_ref()
            .is_some_and(|c| CompletionSettle::open(&c.settle));
        match self.view.as_ref().map(|v| &v.screen) {
            Some(Screen::SessionList(list)) if open => list
                .completion
                .as_ref()
                .map(|c| c.candidates.clone())
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    /// Re-pairing starts from the address the device is paired with.
    fn prefill(&mut self, view: &ViewState) {
        if let Screen::Pairing(PairingView {
            paired: Some(info), ..
        }) = &view.screen
            && let Some(address) = &info.server_address
            && self.form.address.is_empty()
        {
            self.revs += 1;
            self.form = PairForm {
                address: address.clone(),
                rev: self.revs,
                ..PairForm::default()
            };
        }
    }

    /// Fills every field from a pairing URI, in the core's canonical form.
    fn fill(&mut self, uri: PairUri) {
        self.revs += 1;
        self.form = PairForm {
            address: uri.address,
            code: uri.code,
            fingerprint: uri.fingerprint,
            rev: self.revs,
        };
    }
}

/// The open session in `view`, if a session is the screen.
/// The blob expanding `entry` downloads: present's [`wanted_blob`], unless
/// it was asked for already (a failed one waits for an explicit retry).
fn expand_fetch(entry: &Entry) -> Option<BlobId> {
    wanted_blob(entry)
        .filter(|b| matches!(b.state, BlobState::NotDownloaded { .. }))
        .map(|b| b.id)
}

fn open_session(view: &Option<ViewState>) -> Option<&SessionView> {
    match &view.as_ref()?.screen {
        Screen::Session(session) => Some(session),
        _ => None,
    }
}

/// A message the user wrote, logged or still on its way.
fn prompt(item: &TimelineItem) -> Option<&str> {
    match item {
        TimelineItem::Pending { text, .. }
        | TimelineItem::Logged {
            entry: Entry::UserMessage { text, .. },
            ..
        } => Some(text),
        TimelineItem::Logged { .. } => None,
    }
}

/// The seq of a logged item's key; pending messages do not expand.
fn logged(key: ItemKey) -> Option<Seq> {
    match key {
        ItemKey::Logged(seq) => Some(seq),
        ItemKey::Pending(_) => None,
    }
}

#[cfg(test)]
#[path = "head_tests.rs"]
mod tests;
