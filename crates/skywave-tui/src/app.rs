//! Head-side state: the latest [`ViewState`] from the core plus presentation
//! state the core does not care about (form buffers, focus, the help
//! overlay, list selection). Keys become core [`Action`]s here; nothing in
//! this module decides anything about pairing, sync or delivery (D2).

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ops::Deref;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use skywave_core::search::BlobText;
use skywave_core::{
    Action, AgentView, ApprovalId, BlobId, BlobPolicy, BlobState, BlobView, CallId, CommandId,
    Completion, ComposerMode, ContextId, ConversationItem, ConversationPicker, CoreError, Decision,
    Entry, Link, ListKind, PairUri, PairingView, Route, RunWhen, Screen, Seq, ServerAddress,
    SessionId, SessionListView, SessionRow, SessionView, ShellRun, ShellRunState, TaskId,
    TimelineItem, ToolOutcome, ViewState, parse_pair_uri,
};
use skywave_present::format::{
    BackgroundRow, StopAction, background_rows, bytes, mode_needs_confirmation, move_refusal,
    offered_modes,
};
use skywave_present::policy::{
    AlertWatcher, CompletionSettle, DraftSaver, ExpandFetch, PromptHistory, ReadMarker, Recall,
    ShellAlertWatcher, TimelineSearch, looks_like_pair_uri, message_refusal, shell_stop,
    stop_needs_confirmation, unanswered,
};
use skywave_present::sections::{
    Content, ItemKey, call_content, discardable, find, gated_item, main_content, sections,
    selectable, starts_turn, wanted_blob,
};
use skywave_present::usage::{Grouping, Span};

use crate::clipboard::{Clip, CopyError};
use crate::composer::Composer;
use crate::notify;
use crate::ui::TimelineCache;

mod workdir;

pub(crate) use workdir::{BlobLines, ReadLines};
pub use workdir::{WorkdirPick, WorkdirRow, WorkdirUi, shown_blob, workdir_rows};

pub use skywave_present::policy::{DRAFT_SAVE_AFTER, READ_MARK_EVERY, dismissable};

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

    /// Fills every field from a pairing URI, already in the core's
    /// canonical form, and focuses the last, so Enter submits.
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

/// Rows of completion candidates shown at most.
pub const POPUP_ROWS: usize = 8;

/// What a [`Prompt`] asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    /// "New session", on the session list: the server workdir, with Tab
    /// completion. Whether the path is acceptable is the server's call
    /// (D14).
    Create,
    /// "Continue an outside conversation" by its id, from `p` in the
    /// conversation picker, in two steps: the server workdir (Tab
    /// completion, like [`Create`]), then the agent's own id of the
    /// conversation to fork, which the server checks. The step is
    /// [`Prompt::workdir`].
    ///
    /// [`Create`]: PromptKind::Create
    Adopt,
    /// A new name for the session, on the session list or the session
    /// itself; blank restores the default title (the core decides).
    Rename(SessionId),
    /// Where the paired server is now, on settings: Enter reconnects there
    /// without re-pairing; an address that does not parse stays to fix.
    ServerAddress,
}

/// A one-line prompt over the session list or a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    pub kind: PromptKind,
    pub text: String,
    /// Enter was pressed; the prompt closes once the core takes the action
    /// and stays open with the text if dispatching fails.
    submitted: bool,
    /// [`PromptKind::Create`]: Tab completion of the workdir; its popup
    /// shows the core's [`Completion`].
    completion: CompletionSettle,
    /// [`PromptKind::Adopt`]: the workdir chosen in the first step, once
    /// the prompt asks for the conversation id; `None` while it still asks
    /// for the workdir.
    workdir: Option<String>,
}

impl Prompt {
    fn new(kind: PromptKind, text: String) -> Prompt {
        Prompt {
            kind,
            text,
            submitted: false,
            completion: CompletionSettle::new(),
            workdir: None,
        }
    }

    /// The prompt asks for the workdir (a new session, or the first step of
    /// an adopting one), so it completes it.
    pub(crate) fn asks_workdir(&self) -> bool {
        match self.kind {
            PromptKind::Create => true,
            PromptKind::Adopt => self.workdir.is_none(),
            PromptKind::Rename(_) | PromptKind::ServerAddress => false,
        }
    }

    fn insert(&mut self, text: &str) {
        self.text.extend(text.chars().filter(|c| !c.is_control()));
    }

    /// The workdir changed: while the popup is open, complete it again.
    fn edited(&mut self) -> Option<Action> {
        self.completion.edited(&self.text)
    }

    /// Whether the prompt still belongs on `screen`: a new-session prompt
    /// on the session list, a rename there or on its session, an address
    /// on settings.
    fn fits(&self, screen: &Screen) -> bool {
        match (self.kind, screen) {
            (PromptKind::ServerAddress, screen) => matches!(screen, Screen::Settings(_)),
            (_, Screen::SessionList(_)) => true,
            (PromptKind::Rename(id), Screen::Session(view)) => view.id == id,
            _ => false,
        }
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

/// Where the timeline is scrolled back to, relative to an item, so the view
/// stays put when items above it grow (an expanded call) or below it arrive.
/// The item is the first one starting at or below the top line, so even an
/// item cut by the top edge can grow without moving what follows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Anchor {
    pub key: ItemKey,
    /// Lines from the item's first line to the top line: negative when the
    /// top line is above the item, positive only inside the last item.
    pub offset: isize,
}

/// The timeline's layout in the last frame, recorded by the renderer so
/// paging and keeping the selection in view know how tall things are. Stale
/// by at most one frame; the renderer clamps whatever the keys make of it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Geometry {
    /// Each timeline item, by timeline index.
    pub keys: Vec<ItemKey>,
    /// First line of each timeline item, by timeline index.
    pub starts: Vec<usize>,
    /// Lines of the whole timeline.
    pub total: usize,
    /// Lines that fit on screen.
    pub height: usize,
    /// Top line shown in the last frame; where an anchor whose item is gone
    /// falls back to.
    pub top: usize,
    /// Line of the `── new ──` rule, if the timeline has one.
    pub rule: Option<usize>,
    /// Where the search's current match is in its blob's content, if that
    /// line is laid out: the match's item, and the line of the timeline.
    pub matched: Option<(ItemKey, usize)>,
    /// The item `v` was revealing when this frame was laid out
    /// ([`SessionUi::reveal`]), so its height here is the expanded one.
    pub reveal: Option<ItemKey>,
}

impl Geometry {
    /// Top line when following the newest item.
    pub fn bottom(&self) -> usize {
        self.total.saturating_sub(self.height)
    }

    /// The top line shown for `scroll`.
    pub fn top(&self, scroll: Option<Anchor>) -> usize {
        match scroll {
            None => self.bottom(),
            Some(anchor) => self.line(anchor).unwrap_or(self.top).min(self.bottom()),
        }
    }

    /// The scroll state a session opens at: pinned to the rule when the
    /// unread part below it does not fit on screen, else following.
    pub fn opening(&self) -> Option<Anchor> {
        let rule = self.rule?;
        (self.total.saturating_sub(rule) > self.height)
            .then(|| self.pin(rule))
            .flatten()
    }

    /// The scroll state that shows the search's current match in its blob
    /// (`key` its item) a third down the screen, if the last frame laid
    /// that line out.
    pub fn match_scroll(&self, key: ItemKey) -> Option<Option<Anchor>> {
        let (_, line) = self.matched.filter(|(on, _)| *on == key)?;
        Some(self.pin(line.saturating_sub(self.height / 3)))
    }

    /// `scroll`, or following when the line it pins is on screen at the
    /// bottom anyway.
    pub fn follow_at_bottom(&self, scroll: Option<Anchor>) -> Option<Anchor> {
        scroll.filter(|anchor| self.line(*anchor).is_none_or(|line| line < self.bottom()))
    }

    /// The line `anchor` stands for, if its item is still there.
    fn line(&self, anchor: Anchor) -> Option<usize> {
        let index = self.keys.iter().position(|key| *key == anchor.key)?;
        let start = *self.starts.get(index)?;
        Some(start.saturating_add_signed(anchor.offset))
    }

    /// The scroll state that puts `line` at the top: following when that is
    /// the bottom anyway.
    fn pin(&self, line: usize) -> Option<Anchor> {
        if line >= self.bottom() {
            return None;
        }
        let index = self
            .starts
            .partition_point(|&start| start < line)
            .min(self.starts.len().checked_sub(1)?);
        let offset = line as isize - self.starts[index] as isize;
        Some(Anchor {
            key: self.keys[index],
            offset,
        })
    }

    /// The item `line` belongs to.
    fn index_at(&self, line: usize) -> Option<usize> {
        self.starts
            .partition_point(|&start| start <= line)
            .checked_sub(1)
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
    /// What the timeline shows and where the user is in it; fresh whenever
    /// the shown subagent changes ([`SessionUi::show`]).
    pub shown: ViewUi,
    /// The Agent call of the subagent [`SessionUi::shown`] belongs to;
    /// `None` for the main view.
    subagent: Option<CallId>,
    /// The message being written. Filled from the core's draft (D5: per
    /// device) when the session opens, and again only when the core
    /// rewrites the draft itself (`draft_rev`, e.g. a queued message taken
    /// back to edit); the core hears of it through [`App::draft_save`].
    pub composer: Composer,
    /// When the composer's edits are saved as the core's draft.
    saver: DraftSaver,
    /// Earlier prompts being recalled into the composer; any edit ends it.
    history: PromptHistory<Composer>,
    /// What the composer writes, as the core last said ([`SessionView::mode`]).
    /// The core's switch is followed in [`SessionUi::follow_mode`], after
    /// the old mode's draft was saved: [`DraftSaver::flushes_before`] the
    /// `SetComposerMode` that Ctrl-T sends.
    pub mode: ComposerMode,
    /// The condition the shell command being written runs on (Ctrl-O),
    /// saved with the shell draft; back to always after each run sent.
    pub when: RunWhen,
    /// Earlier shell commands being recalled, apart from the prompts.
    shell_history: PromptHistory<Composer>,
    /// The highlighted run while the run list has the focus.
    pub shell_selected: Option<CommandId>,
    /// The runs whose whole output the run list shows.
    pub shell_expanded: HashSet<CommandId>,
    /// How many lines the selected run is scrolled past its top, when it
    /// is taller than the run list (PgUp/PgDn); 0 with each new selection.
    pub shell_scroll: usize,
    /// The run list's height and the selected run's, in lines, as last
    /// drawn: what [`SessionUi::page_run`] pages through.
    pub shell_room: Cell<(usize, usize)>,
    /// The downloaded outputs the run list shows expanded, as read once
    /// ([`OUTPUT_SHOW_BYTES`] of each): a downloaded blob's file does not
    /// change. `None` for one that could not be read.
    ///
    /// [`OUTPUT_SHOW_BYTES`]: skywave_present::output::OUTPUT_SHOW_BYTES
    pub shell_outputs: RefCell<HashMap<BlobId, Option<String>>>,
    /// When what is seen is marked read ([`App::read_mark`]).
    marker: ReadMarker,
    /// The list of rules `A` opened in the approval prompt; see
    /// [`SessionUi::rule_pick`].
    rule_pick: Option<RulePick>,
    /// The mode or model list `p`/`m` opened, while it is up.
    pub picker: Option<Picker>,
}

/// The presentation state that belongs to one view of the session, the main
/// one or a subagent's: [`SessionUi`] starts it afresh when the view changes,
/// and keeps what belongs to the session (composer, draft, history, read
/// mark, pickers).
#[derive(Debug, Clone)]
pub struct ViewUi {
    /// Where the user scrolled back to; `None` follows the newest item. New
    /// items never move a pinned view.
    pub scroll: Option<Anchor>,
    /// The selected item, by key so it stays put as items arrive.
    pub selected: Option<ItemKey>,
    /// Items whose full content is shown; only logged items expand.
    pub expanded: HashSet<Seq>,
    /// What the expanded items download.
    pub fetch: ExpandFetch,
    pub geometry: RefCell<Geometry>,
    /// The timeline's items as laid out by the last frames, so a redraw
    /// lays out only what changed.
    pub layout: RefCell<TimelineCache>,
    /// The session opened with news and where to scroll is not settled: it
    /// needs the first frame's geometry ([`Geometry::opening`]). Until then
    /// the renderer shows that scroll state itself and nothing is read.
    pub opening: bool,
    /// The search through the timeline (`/`), while one is open or kept.
    pub search: Option<Search>,
    /// The item `v` revealed, scrolled to its top, while that waits for a
    /// frame to lay it out expanded: if it then fits at the bottom, the
    /// timeline keeps following ([`Geometry::follow_at_bottom`]).
    pub reveal: Option<ItemKey>,
}

impl ViewUi {
    /// `opening`: the session opened with news ([`SessionView::unread_from`])
    /// and shows the main view.
    fn new(opening: bool) -> ViewUi {
        ViewUi {
            scroll: None,
            selected: None,
            expanded: HashSet::new(),
            fetch: ExpandFetch::new(),
            geometry: RefCell::default(),
            layout: RefCell::default(),
            opening,
            search: None,
            reveal: None,
        }
    }
}

/// The rules an approval offers, listed to grant one: which approval, and
/// which rule is highlighted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RulePick {
    approval: ApprovalId,
    selected: usize,
}

