//! The head: core's latest snapshot plus what the GUI head keeps itself
//! (present's policies, the composer, the open prompts, what was sent to
//! Dart), driven the way the TUI drives them. Snapshots, intents, dispatch
//! results and wake-ups go in; signals for Dart and actions for the
//! dispatcher come out, in order. No rinf and no clock here: the loop in
//! `run` sends what comes out, and time comes in as a [`Clock`].

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use skywave_core::attachment::{self, AttachedFile, AttachedImage};
use skywave_core::search::{BlobText, SessionsFound};
use skywave_core::{
    Action, BlobState, BlobView, CallId, CommandId, ComposerMode, ContentHash, Entry, PairUri,
    PairingView, RunWhen, Screen, ScreenAction, Seq, SessionId, SessionView, SnippetId,
    TimelineItem, ViewState, parse_pair_uri,
};
use skywave_present::app_screen::{self as present, Input};
use skywave_present::crop::toggle;
use skywave_present::format;
use skywave_present::policy::{
    AlertWatcher, ConversationOnly, DraftSaver, ExpandFetch, IdleWatcher, PromptHistory,
    ReadMarker, Recall, ShellAlertWatcher, TimelineSearch, looks_like_pair_uri, message_refusal,
    search_history, shell_stop,
};
use skywave_present::sections::{ItemKey, call_content, find, main_content, sections, wanted_blob};
use skywave_present::storage::Grouping as StorageGrouping;
use skywave_present::usage::{Grouping, Span};
use skywave_present::utf16::{chars_to_utf16, utf16_to_chars};
use skywave_proto::limits::{ATTACHMENTS_MAX, ATTACHMENTS_MAX_TOTAL_BYTES};

use crate::attach::{AttachSource, Prepared};
use crate::clip::{COPY_MAX, Copied, copy, copy_output};
use crate::crop::CropLines;
use crate::dispatcher::{Dispatched, Tag};
use crate::global_search::GlobalSearch;
#[cfg(test)]
use crate::lifecycle::Target;
use crate::lifecycle::{Awake, Call, Moved};
use crate::notify::{Kind, NotificationIds};
use crate::prefs::{NotificationPrefs, Prefs};
use crate::screen::{CreateUi, Ui, rename_initial, repair_notice};
use crate::signals::{
    AppScreenTextResult, AppScreenUpdate, ComposerFile, ComposerImage, ComposerModeView,
    ComposerView, CopyWhat, CropLinesUpdate, GuiIntent, HistoryRowView, HistorySearchView,
    LinkUpdate, NotifyKind, PairForm, PlatformEffect, RenamePrompt, ScreenUpdate, SearchUpdate,
    ShellRunRow, ShellUpdate, StorageGroupingView, TimelineRow, TimelineUpdate, UsageGroupingView,
    UsageSpanView, Utf16Range, WorkdirTextUpdate,
};
use crate::timeline::{BlobHeads, Rows, RuleCandidates, parse_key};
use crate::workdir::{WorkdirText, shown_blob, shown_text};
use crate::{app_screen, intents, link, screen, search, shell, timeline};

/// How often running clocks on screen (a tool's elapsed time, how long a
/// message has waited) are brought up to date.
pub const TICK: Duration = Duration::from_secs(1);

/// How often the session list's "N min ago" labels are brought up to date.
const LIST_TICK: Duration = Duration::from_secs(60);

/// The most rows the shell history picker sends, newest first.
const HISTORY_SEARCH_ROWS: usize = 100;

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
    /// The open session's app screen, whole, when it changed (D65).
    AppScreen(AppScreenUpdate),
    Search(SearchUpdate),
    /// The crop page's line window.
    CropLines(CropLinesUpdate),
    /// The text of the workdir diff or file shown, once per blob.
    WorkdirText(WorkdirTextUpdate),
    Effect(PlatformEffect),
    /// Hand `action` to the dispatcher with `tag`.
    Dispatch(Tag, Action),
    /// Prepare `source` (an image or a file) for `session_id`'s draft, off
    /// the hub task; the result comes back to [`Head::on_prepared`].
    Prepare {
        session_id: SessionId,
        source: AttachSource,
    },
    /// Run `Core::search` off the hub task; the answer comes back to
    /// [`Head::on_searched`] with `asked`.
    SearchSessions {
        asked: u64,
        query: String,
        scope: Option<SessionId>,
    },
    /// Pause the engine's network loops (`Core::suspend`).
    Suspend,
    /// Start them again (`Core::resume`).
    Resume,
    /// Write the head's preferences to their file.
    SavePrefs(Prefs),
}

/// What a dispatched action was for, so its result reaches its owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Purpose {
    /// A message from the composer of this session.
    Send(SessionId),
    /// Typed text of the app screen, by the GUI's number for it.
    ScreenText(u32),
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
                // The picker is the open session's, set when published.
                history_search: None,
                to_label: None,
                // The mode's own fields are the open session's, set when
                // published.
                mode: ComposerModeView::Agent,
                shell_enabled: false,
                prompt: None,
                can_stop: false,
                images: Vec::new(),
                files: Vec::new(),
                preparing: 0,
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
    /// The shell history picker's selected row, while it is open.
    history_search: Option<usize>,
    marker: ReadMarker,
    /// The newest row is on screen (Dart says so).
    following: bool,
    /// The `Agent` call of the subagent shown; `None` in the main view.
    subagent: Option<CallId>,
    /// Items whose content shows whole; only logged items expand.
    expanded: HashSet<Seq>,
    /// What the expanded items download.
    fetch: ExpandFetch,
    search: Option<TimelineSearch>,
    heads: BlobHeads,
    /// The safety rule candidates of each shell call, parsed once.
    candidates: RuleCandidates,
    /// The shell runs whose rows carry their whole output.
    shell_expanded: HashSet<CommandId>,
    outputs: shell::Outputs,
    /// The app screen is shown in place of the timeline (D65). Kept here,
    /// not in the core: a session's head only.
    app_screen: bool,
    /// The device the app screen shows, as the user picked it; `None` for
    /// the default device.
    app_screen_device: Option<String>,
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
            history_search: None,
            marker: ReadMarker::new(view.id),
            following: false,
            subagent: view.subagent.as_ref().map(|s| s.call),
            expanded: HashSet::new(),
            fetch: ExpandFetch::new(),
            search: None,
            heads: BlobHeads::default(),
            candidates: RuleCandidates::default(),
            shell_expanded: HashSet::new(),
            outputs: shell::Outputs::default(),
            app_screen: false,
            app_screen_device: None,
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
    /// The app screen last sent, whole.
    app_screen: Option<AppScreenUpdate>,
    search: Option<SearchUpdate>,
    crop_lines: Option<CropLinesUpdate>,
    /// The blob id of the workdir text last sent, so it goes out once per
    /// blob; `None` when no text is shown, so returning to it sends again.
    workdir_text: Option<String>,
}

