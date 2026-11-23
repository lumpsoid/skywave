//! Per-session summaries derived from the event log.
//!
//! The session list reads these instead of replaying every event. The fold is
//! pure: the store persists its result in the same transaction that stores
//! the events, so the projection never runs ahead of or behind the log.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use skywave_proto::{
    AgentSettings, AgentWork, ApprovalId, BlobId, BlobRef, ContextId, DeviceId, Event, EventKind,
    ImportedItem, LineSpan, Seq, SessionId, SessionOrigin, SessionState, TurnUsage, Worktree,
};

/// What the session list needs to know about one session.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSummary {
    pub id: SessionId,
    /// Server directory the session runs in (D14). `None` only if the
    /// session's events arrived without its `SessionCreated`, which a
    /// contiguous log replayed from seq 1 never does.
    pub workdir: Option<String>,
    /// Last state the server recorded for the agent.
    pub state: SessionState,
    /// Approvals requested and not yet resolved or cancelled, oldest first.
    pub pending_approvals: Vec<ApprovalId>,
    /// Time of the session's newest event.
    pub last_activity: DateTime<Utc>,
    /// Seq of the session's newest event.
    pub last_seq: Seq,
    /// Seq of the session's newest `UserMessage`, else of its first seen
    /// event: the session list's order. The agent's own activity does not
    /// move it, so a session moves up only when someone writes to it.
    pub last_message_seq: Seq,
    /// The agent was stopped on request and no message has come since.
    pub stopped: bool,
    /// How many of the session's events are [notable](is_notable): what
    /// unread counts subtract a read mark from.
    pub notable: u64,
    /// The name the newest `SessionRenamed` gave it; `None` for the default
    /// title.
    pub name: Option<String>,
    /// First line of the session's first message, at most
    /// [`FIRST_PROMPT_MAX_CHARS`]; the default title's first choice.
    pub first_prompt: Option<String>,
    /// Hidden from the active list, as the log's last
    /// [`archived_effect`](EventKind::archived_effect) left it: archived by
    /// `SessionArchived`, back with the next message.
    pub archived: bool,
    /// What the session's turns took, as their agent reported it; `None`
    /// until a turn reports anything.
    pub usage: Option<SessionUsage>,
    /// The agent's permission mode and model, folded with
    /// [`AgentSettings::apply`].
    pub agent: AgentSettings,
    /// The agent's background tasks and schedules, folded with
    /// [`AgentWork::apply`]: each set as its last event left it.
    pub work: AgentWork,
    /// The session's current context: its first (which has the session's
    /// id) until a `ContextStarted` or `ContextSwitched` names another.
    pub context: ContextId,
    /// The session adopted an outside conversation (`SessionCreated` from
    /// `SessionOrigin::Agent`) and its `ImportFinished` is not in the
    /// replica yet: an import too long for one pull is still arriving.
    pub importing: bool,
    /// The agent's id of the outside conversation the session adopted (its
    /// `SessionCreated` came from `SessionOrigin::Agent`): what the
    /// conversation picker's `continued_in` looks up.
    pub adopted: Option<String>,
    /// The git worktree the session runs in, from its `SessionCreated`.
    pub worktree: Option<Worktree>,
    /// The session's worktree was removed (`WorktreeRemoved`); its work is refused.
    pub worktree_removed: bool,
    /// Skywave's safety check is on: the newest `SafetyCheckSet`.
    pub safety_check: bool,
}

/// What a session's turns took: the newest turn's figures and the sums over
/// every turn that reported any. Agent-agnostic (D3); folded from
/// `TurnUsage` events, which hold per-turn figures, so this only adds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionUsage {
    /// The newest turn that reported usage, as reported.
    pub last_turn: TurnUsage,
    /// Each figure summed over the turns that reported it; `None` while no
    /// turn has. `model` is the newest one named. The context figures are
    /// never summed and stay `None` here.
    pub total: TurnUsage,
    /// How full the current context is, as of the newest turn's end that
    /// reported a fill (D46). `None` (unknown) in a context no turn has
    /// reported a fill for, and after a turn that compacted it, until a turn
    /// reports one.
    #[serde(default)]
    pub context: Option<ContextFill>,
    /// Each context's last known fill, by the turns stamped with it: what
    /// `context` shows again once the session switches back to one.
    #[serde(default)]
    pub fills: BTreeMap<ContextId, ContextFill>,
}

/// How full a context is: what its next request will read, of what its
/// model holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextFill {
    pub tokens: u64,
    /// `None` when the agent did not say.
    pub window: Option<u64>,
}

impl ContextFill {
    /// The fill `turn` reported, if any.
    fn of(turn: &TurnUsage) -> Option<ContextFill> {
        Some(ContextFill {
            tokens: turn.context_tokens?,
            window: turn.context_window,
        })
    }
}

impl SessionUsage {
    /// Usage after a first `turn`, which ran in `context`.
    pub fn new(context: ContextId, turn: &TurnUsage) -> Self {
        let mut usage = SessionUsage {
            last_turn: turn.clone(),
            total: TurnUsage {
                context_tokens: None,
                context_window: None,
                context_compacted: false,
                ..turn.clone()
            },
            context: None,
            fills: BTreeMap::new(),
        };
        usage.note_fill(context, turn);
        usage
    }

    /// Folds in the next `turn`, which ran in `context`.
    pub fn add(&mut self, context: ContextId, turn: &TurnUsage) {
        fn sum<T: std::ops::Add<Output = T> + Copy>(
            total: Option<T>,
            turn: Option<T>,
        ) -> Option<T> {
            match (total, turn) {
                (Some(total), Some(turn)) => Some(total + turn),
                (total, turn) => total.or(turn),
            }
        }
        self.last_turn = turn.clone();
        self.note_fill(context, turn);
        let total = &mut self.total;
        total.duration_ms = sum(total.duration_ms, turn.duration_ms);
        total.tokens_in = sum(total.tokens_in, turn.tokens_in);
        total.tokens_out = sum(total.tokens_out, turn.tokens_out);
        total.cost_usd = sum(total.cost_usd, turn.cost_usd);
        if turn.model.is_some() {
            total.model.clone_from(&turn.model);
        }
    }

