//! Head-side state: the latest [`ViewState`] from the core plus presentation
//! state the core does not care about (form buffers, focus, the help
//! overlay, list selection). Keys become core [`Action`]s here; nothing in
//! this module decides anything about pairing, sync or delivery (D2).

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::ops::Deref;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use skywave_core::search::BlobText;
use skywave_core::{
    Action, AgentView, ApprovalId, BlobPolicy, BlobState, Completion, ContextId, CoreError,
    Decision, Entry, Link, LinkRejection, ListKind, PairUri, PairingView, PermissionMode, Route,
    Screen, Seq, SessionId, SessionListView, SessionRow, SessionView, TaskId, TimelineItem,
    ToolOutcome, ViewState, parse_pair_uri,
};
use skywave_present::format::{
    BackgroundRow, StopAction, background_rows, bytes, mode_needs_confirmation, move_refusal,
};
use skywave_present::policy::{
    AlertWatcher, CompletionSettle, DraftSaver, PromptHistory, ReadMarker, Recall, TimelineSearch,
    looks_like_pair_uri, stop_needs_confirmation, unanswered,
};
use skywave_present::sections::{
    Content, ItemKey, call_content, discardable, find, gated_item, main_content, sections,
    selectable, starts_turn, wanted_blob,
};

use crate::clipboard::{Clip, CopyError};
use crate::composer::Composer;
use crate::notify;

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
    /// A new name for the session, on the session list or the session
    /// itself; blank restores the default title (the core decides).
    Rename(SessionId),
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
}