/// The Stay connected preference as the head keeps it (D21), offered on
/// Android alone, where the background service is. Choosing it only asks
/// Dart to start or stop the service: what holds the link is Kotlin's
/// report that it runs ([`Awake::service`]), never the preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StayConnected {
    offered: bool,
    on: bool,
}

/// Whether this hub was built for Android.
const ANDROID: bool = cfg!(target_os = "android");

impl Default for StayConnected {
    fn default() -> StayConnected {
        StayConnected {
            offered: ANDROID,
            on: false,
        }
    }
}

impl StayConnected {
    /// What the settings screen shows: `None` where not offered.
    fn shown(self) -> Option<bool> {
        self.offered.then_some(self.on)
    }

    /// Asks for the service again while it is wanted: on a fresh head
    /// (after a hub failure too) and on coming back on screen. Starting a
    /// running service starts nothing, but Kotlin reports `running` again,
    /// which a fresh head, or one whose report was dropped, needs.
    fn restart(self, out: &mut Vec<Out>) {
        if self.offered && self.on {
            out.push(Out::Effect(PlatformEffect::BackgroundService { run: true }));
        }
    }
}

/// The GUI head's state. See the module docs.
#[derive(Debug, Default)]
pub struct Head {
    view: Option<ViewState>,
    alerts: Option<AlertWatcher>,
    /// From 0, not from the first snapshot: a run that finished in a
    /// catch-up before it is still news.
    shell_alerts: ShellAlertWatcher,
    /// From 0, like `shell_alerts`.
    idle_alerts: IdleWatcher,
    /// Each session's system notification ids.
    notifications: NotificationIds,
    next_tag: Tag,
    pending: HashMap<Tag, Purpose>,
    /// Bumped for every value the hub writes into a Dart field.
    revs: u32,
    /// The answer to the newest typed text of the app screen; `None` while it
    /// waits. Kept here, not in a session's head: the GUI's number for a send
    /// is unique while the app runs, so an answer is never mistaken for
    /// another send's.
    app_screen_text: Option<AppScreenTextResult>,
    /// The number of the newest typed text; only its answer is kept.
    app_screen_text_seq: Option<u32>,
    form: PairForm,
    create: Option<CreateUi>,
    create_tag: Option<Tag>,
    rename: Option<RenamePrompt>,
    rename_tag: Option<Tag>,
    repair_dismissed: bool,
    /// The usage screen's days and grouping (`UsageShow`).
    usage: (Span, Grouping),
    /// The storage screen's list (`StorageShow`).
    storage: StorageGrouping,
    /// The text of the workdir diff or file shown.
    workdir_text: WorkdirText,
    /// The crop page's line window (`CropLines`).
    crop_lines: CropLines,
    /// The search page's query and hits (`GuiIntent::GlobalSearchTyped`).
    global_search: GlobalSearch,
    session: Option<SessionHead>,
    /// The core's data dir, where the attachments are stored; set by `run`
    /// once the core is open.
    data_dir: PathBuf,
    /// Images and files being prepared for each session's draft, not yet
    /// answered by [`Head::on_prepared`].
    preparing: HashMap<SessionId, u32>,
    /// Only the conversation shows, in every session until switched off:
    /// each snapshot of the open session is narrowed as it arrives.
    conversation_only: ConversationOnly,
    sent: Sent,
    /// Who holds the link: whether the core runs.
    awake: Awake,
    stay_connected: StayConnected,
    /// The session list shows as one plain list, not grouped by project
    /// (`GuiIntent::SetPlainList`).
    plain_list: bool,
    /// Which alerts become system notifications, from `Prefs`.
    notify: NotificationPrefs,
}

impl Head {
    /// A head on screen with `prefs`, as a boot Dart asked for starts one.
    #[cfg(test)]
    pub fn new(prefs: Prefs) -> Head {
        Head::holding(prefs, Awake::default())
    }

    /// A head that starts with `awake`'s holders rather than on screen:
    /// the one a hub failure off screen restarts, while the service still
    /// runs and Dart reports no move.
    pub fn holding(prefs: Prefs, awake: Awake) -> Head {
        Head {
            stay_connected: StayConnected {
                on: prefs.stay_connected,
                ..StayConnected::default()
            },
            plain_list: prefs.plain_list,
            notify: prefs.notifications,
            awake,
            ..Head::default()
        }
    }

    /// Every preference the head keeps, as `SavePrefs` writes them whole:
    /// switching one must not reset the others.
    fn prefs(&self) -> Prefs {
        Prefs {
            stay_connected: self.stay_connected.on,
            plain_list: self.plain_list,
            notifications: self.notify,
        }
    }

    /// A head for `target` rather than the one built for.
    #[cfg(test)]
    pub(crate) fn with_target(target: Target) -> Head {
        Head {
            awake: Awake::new(target),
            ..Head::new(Prefs::default())
        }
    }

    /// A head for `target`, on Android or not whatever it was built for,
    /// with `prefs`.
    #[cfg(test)]
    pub(crate) fn with_platform(target: Target, android: bool, prefs: Prefs) -> Head {
        Head {
            awake: Awake::new(target),
            stay_connected: StayConnected {
                offered: android,
                on: prefs.stay_connected,
            },
            plain_list: prefs.plain_list,
            notify: prefs.notifications,
            ..Head::default()
        }
    }