    /// The session made `context` current: its last known fill, if any.
    pub fn switch_to(&mut self, context: ContextId) {
        self.context = self.fills.get(&context).copied();
    }

    /// Applies the fill of a turn that ran in `context`. A reported fill is
    /// the context's last known one. A turn that compacted the context
    /// reports none and leaves it unknown; any other keeps the last known.
    fn note_fill(&mut self, context: ContextId, turn: &TurnUsage) {
        if let Some(fill) = ContextFill::of(turn) {
            self.fills.insert(context, fill);
            self.context = Some(fill);
        } else if turn.context_compacted {
            self.fills.remove(&context);
            self.context = None;
        }
    }
}

/// One turn's usage as its `TurnUsage` event logged it: what the usage
/// screen totals across sessions. The store keeps these beside the log, so
/// totals need no replay.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnRecord {
    /// Seq of the `TurnUsage` event.
    pub seq: Seq,
    pub session_id: SessionId,
    /// When the server logged it.
    pub at: DateTime<Utc>,
    pub usage: TurnUsage,
}

impl TurnRecord {
    /// The record of `event` at `seq`, if it reports a turn's usage.
    pub fn of(seq: Seq, event: &Event) -> Option<TurnRecord> {
        match &event.kind {
            EventKind::TurnUsage(usage) => Some(TurnRecord {
                seq,
                session_id: event.session_id,
                at: event.at,
                usage: usage.clone(),
            }),
            _ => None,
        }
    }
}

/// Longest [`SessionSummary::first_prompt`] kept, in chars. The store's
/// backfill migration cuts at the same length, and the server cuts a shell
/// run's `command_line` to it.
pub const FIRST_PROMPT_MAX_CHARS: usize = skywave_proto::limits::FIRST_PROMPT_MAX_CHARS;

impl SessionSummary {
    /// Summary of a session whose first seen event is `event` at `seq`.
    pub fn new(seq: Seq, event: &Event) -> Self {
        let mut summary = SessionSummary {
            id: event.session_id,
            workdir: None,
            state: SessionState::Idle,
            pending_approvals: Vec::new(),
            last_activity: event.at,
            last_seq: seq,
            last_message_seq: seq,
            stopped: false,
            notable: 0,
            name: None,
            first_prompt: None,
            archived: false,
            usage: None,
            agent: AgentSettings::default(),
            work: AgentWork::default(),
            context: ContextId::first_of(event.session_id),
            importing: false,
            adopted: None,
            worktree: None,
            worktree_removed: false,
            safety_check: false,
        };
        summary.apply(seq, event);
        summary
    }

