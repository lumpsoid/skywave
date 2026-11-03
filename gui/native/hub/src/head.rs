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
    Action, BlobId, BlobState, CallId, CommandId, ComposerMode, Entry, PairUri, PairingView,
    RunWhen, Screen, Seq, SessionId, SessionView, SnippetId, TimelineItem, ViewState,
    parse_pair_uri,
};
use skywave_present::policy::{
    AlertWatcher, CompletionSettle, DraftSaver, PromptHistory, ReadMarker, Recall,
    ShellAlertWatcher, TimelineSearch, looks_like_pair_uri, shell_stop,
};
use skywave_present::sections::{ItemKey, call_content, find, main_content, sections, wanted_blob};
use skywave_present::usage::{Grouping, Span};
use skywave_present::utf16::{chars_to_utf16, utf16_to_chars};

use crate::clip::{COPY_MAX, Copied, copy, copy_output};
use crate::dispatcher::{Dispatched, Tag};
use crate::lifecycle::{self, Call, Moved, Target};
use crate::screen::{CreateUi, Ui, rename_initial, revoked};
use crate::signals::{
    ComposerModeView, ComposerView, CopyWhat, GuiIntent, LinkUpdate, PairForm, PlatformEffect,
    RenamePrompt, ScreenUpdate, SearchUpdate, ShellRunRow, ShellUpdate, TimelineRow,
    TimelineUpdate, UsageGroupingView, UsageSpanView,
};
use crate::timeline::{BlobHeads, Rows, parse_key};
use crate::workdir::{WorkdirText, shown_blob};
use crate::{intents, link, screen, search, shell, timeline};

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
    Shell(ShellUpdate),
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
                to_label: None,
                // The mode's own fields are the open session's, set when
                // published.
                mode: ComposerModeView::Agent,
                shell_enabled: false,
                prompt: None,
                can_stop: false,
            },
            text,
            cursor,
        }
    }

    /// The core's draft of `view`'s mode.
    fn of(view: &SessionView, rev: u32) -> Composer {
        let draft = match view.mode {
            ComposerMode::Agent => &view.draft,
            ComposerMode::Shell => &view.shell_draft,
        };
        match draft {
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
    /// What the composer writes, as the core last said
    /// ([`SessionView::mode`]). Followed only once the core switched, after
    /// the old mode's draft was saved: switching on the intent would save
    /// the new mode's text under the old mode.
    mode: ComposerMode,
    /// The condition the shell command being written runs on, saved with
    /// the shell draft; back to always after each run sent.
    when: RunWhen,
    /// Earlier shell commands being recalled, apart from the prompts.
    shell_history: PromptHistory<(String, usize)>,
    marker: ReadMarker,
    /// The newest row is on screen (Dart says so).
    following: bool,
    /// The `Agent` call of the subagent shown; `None` in the main view.
    subagent: Option<CallId>,
    /// Items whose content shows whole; only logged items expand.
    expanded: HashSet<Seq>,
    /// Blobs fetched because their item was expanded, so each is asked
    /// for once and not on every snapshot while its request is in flight.
    fetched: HashSet<BlobId>,
    search: Option<TimelineSearch>,
    heads: BlobHeads,
    /// The shell runs whose rows carry their whole output.
    shell_expanded: HashSet<CommandId>,
    outputs: shell::Outputs,
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
            mode: view.mode,
            when: when_of(view),
            shell_history: PromptHistory::new(),
            marker: ReadMarker::new(view.id),
            following: false,
            subagent: view.subagent.as_ref().map(|s| s.call),
            expanded: HashSet::new(),
            fetched: HashSet::new(),
            search: None,
            heads: BlobHeads::default(),
            shell_expanded: HashSet::new(),
            outputs: shell::Outputs::default(),
            sending: None,
        }
    }

    /// The newest row of the main view is on screen: a subagent's view
    /// says nothing of the main view's news, so nothing is read there.
    fn following(&self, view: &SessionView) -> bool {
        self.following && view.subagent.is_none()
    }

    /// Ends browsing earlier prompts or commands: the shown one stays.
    fn end_history(&mut self) {
        self.history.end();
        self.shell_history.end();
    }

    /// Loads the composer from the core's draft of `view`'s mode, with its
    /// condition, under a new `rev`.
    fn load(&mut self, view: &SessionView, rev: u32) {
        self.composer = Composer::of(view, rev);
        self.when = when_of(view);
        self.end_history();
    }

    fn shell(&self) -> bool {
        self.mode == ComposerMode::Shell
    }
}