/// The list `p` opens over the session to switch the agent's permission
/// mode, and the menu `m` opens to switch its model or effort. Switching
/// works at any time; the agent's answer shows in the status line and the
/// timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Picker {
    /// The modes [`offered_modes`] lists, in order. `confirm` once Enter
    /// chose one that lets every tool run unasked, until `y` sends it.
    Mode { selected: usize, confirm: bool },
    /// The models the agent offers, and a name typed instead: sent as it
    /// is when not blank, so a model can be asked for before any list came.
    Model { selected: usize, name: String },
    /// The menu `m` opens: row 0 is the model, row 1 the effort. `m` and
    /// `e` jump straight to either picker.
    Agent { selected: usize },
    /// The levels the model takes, [`AgentView::efforts`] in order.
    Effort { selected: usize },
    /// The menu `C` opens: row 0 is "New context" (inert while the core
    /// does not offer it), the rows after it [`SessionView::contexts`] as
    /// the core lists them. `moving` is the context `M` asked to move into
    /// a session of its own, until `y` sends it; `note` why `M` was refused
    /// on a row, until the next key.
    Contexts {
        selected: usize,
        moving: Option<ContextId>,
        note: Option<String>,
    },
    /// The agent's background work, opened by `B`: the tasks (a child under
    /// its parent), then the loops, which only inform. `stopping` is the
    /// task Enter asked to stop, until `y` sends it.
    Background {
        selected: usize,
        stopping: Option<TaskId>,
    },
    /// An action that stops the agent while it has work, waiting for `y`.
    Stop(StopKind),
}

/// What waits for `y` in [`Picker::Stop`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopKind {
    /// Stop agent.
    Session,
    /// A new context.
    Clear,
    /// Switch to this context.
    Switch(ContextId),
    /// Move this context, the current one, to a session of its own.
    Move(ContextId),
    /// End the task a subagent runs, from its view.
    Task(TaskId),
}

impl StopKind {
    /// Which action this is, for the wording shared with the other head.
    pub fn stop_action(&self) -> StopAction {
        match self {
            StopKind::Session => StopAction::Agent,
            StopKind::Clear => StopAction::NewContext,
            StopKind::Switch(_) => StopAction::SwitchContext,
            StopKind::Move(_) => StopAction::MoveContext,
            StopKind::Task(_) => StopAction::Task,
        }
    }

    fn action(self, session_id: SessionId) -> Action {
        match self {
            StopKind::Session => Action::StopSession { session_id },
            StopKind::Clear => Action::ClearContext { session_id },
            StopKind::Switch(context) => Action::SwitchContext {
                session_id,
                context,
            },
            StopKind::Move(context) => Action::MoveContext {
                session_id,
                context,
            },
            StopKind::Task(task) => Action::StopTask { session_id, task },
        }
    }
}

/// The rows of [`Picker::Agent`].
const AGENT_ROWS: usize = 2;

impl Picker {
    /// The mode list, on the mode asked for last, else the one in effect.
    fn mode(agent: &AgentView) -> Picker {
        let mode = agent.mode_change.as_ref().map(|c| c.value).or(agent.mode);
        let selected = mode
            .and_then(|mode| {
                offered_modes(agent.bypass_allowed)
                    .iter()
                    .position(|m| *m == mode)
            })
            .unwrap_or(0);
        Picker::Mode {
            selected,
            confirm: false,
        }
    }

    /// The model list, on the model asked for last, else the one running.
    fn model(agent: &AgentView) -> Picker {
        let model = agent
            .model_change
            .as_ref()
            .map(|c| c.value.as_str())
            .or(agent.model.as_deref());
        let selected = model
            .and_then(|model| agent.choice(model))
            .and_then(|choice| agent.models.iter().position(|m| m == choice))
            .unwrap_or(0);
        Picker::Model {
            selected,
            name: String::new(),
        }
    }

    /// The contexts menu, on the current context.
    fn contexts(view: &SessionView) -> Picker {
        let current = view.contexts.iter().position(|c| c.current);
        Picker::Contexts {
            selected: current.map_or(0, |i| i + 1),
            moving: None,
            note: None,
        }
    }

    /// The background work, on its first row.
    fn background() -> Picker {
        Picker::Background {
            selected: 0,
            stopping: None,
        }
    }

    /// `kind` as it asks: at once, or after `y` when it would end work.
    fn stop(&mut self, kind: StopKind, view: &SessionView) -> (bool, Option<Action>) {
        if stop_needs_confirmation(&view.work) {
            *self = Picker::Stop(kind);
            (false, None)
        } else {
            (true, Some(kind.action(view.id)))
        }
    }

    /// The menu, on the model row.
    fn agent() -> Picker {
        Picker::Agent { selected: 0 }
    }

    /// The effort list, on the level asked for last, else the one in force.
    fn effort(agent: &AgentView) -> Picker {
        let effort = agent
            .effort_change
            .as_ref()
            .map(|c| c.value)
            .or(agent.effort.flatten());
        let selected = effort
            .and_then(|effort| agent.efforts().iter().position(|e| *e == effort))
            .unwrap_or(0);
        Picker::Effort { selected }
    }

    /// Keys while the picker is up: ↑↓ choose, Enter switches, Esc closes
    /// (`true` once it should close); Esc in the model or effort list goes
    /// back to the menu. On the model list text keys type a
    /// name. Nothing else reaches the session.
    fn key(&mut self, view: &SessionView, key: KeyEvent, ctrl: bool) -> (bool, Option<Action>) {
        let session_id = view.id;
        match self {
            // y/N, as deleting asks: only `y` switches.
            Picker::Mode {
                selected,
                confirm: confirm @ true,
            } => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => (
                    true,
                    // Only the mode confirmed: none if the server stopped
                    // allowing it meanwhile and the list shrank.
                    offered_modes(view.agent.bypass_allowed)
                        .get(*selected)
                        .copied()
                        .filter(|&mode| mode_needs_confirmation(mode))
                        .map(|mode| Action::SetPermissionMode { session_id, mode }),
                ),
                _ => {
                    *confirm = false;
                    (false, None)
                }
            },
            Picker::Mode { selected, confirm } => match key.code {
                KeyCode::Esc => (true, None),
                KeyCode::Up | KeyCode::Char('k') => {
                    *selected = selected.saturating_sub(1);
                    (false, None)
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let last = offered_modes(view.agent.bypass_allowed).len() - 1;
                    *selected = (*selected + 1).min(last);
                    (false, None)
                }
                KeyCode::Enter => {
                    let Some(&mode) = offered_modes(view.agent.bypass_allowed).get(*selected)
                    else {
                        return (true, None);
                    };
                    if mode_needs_confirmation(mode) {
                        *confirm = true;
                        return (false, None);
                    }
                    (true, Some(Action::SetPermissionMode { session_id, mode }))
                }
                _ => (false, None),
            },
            Picker::Model { selected, name } => {
                let models = &view.agent.models;
                let last = models.len().saturating_sub(1);
                match key.code {
                    KeyCode::Esc => {
                        *self = Picker::agent();
                        return (false, None);
                    }
                    KeyCode::Up => *selected = (*selected).min(last).saturating_sub(1),
                    KeyCode::Down => *selected = (*selected + 1).min(last),
                    // The core trims the name and takes a blank one as no
                    // switch; with nothing typed, the highlighted model.
                    KeyCode::Enter => {
                        let model = if !name.trim().is_empty() {
                            name.clone()
                        } else {
                            match models.get((*selected).min(last)) {
                                Some(choice) => choice.id.clone(),
                                None => return (false, None),
                            }
                        };
                        return (true, Some(Action::SetModel { session_id, model }));
                    }
                    KeyCode::Backspace => {
                        name.pop();
                    }
                    KeyCode::Char('u') if ctrl => name.clear(),
                    KeyCode::Char(c) if !ctrl && !c.is_control() => name.push(c),
                    _ => {}
                }
                (false, None)
            }
            Picker::Agent { selected } => match key.code {
                KeyCode::Esc => (true, None),
                KeyCode::Up | KeyCode::Char('k') => {
                    *selected = selected.saturating_sub(1);
                    (false, None)
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    *selected = (*selected + 1).min(AGENT_ROWS - 1);
                    (false, None)
                }
                KeyCode::Char('m') => {
                    *self = Picker::model(&view.agent);
                    (false, None)
                }
                KeyCode::Char('e') => {
                    *self = Picker::effort(&view.agent);
                    (false, None)
                }
                KeyCode::Enter => {
                    *self = if *selected == 0 {
                        Picker::model(&view.agent)
                    } else {
                        Picker::effort(&view.agent)
                    };
                    (false, None)
                }
                _ => (false, None),
            },
            // y/N, as the mode picker asks: only `y` stops.
            Picker::Stop(kind) => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    (true, Some(kind.clone().action(session_id)))
                }
                _ => (true, None),
            },
            Picker::Background {
                stopping: stopping @ Some(_),
                ..
            } => match (key.code, stopping.take()) {
                (KeyCode::Char('y') | KeyCode::Char('Y'), Some(task)) => {
                    (true, Some(Action::StopTask { session_id, task }))
                }
                _ => (false, None),
            },
            Picker::Background { selected, stopping } => {
                let rows = background_rows(&view.work);
                let last = rows.len().saturating_sub(1);
                *selected = (*selected).min(last);
                match key.code {
                    KeyCode::Esc => (true, None),
                    KeyCode::Up | KeyCode::Char('k') => {
                        *selected = selected.saturating_sub(1);
                        (false, None)
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        *selected = (*selected + 1).min(last);
                        (false, None)
                    }
                    // A subagent's task opens its timeline.
                    KeyCode::Char('o') => match rows.get(*selected) {
                        Some(BackgroundRow::Task(task, _)) if task.call.is_some() => (
                            true,
                            Some(Action::ViewSubagent {
                                session_id,
                                call: task.call,
                            }),
                        ),
                        _ => (false, None),
                    },
                    // A loop only informs; the agent ends it when asked.
                    KeyCode::Enter => {
                        if let Some(BackgroundRow::Task(task, _)) = rows.get(*selected) {
                            *stopping = Some(task.id.clone());
                        }
                        (false, None)
                    }
                    _ => (false, None),
                }
            }
            // y/N, as the mode picker asks: only `y` moves.
            Picker::Contexts {
                moving: moving @ Some(_),
                ..
            } => match (key.code, moving.take()) {
                (KeyCode::Char('y') | KeyCode::Char('Y'), Some(context)) => (
                    true,
                    Some(Action::MoveContext {
                        session_id,
                        context,
                    }),
                ),
                _ => (false, None),
            },
            Picker::Contexts {
                selected,
                moving,
                note,
            } => {
                *note = None;
                // Row 0 is "New context"; the contexts follow.
                let last = view.contexts.len();
                *selected = (*selected).min(last);
                let context = selected.checked_sub(1).and_then(|i| view.contexts.get(i));
                match key.code {
                    KeyCode::Esc => (true, None),
                    KeyCode::Up | KeyCode::Char('k') => {
                        *selected = selected.saturating_sub(1);
                        (false, None)
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        *selected = (*selected + 1).min(last);
                        (false, None)
                    }
                    // Only what the core offers; otherwise the row is inert.
                    KeyCode::Enter => match context {
                        None if view.can_clear => self.stop(StopKind::Clear, view),
                        None => (false, None),
                        // Moved on: the context lives in another session now.
                        Some(c) if c.moved_to.is_some() => {
                            let to = c.moved_to.as_ref().map(|to| to.id);
                            (true, to.map(Action::OpenSession))
                        }
                        // Already current: nothing to switch, just close.
                        Some(c) if c.current => (true, None),
                        Some(c) => self.stop(StopKind::Switch(c.id), view),
                    },
                    // The current context stands for the whole timeline.
                    KeyCode::Char('v') => match context {
                        None => (false, None),
                        Some(c) => (
                            true,
                            Some(Action::ViewContext {
                                session_id,
                                context: (!c.current).then_some(c.id),
                            }),
                        ),
                    },
                    // Only what the core offers; otherwise say why not.
                    KeyCode::Char('M') => {
                        if let Some(c) = context {
                            if c.can_move && c.current && stop_needs_confirmation(&view.work) {
                                // Moving the current one stops the agent.
                                *self = Picker::Stop(StopKind::Move(c.id));
                                return (false, None);
                            } else if c.can_move {
                                *moving = Some(c.id);
                            } else {
                                *note = Some(move_refusal(c).to_owned());
                            }
                        }
                        (false, None)
                    }
                    // Drops this device's refused (or untried) move of it.
                    KeyCode::Char('x') => (
                        false,
                        context.and_then(|c| c.discard_move).map(Action::Discard),
                    ),
                    _ => (false, None),
                }
            }
            Picker::Effort { selected } => {
                let efforts = view.agent.efforts();
                let last = efforts.len().saturating_sub(1);
                match key.code {
                    KeyCode::Esc => {
                        *self = Picker::agent();
                        (false, None)
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        *selected = (*selected).min(last).saturating_sub(1);
                        (false, None)
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        *selected = (*selected + 1).min(last);
                        (false, None)
                    }
                    KeyCode::Enter => match efforts.get((*selected).min(last)) {
                        Some(effort) => (
                            true,
                            Some(Action::SetEffort {
                                session_id,
                                effort: *effort,
                            }),
                        ),
                        None => (false, None),
                    },
                    _ => (false, None),
                }
            }
        }
    }
}