    /// Folds one event of this session into the summary.
    ///
    /// A rename only sets the name, an archive only the archived flag, a
    /// mode or model event only the agent settings, a change of background
    /// tasks or schedules only the agent's work, a context cleared or
    /// switched only the current context, a safety check switch only its
    /// flag, and the models an agent offers or a context moved away nothing
    /// here, imported history only an unset `first_prompt`, a deleted or
    /// cropped blob nothing here: none is activity, so they move neither
    /// `last_seq` nor `last_activity`. Only a `UserMessage` moves
    /// `last_message_seq`, the session's place in the list.
    pub fn apply(&mut self, seq: Seq, event: &Event) {
        debug_assert_eq!(event.session_id, self.id, "event of another session");
        if let Some(archived) = event.kind.archived_effect() {
            self.archived = archived;
        }
        if AgentSettings::folds(&event.kind) {
            self.agent.apply(event.command_id, &event.kind);
            return;
        }
        if AgentWork::folds(&event.kind) {
            self.work.apply(&event.kind);
            return;
        }
        match &event.kind {
            EventKind::SessionRenamed { name, .. } => {
                self.name = name.clone();
                return;
            }
            EventKind::ContextStarted { .. } | EventKind::ContextSwitched { .. } => {
                // Each context shows its own last known fill.
                if self.context != event.context
                    && let Some(usage) = &mut self.usage
                {
                    usage.switch_to(event.context);
                }
                self.context = event.context;
                return;
            }
            EventKind::SessionArchived { .. }
            | EventKind::ModelsOffered { .. }
            | EventKind::AgentVersionUnconfirmed { .. }
            | EventKind::ContextMoved { .. }
            | EventKind::TaskStopRequested { .. }
            | EventKind::TaskStopRefused { .. }
            | EventKind::CallsUnanswered { .. }
            | EventKind::ShellStopRequested { .. }
            | EventKind::ShellStarted { .. }
            | EventKind::ShellOutput { .. }
            | EventKind::ShellFinished { .. }
            | EventKind::BlobDeleted { .. }
            | EventKind::BlobCropped { .. } => return,
            EventKind::ImportFinished { .. } => {
                self.importing = false;
                return;
            }
            EventKind::SafetyCheckSet { on, .. } => {
                self.safety_check = *on;
                return;
            }
            // Imported history is a record, not activity; but its first
            // prompt titles the session, as the import precedes every
            // message sent through Skywave.
            EventKind::Imported { item, .. } => {
                if let ImportedItem::UserMessage { text, .. } = item
                    && self.first_prompt.is_none()
                {
                    self.first_prompt = first_line(text, FIRST_PROMPT_MAX_CHARS);
                }
                return;
            }
            _ => {}
        }
        self.last_seq = seq;
        self.last_activity = event.at;
        if is_notable(&event.kind) {
            self.notable += 1;
        }
        match &event.kind {
            EventKind::SessionCreated {
                workdir,
                from,
                worktree,
                ..
            } => {
                self.workdir = Some(workdir.clone());
                self.worktree = worktree.clone();
                self.context = event.context;
                self.adopted = match from {
                    Some(SessionOrigin::Agent { conversation }) => Some(conversation.clone()),
                    _ => None,
                };
                self.importing = self.adopted.is_some();
            }
            EventKind::WorktreeRemoved { .. } => self.worktree_removed = true,
            EventKind::SessionStateChanged(state) => self.state = state.clone(),
            EventKind::ApprovalRequested { id, .. } | EventKind::QuestionAsked { id, .. } => {
                if !self.pending_approvals.contains(id) {
                    self.pending_approvals.push(*id);
                }
            }
            EventKind::ApprovalResolved { id, .. }
            | EventKind::QuestionAnswered { id, .. }
            | EventKind::ApprovalCancelled { id, .. } => {
                self.pending_approvals.retain(|pending| pending != id);
            }
            // The next message starts the agent again.
            EventKind::UserMessage { text, .. } => {
                self.last_message_seq = seq;
                self.stopped = false;
                if self.first_prompt.is_none() {
                    self.first_prompt = first_line(text, FIRST_PROMPT_MAX_CHARS);
                }
            }
            EventKind::SessionStopped => self.stopped = true,
            EventKind::TurnUsage(turn) => {
                match &mut self.usage {
                    Some(usage) => usage.add(event.context, turn),
                    None => self.usage = Some(SessionUsage::new(event.context, turn)),
                }
                // An old context's turn can end after a switch: it updates only
                // its own fill, so the display is re-derived for the current one.
                if let Some(usage) = &mut self.usage
                    && event.context != self.context
                {
                    usage.switch_to(self.context);
                }
            }
            EventKind::AgentMessage { .. }
            | EventKind::SubagentUsage(_)
            | EventKind::ToolCall { .. }
            | EventKind::ToolResult { .. }
            | EventKind::TurnInterrupted
            | EventKind::SessionRenamed { .. }
            | EventKind::SessionArchived { .. }
            | EventKind::PermissionModeRequested { .. }
            | EventKind::PermissionModeChanged { .. }
            | EventKind::PermissionModeRefused { .. }
            | EventKind::ModelRequested { .. }
            | EventKind::ModelChanged { .. }
            | EventKind::ModelRefused { .. }
            | EventKind::ModelsOffered { .. }
            | EventKind::AgentVersionUnconfirmed { .. }
            | EventKind::EffortRequested { .. }
            | EventKind::EffortChanged { .. }
            | EventKind::EffortRefused { .. }
            | EventKind::ContextStarted { .. }
            | EventKind::ContextSwitched { .. }
            | EventKind::ContextMoved { .. }
            | EventKind::BackgroundTasksChanged { .. }
            | EventKind::SchedulesChanged { .. }
            | EventKind::TaskStopRequested { .. }
            | EventKind::TaskStopRefused { .. }
            | EventKind::CallsUnanswered { .. }
            | EventKind::ShellRequested { .. }
            | EventKind::ShellStopRequested { .. }
            | EventKind::ShellStarted { .. }
            | EventKind::ShellOutput { .. }
            | EventKind::ShellFinished { .. }
            | EventKind::ScreenRequested { .. }
            | EventKind::ScreenDone { .. }
            | EventKind::Imported { .. }
            | EventKind::ImportFinished { .. }
            | EventKind::BlobDeleted { .. }
            | EventKind::BlobCropped { .. }
            | EventKind::SafetyCheckSet { .. }
            | EventKind::SafetyRuleSet { .. }
            | EventKind::SafetyCheckApproved { .. }
            | EventKind::SafetyCheckFeedback { .. }
            | EventKind::FileWritten { .. }
            | EventKind::SessionDeleted { .. }
            | EventKind::Erased => {}
        }
    }

    /// The approval the session is blocked on, if any (the oldest one).
    pub fn pending_approval(&self) -> Option<ApprovalId> {
        self.pending_approvals.first().copied()
    }

    /// Whether the agent is working for the idle alert: a turn, a pending
    /// approval or a task that is not ambient. Schedules and ambient tasks
    /// are not work.
    pub fn busy(&self) -> bool {
        matches!(self.state, SessionState::AgentWorking)
            || !self.pending_approvals.is_empty()
            || self.work.tasks.iter().any(|task| !task.ambient)
    }

    /// What to call the session wherever it is shown: its name, else the
    /// [default title](Self::default_title).
    pub fn title(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.default_title())
    }

    /// The title of an unnamed session: the first line of its first
    /// message, whole (each head fits it to its room), else the last
    /// component of its directory, else the end of its id.
    pub fn default_title(&self) -> String {
        if let Some(prompt) = self.first_prompt.as_deref()
            && let Some(line) = first_line(prompt, usize::MAX)
        {
            return line;
        }
        self.workdir
            .as_deref()
            .and_then(|dir| dir.trim_end_matches('/').rsplit('/').next())
            .filter(|name| !name.is_empty())
            .map_or_else(
                || self.id.0.simple().to_string()[24..].to_owned(),
                str::to_owned,
            )
    }
}

/// The first line of `text` with leading blank lines skipped, trimmed and
/// cut to `max` chars; `None` if `text` is blank.
pub(crate) fn first_line(text: &str, max: usize) -> Option<String> {
    let line = text.trim_start().lines().next()?.trim_end();
    Some(line.chars().take(max).collect())
}