    /// Where the core keeps its data, under which the attachments are stored.
    pub fn set_data_dir(&mut self, dir: PathBuf) {
        self.data_dir = dir;
    }

    pub(crate) fn awake(&self) -> &Awake {
        &self.awake
    }

    /// A snapshot from the core.
    pub fn on_view(&mut self, mut view: ViewState, clock: Clock) -> Vec<Out> {
        let mut out = Vec::new();
        if let Screen::Session(session) = &mut view.screen {
            self.conversation_only.narrow(session);
        }
        match &mut self.alerts {
            // The first snapshot's approvals need no alert.
            None => self.alerts = Some(AlertWatcher::new(&view.attention)),
            Some(alerts) => {
                let update = alerts.observe(&view.attention);
                for session in update.cleared {
                    out.push(Out::Effect(PlatformEffect::Dismiss {
                        id: self.notifications.id(session, Kind::Approval),
                    }));
                }
                // An alert whose approvals were all answered by the same
                // snapshot names no session: nothing is left to show.
                if let Some((alert, session)) = update
                    .alert
                    .and_then(|alert| alert.session_id.map(|s| (alert, s)))
                    && self.notify.all
                    && self.notify.approvals
                {
                    out.push(Out::Effect(PlatformEffect::Notify {
                        id: self.notifications.id(session, Kind::Approval),
                        body: alert.notice(),
                        title: alert.title.unwrap_or_default(),
                        session_id: Some(session.0.to_string()),
                        kind: NotifyKind::Approval,
                    }));
                }
            }
        }
        if let Some(notice) = self.shell_alerts.observe(&view.attention)
            && let Some(last) = &view.attention.last_shell
            && self.notify.all
            && self.notify.shell
        {
            out.push(Out::Effect(PlatformEffect::Notify {
                id: self.notifications.id(last.session_id, Kind::Shell),
                body: notice.notice(),
                title: notice.title,
                session_id: Some(last.session_id.0.to_string()),
                kind: NotifyKind::Shell,
            }));
        }
        let update = self.idle_alerts.observe(&view.attention);
        for session in update.cleared {
            out.push(Out::Effect(PlatformEffect::Dismiss {
                id: self.notifications.id(session, Kind::Idle),
            }));
        }
        for notice in update
            .notices
            .into_iter()
            .filter(|_| self.notify.all && self.notify.idle)
        {
            out.push(Out::Effect(PlatformEffect::Notify {
                id: self.notifications.id(notice.session_id, Kind::Idle),
                body: notice.notice(),
                title: notice.title,
                session_id: Some(notice.session_id.0.to_string()),
                kind: NotifyKind::Idle,
            }));
        }
        if repair_notice(&view).is_none() {
            self.repair_dismissed = false;
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
            // A reply never rewrites the input: only user actions do.
            create.picker.settle(&create.input, Some(completion));
        }
        self.follow_session();
        if let Some(view) = &self.view {
            self.global_search.follow(&view.screen, clock.now);
        }
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
            // A search hit opened this session: its query, marked, with the
            // item jumped to once the events are loaded.
            if let Some(reveal) = self.global_search.take_reveal(view) {
                let mut search = TimelineSearch::new();
                search.edit(view, |text| *text = reveal.query, BlobText::read);
                if let Some(index) = find(view, ItemKey::Logged(reveal.seq))
                    && let Some(jump) = search.jump(view, None, index)
                    && let Some(seq) = jump.expand
                {
                    session.expanded.insert(seq);
                }
                session.search = Some(search);
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
            Purpose::ScreenText(seq) => {
                // Only the newest send's answer counts; an older one's
                // result arriving late must not replace it.
                if self.app_screen_text_seq == Some(seq) {
                    self.app_screen_text = Some(AppScreenTextResult {
                        seq,
                        queued: done.result.is_ok(),
                    });
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

    /// The result of the preparation [`Out::Prepare`] asked for: the image
    /// or file is attached to `session_id`'s draft, or the words for why not
    /// are shown. The session need not be open: the draft belongs to it.
    pub fn on_prepared(
        &mut self,
        session_id: SessionId,
        result: Result<Prepared, String>,
        clock: Clock,
    ) -> Vec<Out> {
        let mut out = Vec::new();
        let count = self.preparing.entry(session_id).or_insert(0);
        *count = count.saturating_sub(1);
        match result {
            Err(text) => out.push(Out::Effect(PlatformEffect::Notice { text })),
            Ok(Prepared::Image(image)) => {
                self.attach_prepared(session_id, image, clock, &mut out);
            }
            Ok(Prepared::File(file)) => {
                self.attach_file_prepared(session_id, file, clock, &mut out);
            }
        }
        self.after(clock, &mut out);
        out
    }

    /// The answer [`Out::SearchSessions`] asked for, numbered `asked`: kept
    /// when it answers the newest search asked.
    pub fn on_searched(&mut self, asked: u64, found: SessionsFound, clock: Clock) -> Vec<Out> {
        let mut out = Vec::new();
        self.global_search.answered(asked, found);
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
    /// retry countdown, a running clock on screen, the list's ages. `None`:
    /// nothing is.
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
            matches!(view.screen, Screen::SessionList(_)).then(|| clock.now + LIST_TICK),
            self.global_search.next_wake(),
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
            GuiIntent::AppReady => {
                self.sent = Sent::default();
                self.stay_connected.restart(out);
            }
            GuiIntent::Act { action } => {
                let action = intents::action(action)?;
                if let (Action::SetCropKeep(keep), Some(Screen::Crop(view))) =
                    (&action, self.view.as_ref().map(|v| &v.screen))
                {
                    self.crop_lines.sent_keep(view.blob, keep.clone());
                }
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
                if composer.text != text {
                    composer.text = text;
                    composer.cursor = cursor;
                    session.saver.edited(clock.now);
                    session.end_history();
                    // The picker's query changed: back to its newest match.
                    if session.history_search.is_some() {
                        session.history_search = Some(0);
                    }
                } else if composer.cursor != cursor {
                    // A caret move is not an edit: it keeps history browsing.
                    composer.cursor = cursor;
                    session.saver.edited(clock.now);
                }
            }
            GuiIntent::ComposerSubmit { session_id } => {
                let Some(session) = self.open_head(&session_id) else {
                    return Ok(());
                };
                session.end_history();
                session.history_search = None;
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
                } else if self.preparing_of(id) > 0 {
                    // The message would go without the attachment being prepared.
                    out.push(Out::Effect(PlatformEffect::Notice {
                        text: "an attachment is still being prepared".to_owned(),
                    }));
                    return Ok(());
                } else if let Some(text) = message_refusal(&text) {
                    // Refused here, not by the server, so it stays in the
                    // composer to cut down.
                    out.push(Out::Effect(PlatformEffect::Notice { text }));
                    return Ok(());
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
            GuiIntent::HistorySearchOpen => {
                if let Some(session) = self.session.as_mut().filter(|s| s.shell()) {
                    session.end_history();
                    session.history_search = Some(0);
                }
            }
            GuiIntent::HistorySearchMove { older } => self.move_history_search(older),
            GuiIntent::HistorySearchAccept => self.accept_history_search(None, clock),
            GuiIntent::HistorySearchPick { index } => {
                self.accept_history_search(Some(index), clock);
            }
            GuiIntent::HistorySearchClose => {
                if let Some(session) = self.session.as_mut() {
                    session.history_search = None;
                }
            }
            GuiIntent::ToggleShellMode { session_id } => {
                let Some(session) = self.open_head(&session_id) else {
                    return Ok(());
                };
                let (id, shell) = (session.id, session.shell());
                session.history_search = None;
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
            GuiIntent::AppScreenToggle { session_id } => {
                let Some(session) = self.open_head(&session_id) else {
                    return Ok(());
                };
                session.app_screen = !session.app_screen;
                let open = session.app_screen;
                let Some(view) = open_session(&self.view) else {
                    return Ok(());
                };
                // The devices are asked for once: not while listed, nor
                // while a listing waits.
                let listing =
                    view.app_screen.devices.is_empty()
                        && !view.app_screen.requests.iter().any(|r| {
                            matches!(r.action, ScreenAction::Devices) && present::waiting(r)
                        });
                if open && listing {
                    let action = present::devices(view);
                    self.dispatch_user(action, Purpose::Plain, clock, out);
                }
            }
            GuiIntent::AppScreenDevices { session_id } => {
                if self.open_head(&session_id).is_none() {
                    return Ok(());
                }
                let Some(view) = open_session(&self.view) else {
                    return Ok(());
                };
                let action = present::devices(view);
                self.dispatch_user(action, Purpose::Plain, clock, out);
            }
            GuiIntent::AppScreenSelect { session_id, serial } => {
                let Some(session) = self.open_head(&session_id) else {
                    return Ok(());
                };
                session.app_screen_device = Some(serial.clone());
                let Some(view) = open_session(&self.view) else {
                    return Ok(());
                };
                let action = present::capture(view, &serial);
                self.dispatch_user(action, Purpose::Plain, clock, out);
            }
            GuiIntent::AppScreenCapture { session_id } => {
                let Some(session) = self.open_head(&session_id) else {
                    return Ok(());
                };
                let selected = session.app_screen_device.clone();
                let Some(view) = open_session(&self.view) else {
                    return Ok(());
                };
                let Some(device) = app_screen::shown_device(view, selected.as_deref()) else {
                    return Ok(());
                };
                let action = present::capture(view, &device);
                self.dispatch_user(action, Purpose::Plain, clock, out);
            }
            GuiIntent::AppScreenTap {
                session_id,
                capture_id,
                x,
                y,
            } => {
                let input = Input::Tap { x, y };
                self.app_screen_input(&session_id, &capture_id, input, Purpose::Plain, clock, out)?;
            }
            GuiIntent::AppScreenSwipe {
                session_id,
                capture_id,
                from_x,
                from_y,
                to_x,
                to_y,
            } => {
                let input = Input::Swipe {
                    from_x,
                    from_y,
                    to_x,
                    to_y,
                };
                self.app_screen_input(&session_id, &capture_id, input, Purpose::Plain, clock, out)?;
            }
            GuiIntent::AppScreenText {
                session_id,
                capture_id,
                text,
                seq,
            } => {
                let input = Input::Text(text);
                let purpose = Purpose::ScreenText(seq);
                // A new send waits for its own answer; an older one's no
                // longer counts.
                self.app_screen_text_seq = Some(seq);
                self.app_screen_text = None;
                let dispatched =
                    self.app_screen_input(&session_id, &capture_id, input, purpose, clock, out);
                // Not queued: the GUI may send the same text again.
                if !matches!(dispatched, Ok(true)) {
                    self.app_screen_text = Some(AppScreenTextResult { seq, queued: false });
                }
                dispatched?;
            }
            GuiIntent::AppScreenKey {
                session_id,
                capture_id,
                key,
            } => {
                let input = Input::Key(app_screen::key(key));
                self.app_screen_input(&session_id, &capture_id, input, Purpose::Plain, clock, out)?;
            }
            GuiIntent::AppScreenDismiss {
                session_id,
                request_id,
            } => {
                let request: CommandId = intents::id("request", &request_id)?;
                if self.open_head(&session_id).is_none() {
                    return Ok(());
                }
                let discardable = open_session(&self.view).is_some_and(|view| {
                    view.app_screen
                        .requests
                        .iter()
                        .any(|r| r.request == request && r.discardable)
                });
                if discardable {
                    self.dispatch_user(Action::Discard(request), Purpose::Plain, clock, out);
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
            GuiIntent::SaveShellCommand { command, name } => {
                self.save_snippet(name, command, ComposerMode::Shell, clock, out);
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
            GuiIntent::AttachFile { session_id, path } => {
                self.prepare_attach(&session_id, AttachSource::File(PathBuf::from(path)), out);
            }
            GuiIntent::AttachImageBytes {
                session_id,
                bytes,
                mime,
            } => {
                tracing::debug!(%mime, bytes = bytes.len(), "image bytes to attach");
                self.prepare_attach(&session_id, AttachSource::Bytes(bytes), out);
            }
            GuiIntent::DetachImage { session_id, sha256 } => {
                let sha256: ContentHash =
                    sha256.parse().map_err(|_| "bad image hash".to_owned())?;
                if let Some(session) = self.open_head(&session_id) {
                    let id = session.id;
                    let action = Action::Detach {
                        session_id: id,
                        sha256,
                    };
                    self.dispatch_user(action, Purpose::Plain, clock, out);
                }
            }
            GuiIntent::DetachFile { session_id, name } => {
                if let Some(session) = self.open_head(&session_id) {
                    let id = session.id;
                    let action = Action::DetachFile {
                        session_id: id,
                        name,
                    };
                    self.dispatch_user(action, Purpose::Plain, clock, out);
                }
            }
            GuiIntent::ToggleConversationOnly => {
                let open = match self.view.as_mut().map(|v| &mut v.screen) {
                    Some(Screen::Session(view)) => Some(&mut **view),
                    _ => None,
                };
                self.conversation_only.toggle(open);
                if let (Some(view), Some(session)) = (open_session(&self.view), &mut self.session)
                    && let Some(search) = &mut session.search
                {
                    search.refresh(view, None, BlobText::read);
                }
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
                let entry = entry.clone();
                let Some(session) = &mut self.session else {
                    return Ok(());
                };
                session.expanded.insert(seq);
                // Expanding again asks again: the first blob here, the
                // item's others through `fetch_expanded`.
                if let Some(fetch) = session.fetch.expand(&entry) {
                    self.dispatch_user(fetch, Purpose::Plain, clock, out);
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
                                .filter_map(|s| s.blob())
                                .chain(message_image_blobs(entry))
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
            GuiIntent::GlobalSearchTyped { query } => {
                self.global_search.typed(clean_query(&query), clock.now);
            }
            GuiIntent::GlobalSearchOpen { key } => {
                if let Some(id) = self.global_search.open(&key) {
                    self.dispatch_user(Action::OpenSession(id), Purpose::Plain, clock, out);
                }
            }
            GuiIntent::CreateOpen => {
                if let Some(Screen::SessionList(list)) = self.view.as_ref().map(|v| &v.screen)
                    && list.kind == skywave_core::ListKind::Active
                {
                    let mut create = CreateUi::default();
                    let action = create.picker.start(&create.input);
                    self.create = Some(create);
                    self.dispatch_user(action, Purpose::Plain, clock, out);
                }
            }
            GuiIntent::CompletionTyped { input } => {
                let Some(create) = &mut self.create else {
                    return Ok(());
                };
                create.input = input.chars().filter(|c| !c.is_control()).collect();
                let action = create.picker.edited(&create.input);
                self.dispatch_user(action, Purpose::Plain, clock, out);
            }
            GuiIntent::CreateConversationTyped { conversation } => {
                if let Some(create) = &mut self.create {
                    create.conversation =
                        conversation.chars().filter(|c| !c.is_control()).collect();
                }
            }
            GuiIntent::CreateWorktreeTyped { worktree } => {
                if let Some(create) = &mut self.create {
                    create.worktree = worktree.chars().filter(|c| !c.is_control()).collect();
                }
            }
            GuiIntent::CompletionComplete => {
                let completion = self.completion();
                let Some(create) = &mut self.create else {
                    return Ok(());
                };
                let (next, action) = create.picker.complete(&create.input, completion.as_ref());
                if let Some(next) = next {
                    self.revs += 1;
                    create.input = next;
                    create.rev = self.revs;
                }
                self.dispatch_user(action, Purpose::Plain, clock, out);
            }
            GuiIntent::CompletionMove { up } => {
                let completion = self.completion();
                if let Some(create) = &mut self.create {
                    if up {
                        create.picker.up();
                    } else {
                        create.picker.down(&create.input, completion.as_ref());
                    }
                }
            }
            GuiIntent::CompletionAccept => {
                let completion = self.completion();
                let Some(create) = &mut self.create else {
                    return Ok(());
                };
                if let Some((next, action)) =
                    create.picker.choose(&create.input, completion.as_ref())
                {
                    self.revs += 1;
                    create.input = next;
                    create.rev = self.revs;
                    self.dispatch_user(action, Purpose::Plain, clock, out);
                } else {
                    // Blank or unacceptable paths are refused by the core
                    // or the server, not here.
                    let action = Action::CreateSession {
                        conversation: Some(create.conversation.clone()),
                        workdir: create.input.clone(),
                        worktree: Some(create.worktree.clone()),
                    };
                    let tag = self.dispatch_user(action, Purpose::Create, clock, out);
                    self.create_tag = Some(tag);
                }
            }
            GuiIntent::CompletionPick { index } => {
                let completion = self.completion();
                let Some(create) = &mut self.create else {
                    return Ok(());
                };
                let Ok(index) = usize::try_from(index) else {
                    return Ok(());
                };
                if let Some((next, action)) =
                    create
                        .picker
                        .pick(index, &create.input, completion.as_ref())
                {
                    self.revs += 1;
                    create.input = next;
                    create.rev = self.revs;
                    self.dispatch_user(action, Purpose::Plain, clock, out);
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
                    worktree: None,
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
                if create.picker.unhighlight() {
                    return Ok(());
                }
                let end = create.picker.cancel(completing);
                self.create = None;
                if let Some(end) = end {
                    self.dispatch_user(end, Purpose::Plain, clock, out);
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
            GuiIntent::RepairDismiss => self.repair_dismissed = true,
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
            GuiIntent::StorageShow { grouping } => {
                self.storage = match grouping {
                    StorageGroupingView::Sessions => StorageGrouping::Sessions,
                    StorageGroupingView::Blobs => StorageGrouping::Blobs,
                    StorageGroupingView::ShellRuns => StorageGrouping::ShellRuns,
                };
            }
            GuiIntent::CropLines { from, count } => self.crop_lines.ask(from, count),
            GuiIntent::CropToggle { first, last } => {
                // Off the crop page, a late tap has nothing to toggle.
                let Some(Screen::Crop(view)) = self.view.as_ref().map(|v| &v.screen) else {
                    return Ok(());
                };
                let keep = toggle(self.crop_lines.keep(view), first, last);
                self.crop_lines.sent_keep(view.blob, keep.clone());
                self.dispatch_user(Action::SetCropKeep(keep), Purpose::Plain, clock, out);
            }
            GuiIntent::AppBackgrounded => self.moved(Moved::Backgrounded, clock, out),
            GuiIntent::AppForegrounded => {
                self.moved(Moved::Foregrounded, clock, out);
                // The system may have stopped the service while away.
                self.stay_connected.restart(out);
            }
            GuiIntent::BackgroundService { running } => {
                let calls = self.awake.service(running);
                self.call(calls, clock, out);
            }
            // Holders stay as they are: switched off, the service holds
            // the link until Kotlin reports it stopped.
            GuiIntent::SetStayConnected { on } => {
                if !self.stay_connected.offered {
                    tracing::debug!(on, "stay connected is not offered here");
                    return Ok(());
                }
                self.stay_connected.on = on;
                out.push(Out::SavePrefs(self.prefs()));
                out.push(Out::Effect(PlatformEffect::BackgroundService { run: on }));
            }
            // Not platform-gated: the list reads the same everywhere. The
            // screen goes out with the rest of the intent's result.
            GuiIntent::SetPlainList { on } => {
                self.plain_list = on;
                out.push(Out::SavePrefs(self.prefs()));
            }
            // Saved whole; the screen is sent again by `publish`, which sees
            // the switches change.
            GuiIntent::SetNotifications {
                all,
                approvals,
                idle,
                shell,
            } => {
                self.notify = NotificationPrefs {
                    all,
                    approvals,
                    idle,
                    shell,
                };
                out.push(Out::SavePrefs(self.prefs()));
            }
            // The hub task's, which keeps it from every head.
            GuiIntent::HubFailedSeen { .. } => {}
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
    /// is [`Awake::moved`]'s to say.
    fn moved(&mut self, moved: Moved, clock: Clock, out: &mut Vec<Out>) {
        let calls = self.awake.moved(moved);
        self.call(calls, clock, out);
    }

    /// Makes `calls` in order, so a save is queued on the dispatcher before
    /// the suspend.
    fn call(&mut self, calls: Vec<Call>, clock: Clock, out: &mut Vec<Out>) {
        for call in calls {
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

    /// Downloads what the expanded items need and was not asked for yet
    /// ([`ExpandFetch`]), including what arrived while an item was open.
    fn fetch_expanded(&mut self, clock: Clock, out: &mut Vec<Out>) {
        let Some((view, session)) = open_session(&self.view).zip(self.session.as_mut()) else {
            return;
        };
        for fetch in session.fetch.due(view, &session.expanded) {
            self.dispatch_user(fetch, Purpose::Plain, clock, out);
        }
    }

    /// The end of every step: what is due unforced, then what changed for
    /// Dart.
    fn after(&mut self, clock: Clock, out: &mut Vec<Out>) {
        self.save_draft(clock, false, out);
        self.mark_read(clock, false, out);
        if let Some((asked, query, scope)) = self.global_search.take_due(clock.now) {
            out.push(Out::SearchSessions {
                asked,
                query,
                scope,
            });
        }
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
            link::AgentLogin {
                expires: view.login_expires,
                offered: view.agent_login_offered,
                logged_out: view.agent_logged_out,
            },
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
                history_search: s.history_search.map(|selected| {
                    history_search_view(&view.shell_history, &s.composer.text, selected)
                }),
                mode: match s.mode {
                    ComposerMode::Agent => ComposerModeView::Agent,
                    ComposerMode::Shell => ComposerModeView::Shell,
                },
                shell_enabled: open.is_some_and(|v| v.shell.enabled),
                prompt: shell.map(|view| shell::prompt(view, s.when)),
                can_stop: shell.and_then(shell_stop).is_some(),
                images: match open.filter(|view| view.id == s.id) {
                    Some(view) if !s.shell() => view
                        .draft_images
                        .iter()
                        .map(|image| ComposerImage {
                            sha256: image.sha256.to_string(),
                            path: attachment::attachment_path(&self.data_dir, image.sha256)
                                .display()
                                .to_string(),
                            size_label: format::bytes(image.size),
                        })
                        .collect(),
                    _ => Vec::new(),
                },
                files: match open.filter(|view| view.id == s.id) {
                    Some(view) if !s.shell() => view
                        .draft_files
                        .iter()
                        .map(|file| ComposerFile {
                            name: file.name.clone(),
                            size_label: format::bytes(file.size),
                            replaces: file.replace.is_some(),
                        })
                        .collect(),
                    _ => Vec::new(),
                },
                preparing: self.preparing_of(s.id),
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
            repair_dismissed: self.repair_dismissed,
            usage: self.usage,
            storage: self.storage,
            wall_ms: clock.wall_ms,
            workdir_text,
            stay_connected: self.stay_connected.shown(),
            plain_list: self.plain_list,
            notifications: self.notify,
            conversation_only: self.conversation_only.on(),
            search: &self.global_search,
        };
        let shown = screen::update(view, ui);
        if self.sent.screen.as_ref() != Some(&shown) {
            self.sent.screen = Some(shown.clone());
            out.push(Out::Screen(shown));
        }
        // After the screen, so Dart knows the page its lines are of.
        let lines = match &view.screen {
            Screen::Crop(crop) => self.crop_lines.update(crop),
            _ => {
                self.crop_lines.close();
                self.sent.crop_lines = None;
                None
            }
        };
        if let Some(lines) = lines.filter(|l| self.sent.crop_lines.as_ref() != Some(l)) {
            self.sent.crop_lines = Some(lines.clone());
            out.push(Out::CropLines(lines));
        }
        // Compared by blob id, never by value: cloning a megabyte of lines
        // on every publish would cost the hub more than the send saves.
        let text = match &view.screen {
            Screen::Workdir(workdir) => shown_text(workdir, workdir_text),
            _ => None,
        };
        match text {
            None => self.sent.workdir_text = None,
            Some((blob_id, _)) if self.sent.workdir_text.as_ref() == Some(&blob_id) => {}
            Some((blob_id, text)) => {
                self.sent.workdir_text = Some(blob_id.clone());
                out.push(Out::WorkdirText(WorkdirTextUpdate {
                    blob_id,
                    lines: text.lines.clone(),
                }));
            }
        }
        if !rows {
            return;
        }

        let (id, call, rows) = match (open, &mut self.session) {
            (Some(view), Some(session)) => (
                Some(view.id),
                view.subagent.as_ref().map(|s| s.call),
                timeline::rows(
                    view,
                    &session.expanded,
                    &mut session.heads,
                    &mut session.candidates,
                    &self.data_dir,
                    clock.wall_ms,
                ),
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

        // The app screen goes out whole, when it changed; a head with no
        // session open sends the empty screen.
        let mut app = match (open, &self.session) {
            (Some(view), Some(session)) => app_screen::update(
                Some(view),
                session.app_screen,
                session.app_screen_device.as_deref(),
            ),
            _ => app_screen::update(None, false, None),
        };
        app.text_result = self.app_screen_text;
        if self.sent.app_screen.as_ref() != Some(&app) {
            self.sent.app_screen = Some(app.clone());
            out.push(Out::AppScreen(app));
        }
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

    /// An input to the app screen's device of the open session, aimed at
    /// `capture_id`, with `purpose` for its result. An aim that is no longer
    /// the device's latest capture is said so; blank text types nothing.
    /// Returns whether it dispatched.
    fn app_screen_input(
        &mut self,
        session_id: &str,
        capture_id: &str,
        input: Input,
        purpose: Purpose,
        clock: Clock,
        out: &mut Vec<Out>,
    ) -> Result<bool, String> {
        let aim: CommandId = intents::id("capture", capture_id)?;
        let Some(session) = self.open_head(session_id) else {
            return Ok(false);
        };
        let selected = session.app_screen_device.clone();
        let blank = matches!(&input, Input::Text(text) if text.trim().is_empty());
        let Some(view) = open_session(&self.view) else {
            return Ok(false);
        };
        let Some(device) = app_screen::shown_device(view, selected.as_deref()) else {
            return Ok(false);
        };
        match present::input(view, &device, aim, input) {
            Some(action) => {
                self.dispatch_user(action, purpose, clock, out);
                Ok(true)
            }
            None if blank => Ok(false),
            None => {
                let text = match view.app_screen.latest(&device) {
                    None => Some("capture the screen first"),
                    Some(latest) if latest.capture != aim => {
                        Some("the screen changed; aim again on the new capture")
                    }
                    Some(_) => None,
                };
                if let Some(text) = text {
                    out.push(Out::Effect(PlatformEffect::Notice {
                        text: text.to_owned(),
                    }));
                }
                Ok(false)
            }
        }
    }

    /// The open session's view, when it is `id`'s.
    fn view_of(&self, id: SessionId) -> Option<&SessionView> {
        open_session(&self.view).filter(|view| view.id == id)
    }

    /// The images being prepared for `id`'s draft.
    fn preparing_of(&self, id: SessionId) -> u32 {
        self.preparing.get(&id).copied().unwrap_or(0)
    }

    /// Asks for `source` to be prepared for the open session's draft. Bytes
    /// are always an image, so when the draft and the preparations in flight
    /// fill the image cap, the cap is said and nothing is prepared. A file
    /// may turn out to be an image: `attach_prepared` enforces the cap then.
    fn prepare_attach(&mut self, session_id: &str, source: AttachSource, out: &mut Vec<Out>) {
        let Some(id) = self.open_head(session_id).map(|session| session.id) else {
            return;
        };
        if matches!(source, AttachSource::Bytes(_)) {
            let held = self.view_of(id).map_or(0, |view| view.draft_images.len());
            let preparing = usize::try_from(self.preparing_of(id)).unwrap_or(usize::MAX);
            if held.saturating_add(preparing) >= ATTACHMENTS_MAX {
                out.push(images_full());
                return;
            }
        }
        *self.preparing.entry(id).or_insert(0) += 1;
        out.push(Out::Prepare {
            session_id: id,
            source,
        });
    }

    /// Attaches a prepared non-image `file` to `id`'s draft, unless the draft
    /// holds a file of that name already: a note says so, as the core would
    /// keep the one held and drop this one without a word.
    fn attach_file_prepared(
        &mut self,
        id: SessionId,
        file: AttachedFile,
        clock: Clock,
        out: &mut Vec<Out>,
    ) {
        if let Some(view) = self.view_of(id)
            && view.draft_files.iter().any(|held| held.name == file.name)
        {
            out.push(Out::Effect(PlatformEffect::Notice {
                text: format!("a file named {} is already attached", file.name),
            }));
            return;
        }
        let action = Action::AttachFile {
            session_id: id,
            file,
        };
        self.dispatch_user(action, Purpose::Plain, clock, out);
    }

    /// Attaches a prepared `image` to `id`'s draft, unless the draft holds
    /// it already or it would pass a cap. The caps are checked where the
    /// view of `id` is at hand; the core refuses anything else silently.
    fn attach_prepared(
        &mut self,
        id: SessionId,
        image: AttachedImage,
        clock: Clock,
        out: &mut Vec<Out>,
    ) {
        if let Some(view) = self.view_of(id) {
            if view
                .draft_images
                .iter()
                .any(|held| held.sha256 == image.sha256)
            {
                return;
            }
            if view.draft_images.len() + 1 > ATTACHMENTS_MAX {
                out.push(images_full());
                return;
            }
            let total: u64 = view.draft_images.iter().map(|held| held.size).sum();
            if total + image.size > ATTACHMENTS_MAX_TOTAL_BYTES {
                out.push(Out::Effect(PlatformEffect::Notice {
                    text: format!(
                        "images over {} in total",
                        format::bytes(ATTACHMENTS_MAX_TOTAL_BYTES)
                    ),
                }));
                return;
            }
        }
        let action = Action::Attach {
            session_id: id,
            image,
        };
        self.dispatch_user(action, Purpose::Plain, clock, out);
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

    /// Moves the open picker's selection to the next row (`older`, a row
    /// further down the matches) or the previous one, clamped at both ends.
    fn move_history_search(&mut self, older: bool) {
        let (Some(view), Some(session)) = (&self.view, self.session.as_mut()) else {
            return;
        };
        let Some(selected) = session.history_search else {
            return;
        };
        let matching = search_history(
            &view.shell_history,
            &session.composer.text,
            HISTORY_SEARCH_ROWS,
        )
        .len();
        session.history_search = Some(if older {
            (selected + 1).min(matching.saturating_sub(1))
        } else {
            selected.saturating_sub(1)
        });
    }

    /// Puts row `index` of the open picker (its selected row when `None`)
    /// into the composer, without running it, and closes the picker. Ignored
    /// while the picker is closed; a row with no match just closes it.
    fn accept_history_search(&mut self, index: Option<u32>, clock: Clock) {
        let (Some(view), Some(session)) = (&self.view, self.session.as_mut()) else {
            return;
        };
        if session.history_search.is_none() {
            return;
        }
        let row = match index {
            Some(index) => usize::try_from(index).ok(),
            None => session.history_search,
        };
        let command =
            row.and_then(|row| history_row(&view.shell_history, &session.composer.text, row));
        session.history_search = None;
        let Some(command) = command else {
            return;
        };
        let cursor = command.chars().count();
        self.revs += 1;
        session.composer = Composer::new(command, cursor, self.revs);
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

    /// The completion the new-session prompt shows, if the session list is
    /// on screen with one.
    fn completion(&self) -> Option<skywave_core::Completion> {
        match self.view.as_ref().map(|v| &v.screen) {
            Some(Screen::SessionList(list)) => list.completion.clone(),
            _ => None,
        }
    }

    /// Re-pairing starts from the address the device is paired with.
    fn prefill(&mut self, view: &ViewState) {
        if let Screen::Pairing(PairingView {
            paired: Some(info), ..
        }) = &view.screen
            && self.form.address.is_empty()
        {
            self.revs += 1;
            self.form = PairForm {
                address: info.server_address.to_string(),
                rev: self.revs,
                ..PairForm::default()
            };
        }
    }

    /// Fills every field from a pairing URI, in the core's canonical form.
    fn fill(&mut self, uri: PairUri) {
        self.revs += 1;
        self.form = PairForm {
            address: uri.address.to_string(),
            code: uri.code,
            fingerprint: uri.fingerprint,
            rev: self.revs,
        };
    }
}

/// The notice for a draft at its image cap.
fn images_full() -> Out {
    Out::Effect(PlatformEffect::Notice {
        text: format!("at most {ATTACHMENTS_MAX} images per message"),
    })
}

/// The blobs of a logged user message's images; none for any other entry.
fn message_image_blobs(entry: &Entry) -> Vec<&BlobView> {
    match entry {
        Entry::UserMessage { images, .. } => images
            .iter()
            .filter_map(|image| image.content.blob())
            .collect(),
        _ => Vec::new(),
    }
}

/// A typed query as the search takes it: newlines become spaces, other
/// control characters go.
fn clean_query(query: &str) -> String {
    query
        .chars()
        .map(|c| if c == '\n' { ' ' } else { c })
        .filter(|c| !c.is_control())
        .collect()
}

/// The open session in `view`, if a session is the screen.
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

/// The command of row `row` of the picker's matches of `query` over
/// `commands`, if there is such a row.
fn history_row(commands: &[String], query: &str, row: usize) -> Option<String> {
    search_history(commands, query, HISTORY_SEARCH_ROWS)
        .get(row)
        .map(|found| found.command.to_owned())
}

/// The picker as shown for `query` over `commands` (newest first), with
/// `selected` highlighted, clamped to the rows sent.
fn history_search_view(commands: &[String], query: &str, selected: usize) -> HistorySearchView {
    let matches = search_history(commands, query, usize::MAX);
    let matching = matches.len();
    let rows: Vec<HistoryRowView> = matches
        .iter()
        .take(HISTORY_SEARCH_ROWS)
        .map(|found| HistoryRowView {
            command: found.command.to_owned(),
            matched: utf16_ranges(found.command, &found.matched),
        })
        .collect();
    let selected =
        (!rows.is_empty()).then(|| u32::try_from(selected.min(rows.len() - 1)).unwrap_or(u32::MAX));
    let empty_label = if commands.is_empty() {
        Some("No shell commands yet".to_owned())
    } else if rows.is_empty() {
        Some("No match".to_owned())
    } else {
        None
    };
    HistorySearchView {
        rows,
        selected,
        count_label: format!("{matching}/{}", commands.len()),
        empty_label,
    }
}

/// The UTF-16 ranges of the chars at `chars` (char indices, ascending) of
/// `text`; adjacent chars make one range.
fn utf16_ranges(text: &str, chars: &[usize]) -> Vec<Utf16Range> {
    let mut ranges: Vec<Utf16Range> = Vec::new();
    let mut offset = 0;
    for (index, c) in text.chars().enumerate() {
        let start = offset;
        offset += u32::try_from(c.len_utf16()).unwrap_or(u32::MAX);
        if !chars.contains(&index) {
            continue;
        }
        match ranges.last_mut() {
            Some(last) if last.end == start => last.end = offset,
            _ => ranges.push(Utf16Range { start, end: offset }),
        }
    }
    ranges
}

#[cfg(test)]
#[path = "head_tests.rs"]
mod tests;