/// A search through the open session's timeline: the head-independent
/// part is a [`TimelineSearch`] (deref to it); whether its prompt is open
/// and where its match scrolls to are the terminal's.
#[derive(Debug, Clone)]
pub struct Search {
    search: TimelineSearch,
    /// The prompt is open: keys edit the query.
    pub editing: bool,
    /// The current match's line in its blob waits to be scrolled to: the
    /// next frame shows it ([`Geometry::match_scroll`]), the next key
    /// keeps that scroll state.
    pub pin: bool,
}

impl Deref for Search {
    type Target = TimelineSearch;

    fn deref(&self) -> &TimelineSearch {
        &self.search
    }
}

impl Search {
    fn new() -> Search {
        Search {
            search: TimelineSearch::new(),
            editing: true,
            pin: false,
        }
    }

    /// Edits the query and finds its matches again.
    fn edit(&mut self, view: &SessionView, edit: impl FnOnce(&mut String)) {
        self.search.edit(view, edit, BlobText::read);
        self.pin = false;
    }

    /// Finds the matches in a new snapshot; see [`TimelineSearch::refresh`].
    fn refresh(&mut self, view: &SessionView, old: Option<usize>) {
        self.search.refresh(view, old, BlobText::read);
    }
}

impl SessionUi {
    fn new(view: &SessionView) -> SessionUi {
        SessionUi {
            id: view.id,
            focus: Focus::Composer,
            shown: ViewUi::new(view.unread_from.is_some() && view.subagent.is_none()),
            subagent: view.subagent.as_ref().map(|s| s.call),
            composer: composer_of(view),
            saver: DraftSaver::new(view),
            history: PromptHistory::new(),
            mode: view.mode,
            when: when_of(view),
            shell_history: PromptHistory::new(),
            shell_selected: None,
            shell_expanded: HashSet::new(),
            shell_scroll: 0,
            shell_room: Cell::new((0, 0)),
            shell_outputs: RefCell::default(),
            marker: ReadMarker::new(view.id),
            rule_pick: None,
            picker: None,
        }
    }

    /// Starts the view state afresh when `view` shows another subagent than
    /// before; the rest of the session's state stays. Stepping out to the
    /// view the `Agent` call is in selects that row and scrolls to it.
    fn show(&mut self, view: &SessionView) {
        let call = view.subagent.as_ref().map(|s| s.call);
        if call == self.subagent {
            return;
        }
        let left = std::mem::replace(&mut self.subagent, call);
        self.shown = ViewUi::new(false);
        let row = left.and_then(|left| {
            view.timeline.iter().find(|item| {
                matches!(item, TimelineItem::Logged { entry: Entry::Tool(tool), .. } if tool.call == left)
            })
        });
        if let Some(item) = row {
            self.shown.selected = selectable(item);
            self.shown.scroll = Some(Anchor {
                key: ItemKey::of(item),
                offset: 0,
            });
            self.shown.reveal = Some(ItemKey::of(item));
        }
    }

    /// The highlighted rule while the list `A` opened is up: only while its
    /// approval is still the one waiting here unanswered.
    pub fn rule_pick(&self, view: &SessionView) -> Option<usize> {
        let pick = self.rule_pick?;
        let prompt = view.pending_approval.as_ref()?;
        (prompt.id == pick.approval && unanswered(prompt)).then_some(pick.selected)
    }

    /// `A`: approve and grant the rule the approval offers; with several,
    /// list them to pick one first.
    fn allow_always(&mut self, view: &SessionView) -> Option<Action> {
        let prompt = view.pending_approval.as_ref().filter(|p| unanswered(p))?;
        match prompt.suggestions.len() {
            0 => None,
            1 => Some(Action::ResolveApproval {
                id: prompt.id,
                decision: Decision::Approve,
                rule: Some(0),
            }),
            _ => {
                self.rule_pick = Some(RulePick {
                    approval: prompt.id,
                    selected: 0,
                });
                None
            }
        }
    }

    /// Keys while the rule list is up: ↑↓ pick, Enter grants, Esc goes back
    /// to a/x, which still answer. Nothing else reaches the timeline.
    fn rule_pick_key(&mut self, view: &SessionView, key: KeyCode) -> Option<Action> {
        let selected = self.rule_pick(view)?;
        let prompt = view.pending_approval.as_ref()?;
        let last = prompt.suggestions.len().saturating_sub(1);
        let mut pick = |selected| {
            if let Some(pick) = &mut self.rule_pick {
                pick.selected = selected;
            }
            None
        };
        match key {
            KeyCode::Up | KeyCode::Char('k') => pick(selected.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => pick((selected + 1).min(last)),
            KeyCode::Enter => {
                self.rule_pick = None;
                Some(Action::ResolveApproval {
                    id: prompt.id,
                    decision: Decision::Approve,
                    rule: u8::try_from(selected).ok(),
                })
            }
            KeyCode::Esc => {
                self.rule_pick = None;
                None
            }
            KeyCode::Char('a') | KeyCode::Char('x') => {
                self.rule_pick = None;
                let decision = if key == KeyCode::Char('a') {
                    Decision::Approve
                } else {
                    Decision::Deny
                };
                answer(view, decision)
            }
            _ => None,
        }
    }

    /// The scroll state shown: while [`SessionUi::opening`], the one the
    /// opening will settle on; while a search match waits to be scrolled
    /// to ([`Search::pin`]), the one that shows it.
    pub fn shown_scroll(&self) -> Option<Anchor> {
        if self.shown.opening {
            self.shown.geometry.borrow().opening()
        } else {
            self.pinned_scroll()
                .unwrap_or_else(|| self.revealed_scroll())
        }
    }

    /// The scroll state, settled to following if the last frame laid out
    /// the item `v` revealed and it fits at the bottom.
    fn revealed_scroll(&self) -> Option<Anchor> {
        let geometry = self.shown.geometry.borrow();
        if self.shown.reveal.is_some() && geometry.reveal == self.shown.reveal {
            geometry.follow_at_bottom(self.shown.scroll)
        } else {
            self.shown.scroll
        }
    }

    /// While [`Search::pin`]: the scroll state that shows the current
    /// match's line in its blob, if the last frame laid it out.
    pub fn pinned_scroll(&self) -> Option<Option<Anchor>> {
        let search = self.shown.search.as_ref().filter(|s| s.pin)?;
        self.shown.geometry.borrow().match_scroll(search.current?)
    }

    /// Applies the opening scroll, or the one showing a search match in a
    /// blob, once a frame has laid the timeline out.
    fn settle(&mut self) {
        if self.shown.geometry.borrow().height == 0 {
            return;
        }
        if self.shown.opening {
            self.shown.scroll = self.shown.geometry.borrow().opening();
            self.shown.opening = false;
        }
        if self.shown.search.as_ref().is_some_and(|s| s.pin) {
            // Not laid out (cut by the file's end, say): the item's top.
            if let Some(scroll) = self.pinned_scroll() {
                self.shown.scroll = scroll;
            }
            if let Some(search) = &mut self.shown.search {
                search.pin = false;
            }
        }
        if self.shown.reveal.is_some() && self.shown.geometry.borrow().reveal == self.shown.reveal {
            self.shown.scroll = self.revealed_scroll();
            self.shown.reveal = None;
            self.shown.geometry.borrow_mut().reveal = None;
        }
    }

    /// The newest item is on screen and the opening scroll settled: what
    /// reaches the session is seen.
    fn following(&self) -> bool {
        !self.shown.opening && self.shown.scroll.is_none()
    }

    /// While browsing earlier prompts: the shown one's place, 1 the newest,
    /// and how many there are.
    pub fn history_position(&self) -> Option<(usize, usize)> {
        match self.mode {
            ComposerMode::Agent => self.history.position(),
            ComposerMode::Shell => self.shell_history.position(),
        }
    }

    /// Ends browsing earlier prompts or commands: the shown one stays.
    fn end_history(&mut self) {
        self.history.end();
        self.shell_history.end();
    }

    /// Whether the composer writes shell commands.
    pub fn shell(&self) -> bool {
        self.mode == ComposerMode::Shell
    }

    /// Tells the draft saver of composer edits made since it last heard,
    /// as made at `now`.
    fn note_edits(&mut self, now: Instant) {
        if std::mem::take(&mut self.composer.edited) {
            self.saver.edited(now);
        }
    }

    /// Shows the next older (`older`) or newer earlier prompt; newer than
    /// the newest is the text that was being written. The shown text is
    /// the draft: it is saved like typed text.
    /// In shell mode the same over earlier shell commands, from a history
    /// of their own.
    fn recall(&mut self, view: &SessionView, older: bool) {
        let shell = self.shell();
        let history = if shell {
            &mut self.shell_history
        } else {
            &mut self.history
        };
        if older {
            let composer = &self.composer;
            let source = || {
                if shell {
                    view.shell.commands()
                } else {
                    view.prompts()
                }
            };
            if let Some(prompt) = history.older(source, || composer.clone()) {
                self.composer.replace(prompt);
            }
            return;
        }
        match history.newer() {
            Some(Recall::Prompt(prompt)) => self.composer.replace(prompt),
            Some(Recall::Stash(stash)) => {
                self.composer = stash;
                self.composer.edited = true;
            }
            None => {}
        }
    }

    /// Refills the composer, focused, when the core rewrote the draft.
    fn follow_draft(&mut self, view: &SessionView) {
        if self.saver.refill(view) {
            self.composer = composer_of(view);
            self.when = when_of(view);
            self.end_history();
            self.focus = Focus::Composer;
        }
    }

    /// Follows the core's switch of the composer's mode: the old mode's
    /// draft was saved before the switch went out, so the composer loads
    /// the new mode's draft, condition included, focused. Both drafts stay
    /// in the core.
    fn follow_mode(&mut self, view: &SessionView) {
        if view.mode == self.mode {
            return;
        }
        self.mode = view.mode;
        self.composer = composer_of(view);
        self.when = when_of(view);
        self.end_history();
        self.focus = Focus::Composer;
    }

    /// The selected run, while it is still in the session's runs.
    pub fn selected_run<'a>(&self, view: &'a SessionView) -> Option<&'a ShellRun> {
        let id = self.shell_selected?;
        view.shell.runs.iter().find(|run| run.run == id)
    }

    /// Moves the run selection up (older) or down; with none, either
    /// starts at the newest run.
    fn select_run(&mut self, view: &SessionView, up: bool) {
        let runs = &view.shell.runs;
        let Some(last) = runs.len().checked_sub(1) else {
            return;
        };
        let at = self
            .selected_run(view)
            .and_then(|run| runs.iter().position(|r| r.run == run.run));
        let index = match at {
            None => last,
            Some(i) if up => i.saturating_sub(1),
            Some(i) => (i + 1).min(last),
        };
        self.pick_run(Some(runs[index].run));
    }

    /// Selects `run`, from its top.
    fn pick_run(&mut self, run: Option<CommandId>) {
        self.shell_selected = run;
        self.shell_scroll = 0;
    }

    /// PgUp/PgDn on the run list: pages through the selected run while it
    /// is taller than the list, to its first or last line and no further.
    fn page_run(&mut self, up: bool) {
        let (height, lines) = self.shell_room.get();
        let step = height.saturating_sub(1).max(1);
        let most = lines.saturating_sub(height);
        self.shell_scroll = if up {
            self.shell_scroll.min(most).saturating_sub(step)
        } else {
            (self.shell_scroll + step).min(most)
        };
    }

    /// Enter/Space on a run: shows its whole output, or only the tail
    /// again. Expanding a cut preview downloads the output if needed.
    fn toggle_run(&mut self, view: &SessionView) -> Option<Action> {
        let run = self.selected_run(view)?;
        self.shell_scroll = 0;
        if !self.shell_expanded.insert(run.run) {
            self.shell_expanded.remove(&run.run);
            return None;
        }
        run.output.as_ref().and_then(output_download)
    }
}