/// Whether an event is news for whoever was away: the agent's output (a
/// message, an approval request, an error). Messages from any device never
/// are, nor a subagent's messages: they show only in its own view. An
/// approval counts whoever asks, as it blocks the agent. The store's
/// backfill migration counts the same kinds (its rows had no subagents).
pub fn is_notable(kind: &EventKind) -> bool {
    match kind {
        EventKind::AgentMessage { parent: None, .. }
        | EventKind::ApprovalRequested { .. }
        | EventKind::QuestionAsked { .. } => true,
        EventKind::SessionStateChanged(state) => matches!(state, SessionState::Errored { .. }),
        EventKind::SessionCreated { .. }
        | EventKind::UserMessage { .. }
        | EventKind::ToolCall(_)
        | EventKind::ToolResult(_)
        | EventKind::ApprovalResolved { .. }
        | EventKind::QuestionAnswered { .. }
        | EventKind::ApprovalCancelled { .. }
        | EventKind::TurnInterrupted
        | EventKind::SessionStopped
        | EventKind::SessionRenamed { .. }
        | EventKind::SessionArchived { .. }
        | EventKind::WorktreeRemoved { .. }
        | EventKind::TurnUsage(_)
        | EventKind::SubagentUsage(_)
        | EventKind::PermissionModeRequested { .. }
        | EventKind::PermissionModeChanged { .. }
        | EventKind::PermissionModeRefused { .. }
        | EventKind::ModelRequested { .. }
        | EventKind::ModelChanged { .. }
        | EventKind::ModelRefused { .. }
        | EventKind::ModelsOffered { .. }
        | EventKind::AgentVersionUnconfirmed { .. }
        | EventKind::EffortRequested { .. }
        | EventKind::EffortChanged { .. }
        | EventKind::EffortRefused { .. }
        | EventKind::ContextStarted { .. }
        | EventKind::ContextSwitched { .. }
        | EventKind::ContextMoved { .. }
        | EventKind::BackgroundTasksChanged { .. }
        | EventKind::SchedulesChanged { .. }
        | EventKind::TaskStopRequested { .. }
        | EventKind::TaskStopRefused { .. }
        | EventKind::AgentMessage {
            parent: Some(_), ..
        }
        | EventKind::CallsUnanswered { .. }
        | EventKind::ShellRequested { .. }
        | EventKind::ShellStopRequested { .. }
        | EventKind::ShellStarted { .. }
        | EventKind::ShellOutput { .. }
        | EventKind::ShellFinished { .. }
        | EventKind::ScreenRequested { .. }
        | EventKind::ScreenDone { .. }
        | EventKind::Imported { .. }
        | EventKind::ImportFinished { .. }
        | EventKind::BlobDeleted { .. }
        | EventKind::BlobCropped { .. }
        | EventKind::SafetyCheckSet { .. }
        | EventKind::SafetyRuleSet { .. }
        | EventKind::SafetyCheckApproved { .. }
        | EventKind::SafetyCheckFeedback { .. }
        | EventKind::FileWritten { .. }
        | EventKind::SessionDeleted { .. }
        | EventKind::Erased => false,
    }
}

/// The blobs an event references: a tool call's input and the whole of its
/// display body, a result's output, an approval's input, a finished shell
/// run's whole output; the same for an imported call or result, so a
/// policy that fetches everything fetches imported history too.
pub(crate) fn blob_refs(kind: &EventKind) -> impl Iterator<Item = &BlobRef> {
    kind.blobs().map(|(_, blob)| blob)
}

/// What the server did to a blob's content, as its `BlobDeleted` or
/// `BlobCropped` says. Every replica keeps it by the blob it names, so an
/// event's `BlobRef` resolves through it to what is left: nothing, or the
/// cropped blob (which may have a fate of its own: a crop of a crop).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BlobFate {
    Deleted {
        by: DeviceId,
    },
    Cropped {
        /// The blob holding what was kept.
        into: BlobRef,
        /// The line spans kept, as the command sent them.
        kept: Vec<LineSpan>,
        /// The line count of the cropped blob.
        lines: u32,
        by: DeviceId,
    },
}

impl BlobFate {
    /// The blob `kind` decides the fate of, with that fate.
    fn of(kind: &EventKind) -> Option<(BlobId, BlobFate)> {
        match kind {
            EventKind::BlobDeleted { blob, by, .. } => Some((*blob, BlobFate::Deleted { by: *by })),
            EventKind::BlobCropped {
                blob,
                into,
                kept,
                lines,
                by,
                ..
            } => Some((
                *blob,
                BlobFate::Cropped {
                    into: into.clone(),
                    kept: kept.clone(),
                    lines: *lines,
                    by: *by,
                },
            )),
            _ => None,
        }
    }