impl Prompt {
    fn new(kind: PromptKind, text: String) -> Prompt {
        Prompt {
            kind,
            text,
            submitted: false,
            completion: CompletionSettle::new(),
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
    /// on the session list, a rename there or on its session.
    fn fits(&self, screen: &Screen) -> bool {
        match (self.kind, screen) {
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
    /// Where the user scrolled back to; `None` follows the newest item. New
    /// items never move a pinned view.
    pub scroll: Option<Anchor>,
    /// The selected item, by key so it stays put as items arrive.
    pub selected: Option<ItemKey>,
    /// Items whose full content is shown; only logged items expand.
    pub expanded: HashSet<Seq>,
    pub geometry: RefCell<Geometry>,
    /// The message being written. Filled from the core's draft (D5: per
    /// device) when the session opens, and again only when the core
    /// rewrites the draft itself (`draft_rev`, e.g. a queued message taken
    /// back to edit); the core hears of it through [`App::draft_save`].
    pub composer: Composer,
    /// When the composer's edits are saved as the core's draft.
    saver: DraftSaver,
    /// Earlier prompts being recalled into the composer; any edit ends it.
    history: PromptHistory<Composer>,
    /// The session opened with news and where to scroll is not settled: it
    /// needs the first frame's geometry ([`Geometry::opening`]). Until then
    /// the renderer shows that scroll state itself and nothing is read.
    pub opening: bool,
    /// When what is seen is marked read ([`App::read_mark`]).
    marker: ReadMarker,
    /// The search through the timeline (`/`), while one is open or kept.
    pub search: Option<Search>,
    /// The item `v` revealed, scrolled to its top, while that waits for a
    /// frame to lay it out expanded: if it then fits at the bottom, the
    /// timeline keeps following ([`Geometry::follow_at_bottom`]).
    pub reveal: Option<ItemKey>,
    /// The list of rules `A` opened in the approval prompt; see
    /// [`SessionUi::rule_pick`].
    rule_pick: Option<RulePick>,
    /// The mode or model list `p`/`m` opened, while it is up.
    pub picker: Option<Picker>,
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
    /// The modes, [`PermissionMode::ALL`] in order. `confirm` once Enter
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopKind {
    /// Stop agent.
    Session,
    /// A new context.
    Clear,
    /// Switch to this context.
    Switch(ContextId),
    /// Move this context, the current one, to a session of its own.
    Move(ContextId),
}

impl StopKind {
    /// Which action this is, for the wording shared with the other head.
    pub fn stop_action(self) -> StopAction {
        match self {
            StopKind::Session => StopAction::Agent,
            StopKind::Clear => StopAction::NewContext,
            StopKind::Switch(_) => StopAction::SwitchContext,
            StopKind::Move(_) => StopAction::MoveContext,
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
            .and_then(|mode| PermissionMode::ALL.iter().position(|m| *m == mode))
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
                    Some(Action::SetPermissionMode {
                        session_id,
                        mode: PermissionMode::ALL[*selected],
                    }),
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
                    *selected = (*selected + 1).min(PermissionMode::ALL.len() - 1);
                    (false, None)
                }
                KeyCode::Enter => {
                    let mode = PermissionMode::ALL[*selected];
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
                KeyCode::Char('y') | KeyCode::Char('Y') => (true, Some(kind.action(session_id))),
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
            scroll: None,
            selected: None,
            expanded: HashSet::new(),
            geometry: RefCell::default(),
            composer: composer_of(view),
            saver: DraftSaver::new(view),
            history: PromptHistory::new(),
            opening: view.unread_from.is_some(),
            marker: ReadMarker::new(view.id),
            search: None,
            reveal: None,
            rule_pick: None,
            picker: None,
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
        if self.opening {
            self.geometry.borrow().opening()
        } else {
            self.pinned_scroll()
                .unwrap_or_else(|| self.revealed_scroll())
        }
    }

    /// The scroll state, settled to following if the last frame laid out
    /// the item `v` revealed and it fits at the bottom.
    fn revealed_scroll(&self) -> Option<Anchor> {
        let geometry = self.geometry.borrow();
        if self.reveal.is_some() && geometry.reveal == self.reveal {
            geometry.follow_at_bottom(self.scroll)
        } else {
            self.scroll
        }
    }

    /// While [`Search::pin`]: the scroll state that shows the current
    /// match's line in its blob, if the last frame laid it out.
    pub fn pinned_scroll(&self) -> Option<Option<Anchor>> {
        let search = self.search.as_ref().filter(|s| s.pin)?;
        self.geometry.borrow().match_scroll(search.current?)
    }

    /// Applies the opening scroll, or the one showing a search match in a
    /// blob, once a frame has laid the timeline out.
    fn settle(&mut self) {
        if self.geometry.borrow().height == 0 {
            return;
        }
        if self.opening {
            self.scroll = self.geometry.borrow().opening();
            self.opening = false;
        }
        if self.search.as_ref().is_some_and(|s| s.pin) {
            // Not laid out (cut by the file's end, say): the item's top.
            if let Some(scroll) = self.pinned_scroll() {
                self.scroll = scroll;
            }
            if let Some(search) = &mut self.search {
                search.pin = false;
            }
        }
        if self.reveal.is_some() && self.geometry.borrow().reveal == self.reveal {
            self.scroll = self.revealed_scroll();
            self.reveal = None;
            self.geometry.borrow_mut().reveal = None;
        }
    }

    /// The newest item is on screen and the opening scroll settled: what
    /// reaches the session is seen.
    fn following(&self) -> bool {
        !self.opening && self.scroll.is_none()
    }

    /// While browsing earlier prompts: the shown one's place, 1 the newest,
    /// and how many there are.
    pub fn history_position(&self) -> Option<(usize, usize)> {
        self.history.position()
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
    fn recall(&mut self, view: &SessionView, older: bool) {
        if older {
            let composer = &self.composer;
            if let Some(prompt) = self.history.older(view, || composer.clone()) {
                self.composer.replace(prompt);
            }
            return;
        }
        match self.history.newer() {
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
            self.history.end();
            self.focus = Focus::Composer;
        }
    }
}

fn composer_of(view: &SessionView) -> Composer {
    view.draft
        .as_ref()
        .map(|d| Composer::from_draft(d.text.clone(), d.cursor))
        .unwrap_or_default()
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
    /// `D` on the archived list asks whether to delete this session from
    /// this device; `y` does, any other key does not.
    pub delete: Option<SessionId>,
    /// The open session's focus, scroll, expanded items and composer.
    pub session: Option<SessionUi>,
    /// The user chose to browse local data behind the revoked screen.
    pub revoked_dismissed: bool,
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
    /// An alert that new approvals came in, for the event loop to send
    /// ([`App::take_alert`]): the newest one's session title.
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
                delete: None,
                session: None,
                revoked_dismissed: false,
                error: None,
                notice: None,
                copy: None,
                alerts: AlertWatcher::new(&view.attention),
                alert: None,
                jumped: false,
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

    /// Takes a new snapshot from the core.
    pub fn set_view(&mut self, view: ViewState) {
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
        if self
            .ui
            .prompt
            .as_ref()
            .is_some_and(|p| !p.fits(&view.screen))
        {
            self.ui.prompt = None;
        }
        if let Some(alert) = self.ui.alerts.observe(&view.attention) {
            self.ui.notice = Some(alert.notice());
            self.ui.alert = Some(alert.title.unwrap_or_default());
        }
        let old_index = self
            .session_list()
            .and_then(|list| self.selected_index(list));
        let old_item = self
            .session()
            .and_then(|(view, ui)| find(view, ui.selected?));
        let old_match = self
            .session()
            .and_then(|(view, ui)| find(view, ui.search.as_ref()?.current?));
        self.view = view;
        if let (Some(create), Screen::SessionList(list)) = (&mut self.ui.prompt, &self.view.screen)
            && create.kind == PromptKind::Create
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
        if let (Screen::Session(view), Some(session)) = (&self.view.screen, &mut self.ui.session) {
            session.keep_selection(view, old_item);
            session.keep_scroll(view);
            session.follow_draft(view);
            if let Some(search) = &mut session.search {
                search.refresh(view, old_match);
            }
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

    /// The screen shows a clock that runs: a tool call's elapsed time.
    pub fn live(&self) -> bool {
        self.session().is_some_and(|(view, _)| {
            view.timeline.iter().any(|item| {
                matches!(
                    item,
                    TimelineItem::Logged { entry: Entry::Tool(tool), .. }
                        if matches!(tool.outcome, ToolOutcome::Running { .. })
                )
            })
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

    /// The alert [`App::set_view`] asked for, if any: the session title to
    /// name, maybe empty.
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
        let (size, action) = match blob.state {
            BlobState::Available { path } => {
                self.ui.copy = Some(Clip::File { path, strip });
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
        session
            .saver
            .save(now, force, composer.text(), composer.cursor_chars())
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
        let following = session.following();
        session.marker.mark(now, view.last_seq, following, force)
    }

    /// How long from `now` until [`App::read_mark`] has something to mark
    /// unforced; `None` while there is nothing to mark.
    pub fn read_mark_due_in(&self, now: Instant) -> Option<Duration> {
        let (view, ui) = self.session()?;
        if ui.opening {
            // Settles as soon as a frame was drawn.
            return (ui.geometry.borrow().height > 0).then_some(Duration::ZERO);
        }
        let wake = ui.marker.next_wake(now, view.last_seq, ui.following())?;
        Some(wake.saturating_duration_since(now))
    }

    /// Records the outcome of dispatching a [`App::read_mark`] action; only
    /// an error matters.
    pub fn read_marked(&mut self, result: Result<(), CoreError>) {
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

    /// The revoked full-screen state covers the current screen.
    pub fn revoked_screen(&self) -> bool {
        matches!(self.view.link, Link::Rejected(LinkRejection::Revoked))
            && !matches!(self.view.screen, Screen::Pairing(_))
            && !self.ui.revoked_dismissed
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
        !self.revoked_screen()
            && match self.view.screen {
                Screen::Pairing(_) => true,
                Screen::SessionList(_) => self.ui.prompt.is_some(),
                Screen::Session(_) if self.ui.prompt.is_some() => true,
                Screen::Session(_) => self.ui.session.as_ref().is_some_and(|s| match &s.picker {
                    Some(picker) => matches!(picker, Picker::Model { .. }),
                    None => {
                        s.focus == Focus::Composer || s.search.as_ref().is_some_and(|s| s.editing)
                    }
                }),
                _ => false,
            }
    }

    /// Bracketed paste: on the pairing screen a pairing URI fills the whole
    /// form, a malformed one is reported, anything else goes into the
    /// focused field; an open prompt
    /// (new session, rename) takes it; on a session it goes into the
    /// search prompt while that is open, else into the composer, line
    /// breaks included.
    pub fn on_paste(&mut self, text: &str) {
        if self.ui.help || self.revoked_screen() {
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
            && let Some(search) = session.search.as_mut().filter(|s| s.editing)
        {
            search.edit(view, |query| {
                query.extend(text.chars().map(|c| if c == '\n' { ' ' } else { c }));
                query.retain(|c| !c.is_control());
            });
            return;
        }
        if let Some(session) = &mut self.ui.session {
            session.focus = Focus::Composer;
            session.history.end();
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
            if self.revoked_screen() || self.view.attention.elsewhere == 0 {
                return None;
            }
            self.ui.jumped = true;
            return Some(Action::OpenNextApproval);
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
        match self.ui.prompt.as_ref().map(|p| p.kind) {
            Some(PromptKind::Create) => return self.create_key(key, ctrl),
            Some(PromptKind::Rename(id)) => return self.rename_key(id, key, ctrl),
            None => {}
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
                KeyCode::Delete => dismissable(list).map(|p| Action::Discard(p.command_id)),
                _ => None,
            },
            Screen::Session(_) => self.session_key(key, ctrl),
            Screen::Settings(settings) => match key.code {
                KeyCode::Char('q') => self.quit(),
                KeyCode::Esc => Some(Action::Back),
                KeyCode::Char('b') => Some(Action::SetBlobPolicy(match settings.blob_policy {
                    BlobPolicy::Eager => BlobPolicy::OnDemand,
                    BlobPolicy::OnDemand => BlobPolicy::Eager,
                })),
                KeyCode::Char('p') => Some(Action::Navigate(Route::Pairing)),
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
            KeyCode::Enter => {
                create.submitted = true;
                return Some(Action::CreateSession {
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

    /// Keys on the session view. An open search prompt takes every key;
    /// otherwise Ctrl-F, Ctrl-X, PgUp/PgDn and Tab work in both focuses and
    /// the rest depends on [`Focus`].
    fn session_key(&mut self, key: KeyEvent, ctrl: bool) -> Option<Action> {
        let Screen::Session(view) = &self.view.screen else {
            return None;
        };
        let session = self.ui.session.as_mut()?;
        // Keys move from where the opening scrolled to.
        session.settle();
        // Not laid out yet: the revealed item stays at the top.
        session.reveal = None;
        if let Some(picker) = &mut session.picker {
            let (close, action) = picker.key(view, key, ctrl);
            if close {
                session.picker = None;
            }
            return action;
        }
        if let Some(search) = &mut session.search {
            search.search.wrapped = false;
            if search.editing {
                session.search_key(view, key, ctrl);
                return None;
            }
        }
        match key.code {
            KeyCode::Char('f') if ctrl => {
                session.open_search();
                return None;
            }
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
                session.history.end();
                let draft = &mut session.composer;
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
            Focus::Timeline => match key.code {
                // A kept search goes first; the next Esc leaves.
                KeyCode::Esc if session.search.is_some() => {
                    session.search = None;
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
                    session.scroll = session.geometry.borrow().pin(0);
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
                    session.scroll = None;
                    None
                }
                KeyCode::Enter | KeyCode::Char(' ') => session.toggle(view),
                KeyCode::Char('d') => session
                    .selected_entry(view)
                    .and_then(wanted_blob)
                    .map(|blob| Action::DownloadBlob(blob.id)),
                KeyCode::Char(c @ ('y' | 'Y')) => {
                    let item = session.selected.and_then(|key| find(view, key));
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
                    discardable(view, session.selected).map(|(id, _)| Action::EditQueued(id))
                }
                KeyCode::Delete => {
                    discardable(view, session.selected).map(|(id, _)| Action::Discard(id))
                }
                KeyCode::Char('a') => answer(view, Decision::Approve),
                KeyCode::Char('x') => answer(view, Decision::Deny),
                // Shifted, so a stray key does not grant a lasting rule.
                KeyCode::Char('A') => session.allow_always(view),
                KeyCode::Char('v') => {
                    session.reveal_gated(view);
                    None
                }
                // Shifted, so a stray key does not end the agent's run.
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
        let geometry = self.geometry.borrow();
        let page = geometry.height.saturating_sub(1).max(1);
        let top = geometry.top(self.scroll);
        let top = if up {
            top.saturating_sub(page)
        } else {
            top + page
        };
        let scroll = geometry.pin(top);
        drop(geometry);
        self.scroll = scroll;
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
            .selected
            .and_then(|key| items.iter().position(|(_, k)| *k == key));
        let (index, key) = match (current, up) {
            (None, _) => last,
            (Some(i), true) => items[i.saturating_sub(1)],
            (Some(i), false) => items[(i + 1).min(items.len() - 1)],
        };
        self.selected = Some(key);
        self.reveal(index);
    }

    /// Keeps the selection on its item as the timeline changes: a pending
    /// message stays selected once logged. If the item is gone, selects the
    /// selectable item at its old index `old`, else the one before, so the
    /// cursor does not jump.
    fn keep_selection(&mut self, view: &SessionView, old: Option<usize>) {
        let Some(key) = self.selected else {
            return;
        };
        let index = find(view, key).or_else(|| {
            let old = old?;
            let after =
                (old..view.timeline.len()).find(|&i| selectable(&view.timeline[i]).is_some());
            after.or_else(|| (0..old).rfind(|&i| selectable(&view.timeline[i]).is_some()))
        });
        self.selected = index.and_then(|i| selectable(&view.timeline[i]));
    }

    /// Keeps the scroll anchor on its item as the timeline changes: on a
    /// pending message, it moves to the logged message that message became,
    /// at the same offset, so the view stays put. An anchor whose item is
    /// gone stays, for the renderer to fall back from.
    fn keep_scroll(&mut self, view: &SessionView) {
        if let Some(anchor) = &mut self.scroll
            && let Some(index) = find(view, anchor.key)
        {
            anchor.key = ItemKey::of(&view.timeline[index]);
        }
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
        let scroll = geometry.pin(top);
        drop(geometry);
        self.scroll = scroll;
    }

    /// Moves to the start of the previous (`back`) or next turn: its first
    /// line goes to the top and its first selectable item is selected.
    /// Steps from the selected item, else from the top of the screen.
    fn step_turn(&mut self, view: &SessionView, back: bool) {
        let geometry = self.geometry.borrow();
        let current = self
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
        self.scroll = scroll;
        if let Some(first) = first_selectable(target) {
            self.selected = selectable(&view.timeline[first]);
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
        self.selected = Some(key);
        self.expanded.insert(seq);
        // Anchored to the item, not to a line: it grows by expanding, and the
        // renderer clamps the anchor to the bottom.
        self.scroll = Some(Anchor {
            key: ItemKey::of(item),
            offset: 0,
        });
        self.reveal = Some(ItemKey::of(item));
    }

    /// Opens an empty search prompt; `n`/`N` then work on the timeline.
    fn open_search(&mut self) {
        self.search = Some(Search::new());
        self.focus = Focus::Timeline;
    }

    /// Keys while the search prompt is open: matches follow every edit.
    /// Enter keeps the search and jumps to the nearest match; with nothing
    /// typed it closes the prompt, like Esc.
    fn search_key(&mut self, view: &SessionView, key: KeyEvent, ctrl: bool) {
        let Some(search) = &mut self.search else {
            return;
        };
        match key.code {
            KeyCode::Esc => self.search = None,
            KeyCode::Enter if search.query.is_empty() => self.search = None,
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
        let Some(search) = &mut self.search else {
            return;
        };
        let Some(jump) = search.search.jump(view, older, from) else {
            return;
        };
        let hidden = jump.expand.map(|seq| self.expanded.insert(seq));
        // A match deep in a blob is scrolled to by the next frame, which
        // lays its line out.
        search.pin = hidden.is_some() && jump.in_blob;
        // Items that cannot be selected (an agent error) only scroll.
        if let Some(key) = selectable(&view.timeline[jump.index]) {
            self.selected = Some(key);
        }
        if hidden == Some(true) || (hidden.is_some() && jump.in_blob) {
            // It grows by expanding: anchored to its top, as `v` does,
            // until the match's line is laid out.
            self.scroll = Some(Anchor {
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
        self.selected
            .and_then(|key| find(view, key))
            .or_else(|| {
                let geometry = self.geometry.borrow();
                let bottom = geometry.top(self.scroll) + geometry.height;
                geometry.index_at(bottom.min(geometry.total).checked_sub(1)?)
            })
            .unwrap_or(view.timeline.len().saturating_sub(1))
    }

    /// The selected logged item's entry; a pending message has none.
    pub(crate) fn selected_entry<'a>(&self, view: &'a SessionView) -> Option<&'a Entry> {
        let Some(ItemKey::Logged(seq)) = self.selected else {
            return None;
        };
        view.timeline.iter().find_map(|item| match item {
            TimelineItem::Logged { seq: s, entry, .. } if *s == seq => Some(entry),
            _ => None,
        })
    }

    /// Expands or collapses the selected item. Expanding a blob that is not
    /// here yet asks for it (spec §8: expanding downloads if needed).
    fn toggle(&mut self, view: &SessionView) -> Option<Action> {
        let Some(ItemKey::Logged(seq)) = self.selected else {
            return None;
        };
        if self.expanded.remove(&seq) {
            return None;
        }
        let entry = self.selected_entry(view)?;
        if sections(entry).is_empty() {
            return None;
        }
        self.expanded.insert(seq);
        let blob = wanted_blob(entry)?;
        matches!(blob.state, BlobState::NotDownloaded { .. })
            .then_some(Action::DownloadBlob(blob.id))
    }
}