/// The link stopped for a reason only pairing again fixes.
fn needs_repair(link: &Link) -> bool {
    matches!(link, Link::Rejected(rejection) if rejection.needs_repair())
}

/// The download a run's output blob needs before it can be shown or
/// copied: none while it is here or on its way.
fn output_download(blob: &BlobView) -> Option<Action> {
    matches!(
        blob.state,
        BlobState::NotDownloaded { .. } | BlobState::Failed { .. }
    )
    .then_some(Action::DownloadBlob(blob.id))
}

/// The composer's text as the core keeps it for `view`'s mode.
fn composer_of(view: &SessionView) -> Composer {
    let draft = match view.mode {
        ComposerMode::Agent => &view.draft,
        ComposerMode::Shell => &view.shell_draft,
    };
    draft
        .as_ref()
        .map(|d| Composer::from_draft(d.text.clone(), d.cursor))
        .unwrap_or_default()
}

/// The condition the shell draft was saved with.
fn when_of(view: &SessionView) -> RunWhen {
    view.shell_draft
        .as_ref()
        .map_or(RunWhen::Always, |d| d.when)
}

/// Ctrl-O's cycle: always → if ok → if failed → always.
fn next_when(when: RunWhen) -> RunWhen {
    match when {
        RunWhen::Always => RunWhen::OnSuccess,
        RunWhen::OnSuccess => RunWhen::OnFailure,
        RunWhen::OnFailure => RunWhen::Always,
    }
}

/// What the usage screen shows.
#[derive(Debug, Clone, Default)]
pub struct UsageUi {
    pub span: Span,
    pub grouping: Grouping,
    /// The first row shown; `j`/`k` scroll.
    pub top: usize,
    /// The highest [`top`](Self::top) the last frame could use, as
    /// [`UiState::help_end`] is for the help.
    pub end: Cell<usize>,
}

/// Presentation state owned by the head.
#[derive(Debug, Clone)]
pub struct UiState {
    pub help: bool,
    /// The first line the help overlay shows; `j`/`k` scroll it.
    pub help_top: usize,
    /// The highest [`help_top`](Self::help_top) the last frame could use:
    /// the overlay records it so scrolling stops at the last line.
    pub help_end: Cell<usize>,
    pub form: PairingForm,
    /// Selected session on the session list, by id so it stays put when
    /// rows arrive or reorder.
    pub selected: Option<SessionId>,
    /// The open "new session" or rename prompt, if any.
    pub prompt: Option<Prompt>,
    /// The conversation picker's highlighted row, by the agent's id so it
    /// stays put when a refresh reorders the rows; the first row when
    /// `None` or gone. Whether the picker is open is the core's
    /// ([`SessionListView::conversations`]).
    pub conversation: Option<String>,
    /// `D` on the archived list asks whether to delete this session from
    /// this device; `y` does, any other key does not.
    pub delete: Option<SessionId>,
    /// The open session's focus, scroll, expanded items and composer.
    pub session: Option<SessionUi>,
    /// The usage screen's span, grouping and scroll; kept while away.
    pub usage: UsageUi,
    /// The workdir screen's selection, scroll and read blob text.
    pub workdir: WorkdirUi,
    /// The user chose to browse local data behind the re-pair screen.
    pub repair_dismissed: bool,
    /// The last failed dispatch, shown in the footer until the next key.
    pub error: Option<String>,
    /// What happened to the last key's request, e.g. a copy; shown in the
    /// footer until the next key.
    pub notice: Option<String>,
    /// A copy to the terminal's clipboard, for the event loop to send
    /// ([`App::take_copy`]).
    pub copy: Option<Clip>,
    /// Watches the core's [`alerts`](skywave_core::Attention::alerts)
    /// count; the first snapshot's needs no alert.
    pub alerts: AlertWatcher,
    /// Watches the core's [`shell_alerts`](skywave_core::Attention::shell_alerts)
    /// count from 0: a run that finished before the first snapshot is news.
    pub shell_alerts: ShellAlertWatcher,
    /// An alert for the event loop to send ([`App::take_alert`]), as the
    /// notification's text: new approvals came in (naming the newest
    /// one's session), else a long shell run of this device's finished.
    pub alert: Option<String>,
    /// Ctrl-G asked for the next approval's session: it opens on the
    /// timeline, so `a`/`x` answer at once.
    pub jumped: bool,
    pub quit: bool,
}

/// The TUI's state: the core's latest snapshot plus [`UiState`].
#[derive(Debug, Clone)]
pub struct App {
    view: ViewState,
    ui: UiState,
}

impl App {
    pub fn new(view: ViewState) -> App {
        let mut app = App {
            view: view.clone(),
            ui: UiState {
                help: false,
                help_top: 0,
                help_end: Cell::new(0),
                form: PairingForm::default(),
                selected: None,
                prompt: None,
                conversation: None,
                delete: None,
                session: None,
                usage: UsageUi::default(),
                workdir: WorkdirUi::default(),
                repair_dismissed: false,
                error: None,
                notice: None,
                copy: None,
                alerts: AlertWatcher::new(&view.attention),
                shell_alerts: ShellAlertWatcher::new(),
                alert: None,
                jumped: false,
                quit: false,
            },
        };
        app.observe_shell_alerts(&view);
        app.prefill(&view);
        app.keep_selection(None);
        app.follow_session();
        app.follow_workdir();
        app
    }

    pub fn view(&self) -> &ViewState {
        &self.view
    }

    pub fn ui(&self) -> &UiState {
        &self.ui
    }