/// The condition the shell draft was saved with.
fn when_of(view: &SessionView) -> RunWhen {
    view.shell_draft
        .as_ref()
        .map_or(RunWhen::Always, |d| d.when)
}

/// What Dart was sent last, so only changes go out.
#[derive(Debug, Default)]
struct Sent {
    link: Option<LinkUpdate>,
    screen: Option<ScreenUpdate>,
    /// The session the rows are of, the subagent whose view they are
    /// (`None` for the main view), and the rows.
    timeline: Option<(Option<SessionId>, Option<CallId>, Vec<TimelineRow>)>,
    /// The session the shell runs are of, and the runs.
    shell: Option<(Option<SessionId>, Vec<ShellRunRow>)>,
    search: Option<SearchUpdate>,
}

/// The GUI head's state. See the module docs.
#[derive(Debug, Default)]
pub struct Head {
    view: Option<ViewState>,
    alerts: Option<AlertWatcher>,
    /// From 0, not from the first snapshot: a run that finished in a
    /// catch-up before it is still news.
    shell_alerts: ShellAlertWatcher,
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
    /// The usage screen's days and grouping (`UsageShow`).
    usage: (Span, Grouping),
    /// The text of the workdir diff or file shown.
    workdir_text: WorkdirText,
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
        if let Some(notice) = self.shell_alerts.observe(&view.attention) {
            out.push(Out::Effect(PlatformEffect::Notify {
                body: notice.notice(),
                title: notice.title,
                session_id: view
                    .attention
                    .last_shell
                    .as_ref()
                    .map(|s| s.session_id.0.to_string()),
            }));
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
        let switched = open_session(&self.view)
            .zip(self.session.as_ref())
            .is_some_and(|(view, session)| view.mode != session.mode);
        if switched {
            // What was typed while the switch was on its way is the old
            // mode's: saved under it before the composer swaps.
            self.save_draft(clock, true, &mut out);
        }
        if let (Some(view), Some(session)) = (open_session(&self.view), &mut self.session) {
            // The core switched modes: the other mode's draft shows, with
            // its condition. Both drafts stay in the core.
            if view.mode != session.mode {
                session.mode = view.mode;
                self.revs += 1;
                session.load(view, self.revs);
            }
            if session.saver.refill(view) {
                self.revs += 1;
                session.load(view, self.revs);
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
                        // A run's condition is for that run only.
                        if session.shell() {
                            session.when = RunWhen::Always;
                        }
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

    /// The hub is stopping: what must reach the core before it closes, the
    /// open session's unsaved draft and read mark, both forced. Only
    /// dispatches: Dart may be gone already.
    pub fn on_stop(&mut self, clock: Clock) -> Vec<Out> {
        let mut out = Vec::new();
        self.moved(Moved::Closed, clock, &mut out);
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
                .and_then(|(s, v)| s.marker.next_wake(clock.now, v.last_seq, s.following(v))),
            link::next_change(&view.link, clock.wall_ms).map(|d| clock.now + d),
            open.filter(|v| timeline::live(v) || shell::live(v))
                .map(|_| clock.now + TICK),
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
                    session.end_history();
                }
            }
            GuiIntent::ComposerSubmit { session_id } => {
                let Some(session) = self.open_head(&session_id) else {
                    return Ok(());
                };
                session.end_history();
                let id = session.id;
                let text = session.composer.text.clone();
                let action = if session.shell() {
                    // Nothing to run: the condition picked stays.
                    if text.trim().is_empty() {
                        return Ok(());
                    }
                    Action::RunShell {
                        session_id: id,
                        command: text,
                        when: session.when,
                    }
                } else {
                    // A blank message is refused by the core.
                    Action::SendMessage {
                        session_id: id,
                        text,
                    }
                };
                let tag = self.dispatch_user(action, Purpose::Send(id), clock, out);
                if let Some(session) = &mut self.session {
                    session.sending = Some(tag);
                }
            }
            GuiIntent::HistoryOlder => self.recall(true, clock),
            GuiIntent::HistoryNewer => self.recall(false, clock),
            GuiIntent::ToggleShellMode { session_id } => {
                let Some(session) = self.open_head(&session_id) else {
                    return Ok(());
                };
                let (id, shell) = (session.id, session.shell());
                // Into shell mode only while the server runs shell
                // commands; back out always, should it have stopped since.
                let enabled = open_session(&self.view).is_some_and(|v| v.shell.enabled);
                if !shell && !enabled {
                    out.push(Out::Effect(PlatformEffect::Notice {
                        text: "shell commands are off on this server".to_owned(),
                    }));
                    return Ok(());
                }
                let mode = if shell {
                    ComposerMode::Agent
                } else {
                    ComposerMode::Shell
                };
                // Saves the draft under the mode it was written in first;
                // the composer swaps once the core switched (`on_view`).
                let action = Action::SetComposerMode {
                    session_id: id,
                    mode,
                };
                self.dispatch_user(action, Purpose::Plain, clock, out);
            }
            GuiIntent::CycleCondition { session_id } => {
                if let Some(session) = self.open_head(&session_id).filter(|s| s.shell()) {
                    session.when = shell::next_when(session.when);
                    // Saved with the draft, blank or not.
                    session.saver.edited(clock.now);
                }
            }
            GuiIntent::StopShell { session_id } => {
                let action = open_session(&self.view)
                    .filter(|view| view.id.0.to_string() == session_id)
                    .and_then(shell_stop);
                if let Some(action) = action {
                    self.dispatch_user(action, Purpose::Plain, clock, out);
                }
            }
            GuiIntent::StopShellRun { session_id, run_id } => {
                let run = intents::id("run", &run_id)?;
                let action = open_session(&self.view)
                    .filter(|view| view.id.0.to_string() == session_id)
                    .and_then(|view| shell::run_stop(view, run));
                if let Some(action) = action {
                    self.dispatch_user(action, Purpose::Plain, clock, out);
                }
            }
            GuiIntent::ExpandShellRun { session_id, run_id } => {
                let run: CommandId = intents::id("run", &run_id)?;
                let Some(view) = open_session(&self.view) else {
                    return Ok(());
                };
                let fetch = shell::output_fetch(view, run);
                let Some(session) = self.open_head(&session_id) else {
                    return Ok(());
                };
                session.shell_expanded.insert(run);
                if let Some(fetch) = fetch {
                    self.dispatch_user(fetch, Purpose::Plain, clock, out);
                }
            }
            GuiIntent::CollapseShellRun { session_id, run_id } => {
                let run: CommandId = intents::id("run", &run_id)?;
                if let Some(session) = self.open_head(&session_id) {
                    session.shell_expanded.remove(&run);
                }
            }
            GuiIntent::CopyShellOutput { session_id, run_id } => {
                let run: CommandId = intents::id("run", &run_id)?;
                let copied = open_session(&self.view)
                    .filter(|view| view.id.0.to_string() == session_id)
                    .and_then(|view| view.shell.runs.iter().find(|r| r.run == run))
                    .map(|run| copy_output(run, COPY_MAX));
                if let Some(copied) = copied {
                    self.copied(copied, clock, out)?;
                }
            }
            GuiIntent::RunSnippet {
                session_id,
                snippet_id,
            } => {
                let id: SnippetId = intents::id("snippet", &snippet_id)?;
                let Some(command) = self
                    .view
                    .as_ref()
                    .and_then(|view| view.snippets.iter().find(|s| s.id == id))
                    .filter(|s| s.kind == ComposerMode::Shell)
                    .map(|s| s.text.clone())
                else {
                    return Ok(());
                };
                let Some(session) = self.open_head(&session_id).filter(|s| s.shell()) else {
                    return Ok(());
                };
                let action = Action::RunShell {
                    session_id: session.id,
                    command,
                    when: RunWhen::Always,
                };
                self.dispatch_user(action, Purpose::Plain, clock, out);
                // The enqueue dropped the core's shell draft, which the
                // composer still shows: it is unsaved again, and saved
                // before the `UseSnippet` that follows.
                if let Some(session) = &mut self.session {
                    session.saver.edited(clock.now);
                }
                self.dispatch_user(Action::UseSnippet(id), Purpose::Plain, clock, out);
            }
            GuiIntent::SaveSnippet { key, name } => {
                let key = parse_key(&key)?;
                let text = open_session(&self.view)
                    .and_then(|view| prompt(&view.timeline[find(view, key)?]))
                    .map(str::to_owned);
                if let Some(text) = text {
                    self.save_snippet(name, text, ComposerMode::Agent, clock, out);
                }
            }
            GuiIntent::SaveComposerSnippet { session_id, name } => {
                let Some(session) = self.open_head(&session_id) else {
                    return Ok(());
                };
                let (text, kind) = (session.composer.text.clone(), session.mode);
                self.save_snippet(name, text, kind, clock, out);
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
                session.end_history();
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
                // A shell run's output shows in the app (`ExpandShellRun`).
                let state = match self.view.as_ref().map(|view| &view.screen) {
                    Some(Screen::Workdir(view)) => shown_blob(view)
                        .filter(|b| b.id == id)
                        .map(|b| b.state.clone()),
                    _ => open_session(&self.view).and_then(|view| {
                        view.timeline.iter().find_map(|item| match item {
                            TimelineItem::Logged { entry, .. } => sections(entry)
                                .into_iter()
                                .filter_map(|s| s.blob)
                                .find(|b| b.id == id)
                                .map(|b| b.state.clone()),
                            TimelineItem::Pending { .. } => None,
                        })
                    }),
                };
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
                if let Some(content) = content {
                    self.copied(copy(content, COPY_MAX), clock, out)?;
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
            GuiIntent::CreateConversationTyped { conversation } => {
                if let Some(create) = &mut self.create {
                    create.conversation =
                        conversation.chars().filter(|c| !c.is_control()).collect();
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
                        conversation: Some(create.conversation.clone()),
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
            GuiIntent::ConversationsOpen => {
                if let Some(Screen::SessionList(list)) = self.view.as_ref().map(|v| &v.screen)
                    && list.kind == skywave_core::ListKind::Active
                {
                    self.dispatch_user(Action::ListConversations, Purpose::Plain, clock, out);
                }
            }
            GuiIntent::ConversationPick { conversation_id } => {
                let workdir = match self.view.as_ref().map(|v| &v.screen) {
                    Some(Screen::SessionList(list)) => list
                        .conversations
                        .iter()
                        .flat_map(|picker| &picker.items)
                        .find(|item| item.id == conversation_id)
                        .map(|item| item.workdir.clone()),
                    _ => None,
                };
                let Some(workdir) = workdir else {
                    return Ok(());
                };
                // The core closes the picker as it takes the session.
                let action = Action::CreateSession {
                    workdir,
                    conversation: Some(conversation_id),
                };
                let tag = self.dispatch_user(action, Purpose::Create, clock, out);
                self.create_tag = Some(tag);
            }
            GuiIntent::ConversationsClose => {
                let open = matches!(
                    self.view.as_ref().map(|v| &v.screen),
                    Some(Screen::SessionList(list)) if list.conversations.is_some()
                );
                if open {
                    self.dispatch_user(Action::EndConversations, Purpose::Plain, clock, out);
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
            GuiIntent::UsageShow { span, grouping } => {
                self.usage = (
                    match span {
                        UsageSpanView::Today => Span::Today,
                        UsageSpanView::Week => Span::Week,
                        UsageSpanView::Month => Span::Month,
                        UsageSpanView::All => Span::All,
                    },
                    match grouping {
                        UsageGroupingView::Day => Grouping::Day,
                        UsageGroupingView::Session => Grouping::Session,
                        UsageGroupingView::Workdir => Grouping::Workdir,
                        UsageGroupingView::Model => Grouping::Model,
                    },
                );
            }
            GuiIntent::AppBackgrounded => self.moved(Moved::Backgrounded, clock, out),
            GuiIntent::AppForegrounded => self.moved(Moved::Foregrounded, clock, out),
        }
        Ok(())
    }

    /// Carries out what a copy came to: the clipboard and a notice, or the
    /// download it needs first; an unreadable file is the error returned.
    fn copied(&mut self, copied: Copied, clock: Clock, out: &mut Vec<Out>) -> Result<(), String> {
        match copied {
            Copied::Text { text, notice } => {
                out.push(Out::Effect(PlatformEffect::CopyToClipboard { text }));
                out.push(Out::Effect(PlatformEffect::Notice { text: notice }));
            }
            Copied::Fetch { fetch, notice } => {
                if let Some(id) = fetch {
                    self.dispatch_user(Action::DownloadBlob(id), Purpose::Plain, clock, out);
                }
                out.push(Out::Effect(PlatformEffect::Notice { text: notice }));
            }
            Copied::Refused(text) => out.push(Out::Effect(PlatformEffect::Notice { text })),
            Copied::Failed(text) => return Err(text),
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
    fn save_snippet(
        &mut self,
        name: String,
        text: String,
        kind: ComposerMode,
        clock: Clock,
        out: &mut Vec<Out>,
    ) {
        if text.trim().is_empty() {
            return;
        }
        self.dispatch_user(
            Action::SaveSnippet { name, text, kind },
            Purpose::Plain,
            clock,
            out,
        );
        out.push(Out::Effect(PlatformEffect::Notice {
            text: match kind {
                ComposerMode::Agent => "saved as a snippet",
                ComposerMode::Shell => "saved as a command",
            }
            .to_owned(),
        }));
    }

    /// Saves the open session's draft when [`DraftSaver::save`] says so.
    fn save_draft(&mut self, clock: Clock, force: bool, out: &mut Vec<Out>) {
        let Some(session) = &mut self.session else {
            return;
        };
        let composer = &session.composer;
        let when = match session.mode {
            ComposerMode::Agent => RunWhen::Always,
            ComposerMode::Shell => session.when,
        };
        if let Some(action) = session.saver.save(
            clock.now,
            force,
            session.mode,
            &composer.text,
            composer.cursor,
            when,
        ) {
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
                .mark(clock.now, view.last_seq, session.following(view), force)
        {
            self.dispatch(action, Purpose::Plain, out);
        }
    }

    /// The end of every step: what is due unforced, then what changed for
    /// Dart.
    /// Expanding downloads what is not here yet, including what
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

        let open = open_session(&self.view);
        let composer = self.session.as_ref().map(|s| {
            let history = if s.shell() {
                &s.shell_history
            } else {
                &s.history
            };
            let shell = open.filter(|_| s.shell());
            ComposerView {
                history_label: history.position().map(|(at, of)| format!("{at} of {of}")),
                mode: match s.mode {
                    ComposerMode::Agent => ComposerModeView::Agent,
                    ComposerMode::Shell => ComposerModeView::Shell,
                },
                shell_enabled: open.is_some_and(|v| v.shell.enabled),
                prompt: shell.map(|view| shell::prompt(view, s.when)),
                can_stop: shell.and_then(shell_stop).is_some(),
                ..s.composer.shown.clone()
            }
        });
        let workdir_text = match &view.screen {
            Screen::Workdir(workdir) => self.workdir_text.of(workdir),
            _ => None,
        };
        let ui = Ui {
            form: &self.form,
            create: self.create.as_ref(),
            rename: self.rename.as_ref(),
            composer: composer.as_ref(),
            revoked_dismissed: self.revoked_dismissed,
            usage: self.usage,
            wall_ms: clock.wall_ms,
            workdir_text,
        };
        let shown = screen::update(view, ui);
        if self.sent.screen.as_ref() != Some(&shown) {
            self.sent.screen = Some(shown.clone());
            out.push(Out::Screen(shown));
        }
        if !rows {
            return;
        }

        let (id, call, rows) = match (open, &mut self.session) {
            (Some(view), Some(session)) => (
                Some(view.id),
                view.subagent.as_ref().map(|s| s.call),
                timeline::rows(view, &session.expanded, &mut session.heads, clock.wall_ms),
            ),
            _ => (None, None, Rows::default()),
        };
        // Another view's rows are another list: a snapshot, not a diff.
        let updates = match &self.sent.timeline {
            Some((sent_id, sent_call, old)) if (*sent_id, *sent_call) == (id, call) => {
                timeline::diff(old, &rows)
            }
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
        self.sent.timeline = Some((id, call, rows.rows));
        if self.sent.search.as_ref() != Some(&found) {
            self.sent.search = Some(found.clone());
            out.push(Out::Search(found));
        }

        // The runs are the session's whichever view of it shows.
        let runs = match (open, &mut self.session) {
            (Some(view), Some(session)) => shell::rows(
                view,
                &session.shell_expanded,
                &mut session.outputs,
                clock.wall_ms,
            ),
            _ => Vec::new(),
        };
        let updates = match &self.sent.shell {
            Some((sent_id, old)) if *sent_id == id => shell::diff(old, &runs),
            _ => None,
        };
        match updates {
            Some(updates) => out.extend(updates.into_iter().map(Out::Shell)),
            None => out.push(Out::Shell(ShellUpdate::Snapshot {
                session_id: id.map(|id| id.0.to_string()),
                runs: runs.clone(),
            })),
        }
        self.sent.shell = Some((id, runs));
    }

    /// A fresh session head when another session opens; none off a session.
    /// Another view of the same session (a subagent's, or back to the main
    /// view) is another page in Dart, whose new composer field takes only a
    /// new `rev`: the text Dart reported last goes out again under one.
    fn follow_session(&mut self) {
        match open_session(&self.view) {
            Some(view) if self.session.as_ref().is_none_or(|s| s.id != view.id) => {
                self.revs += 1;
                self.session = Some(SessionHead::new(view, self.revs));
            }
            Some(view) => {
                let call = view.subagent.as_ref().map(|s| s.call);
                if let Some(session) = self.session.as_mut().filter(|s| s.subagent != call) {
                    session.subagent = call;
                    self.revs += 1;
                    let Composer { text, cursor, .. } = &session.composer;
                    session.composer = Composer::new(text.clone(), *cursor, self.revs);
                }
            }
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
        let shell = session.shell();
        // Shell commands from a history of their own.
        let history = if shell {
            &mut session.shell_history
        } else {
            &mut session.history
        };
        let next = if older {
            let source = || {
                if shell {
                    view.shell.commands()
                } else {
                    view.prompts()
                }
            };
            history
                .older(source, || stash)
                .map(|prompt| (prompt.to_owned(), prompt.chars().count()))
        } else {
            history.newer().map(|recall| match recall {
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