    /// Every fate `kind` decides: [`BlobFate::of`]'s, and each capture a
    /// `ScreenDone` pruned, deleted as by the device that asked (D65).
    pub(crate) fn all(kind: &EventKind) -> Vec<(BlobId, BlobFate)> {
        let mut fates: Vec<_> = Self::of(kind).into_iter().collect();
        if let EventKind::ScreenDone { pruned, by, .. } = kind {
            fates.extend(
                pruned
                    .iter()
                    .map(|blob| (*blob, BlobFate::Deleted { by: *by })),
            );
        }
        fates
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use skywave_proto::{Decision, DeviceId, Full, ToolCall};

    fn event(session_id: SessionId, minute: u32, kind: EventKind) -> Event {
        Event {
            session_id,
            at: Utc.with_ymd_and_hms(2026, 10, 29, 12, minute, 0).unwrap(),
            command_id: kind.logged_by_command().then(skywave_proto::CommandId::new),
            context: ContextId::first_of(session_id),
            kind,
        }
    }

    #[test]
    fn the_current_context_follows_starts_and_switches() {
        let sid = SessionId::new();
        let first = ContextId::first_of(sid);
        let mut summary = SessionSummary::new(
            Seq(1),
            &event(
                sid,
                0,
                EventKind::SessionCreated {
                    workdir: "/srv/x".into(),
                    context_name: "start".into(),
                    from: None,
                    worktree: None,
                },
            ),
        );
        assert_eq!(summary.context, first);
        let by = DeviceId::new();
        let fresh = ContextId::new();
        let in_context = |context, kind| Event {
            context,
            ..event(sid, 1, kind)
        };
        summary.apply(
            Seq(2),
            &in_context(
                fresh,
                EventKind::ContextStarted {
                    by,
                    name: "fresh".into(),
                },
            ),
        );
        assert_eq!(summary.context, fresh);
        // Other events leave it, whatever their stamp.
        summary.apply(
            Seq(3),
            &in_context(
                first,
                EventKind::AgentMessage {
                    parent: None,
                    text: "x".into(),
                    full: None,
                },
            ),
        );
        assert_eq!(summary.context, fresh);
        summary.apply(
            Seq(4),
            &in_context(first, EventKind::ContextSwitched { by }),
        );
        assert_eq!(summary.context, first);
    }

    #[test]
    fn the_context_fill_is_each_contexts_last_known_and_unknown_after_a_compact() {
        let sid = SessionId::new();
        let first = ContextId::first_of(sid);
        let created = EventKind::SessionCreated {
            workdir: "/srv/x".into(),
            context_name: "start".into(),
            from: None,
            worktree: None,
        };
        let mut summary = SessionSummary::new(Seq(1), &event(sid, 0, created));
        let turn = |tokens: Option<u64>| {
            EventKind::TurnUsage(TurnUsage {
                duration_ms: Some(1_000),
                context_tokens: tokens,
                context_window: tokens.map(|_| 1_000_000),
                ..TurnUsage::default()
            })
        };
        let compact = || {
            EventKind::TurnUsage(TurnUsage {
                duration_ms: Some(1_000),
                context_compacted: true,
                ..TurnUsage::default()
            })
        };
        let fill = |summary: &SessionSummary| summary.usage.as_ref().unwrap().context;
        let at = |context, kind| Event {
            context,
            ..event(sid, 1, kind)
        };
        summary.apply(Seq(2), &event(sid, 1, turn(Some(19_300))));
        let known = Some(ContextFill {
            tokens: 19_300,
            window: Some(1_000_000),
        });
        assert_eq!(fill(&summary), known);
        // Never summed.
        let usage = summary.usage.clone().unwrap();
        assert_eq!(
            (usage.total.context_tokens, usage.total.context_window),
            (None, None)
        );
        summary.apply(Seq(3), &event(sid, 2, turn(Some(20_000))));
        let usage = summary.usage.clone().unwrap();
        assert_eq!(usage.context.unwrap().tokens, 20_000);
        assert_eq!(usage.total.context_tokens, None);
        // A turn that reports no fill keeps the last known.
        summary.apply(Seq(4), &event(sid, 3, turn(None)));
        assert_eq!(fill(&summary).unwrap().tokens, 20_000);
        // A compaction: unknown until the next turn.
        summary.apply(Seq(5), &event(sid, 4, compact()));
        assert_eq!(fill(&summary), None);
        summary.apply(Seq(6), &event(sid, 5, turn(Some(19_300))));
        assert_eq!(fill(&summary), known);

        // A switch back to the current context changes nothing.
        let by = DeviceId::new();
        summary.apply(Seq(7), &at(first, EventKind::ContextSwitched { by }));
        assert_eq!(fill(&summary), known);
        // A new context starts unknown.
        let fresh = ContextId::new();
        let started = EventKind::ContextStarted {
            by,
            name: "fresh".into(),
        };
        summary.apply(Seq(8), &at(fresh, started));
        assert_eq!(fill(&summary), None);
        summary.apply(Seq(9), &at(fresh, turn(Some(9_000))));
        assert_eq!(fill(&summary).unwrap().tokens, 9_000);
        // Back to the first context: its own last fill.
        summary.apply(Seq(10), &at(first, EventKind::ContextSwitched { by }));
        assert_eq!(fill(&summary), known);
        // Back to the fresh one: its own too.
        summary.apply(Seq(11), &at(fresh, EventKind::ContextSwitched { by }));
        assert_eq!(fill(&summary).unwrap().tokens, 9_000);
        // An old context's turn ending after the switch updates only its own
        // fill: the current one keeps its own.
        summary.apply(Seq(12), &at(first, turn(Some(150_000))));
        assert_eq!(fill(&summary).unwrap().tokens, 9_000);
        summary.apply(Seq(13), &at(first, compact()));
        assert_eq!(fill(&summary).unwrap().tokens, 9_000);
        // Switching to the old context shows what its turns left: the compact
        // removed its fill.
        summary.apply(Seq(14), &at(first, EventKind::ContextSwitched { by }));
        assert_eq!(fill(&summary), None);

        // A first turn that compacted does not make the total compacted.
        let compacted = TurnUsage {
            duration_ms: Some(1_000),
            context_compacted: true,
            ..TurnUsage::default()
        };
        let usage = SessionUsage::new(first, &compacted);
        assert!(usage.last_turn.context_compacted);
        assert!(!usage.total.context_compacted);

        // The fills are stored with the usage. A stored usage without them
        // (protocol v34's) reads with none.
        let usage = summary.usage.clone().unwrap();
        let json = serde_json::to_string(&usage).unwrap();
        assert_eq!(serde_json::from_str::<SessionUsage>(&json).unwrap(), usage);
        let mut stored: serde_json::Value = serde_json::from_str(&json).unwrap();
        stored.as_object_mut().unwrap().remove("fills");
        let read: SessionUsage = serde_json::from_value(stored).unwrap();
        assert!(read.fills.is_empty());
    }

    #[test]
    fn session_created_keeps_its_worktree() {
        let sid = SessionId::new();
        let worktree = Worktree {
            repo: "/srv/app".into(),
            branch: "feat/x".into(),
        };
        let created = |worktree| {
            event(
                sid,
                0,
                EventKind::SessionCreated {
                    workdir: "/srv/app".into(),
                    context_name: "start".into(),
                    from: None,
                    worktree,
                },
            )
        };
        let summary = SessionSummary::new(Seq(1), &created(Some(worktree.clone())));
        assert_eq!(summary.worktree, Some(worktree));
        let plain = SessionSummary::new(Seq(1), &created(None));
        assert_eq!(plain.worktree, None, "a session without one has none");
    }

    #[test]
    fn worktree_removed_marks_the_session() {
        let sid = SessionId::new();
        let worktree = Worktree {
            repo: "/srv/app".into(),
            branch: "feat/x".into(),
        };
        let created = event(
            sid,
            0,
            EventKind::SessionCreated {
                workdir: "/srv/app".into(),
                context_name: "start".into(),
                from: None,
                worktree: Some(worktree.clone()),
            },
        );
        let mut summary = SessionSummary::new(Seq(1), &created);
        assert!(!summary.worktree_removed);
        let by = DeviceId::new();
        summary.apply(Seq(2), &event(sid, 1, EventKind::WorktreeRemoved { by }));
        assert!(summary.worktree_removed);
        assert_eq!(
            summary.worktree,
            Some(worktree),
            "heads still group it by repo"
        );
    }

    #[test]
    fn folds_workdir_state_and_approvals() {
        let sid = SessionId::new();
        let (a1, a2) = (ApprovalId::new(), ApprovalId::new());
        let created = event(
            sid,
            0,
            EventKind::SessionCreated {
                workdir: "/srv/x".into(),
                context_name: "start".into(),
                from: None,
                worktree: None,
            },
        );
        let mut summary = SessionSummary::new(Seq(1), &created);
        assert_eq!(summary.workdir.as_deref(), Some("/srv/x"));
        assert_eq!(summary.state, SessionState::Idle);

        let steps = [
            EventKind::SessionStateChanged(SessionState::AgentWorking),
            EventKind::ApprovalRequested {
                id: a1,
                call: None,
                tool: "bash".into(),
                summary: "ls".into(),
                input: None,
                suggestions: Vec::new(),
                shell: None,
            },
            EventKind::ApprovalRequested {
                id: a2,
                call: None,
                tool: "bash".into(),
                summary: "pwd".into(),
                input: None,
                suggestions: Vec::new(),
                shell: None,
            },
        ];
        for (i, kind) in steps.into_iter().enumerate() {
            let n = u32::try_from(i).unwrap() + 1;
            summary.apply(Seq(u64::from(n) + 1), &event(sid, n, kind));
        }
        assert_eq!(summary.state, SessionState::AgentWorking);
        assert_eq!(summary.pending_approval(), Some(a1));
        assert_eq!(summary.notable, 2, "both approval requests are news");

        let resolved = EventKind::ApprovalResolved {
            id: a1,
            decision: Decision::Approve,
            by: DeviceId::new(),
            rule: None,
        };
        summary.apply(Seq(5), &event(sid, 5, resolved));
        assert_eq!(summary.pending_approval(), Some(a2));

        let cancelled = EventKind::ApprovalCancelled {
            id: a2,
            reason: "daemon restarted".into(),
        };
        summary.apply(Seq(6), &event(sid, 6, cancelled));
        assert_eq!(summary.pending_approval(), None);
        assert_eq!(summary.last_seq, Seq(6));
        assert_eq!(
            summary.last_activity,
            event(sid, 6, EventKind::TurnInterrupted).at
        );
    }

    #[test]
    fn the_safety_check_switch_folds_without_moving_activity() {
        let sid = SessionId::new();
        let created = EventKind::SessionCreated {
            workdir: "/srv/x".into(),
            context_name: "start".into(),
            from: None,
            worktree: None,
        };
        let mut summary = SessionSummary::new(Seq(1), &event(sid, 0, created));
        assert!(!summary.safety_check, "off until a switch says otherwise");
        let (last_seq, last_activity) = (summary.last_seq, summary.last_activity);

        let by = DeviceId::new();
        let switch = |on| EventKind::SafetyCheckSet { on, by };
        summary.apply(Seq(2), &event(sid, 1, switch(true)));
        assert!(summary.safety_check);
        summary.apply(Seq(3), &event(sid, 2, switch(false)));
        assert!(!summary.safety_check);
        assert_eq!(summary.last_seq, last_seq, "a setting is not activity");
        assert_eq!(summary.last_activity, last_activity);
        assert_eq!(summary.notable, 0);
    }

    #[test]
    fn title_is_the_directory_name_else_the_id() {
        let sid = SessionId::new();
        let mut summary = SessionSummary::new(Seq(1), &event(sid, 0, EventKind::TurnInterrupted));
        let short = sid.0.simple().to_string()[24..].to_owned();
        assert_eq!(summary.title(), short);
        for (workdir, title) in [("/srv/app", "app"), ("/srv/app/", "app"), ("app", "app")] {
            summary.workdir = Some(workdir.into());
            assert_eq!(summary.title(), title);
        }
        summary.workdir = Some("/".into());
        assert_eq!(summary.title(), short);
    }

    #[test]
    fn title_is_the_name_else_the_first_prompt_else_the_directory() {
        let sid = SessionId::new();
        let created = EventKind::SessionCreated {
            workdir: "/srv/app".into(),
            context_name: "start".into(),
            from: None,
            worktree: None,
        };
        let mut summary = SessionSummary::new(Seq(1), &event(sid, 0, created));
        assert_eq!(summary.title(), "app");

        let message = |text: &str| EventKind::UserMessage {
            device_id: DeviceId::new(),
            text: text.into(),
            attachments: Vec::new(),
        };
        summary.apply(
            Seq(2),
            &event(sid, 1, message("\n  fix the flaky test  \nin ci")),
        );
        assert_eq!(summary.first_prompt.as_deref(), Some("fix the flaky test"));
        summary.apply(Seq(3), &event(sid, 2, message("and then this")));
        assert_eq!(
            summary.title(),
            "fix the flaky test",
            "the first prompt stays"
        );

        let rename = |name: Option<&str>| EventKind::SessionRenamed {
            name: name.map(str::to_owned),
            by: DeviceId::new(),
        };
        summary.apply(Seq(4), &event(sid, 3, rename(Some("flaky"))));
        assert_eq!(summary.title(), "flaky");
        assert_eq!(summary.default_title(), "fix the flaky test");
        summary.apply(Seq(5), &event(sid, 4, rename(None)));
        assert_eq!(
            summary.title(),
            "fix the flaky test",
            "no name restores the default"
        );

        summary.first_prompt = Some("x".repeat(60));
        assert_eq!(
            summary.title(),
            "x".repeat(60),
            "a head fits it, not the core"
        );
    }

    #[test]
    fn a_rename_is_not_activity() {
        let sid = SessionId::new();
        let created = EventKind::SessionCreated {
            workdir: "/srv/app".into(),
            context_name: "start".into(),
            from: None,
            worktree: None,
        };
        let mut summary = SessionSummary::new(Seq(1), &event(sid, 0, created));
        let renamed = EventKind::SessionRenamed {
            name: Some("x".into()),
            by: DeviceId::new(),
        };
        assert!(!is_notable(&renamed));
        summary.apply(Seq(9), &event(sid, 30, renamed));
        assert_eq!(summary.name.as_deref(), Some("x"));
        assert_eq!(summary.last_seq, Seq(1));
        assert_eq!(
            summary.last_activity,
            event(sid, 0, EventKind::TurnInterrupted).at
        );
        assert_eq!(summary.notable, 0);
    }

    #[test]
    fn a_deleted_or_cropped_blob_is_not_activity() {
        let sid = SessionId::new();
        let created = EventKind::SessionCreated {
            workdir: "/srv/app".into(),
            context_name: "start".into(),
            from: None,
            worktree: None,
        };
        let mut summary = SessionSummary::new(Seq(1), &event(sid, 0, created));
        let deleted = EventKind::BlobDeleted {
            blob: skywave_proto::BlobId::new(),
            holds: skywave_proto::BlobUse::ShellOutput,
            by: DeviceId::new(),
        };
        let cropped = EventKind::BlobCropped {
            blob: skywave_proto::BlobId::new(),
            holds: skywave_proto::BlobUse::ToolOutput,
            into: skywave_proto::BlobRef {
                id: skywave_proto::BlobId::new(),
                size: 4,
                sha256: skywave_proto::ContentHash::of(b"1\n3\n"),
            },
            kept: vec![skywave_proto::LineSpan { first: 0, count: 1 }],
            lines: 3,
            by: DeviceId::new(),
        };
        assert!(!is_notable(&deleted));
        assert!(!is_notable(&cropped));
        summary.apply(Seq(9), &event(sid, 30, deleted));
        summary.apply(Seq(10), &event(sid, 30, cropped));
        assert_eq!(summary.last_seq, Seq(1));
        assert_eq!(
            summary.last_activity,
            event(sid, 0, EventKind::TurnInterrupted).at
        );
        assert_eq!(summary.notable, 0);
        // Control: activity at the same time does move them.
        summary.apply(
            Seq(11),
            &event(
                sid,
                30,
                EventKind::AgentMessage {
                    parent: None,
                    text: "hi".into(),
                    full: None,
                },
            ),
        );
        assert_eq!(summary.last_seq, Seq(11));
    }

    #[test]
    fn a_context_cleared_or_switched_is_not_activity() {
        let sid = SessionId::new();
        let created = EventKind::SessionCreated {
            workdir: "/srv/app".into(),
            context_name: "start".into(),
            from: None,
            worktree: None,
        };
        let mut summary = SessionSummary::new(Seq(1), &event(sid, 0, created));
        let first = ContextId::first_of(sid);
        assert_eq!(summary.context, first);
        let by = DeviceId::new();
        let cleared = ContextId::new();
        let started = EventKind::ContextStarted {
            by,
            name: "velvet walrus".into(),
        };
        assert!(!is_notable(&started));
        summary.apply(
            Seq(5),
            &Event {
                context: cleared,
                ..event(sid, 20, started)
            },
        );
        assert_eq!(summary.context, cleared);
        summary.apply(
            Seq(9),
            &Event {
                context: first,
                ..event(sid, 30, EventKind::ContextSwitched { by })
            },
        );
        assert_eq!(summary.context, first);
        assert_eq!(summary.last_seq, Seq(1));
        assert_eq!(
            summary.last_activity,
            event(sid, 0, EventKind::TurnInterrupted).at
        );
        assert_eq!(summary.notable, 0);
        // A positive control: a message is activity.
        let message = EventKind::UserMessage {
            device_id: by,
            text: "hi".into(),
            attachments: Vec::new(),
        };
        summary.apply(Seq(10), &event(sid, 40, message));
        assert_eq!(summary.last_seq, Seq(10));
    }

    #[test]
    fn archiving_is_not_activity_and_a_message_brings_it_back() {
        let sid = SessionId::new();
        let created = EventKind::SessionCreated {
            workdir: "/srv/app".into(),
            context_name: "start".into(),
            from: None,
            worktree: None,
        };
        let mut summary = SessionSummary::new(Seq(1), &event(sid, 0, created));
        assert!(!summary.archived);
        let archive = |archived| EventKind::SessionArchived {
            archived,
            by: DeviceId::new(),
        };
        assert!(!is_notable(&archive(true)));
        summary.apply(Seq(5), &event(sid, 10, archive(true)));
        assert!(summary.archived);
        assert_eq!(summary.last_seq, Seq(1));
        assert_eq!(summary.notable, 0);
        summary.apply(Seq(6), &event(sid, 11, archive(true)));
        assert!(summary.archived, "absolute: a repeat does not flip it");
        let message = EventKind::UserMessage {
            device_id: DeviceId::new(),
            text: "back".into(),
            attachments: Vec::new(),
        };
        summary.apply(Seq(7), &event(sid, 12, message));
        assert!(!summary.archived);
        assert_eq!(summary.last_seq, Seq(7));
        summary.apply(Seq(8), &event(sid, 13, archive(true)));
        summary.apply(Seq(9), &event(sid, 14, archive(false)));
        assert!(!summary.archived);
    }

    #[test]
    fn turn_usage_sums_what_each_turn_reports() {
        let sid = SessionId::new();
        let created = EventKind::SessionCreated {
            workdir: "/srv/app".into(),
            context_name: "start".into(),
            from: None,
            worktree: None,
        };
        let mut summary = SessionSummary::new(Seq(1), &event(sid, 0, created));
        assert_eq!(summary.usage, None);

        let first = TurnUsage {
            duration_ms: Some(12_400),
            tokens_in: Some(3_100),
            tokens_out: Some(812),
            cost_usd: Some(0.25),
            model: Some("model-a".into()),
            ..TurnUsage::default()
        };
        let kind = EventKind::TurnUsage(first.clone());
        assert!(!is_notable(&kind));
        assert_eq!(kind.archived_effect(), None);
        summary.apply(Seq(2), &event(sid, 1, kind));
        let usage = summary.usage.clone().unwrap();
        assert_eq!((&usage.last_turn, &usage.total), (&first, &first));
        assert_eq!(summary.last_seq, Seq(2), "part of the turn's end");
        assert_eq!(summary.notable, 0);

        // A turn missing figures adds nothing to them, and an earlier model
        // stays until another is named.
        let second = TurnUsage {
            duration_ms: Some(600),
            cost_usd: Some(0.5),
            ..TurnUsage::default()
        };
        summary.apply(Seq(3), &event(sid, 2, EventKind::TurnUsage(second.clone())));
        let third = TurnUsage {
            tokens_out: Some(188),
            model: Some("model-b".into()),
            ..TurnUsage::default()
        };
        summary.apply(Seq(4), &event(sid, 3, EventKind::TurnUsage(third.clone())));
        let usage = summary.usage.unwrap();
        assert_eq!(usage.last_turn, third);
        assert_eq!(
            usage.total,
            TurnUsage {
                duration_ms: Some(13_000),
                tokens_in: Some(3_100),
                tokens_out: Some(1_000),
                cost_usd: Some(0.75),
                model: Some("model-b".into()),
                ..TurnUsage::default()
            }
        );
    }

    #[test]
    fn only_the_main_agents_messages_are_notable() {
        let message = |parent| EventKind::AgentMessage {
            text: "done".into(),
            parent,
            full: None,
        };
        assert!(is_notable(&message(None)));
        assert!(!is_notable(&message(Some(skywave_proto::CallId::new()))));
        let unanswered = EventKind::CallsUnanswered {
            calls: vec![skywave_proto::CallId::new()],
        };
        assert!(!is_notable(&unanswered));

        let sid = SessionId::new();
        let created = EventKind::SessionCreated {
            workdir: "/srv/app".into(),
            context_name: "start".into(),
            from: None,
            worktree: None,
        };
        let mut summary = SessionSummary::new(Seq(1), &event(sid, 0, created));
        summary.apply(Seq(2), &event(sid, 1, unanswered));
        assert_eq!((summary.last_seq, summary.notable), (Seq(1), 0));
    }

    #[test]
    fn only_a_shell_request_is_activity_and_no_shell_event_is_notable() {
        use skywave_proto::{CommandId, RunWhen, ShellEnd};
        let sid = SessionId::new();
        let (by, run) = (DeviceId::new(), CommandId::new());
        let output = BlobRef {
            id: skywave_proto::BlobId::new(),
            size: 9_000,
            sha256: skywave_proto::ContentHash::of(b""),
        };
        let daemon = [
            EventKind::ShellStopRequested { run, by },
            EventKind::ShellStarted {
                run,
                cwd: "/srv/app".into(),
            },
            EventKind::ShellOutput {
                run,
                tail: "x".into(),
                lines: 1,
                bytes: 2,
            },
            EventKind::ShellFinished {
                run,
                by,
                started_at: None,
                command_line: "make".into(),
                end: ShellEnd::Exited { code: 2 },
                cwd: None,
                preview: "error".into(),
                lines: 1,
                output: Some(output.clone()),
                truncated: true,
            },
        ];
        let created = EventKind::SessionCreated {
            workdir: "/srv/app".into(),
            context_name: "start".into(),
            from: None,
            worktree: None,
        };
        let mut summary = SessionSummary::new(Seq(1), &event(sid, 0, created));
        let archive = EventKind::SessionArchived { archived: true, by };
        summary.apply(Seq(2), &event(sid, 1, archive));
        let start = summary.last_activity;
        for (i, kind) in daemon.into_iter().enumerate() {
            assert!(!is_notable(&kind), "{kind:?}");
            summary.apply(Seq(3 + i as u64), &event(sid, 2 + i as u32, kind));
            assert_eq!((summary.last_seq, summary.last_activity), (Seq(1), start));
        }
        assert!(summary.archived, "no daemon shell event unarchives");
        assert_eq!(summary.notable, 0);
        // The finished run's whole output is a blob to fetch.
        let finished = EventKind::ShellFinished {
            run,
            by,
            started_at: None,
            command_line: "make".into(),
            end: ShellEnd::Lost,
            cwd: None,
            preview: String::new(),
            lines: 0,
            output: Some(output.clone()),
            truncated: true,
        };
        assert_eq!(blob_refs(&finished).collect::<Vec<_>>(), vec![&output]);

        let requested = EventKind::ShellRequested {
            command: "make".into(),
            when: RunWhen::OnSuccess,
            by,
        };
        assert!(!is_notable(&requested));
        summary.apply(Seq(9), &event(sid, 30, requested));
        assert_eq!(summary.last_seq, Seq(9));
        assert_eq!(
            summary.last_activity,
            event(sid, 30, EventKind::TurnInterrupted).at
        );
        assert!(!summary.archived, "a shell request brings the session back");
        assert_eq!(summary.notable, 0);
    }

    #[test]
    fn a_tool_call_references_its_input_and_its_whole_body() {
        let blob = |size| BlobRef {
            id: skywave_proto::BlobId::new(),
            size,
            sha256: skywave_proto::ContentHash::of(b""),
        };
        let (input, full) = (blob(10), blob(20));
        let call = |input, full| {
            EventKind::ToolCall(ToolCall {
                parent: None,
                call: Some(skywave_proto::CallId::new()),
                name: "Edit".into(),
                display: skywave_proto::ToolDisplay {
                    kind: skywave_proto::ToolKind::FileEdit,
                    title: "a.rs".into(),
                    detail: None,
                    body: Some(skywave_proto::Body {
                        format: skywave_proto::BodyFormat::Diff,
                        preview: "-a\n+b".into(),
                        lines: 2,
                        full: Some(full),
                    }),
                },
                input: Some(input),
            })
        };
        let kept = call(Full::Blob(input.clone()), Full::Blob(full.clone()));
        assert_eq!(blob_refs(&kept).collect::<Vec<_>>(), vec![&input, &full]);
        // Content the server could not keep has no blob to fetch.
        let lost = call(Full::Lost, Full::Lost);
        assert_eq!(blob_refs(&lost).count(), 0);
        assert_eq!(blob_refs(&EventKind::TurnInterrupted).count(), 0);
    }
}