    /// Takes a new snapshot from the core.
    pub fn set_view(&mut self, view: ViewState) {
        if !needs_repair(&view.link) {
            self.ui.repair_dismissed = false;
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
        if self
            .ui
            .prompt
            .as_ref()
            .is_some_and(|p| !p.fits(&view.screen))
        {
            self.ui.prompt = None;
        }
        self.observe_shell_alerts(&view);
        // After the shell's: an approval waits on the user, so it wins the
        // one notification. A bell cannot be taken back, so a session no
        // longer waiting (`cleared`) needs nothing here.
        if let Some(alert) = self.ui.alerts.observe(&view.attention).alert {
            self.ui.notice = Some(alert.notice());
            self.ui.alert = Some(notify::message(alert.title.as_deref()));
        }
        let old_index = self
            .session_list()
            .and_then(|list| self.selected_index(list));
        let old_item = self
            .session()
            .and_then(|(view, ui)| find(view, ui.shown.selected?));
        let old_match = self
            .session()
            .and_then(|(view, ui)| find(view, ui.shown.search.as_ref()?.current?));
        self.view = view;
        if self.conversation_picker().is_none() {
            self.ui.conversation = None;
        }
        if let (Some(create), Screen::SessionList(list)) = (&mut self.ui.prompt, &self.view.screen)
            && create.asks_workdir()
            && let Some(completion) = &list.completion
        {
            create.completion.settle(&mut create.text, completion);
        }
        // The question goes with its row, e.g. when a message brings the
        // session back.
        if self.ui.delete.is_some() && self.delete_row().is_none() {
            self.ui.delete = None;
        }
        self.keep_selection(old_index);
        self.follow_session();
        self.follow_workdir();
        if let (Screen::Session(view), Some(session)) = (&self.view.screen, &mut self.ui.session) {
            session.keep_selection(view, old_item);
            session.keep_scroll(view);
            session.follow_mode(view);
            session.follow_draft(view);
            if let Some(search) = &mut session.shown.search {
                search.refresh(view, old_match);
            }
        }
    }

    /// Alerts, and says in the footer, that a long shell run of this
    /// device's finished, when `view` counts a new one.
    fn observe_shell_alerts(&mut self, view: &ViewState) {
        if let Some(notice) = self.ui.shell_alerts.observe(&view.attention) {
            self.ui.notice = Some(notice.notice());
            self.ui.alert = Some(notify::shell_message(&notice.notice()));
        }
    }

    /// Fresh session presentation state when another session opens; on
    /// the timeline after Ctrl-G.
    fn follow_session(&mut self) {
        match &self.view.screen {
            Screen::Session(session) => {
                if self.ui.session.as_ref().is_none_or(|s| s.id != session.id) {
                    let mut ui = SessionUi::new(session);
                    if std::mem::take(&mut self.ui.jumped) {
                        ui.focus = Focus::Timeline;
                    }
                    self.ui.session = Some(ui);
                } else if let Some(ui) = &mut self.ui.session {
                    ui.show(session);
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

    /// The screen shows a clock that runs: a tool call's or a shell run's
    /// elapsed time.
    pub fn live(&self) -> bool {
        self.session().is_some_and(|(view, _)| {
            view.timeline.iter().any(|item| {
                matches!(
                    item,
                    TimelineItem::Logged { entry: Entry::Tool(tool), .. }
                        if matches!(tool.outcome, ToolOutcome::Running { .. })
                )
            }) || view
                .shell
                .runs
                .iter()
                .any(|run| run.state == ShellRunState::Running)
        })
    }

    /// The open session's composer.
    pub fn composer(&self) -> Option<&Composer> {
        self.session().map(|(_, ui)| &ui.composer)
    }

    /// Records the outcome of dispatching an action from [`App::on_key`].
    pub fn dispatched(&mut self, result: Result<(), CoreError>) {
        if let Some(prompt) = &mut self.ui.prompt
            && prompt.submitted
        {
            if result.is_ok() {
                self.ui.prompt = None;
            } else {
                prompt.submitted = false;
            }
        }
        if let Some(session) = &mut self.ui.session
            && session.composer.submitted
        {
            session.composer.submitted = false;
            if result.is_ok() {
                // The enqueue dropped the core's draft. Clearing counts as
                // an edit: if the core refused a blank message instead, the
                // next save drops its draft; otherwise that save writes
                // nothing.
                session.composer.clear();
                // A run's condition is for that run only.
                if session.shell() {
                    session.when = RunWhen::Always;
                }
                session.note_edits(Instant::now());
            }
        }
        if result.is_err() {
            self.ui.notice = None;
        }
        self.ui.error = result.err().map(|err| err.to_string());
    }

    /// The copy [`App::on_key`] asked for; report the outcome of sending it
    /// with [`App::copied`].
    pub fn take_copy(&mut self) -> Option<Clip> {
        self.ui.copy.take()
    }

    /// The alert [`App::set_view`] asked for, if any: the notification's
    /// text, made safe to send.
    pub fn take_alert(&mut self) -> Option<String> {
        self.ui.alert.take()
    }

    /// Records the outcome of sending a [`App::take_copy`] copy: how many
    /// bytes went to the terminal, or why none did.
    pub fn copied(&mut self, result: Result<usize, CopyError>) {
        match result {
            Ok(n) => {
                self.ui.notice = Some(format!(
                    "copied {} · sent to terminal clipboard",
                    bytes(n as u64)
                ));
            }
            Err(CopyError::TooLarge { size, max, file }) => {
                let file = file
                    .map(|path| format!(" · file: {}", path.display()))
                    .unwrap_or_default();
                self.ui.notice = Some(format!(
                    "too large for the terminal clipboard ({} > {}){file}",
                    bytes(size),
                    bytes(max as u64)
                ));
            }
            Err(CopyError::Read { file, error }) => {
                self.ui.notice = None;
                self.ui.error = Some(format!("copy: cannot read {}: {error}", file.display()));
            }
            Err(CopyError::Write(error)) => {
                self.ui.notice = None;
                self.ui.error = Some(format!("copy: {error}"));
            }
        }
    }

    /// Asks for `content` to be copied: text or a downloaded blob go to
    /// [`UiState::copy`]; a blob not here yet is downloaded first (never
    /// just its preview), and the notice says to press `key` again.
    fn copy(&mut self, key: char, content: Option<Content>) -> Option<Action> {
        let (blob, strip) = match content? {
            Content::Text(text) => {
                self.ui.copy = Some(Clip::Text(text));
                return None;
            }
            Content::Blob { blob, strip } => (blob, strip),
        };
        self.copy_blob(key, blob, |path| Clip::File { path, strip })
    }

    /// `y` on a shell run: its whole output once downloaded, ANSI
    /// stripped; until then the preview, which holds all of it when there
    /// is no blob and is stripped already.
    fn copy_run(&mut self, run: &ShellRun) -> Option<Action> {
        match &run.output {
            Some(blob) if matches!(blob.state, BlobState::Available { .. }) => {
                self.copy_blob('y', blob.clone(), |path| Clip::Output { path })
            }
            _ => {
                self.ui.copy = Some(Clip::Text(run.tail_or_preview.clone()));
                None
            }
        }
    }

    /// Copies `blob` as `clip` makes of its file once it is here; else
    /// downloads it, and the notice says to press `key` again.
    fn copy_blob(
        &mut self,
        key: char,
        blob: BlobView,
        clip: impl FnOnce(PathBuf) -> Clip,
    ) -> Option<Action> {
        let (size, action) = match blob.state {
            BlobState::Available { path } => {
                self.ui.copy = Some(clip(path));
                return None;
            }
            BlobState::NotDownloaded { size } | BlobState::Failed { size, .. } => {
                (size, Some(Action::DownloadBlob(blob.id)))
            }
            BlobState::Requested { size } | BlobState::Downloading { size, .. } => (size, None),
        };
        self.ui.notice = Some(format!(
            "downloading {} · press {key} again when it is here",
            bytes(size)
        ));
        action
    }

    /// Saves the open session's draft when it has unsaved edits: always if
    /// `force` (leaving the session, quitting), else once the first of them
    /// is [`DRAFT_SAVE_AFTER`] old at `now`. Report the outcome with
    /// [`App::draft_saved`], not [`App::dispatched`].
    pub fn draft_save(&mut self, now: Instant, force: bool) -> Option<Action> {
        let session = self.ui.session.as_mut()?;
        let composer = &session.composer;
        let when = match session.mode {
            ComposerMode::Agent => RunWhen::Always,
            ComposerMode::Shell => session.when,
        };
        session.saver.save(
            now,
            force,
            session.mode,
            composer.text(),
            composer.cursor_chars(),
            when,
        )
    }

    /// How long from `now` until [`App::draft_save`] has something to save
    /// unforced; `None` while there are no unsaved edits.
    pub fn draft_due_in(&self, now: Instant) -> Option<Duration> {
        let wake = self.ui.session.as_ref()?.saver.next_wake()?;
        Some(wake.saturating_duration_since(now))
    }

    /// Records the outcome, at `now`, of dispatching a [`App::draft_save`]
    /// action. Only an error matters: the composer is not a submitted
    /// message and must not be cleared like one, and its edits are unsaved
    /// again ([`DraftSaver::failed`]).
    pub fn draft_saved(&mut self, now: Instant, result: Result<(), CoreError>) {
        if let Err(err) = result {
            self.ui.error = Some(err.to_string());
            if let Some(session) = self.ui.session.as_mut() {
                session.saver.failed(now);
            }
        }
    }

    /// Marks the open session read through its newest event when that is on
    /// screen (the timeline follows) and newer than the last mark: at once
    /// if `force` (leaving the session, quitting) or for the first mark,
    /// else at most once per [`READ_MARK_EVERY`]. Report the outcome with
    /// [`App::read_marked`].
    pub fn read_mark(&mut self, now: Instant, force: bool) -> Option<Action> {
        let Screen::Session(view) = &self.view.screen else {
            return None;
        };
        let session = self.ui.session.as_mut().filter(|s| s.id == view.id)?;
        session.settle();
        // The main view's news is not read from a subagent's.
        let following = session.following() && view.subagent.is_none();
        session.marker.mark(now, view.last_seq, following, force)
    }

    /// How long from `now` until [`App::read_mark`] has something to mark
    /// unforced; `None` while there is nothing to mark.
    pub fn read_mark_due_in(&self, now: Instant) -> Option<Duration> {
        let (view, ui) = self.session()?;
        if ui.shown.opening {
            // Settles as soon as a frame was drawn.
            return (ui.shown.geometry.borrow().height > 0).then_some(Duration::ZERO);
        }
        let following = ui.following() && view.subagent.is_none();
        let wake = ui.marker.next_wake(now, view.last_seq, following)?;
        Some(wake.saturating_duration_since(now))
    }

    /// Records the outcome of dispatching a [`App::read_mark`] action; only
    /// an error matters.
    pub fn read_marked(&mut self, result: Result<(), CoreError>) {
        if let Err(err) = result {
            self.ui.error = Some(err.to_string());
        }
    }

    /// The downloads the open view's expanded items need and nobody asked
    /// for yet ([`ExpandFetch`]): also a blob that arrived while its item
    /// was open, e.g. the output of an edit expanded with `v` before its
    /// result logged. Each blob once. Report each outcome with
    /// [`App::fetched`].
    pub fn expanded_fetches(&mut self) -> Vec<Action> {
        let Screen::Session(view) = &self.view.screen else {
            return Vec::new();
        };
        let Some(session) = self.ui.session.as_mut().filter(|s| s.id == view.id) else {
            return Vec::new();
        };
        session.shown.fetch.due(view, &session.shown.expanded)
    }

    /// Records the outcome of dispatching an [`App::expanded_fetches`]
    /// action; only an error matters.
    pub fn fetched(&mut self, result: Result<(), CoreError>) {
        if let Err(err) = result {
            self.ui.error = Some(err.to_string());
        }
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

    /// The re-pair full-screen state covers the current screen: the link
    /// stopped for a reason only pairing again fixes.
    pub fn repair_screen(&self) -> bool {
        needs_repair(&self.view.link)
            && !matches!(self.view.screen, Screen::Pairing(_))
            && !self.ui.repair_dismissed
    }

    /// The focused element takes text, so letters are input, not commands.
    /// The archived session `D` asks about, while the question is open.
    pub fn delete_row(&self) -> Option<&SessionRow> {
        let id = self.ui.delete?;
        match &self.view.screen {
            Screen::SessionList(list) if list.kind == ListKind::Archived => {
                list.sessions.iter().find(|row| row.id == id)
            }
            _ => None,
        }
    }

    pub fn typing(&self) -> bool {
        !self.repair_screen()
            && match self.view.screen {
                Screen::Pairing(_) => true,
                Screen::SessionList(_) => self.ui.prompt.is_some(),
                Screen::Session(_) | Screen::Settings(_) if self.ui.prompt.is_some() => true,
                Screen::Session(_) => self.ui.session.as_ref().is_some_and(|s| match &s.picker {
                    Some(picker) => matches!(picker, Picker::Model { .. }),
                    None => {
                        s.focus == Focus::Composer
                            || s.shown.search.as_ref().is_some_and(|s| s.editing)
                    }
                }),
                _ => false,
            }
    }

    /// Bracketed paste: on the pairing screen a pairing URI fills the whole
    /// form, a malformed one is reported, anything else goes into the
    /// focused field; an open prompt
    /// (new session, rename, server address) takes it; on a session it goes into the
    /// search prompt while that is open, else into the composer, line
    /// breaks included.
    pub fn on_paste(&mut self, text: &str) {
        if self.ui.help || self.repair_screen() {
            return;
        }
        if let Some(prompt) = &mut self.ui.prompt {
            prompt.insert(text.trim());
            prompt.completion.pasted();
            return;
        }
        // A model name; the mode list takes no text.
        if let Some(picker) = self.ui.session.as_mut().and_then(|s| s.picker.as_mut()) {
            if let Picker::Model { name, .. } = picker {
                name.extend(text.trim().chars().filter(|c| !c.is_control()));
            }
            return;
        }
        if let (Screen::Session(view), Some(session)) = (&self.view.screen, &mut self.ui.session)
            && let Some(search) = session.shown.search.as_mut().filter(|s| s.editing)
        {
            search.edit(view, |query| {
                query.extend(text.chars().map(|c| if c == '\n' { ' ' } else { c }));
                query.retain(|c| !c.is_control());
            });
            return;
        }
        if let Some(session) = &mut self.ui.session {
            session.focus = Focus::Composer;
            session.end_history();
            session.composer.insert(text);
            session.note_edits(Instant::now());
            return;
        }
        if !self.typing() {
            return;
        }
        match parse_pair_uri(text) {
            Ok(uri) => self.ui.form.fill(uri),
            Err(err) if looks_like_pair_uri(text) => self.ui.error = Some(err.to_string()),
            Err(_) => self.ui.form.insert(text.trim()),
        }
    }

    /// Handles a key press; returns the action to dispatch, if any.
    pub fn on_key(&mut self, key: KeyEvent) -> Option<Action> {
        let action = self.key(key);
        if let Some(session) = &mut self.ui.session {
            session.note_edits(Instant::now());
        }
        action
    }

    fn key(&mut self, key: KeyEvent) -> Option<Action> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        self.ui.error = None;
        self.ui.notice = None;
        self.ui.jumped = false;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            self.ui.quit = true;
            return None;
        }
        // y/N: only `y` deletes; any other key keeps the session.
        if let Some(row) = self.delete_row() {
            let id = row.id;
            self.ui.delete = None;
            return matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y'))
                .then_some(Action::DeleteSessionLocally { session_id: id });
        }
        // Help takes every key: none reaches the screen beneath.
        if self.ui.help {
            let end = self.ui.help_end.get();
            match key.code {
                KeyCode::Esc | KeyCode::F(1) | KeyCode::Char('?') | KeyCode::Char('q') => {
                    self.ui.help = false;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.ui.help_top = (self.ui.help_top + 1).min(end);
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.ui.help_top = self.ui.help_top.min(end).saturating_sub(1);
                }
                KeyCode::Home | KeyCode::Char('g') => self.ui.help_top = 0,
                KeyCode::End | KeyCode::Char('G') => self.ui.help_top = end,
                _ => {}
            }
            return None;
        }
        if key.code == KeyCode::F(1) || (!self.typing() && key.code == KeyCode::Char('?')) {
            self.ui.help = true;
            self.ui.help_top = 0;
            return None;
        }
        if ctrl && key.code == KeyCode::Char('r') {
            return Some(Action::RetryNow);
        }
        // Even while typing: the draft is saved on the way out.
        if ctrl && key.code == KeyCode::Char('g') {
            if self.repair_screen() || self.view.attention.elsewhere == 0 {
                return None;
            }
            self.ui.jumped = true;
            return Some(Action::OpenNextApproval);
        }
        if self.repair_screen() {
            return match key.code {
                KeyCode::Char('p') | KeyCode::Enter => Some(Action::Navigate(Route::Pairing)),
                KeyCode::Esc => {
                    self.ui.repair_dismissed = true;
                    None
                }
                KeyCode::Char('q') => {
                    self.ui.quit = true;
                    None
                }
                _ => None,
            };
        }
        match self.ui.prompt.as_ref().map(|p| p.kind) {
            Some(PromptKind::Create) => return self.create_key(key, ctrl),
            Some(PromptKind::Adopt)
                if self.ui.prompt.as_ref().is_some_and(|p| p.asks_workdir()) =>
            {
                return self.create_key(key, ctrl);
            }
            Some(PromptKind::Adopt) => return self.adopt_id_key(key, ctrl),
            Some(PromptKind::Rename(id)) => return self.rename_key(id, key, ctrl),
            Some(PromptKind::ServerAddress) => return self.address_key(key, ctrl),
            None => {}
        }
        if self.conversation_picker().is_some() {
            return self.conversations_key(key);
        }
        match &self.view.screen {
            Screen::Pairing(_) => self.pairing_key(key, ctrl),
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
                // A new session is active: created from the active list.
                KeyCode::Char('n') if list.kind == ListKind::Active => {
                    self.ui.prompt = Some(Prompt::new(PromptKind::Create, String::new()));
                    None
                }
                // A conversation started outside Skywave continues in a new
                // session, so also from the active list: the core asks the
                // server for them and opens the picker.
                KeyCode::Char('c') if list.kind == ListKind::Active => {
                    Some(Action::ListConversations)
                }
                // The row leaves the list; the selection moves to its
                // neighbour (`keep_selection`).
                KeyCode::Char('a') => {
                    let row = &list.sessions[self.selected_index(list)?];
                    Some(Action::SetSessionArchived {
                        session_id: row.id,
                        archived: list.kind == ListKind::Active,
                    })
                }
                KeyCode::Char('A') => Some(Action::Navigate(match list.kind {
                    ListKind::Active => Route::ArchivedSessions,
                    ListKind::Archived => Route::SessionList,
                })),
                KeyCode::Esc if self.view.can_go_back => Some(Action::Back),
                // Deleting from this device asks first; the core says when
                // something about the session still waits on an answer.
                KeyCode::Char('D') if list.kind == ListKind::Archived => {
                    let row = &list.sessions[self.selected_index(list)?];
                    if row.deletable {
                        self.ui.delete = Some(row.id);
                    } else {
                        self.ui.notice = Some(format!(
                            "cannot delete {} yet: answer or cancel first",
                            notify::sanitise(&row.title)
                        ));
                    }
                    None
                }
                KeyCode::Char('r') => {
                    let row = &list.sessions[self.selected_index(list)?];
                    let name = if row.named {
                        row.title.clone()
                    } else {
                        String::new()
                    };
                    self.ui.prompt = Some(Prompt::new(PromptKind::Rename(row.id), name));
                    None
                }
                KeyCode::Char('s') => Some(Action::Navigate(Route::Settings)),
                KeyCode::Char('u') => Some(Action::Navigate(Route::Usage)),
                KeyCode::Delete => dismissable(list).map(|p| Action::Discard(p.command_id)),
                _ => None,
            },
            Screen::Session(_) => self.session_key(key, ctrl),
            Screen::Workdir(_) => self.workdir_key(key),
            Screen::Usage(_) => {
                let usage = &mut self.ui.usage;
                match key.code {
                    KeyCode::Char('q') => return self.quit(),
                    KeyCode::Esc => return Some(Action::Back),
                    KeyCode::Char('r') => {
                        usage.span = usage.span.next();
                        usage.top = 0;
                    }
                    KeyCode::Tab => {
                        usage.grouping = usage.grouping.next();
                        usage.top = 0;
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        usage.top = (usage.top + 1).min(usage.end.get());
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        usage.top = usage.top.min(usage.end.get()).saturating_sub(1);
                    }
                    KeyCode::Home | KeyCode::Char('g') => usage.top = 0,
                    _ => {}
                }
                None
            }
            Screen::Settings(settings) => match key.code {
                KeyCode::Char('q') => self.quit(),
                KeyCode::Esc => Some(Action::Back),
                KeyCode::Char('b') => Some(Action::SetBlobPolicy(match settings.blob_policy {
                    BlobPolicy::Eager => BlobPolicy::OnDemand,
                    BlobPolicy::OnDemand => BlobPolicy::Eager,
                })),
                KeyCode::Char('p') => Some(Action::Navigate(Route::Pairing)),
                KeyCode::Char('a') if settings.device_id.is_some() => {
                    let address = settings.server_address.clone().unwrap_or_default();
                    self.ui.prompt = Some(Prompt::new(PromptKind::ServerAddress, address));
                    None
                }
                _ => None,
            },
        }
    }

    fn pairing_key(&mut self, key: KeyEvent, ctrl: bool) -> Option<Action> {
        let form = &mut self.ui.form;
        // A URI typed or pasted without bracketed paste lands in one field.
        if key.code == KeyCode::Enter && looks_like_pair_uri(form.value(form.focus)) {
            match parse_pair_uri(form.value(form.focus)) {
                Ok(uri) => form.fill(uri),
                Err(err) => self.ui.error = Some(err.to_string()),
            }
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
            KeyCode::Esc if self.view.can_go_back => return Some(Action::Back),
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

    /// The completion popup of the new-session prompt, if it is open and
    /// has something to show, with the highlighted row.
    pub fn completion_popup(&self) -> Option<(&Completion, usize)> {
        let create = self.ui.prompt.as_ref().filter(|c| c.completion.open())?;
        let Screen::SessionList(list) = &self.view.screen else {
            return None;
        };
        let completion = list.completion.as_ref()?;
        Some((completion, create.completion.selected()))
    }

    /// The conversation picker, while the core has it open on the session
    /// list.
    pub fn conversation_picker(&self) -> Option<&ConversationPicker> {
        self.session_list()?.conversations.as_ref()
    }

    /// The highlighted row of `picker`: [`UiState::conversation`], else
    /// the first. `None` while it lists nothing.
    pub fn conversation_index(&self, picker: &ConversationPicker) -> Option<usize> {
        if picker.items.is_empty() {
            return None;
        }
        let id = self.ui.conversation.as_deref();
        Some(
            picker
                .items
                .iter()
                .position(|item| Some(item.id.as_str()) == id)
                .unwrap_or(0),
        )
    }

    /// The highlighted conversation, if the picker lists any.
    pub fn conversation_item(&self) -> Option<&ConversationItem> {
        let picker = self.conversation_picker()?;
        Some(&picker.items[self.conversation_index(picker)?])
    }

    /// Keys while the conversation picker is open: ↑↓ choose, Enter
    /// continues the highlighted conversation in a new session (the core
    /// closes the picker), `o` opens the session that already runs it, `p`
    /// pastes an id instead (the two-step prompt), `r` lists again, Esc
    /// closes.
    fn conversations_key(&mut self, key: KeyEvent) -> Option<Action> {
        let picker = self.conversation_picker()?;
        let index = self.conversation_index(picker);
        let item = index.map(|i| &picker.items[i]);
        match key.code {
            KeyCode::Esc => Some(Action::EndConversations),
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Down | KeyCode::Char('j') => {
                let (Some(index), Some(last)) = (index, picker.items.len().checked_sub(1)) else {
                    return None;
                };
                let down = matches!(key.code, KeyCode::Down | KeyCode::Char('j'));
                let to = if down {
                    (index + 1).min(last)
                } else {
                    index.saturating_sub(1)
                };
                self.ui.conversation = Some(picker.items[to].id.clone());
                None
            }
            KeyCode::Enter => item.map(|item| Action::CreateSession {
                workdir: item.workdir.clone(),
                conversation: Some(item.id.clone()),
            }),
            KeyCode::Char('o') => match item?.session {
                Some(session) => Some(Action::OpenSession(session)),
                None => {
                    self.ui.notice = Some("no session runs this conversation".to_owned());
                    None
                }
            },
            KeyCode::Char('p') => {
                self.ui.prompt = Some(Prompt::new(PromptKind::Adopt, String::new()));
                Some(Action::EndConversations)
            }
            KeyCode::Char('r') => Some(Action::ListConversations),
            KeyCode::Char('q') => self.quit(),
            _ => None,
        }
    }

    /// Keys while the new-session prompt is open. Tab completes the
    /// workdir; while the popup lists candidates, ↑↓ pick one and Tab or
    /// Enter take it, and every edit completes again.
    fn create_key(&mut self, key: KeyEvent, ctrl: bool) -> Option<Action> {
        let candidates = self
            .completion_popup()
            .map(|(completion, _)| completion.candidates.clone())
            .unwrap_or_default();
        let completing =
            matches!(&self.view.screen, Screen::SessionList(list) if list.completion.is_some());
        let create = self.ui.prompt.as_mut()?;
        create.completion.input();
        match key.code {
            KeyCode::Esc if create.completion.open() => create.completion.close(),
            KeyCode::Esc => {
                let end = create.completion.cancel(completing);
                self.ui.prompt = None;
                return end;
            }
            KeyCode::Up if !candidates.is_empty() => create.completion.up(),
            KeyCode::Down if !candidates.is_empty() => create.completion.down(candidates.len()),
            KeyCode::Tab | KeyCode::Enter if !candidates.is_empty() => {
                if let Some(chosen) = create.completion.choose(&candidates) {
                    create.text = chosen;
                }
            }
            KeyCode::Tab => return Some(create.completion.complete(&create.text)),
            // Blank or unacceptable paths are refused by the core or the
            // server, not here.
            // Adopting: the workdir is settled, the conversation id is next.
            KeyCode::Enter if create.kind == PromptKind::Adopt => {
                create.workdir = Some(create.text.trim().to_owned());
                create.text.clear();
                return create.completion.cancel(completing);
            }
            KeyCode::Enter => {
                create.submitted = true;
                return Some(Action::CreateSession {
                    conversation: None,
                    workdir: create.text.clone(),
                });
            }
            KeyCode::Backspace => {
                create.text.pop();
                return create.edited();
            }
            KeyCode::Char('u') if ctrl => {
                create.text.clear();
                return create.edited();
            }
            KeyCode::Char(c) if !ctrl => {
                create.insert(c.encode_utf8(&mut [0; 4]));
                return create.edited();
            }
            _ => {}
        }
        None
    }

    /// Keys while the adopt prompt asks for the conversation id: plain text
    /// input. Enter creates the session forking that conversation (the
    /// server says whether it is one), Esc cancels, Tab does nothing.
    fn adopt_id_key(&mut self, key: KeyEvent, ctrl: bool) -> Option<Action> {
        let prompt = self.ui.prompt.as_mut()?;
        match key.code {
            KeyCode::Esc => self.ui.prompt = None,
            KeyCode::Enter => {
                prompt.submitted = true;
                return Some(Action::CreateSession {
                    workdir: prompt.workdir.clone().unwrap_or_default(),
                    conversation: Some(prompt.text.clone()),
                });
            }
            KeyCode::Backspace => {
                prompt.text.pop();
            }
            KeyCode::Char('u') if ctrl => prompt.text.clear(),
            KeyCode::Char(c) if !ctrl => prompt.insert(c.encode_utf8(&mut [0; 4])),
            _ => {}
        }
        None
    }

    /// Keys while the rename prompt is open. Enter renames (the core takes a
    /// blank name as "back to the default title"), Esc cancels.
    fn rename_key(&mut self, session_id: SessionId, key: KeyEvent, ctrl: bool) -> Option<Action> {
        let prompt = self.ui.prompt.as_mut()?;
        match key.code {
            KeyCode::Esc => self.ui.prompt = None,
            KeyCode::Enter => {
                prompt.submitted = true;
                return Some(Action::RenameSession {
                    session_id,
                    name: prompt.text.clone(),
                });
            }
            KeyCode::Backspace => {
                prompt.text.pop();
            }
            KeyCode::Char('u') if ctrl => prompt.text.clear(),
            KeyCode::Char(c) if !ctrl => prompt.insert(c.encode_utf8(&mut [0; 4])),
            _ => {}
        }
        None
    }

    /// Keys while the server address prompt is open. Enter moves the link
    /// to the address, or shows why it is not one; Esc cancels.
    fn address_key(&mut self, key: KeyEvent, ctrl: bool) -> Option<Action> {
        let prompt = self.ui.prompt.as_mut()?;
        match key.code {
            KeyCode::Esc => self.ui.prompt = None,
            KeyCode::Enter => match ServerAddress::parse(&prompt.text) {
                Ok(_) => {
                    prompt.submitted = true;
                    return Some(Action::SetServerAddress {
                        address: prompt.text.clone(),
                    });
                }
                Err(err) => self.ui.error = Some(err.to_string()),
            },
            KeyCode::Backspace => {
                prompt.text.pop();
            }
            KeyCode::Char('u') if ctrl => prompt.text.clear(),
            KeyCode::Char(c) if !ctrl => prompt.insert(c.encode_utf8(&mut [0; 4])),
            _ => {}
        }
        None
    }

    /// Keys on the session view. An open search prompt takes every key;
    /// otherwise Ctrl-F, Ctrl-X, PgUp/PgDn (the timeline's; in shell mode
    /// the run list's, only while it has the focus) and Tab work in both
    /// focuses and the rest depends on [`Focus`].
    fn session_key(&mut self, key: KeyEvent, ctrl: bool) -> Option<Action> {
        let Screen::Session(view) = &self.view.screen else {
            return None;
        };
        let session = self.ui.session.as_mut()?;
        // Keys move from where the opening scrolled to.
        session.settle();
        // Not laid out yet: the revealed item stays at the top.
        session.shown.reveal = None;
        if let Some(picker) = &mut session.picker {
            let (close, action) = picker.key(view, key, ctrl);
            if close {
                session.picker = None;
            }
            return action;
        }
        if let Some(search) = &mut session.shown.search {
            search.search.wrapped = false;
            if search.editing {
                session.search_key(view, key, ctrl);
                return None;
            }
        }
        let shell = session.shell();
        match key.code {
            // Into shell mode only while the server runs shell commands;
            // back out always, should it have stopped since.
            KeyCode::Char('t') if ctrl => {
                if !shell && !view.shell.enabled {
                    self.ui.notice = Some("shell commands are off on this server".into());
                    return None;
                }
                let mode = if shell {
                    ComposerMode::Agent
                } else {
                    ComposerMode::Shell
                };
                return Some(Action::SetComposerMode {
                    session_id: view.id,
                    mode,
                });
            }
            KeyCode::Char('o') if ctrl && shell => {
                session.when = next_when(session.when);
                session.composer.edited = true;
                return None;
            }
            KeyCode::Char('x') if ctrl && shell => return shell_stop(view),
            KeyCode::Char('x') if ctrl => {
                return Some(Action::Interrupt {
                    session_id: view.id,
                });
            }
            // The timeline's: the run list has no search, and its pages
            // are the selected run's, while it has the focus.
            KeyCode::Char('f') if ctrl && shell => return None,
            KeyCode::PageUp | KeyCode::PageDown if shell && session.focus == Focus::Composer => {
                return None;
            }
            KeyCode::Char('f') if ctrl => {
                session.open_search();
                return None;
            }
            KeyCode::PageUp if !shell => {
                session.page(true);
                return None;
            }
            KeyCode::PageDown if !shell => {
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
                // Earlier prompts: Ctrl-P/N from any line, Up/Down past the
                // first/last line. Any other key ends browsing.
                let older = match key.code {
                    KeyCode::Char('p') if ctrl => Some(true),
                    KeyCode::Char('n') if ctrl => Some(false),
                    KeyCode::Up => Some(true),
                    KeyCode::Down => Some(false),
                    _ => None,
                };
                if let Some(older) = older {
                    if ctrl || !session.composer.vertical(older) {
                        session.recall(view, older);
                    }
                    return None;
                }
                session.end_history();
                let when = session.when;
                let draft = &mut session.composer;
                match key.code {
                    KeyCode::Esc => session.focus = Focus::Timeline,
                    KeyCode::Enter if alt || shift => draft.newline(),
                    KeyCode::Char('j') if ctrl => draft.newline(),
                    // Nothing to run: the condition picked stays.
                    KeyCode::Enter if shell && draft.is_blank() => {}
                    KeyCode::Enter if shell => {
                        draft.submitted = true;
                        return Some(Action::RunShell {
                            session_id: view.id,
                            command: draft.text().to_owned(),
                            when,
                        });
                    }
                    // A blank draft is refused by the core; one over the
                    // cap here, keeping its text.
                    KeyCode::Enter => {
                        if let Some(refusal) = message_refusal(draft.text()) {
                            self.ui.notice = Some(refusal);
                            return None;
                        }
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
                    KeyCode::Home => draft.home(),
                    KeyCode::End => draft.end(),
                    KeyCode::Char('u') if ctrl => draft.clear(),
                    KeyCode::Char(c) if !ctrl => draft.insert(c.encode_utf8(&mut [0; 4])),
                    _ => {}
                }
                None
            }
            Focus::Timeline if session.rule_pick(view).is_some() => {
                session.rule_pick_key(view, key.code)
            }
            // In shell mode the run list stands in for the timeline.
            Focus::Timeline if shell => match key.code {
                KeyCode::Esc => Some(Action::Back),
                KeyCode::Char('q') => {
                    self.ui.quit = true;
                    None
                }
                KeyCode::Char('i') | KeyCode::Char('c') => {
                    session.focus = Focus::Composer;
                    None
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    session.select_run(view, true);
                    None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    session.select_run(view, false);
                    None
                }
                KeyCode::Home | KeyCode::Char('g') => {
                    session.pick_run(view.shell.runs.first().map(|r| r.run));
                    None
                }
                KeyCode::End | KeyCode::Char('G') => {
                    session.pick_run(view.shell.runs.last().map(|r| r.run));
                    None
                }
                KeyCode::PageUp | KeyCode::PageDown => {
                    session.page_run(key.code == KeyCode::PageUp);
                    None
                }
                KeyCode::Enter | KeyCode::Char(' ') => session.toggle_run(view),
                KeyCode::Char('d') => session
                    .selected_run(view)?
                    .output
                    .as_ref()
                    .and_then(output_download),
                KeyCode::Char('y') => {
                    let run = session.selected_run(view)?.clone();
                    self.copy_run(&run)
                }
                // Only what the core marks discardable, as in the timeline.
                KeyCode::Char('e') => session
                    .selected_run(view)
                    .filter(|run| run.discardable)
                    .map(|run| Action::EditQueued(run.run)),
                KeyCode::Delete => session
                    .selected_run(view)
                    .filter(|run| run.discardable)
                    .map(|run| Action::Discard(run.run)),
                KeyCode::Char('a') => answer(view, Decision::Approve),
                KeyCode::Char('x') => answer(view, Decision::Deny),
                KeyCode::Char('w') => Some(Action::OpenWorkdir {
                    session_id: view.id,
                }),
                _ => None,
            },
            Focus::Timeline => match key.code {
                // A kept search goes first; the next Esc leaves.
                KeyCode::Esc if session.shown.search.is_some() => {
                    session.shown.search = None;
                    None
                }
                // Back to the list the session is on (the core knows which).
                KeyCode::Esc => Some(Action::Back),
                KeyCode::Char('/') => {
                    session.open_search();
                    None
                }
                KeyCode::Char('n') => {
                    session.jump(view, Some(true));
                    None
                }
                KeyCode::Char('N') => {
                    session.jump(view, Some(false));
                    None
                }
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
                    session.shown.scroll = session.shown.geometry.borrow().pin(0);
                    None
                }
                KeyCode::Char('[') => {
                    session.step_turn(view, true);
                    None
                }
                KeyCode::Char(']') => {
                    session.step_turn(view, false);
                    None
                }
                KeyCode::End | KeyCode::Char('G') => {
                    session.shown.scroll = None;
                    None
                }
                KeyCode::Enter | KeyCode::Char(' ') => session.toggle(view),
                KeyCode::Char('o') => session.open_subagent(view),
                KeyCode::Char('d') => session
                    .selected_entry(view)
                    .and_then(wanted_blob)
                    .map(|blob| Action::DownloadBlob(blob.id)),
                KeyCode::Char(c @ ('y' | 'Y')) => {
                    let item = session.shown.selected.and_then(|key| find(view, key));
                    let item = item.map(|index| &view.timeline[index]);
                    let content = if c == 'y' {
                        item.and_then(main_content)
                    } else {
                        item.and_then(call_content)
                    };
                    self.copy(c, content)
                }
                // Only what the core marks discardable; anything else (it
                // may have reached the server) is left alone.
                KeyCode::Char('e') => {
                    discardable(view, session.shown.selected).map(|(id, _)| Action::EditQueued(id))
                }
                KeyCode::Delete => {
                    discardable(view, session.shown.selected).map(|(id, _)| Action::Discard(id))
                }
                KeyCode::Char('a') => answer(view, Decision::Approve),
                KeyCode::Char('x') => answer(view, Decision::Deny),
                // Shifted, so a stray key does not grant a lasting rule.
                KeyCode::Char('A') => session.allow_always(view),
                KeyCode::Char('v') => {
                    session.reveal_gated(view);
                    None
                }
                KeyCode::Char('w') => Some(Action::OpenWorkdir {
                    session_id: view.id,
                }),
                // Shifted, so a stray key does not end the agent's run.
                // In a subagent's view it stops that subagent's task instead,
                // always asking: it ends one thing, not the run. A foreground
                // subagent has no task to stop; Ctrl-X ends it with the turn.
                KeyCode::Char('S') if view.subagent.is_some() => {
                    let task = view
                        .subagent
                        .as_ref()
                        .and_then(|s| s.status.task.clone().filter(|_| s.status.live));
                    session.picker = task.map(|task| Picker::Stop(StopKind::Task(task)));
                    None
                }
                KeyCode::Char('S') => {
                    if stop_needs_confirmation(&view.work) {
                        session.picker = Some(Picker::Stop(StopKind::Session));
                        None
                    } else {
                        Some(Action::StopSession {
                            session_id: view.id,
                        })
                    }
                }
                KeyCode::Char('B') => {
                    session.picker = Some(Picker::background());
                    None
                }
                KeyCode::Char('p') => {
                    session.picker = Some(Picker::mode(&view.agent));
                    None
                }
                KeyCode::Char('m') => {
                    session.picker = Some(Picker::agent());
                    None
                }
                KeyCode::Char('C') => {
                    session.picker = Some(Picker::contexts(view));
                    None
                }
                KeyCode::Char('R') => {
                    let name = if view.named {
                        view.title.clone()
                    } else {
                        String::new()
                    };
                    self.ui.prompt = Some(Prompt::new(PromptKind::Rename(view.id), name));
                    None
                }
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
        rule: None,
    })
}

impl SessionUi {
    /// Scrolls a page back (`up`) or forward; reaching the bottom follows
    /// again.
    fn page(&mut self, up: bool) {
        let geometry = self.shown.geometry.borrow();
        let page = geometry.height.saturating_sub(1).max(1);
        let top = geometry.top(self.shown.scroll);
        let top = if up {
            top.saturating_sub(page)
        } else {
            top + page
        };
        let scroll = geometry.pin(top);
        drop(geometry);
        self.shown.scroll = scroll;
    }

    /// Moves the selection to the previous (`up`) or next selectable item
    /// and scrolls it into view. Nothing selected starts from the newest.
    fn select(&mut self, view: &SessionView, up: bool) {
        let items: Vec<(usize, ItemKey)> = view
            .timeline
            .iter()
            .enumerate()
            .filter_map(|(i, item)| Some((i, selectable(item)?)))
            .collect();
        let Some(&last) = items.last() else {
            return;
        };
        let current = self
            .shown
            .selected
            .and_then(|key| items.iter().position(|(_, k)| *k == key));
        let (index, key) = match (current, up) {
            (None, _) => last,
            (Some(i), true) => items[i.saturating_sub(1)],
            (Some(i), false) => items[(i + 1).min(items.len() - 1)],
        };
        self.shown.selected = Some(key);
        self.reveal(index);
    }

    /// Keeps the selection on its item as the timeline changes: a pending
    /// message stays selected once logged. If the item is gone, selects the
    /// selectable item at its old index `old`, else the one before, so the
    /// cursor does not jump.
    fn keep_selection(&mut self, view: &SessionView, old: Option<usize>) {
        let Some(key) = self.shown.selected else {
            return;
        };
        let index = find(view, key).or_else(|| {
            let old = old?;
            let after =
                (old..view.timeline.len()).find(|&i| selectable(&view.timeline[i]).is_some());
            after.or_else(|| (0..old).rfind(|&i| selectable(&view.timeline[i]).is_some()))
        });
        self.shown.selected = index.and_then(|i| selectable(&view.timeline[i]));
    }

    /// Keeps the scroll anchor on its item as the timeline changes: on a
    /// pending message, it moves to the logged message that message became,
    /// at the same offset, so the view stays put. An anchor whose item is
    /// gone stays, for the renderer to fall back from.
    fn keep_scroll(&mut self, view: &SessionView) {
        if let Some(anchor) = &mut self.shown.scroll
            && let Some(index) = find(view, anchor.key)
        {
            anchor.key = ItemKey::of(&view.timeline[index]);
        }
    }

    /// Scrolls so the timeline item at `index` is on screen, its top first
    /// if it does not fit.
    fn reveal(&mut self, index: usize) {
        let geometry = self.shown.geometry.borrow();
        let Some((start, end)) = geometry.item(index) else {
            return;
        };
        let top = geometry.top(self.shown.scroll);
        let top = if start < top || end - start > geometry.height {
            start
        } else if end > top + geometry.height {
            end - geometry.height
        } else {
            return;
        };
        let scroll = geometry.pin(top);
        drop(geometry);
        self.shown.scroll = scroll;
    }

    /// Moves to the start of the previous (`back`) or next turn: its first
    /// line goes to the top and its first selectable item is selected.
    /// Steps from the selected item, else from the top of the screen.
    fn step_turn(&mut self, view: &SessionView, back: bool) {
        let geometry = self.shown.geometry.borrow();
        let current = self
            .shown
            .selected
            .and_then(|key| {
                view.timeline
                    .iter()
                    .position(|item| selectable(item) == Some(key))
            })
            .or_else(|| geometry.index_at(geometry.top))
            .unwrap_or(0);
        // First item of each turn; what comes before the first message is
        // a turn too.
        let mut turns: Vec<usize> = view
            .timeline
            .iter()
            .enumerate()
            .filter(|(i, item)| *i == 0 || starts_turn(item))
            .map(|(i, _)| i)
            .collect();
        turns.push(view.timeline.len());
        let Some(here) = turns.iter().rposition(|&start| start <= current) else {
            return;
        };
        let first_selectable = |turn: usize| {
            (turns[turn]..turns[turn + 1]).find(|&i| selectable(&view.timeline[i]).is_some())
        };
        // Back from inside a turn goes to its start, from its start to the
        // turn before.
        let target = if back {
            match first_selectable(here) {
                Some(first) if first < current => Some(here),
                _ => here.checked_sub(1),
            }
        } else {
            (here + 2 < turns.len()).then_some(here + 1)
        };
        let Some(target) = target else {
            return;
        };
        let Some(&line) = geometry.starts.get(turns[target]) else {
            return;
        };
        let scroll = geometry.pin(line);
        drop(geometry);
        self.shown.scroll = scroll;
        if let Some(first) = first_selectable(target) {
            self.shown.selected = selectable(&view.timeline[first]);
        }
    }

    /// Selects and expands the item the pending approval gates and scrolls
    /// its top into view; if it then fits at the bottom, the timeline keeps
    /// following. Downloads nothing: what is not here says so.
    fn reveal_gated(&mut self, view: &SessionView) {
        let Some((_, item)) = gated_item(view) else {
            return;
        };
        let Some(key @ ItemKey::Logged(seq)) = selectable(item) else {
            return;
        };
        self.shown.selected = Some(key);
        self.shown.expanded.insert(seq);
        // Anchored to the item, not to a line: it grows by expanding, and the
        // renderer clamps the anchor to the bottom.
        self.shown.scroll = Some(Anchor {
            key: ItemKey::of(item),
            offset: 0,
        });
        self.shown.reveal = Some(ItemKey::of(item));
    }

    /// Opens an empty search prompt; `n`/`N` then work on the timeline.
    fn open_search(&mut self) {
        self.shown.search = Some(Search::new());
        self.focus = Focus::Timeline;
    }

    /// Keys while the search prompt is open: matches follow every edit.
    /// Enter keeps the search and jumps to the nearest match; with nothing
    /// typed it closes the prompt, like Esc.
    fn search_key(&mut self, view: &SessionView, key: KeyEvent, ctrl: bool) {
        let Some(search) = &mut self.shown.search else {
            return;
        };
        match key.code {
            KeyCode::Esc => self.shown.search = None,
            KeyCode::Enter if search.query.is_empty() => self.shown.search = None,
            KeyCode::Enter => {
                search.editing = false;
                self.jump(view, None);
            }
            KeyCode::Backspace => search.edit(view, |query| {
                query.pop();
            }),
            KeyCode::Char('u') if ctrl => search.edit(view, String::clear),
            KeyCode::Char(c) if !ctrl => search.edit(view, |query| query.push(c)),
            _ => {}
        }
    }

    /// Jumps to the next older (`Some(true)`) or newer match from the
    /// current one, round the ends of the timeline; with no current match,
    /// or `None`, to the nearest one at or above (newer: below) where the
    /// user is. The match is selected if it can be, expanded if it shows
    /// only expanded, and scrolled into view: to its line, for a match in
    /// a blob's content.
    fn jump(&mut self, view: &SessionView, older: Option<bool>) {
        let from = self.search_from(view);
        let Some(search) = &mut self.shown.search else {
            return;
        };
        let Some(jump) = search.search.jump(view, older, from) else {
            return;
        };
        let hidden = jump.expand.map(|seq| self.shown.expanded.insert(seq));
        // A match deep in a blob is scrolled to by the next frame, which
        // lays its line out.
        search.pin = hidden.is_some() && jump.in_blob;
        // Items that cannot be selected (an agent error) only scroll.
        if let Some(key) = selectable(&view.timeline[jump.index]) {
            self.shown.selected = Some(key);
        }
        if hidden == Some(true) || (hidden.is_some() && jump.in_blob) {
            // It grows by expanding: anchored to its top, as `v` does,
            // until the match's line is laid out.
            self.shown.scroll = Some(Anchor {
                key: jump.key,
                offset: 0,
            });
        } else {
            self.reveal(jump.index);
        }
    }

    /// Where a search starts from: the selected item, else the last item
    /// on screen, so matches already in view come first.
    fn search_from(&self, view: &SessionView) -> usize {
        self.shown
            .selected
            .and_then(|key| find(view, key))
            .or_else(|| {
                let geometry = self.shown.geometry.borrow();
                let bottom = geometry.top(self.shown.scroll) + geometry.height;
                geometry.index_at(bottom.min(geometry.total).checked_sub(1)?)
            })
            .unwrap_or(view.timeline.len().saturating_sub(1))
    }

    /// The selected logged item's entry; a pending message has none.
    pub(crate) fn selected_entry<'a>(&self, view: &'a SessionView) -> Option<&'a Entry> {
        let Some(ItemKey::Logged(seq)) = self.shown.selected else {
            return None;
        };
        view.timeline.iter().find_map(|item| match item {
            TimelineItem::Logged { seq: s, entry, .. } if *s == seq => Some(entry),
            _ => None,
        })
    }

    /// Opens the subagent the selected `Agent` call runs.
    pub(crate) fn open_subagent(&self, view: &SessionView) -> Option<Action> {
        match self.selected_entry(view)? {
            Entry::Tool(tool) if tool.subagent.is_some() => Some(Action::ViewSubagent {
                session_id: view.id,
                call: Some(tool.call),
            }),
            _ => None,
        }
    }

    /// Expands or collapses the selected item. Expanding a blob that is not
    /// here yet asks for it, again if it was asked for before; one that
    /// arrives while the item is open is asked for by
    /// [`App::expanded_fetches`].
    fn toggle(&mut self, view: &SessionView) -> Option<Action> {
        let Some(ItemKey::Logged(seq)) = self.shown.selected else {
            return None;
        };
        if self.shown.expanded.remove(&seq) {
            return None;
        }
        let entry = self.selected_entry(view)?;
        if sections(entry).is_empty() {
            return None;
        }
        self.shown.expanded.insert(seq);
        self.shown.fetch.expand(entry)
    }
}
