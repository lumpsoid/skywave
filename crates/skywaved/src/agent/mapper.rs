//! Maps one session's [`AdapterEvent`]s to log events.

use std::collections::{HashMap, HashSet, VecDeque};

use serde_json::Value;
use skywave_proto::limits::{
    AGENT_VERSION_MAX_CHARS, DETAIL_MAX_CHARS, INLINE_MAX_BYTES, MESSAGE_PREVIEW_MAX_BYTES,
    MESSAGE_WHOLE_MAX_BYTES, MODEL_DETAIL_MAX_CHARS, MODEL_LABEL_MAX_CHARS, MODEL_MAX_CHARS,
    MODELS_MAX, PREVIEW_MAX_BYTES, PREVIEW_MAX_LINES, REFUSAL_MAX_CHARS, RULE_LABEL_MAX_CHARS,
    RULE_SUGGESTIONS_MAX, SCHEDULE_WHEN_MAX_CHARS, SCHEDULES_MAX, STATUS_MAX_CHARS,
    TASK_ID_MAX_CHARS, TASKS_MAX, TITLE_MAX_CHARS,
};
use skywave_proto::{
    self as proto, AgentSettings, AgentWork, ApprovalId, BackgroundTask, Body, BodyFormat, CallId,
    CommandId, ContextId, Effort, EventKind, Full, ModelChoice, PermissionMode, Schedule, Seq,
    SessionId, SessionState, TaskId, ToolCall, ToolKind, ToolResult,
};

use super::{
    AdapterEvent, AgentCallId, AgentRequestId, AgentSchedule, AgentTask, DisplayBody,
    ResultDisplay, ToolDisplay, TurnOutcome, store_grants, store_resume_handle,
};
use crate::error::Result;
use crate::{EventLog, Storage};

/// Reason for approvals still pending when their turn ends.
const TURN_ENDED: &str = "turn ended";
const TURN_INTERRUPTED: &str = "turn interrupted";
const AGENT_EXITED: &str = "agent exited";

/// Turns one session's adapter events into log events.
///
/// The supervisor owns one mapper per running agent session and feeds it
/// every [`AdapterEvent`] in order. Every event it logs is stamped with the
/// mapper's context, the one the agent process was started for, even when
/// it arrives after the session switched to another context:
///
/// | adapter event | log |
/// |---------------|-----|
/// | `TurnStarted` | `SessionStateChanged(AgentWorking)` |
/// | `Text` | `AgentMessage`, `parent` linked to its call |
/// | `ToolCall` | `ToolCall` with a new [`CallId`] and a bounded display, `parent` linked to its call |
/// | `ToolResult` | `ToolResult` linked to its call, with a bounded preview |
/// | `ApprovalRequested` | `ApprovalRequested` with a new [`ApprovalId`], linked to its call; a task's (`task` set) is kept apart from the turn's |
/// | `ApprovalCancelled` | `ApprovalCancelled` (if still pending) |
/// | `TurnUsage` | `TurnUsage`, as is (only in an open turn, and not empty) |
/// | `TurnEnded(Completed \| Interrupted)` | the turn's leftover approvals cancelled (not a task's), `CallsUnanswered` for the turn's open calls, `SessionStateChanged(Idle)` |
/// | `TurnEnded(Failed)` | the turn's leftover approvals cancelled (not a task's), `CallsUnanswered` for the turn's open calls, `SessionStateChanged(Errored)` |
/// | `ResumeHandle` | stored in `contexts.agent_state` for the mapper's context, no event |
/// | `Grants` | stored in `sessions.agent_grants` for the mapper's session (whichever context), no event |
/// | `PermissionMode` / `Model` / `Effort` | `PermissionModeChanged` / `ModelChanged` / `EffortChanged` answering nothing, only when it differs from what the log has |
/// | `PermissionModeSet` / `ModelSet` / `EffortSet` | `PermissionModeChanged` / `ModelChanged` / `EffortChanged` answering the oldest request asked; for one asked under no command, only when it differs from what the log has |
/// | `PermissionModeRefused` / `ModelRefused` / `EffortRefused` | `PermissionModeRefused` / `ModelRefused` / `EffortRefused` answering the oldest request asked |
/// | `ModelsOffered` | nothing: the supervisor logs it, comparing across sessions |
/// | `VersionUnconfirmed` | `AgentVersionUnconfirmed`, each version cut to its cap |
/// | `TaskStopAnswered` | nothing: retires the oldest stop asked (the task leaving the set is the outcome) |
/// | `TaskStopRefused` | `TaskStopRefused` answering the oldest stop asked |
/// | `BackgroundTasks` | `CallsUnanswered` for the open calls of the subagents whose task left, then `BackgroundTasksChanged` if the set changed, `call` linked to its call |
/// | `Exited` | leftover approvals cancelled, a task's too, leftover mode, model and effort requests refused, leftover stops dropped, `CallsUnanswered` for every open call; one `Errored` if a turn was open, the exit was abnormal or messages went unsent, naming each that applies, the unsent ones last ("N queued messages were not sent to the agent; send them again") |
///
/// **Settings.** The supervisor tells the mapper of every mode, model or
/// effort request it hands the agent ([`EventMapper::asked_mode`],
/// [`EventMapper::asked_model`], [`EventMapper::asked_effort`]), with the
/// command asking, if any. The agent
/// answers requests in order, so each answer names the oldest one asked.
/// What the agent reports unasked is logged only when it changes what the
/// log has, starting from the [`AgentSettings`] the mapper is made with.
///
/// **Stops.** Likewise the supervisor tells the mapper of every task stop
/// it hands the agent ([`EventMapper::asked_stop`]). Both answers retire
/// the oldest stop asked, the success too though it logs nothing: with two
/// stops in flight, a success that kept its place would hand the next
/// refusal the wrong command. Stops left on `Exited` are dropped, not
/// refused: the empty task set logged on exit settles them.
///
/// **Calls.** Every `ToolCall` gets a fresh [`CallId`]. The mapper remembers
/// which [`AgentCallId`] it stands for; a result or approval naming that id
/// carries the same [`CallId`], so does a text or call naming it as its
/// `parent`, and a task naming it as its `call`. One naming no id or an
/// unknown one carries `None`: a subagent's step whose parent is unknown
/// shows on the main timeline rather than vanishing.
///
/// A call is the turn's unless its nearest ancestor through `parent` that
/// some reported task runs (a *root*) is found: then it is that task's.
/// How long the mapper remembers a call:
/// - a root, until `Exited`: one call can run several times, its task
///   leaving the set and coming back, woken by a shell it started even
///   after the turn that launched it ended;
/// - a task's call, while the task is live, and past it until the turn
///   ends if one is open (or right away if none is); its result does not
///   end it, as a nested subagent's root may be answered before any task
///   names it;
/// - any other call (the main agent's, a foreground subagent's), until the
///   turn ends.
///
/// The mapper tracks whether each logged call is settled: its result came,
/// or it was named unanswered. It logs one `CallsUnanswered` naming the
/// open calls, in log order (nothing if none is open), wherever it knows
/// they are abandoned: at `TurnEnded` the turn's (main agent, subagents
/// with no live task), just before the closing `SessionStateChanged`; on
/// `BackgroundTasks` those of the tasks that left the set (a task that
/// stopped has no call in flight; one woken again makes new calls), just
/// before the set change; on `Exited` all of them. A call the agent gave
/// no id is open for good, as no result can name it. Tasks that left are
/// found from the agent's whole report, not the cut set logged. A result
/// for a call already named is still linked while the mapper remembers
/// the call.
///
/// **Bounds.** Every event is cut to `skywave_proto::limits`, whatever the
/// adapter handed over: a title, detail or status to its first non-blank
/// line and its char cap (a cut one ends in `…`), and so are a refusal's
/// reason and an `Errored` message (to [`REFUSAL_MAX_CHARS`]: a crash
/// reason can be a stray multi-megabyte stdout line) and a rule
/// suggestion's label, of which only the first [`RULE_SUGGESTIONS_MAX`]
/// are kept (the indices the adapter knows them by stay); a body or result preview
/// to its first [`PREVIEW_MAX_LINES`] lines and [`PREVIEW_MAX_BYTES`] bytes.
/// Cuts fall on char boundaries; `lines` counts the whole content. Failure
/// is `is_error`, never a marker in the text. A cut title of a display with
/// no body gets a `Text` body holding its whole text (bounded and blobbed
/// like any body), so a long or multi-line command is never shown in part
/// only; such a title no longer counts as cut for the input blob.
///
/// **Blobs** are made only for content that was cut: a body's text
/// (`Body::full`) when its preview is shorter; a result's output when its
/// preview does not hold all of it (it was cut, or the adapter previewed
/// other text, e.g. without notes meant for the model); a call's input
/// (pretty JSON, or the string itself) when its display does not carry all
/// of it: its title (beside a body) or detail was cut, or the display (the
/// adapter's, by `shows_all_input`, or the generic one) left fields out. A
/// cut body does not count: its blob holds it whole, so a big call's content
/// is stored once. Agent text stays whole in its `AgentMessage` up to
/// [`MESSAGE_WHOLE_MAX_BYTES`]; a longer one is its head
/// ([`MESSAGE_PREVIEW_MAX_BYTES`]) and `full`, a blob holding it whole
/// ([`message`]).
///
/// **Generic display.** A call the adapter gives no display for is shown as
/// kind [`ToolKind::Other`] titled with its input: the input itself if it is
/// a string, else its first string field, else its compact JSON.
///
/// **Approvals.** An approval for a known call is summarized by that call's
/// title and carries no input when its input equals the call's; otherwise
/// its input is a blob. An approval for no known call is summarized like a
/// generic display and blobs its input under the same rule. The mapper
/// remembers which [`AgentRequestId`] each [`ApprovalId`] it logged stands
/// for. When an `ApprovalResolved` for the session appears in the log, the
/// supervisor calls [`EventMapper::take_approval`] for the agent's id and
/// answers the agent.
///
/// What a turn's end cancels is the adapter's rule: it cancels the turn's
/// own leftover approvals before `TurnEnded`, and a background task's when
/// the task leaves the set, so a task's approval can sit pending in an idle
/// session. The `TurnEnded` cancel is only a backstop for the turn's own
/// approvals an adapter left; it never touches a task's.
///
/// Blocking: it runs SQLite and blob writes.
#[derive(Debug)]
pub struct EventMapper {
    session_id: SessionId,
    /// The session context the agent runs in, whose resume handle it
    /// stores.
    context: ContextId,
    /// Approvals this mapper logged that no answer has taken yet, oldest
    /// first.
    approvals: Vec<LoggedApproval>,
    /// Calls logged and remembered, by the agent's id (see **Calls.**).
    calls: HashMap<AgentCallId, LoggedCall>,
    /// Calls logged with no agent id, all open: no result can name them.
    unlinked: Vec<UnlinkedCall>,
    /// The calls any task reported since start has run: the roots.
    roots: HashSet<AgentCallId>,
    /// The calls the tasks of the last reported set run.
    live: HashSet<AgentCallId>,
    /// A `TurnStarted` has been seen and its `TurnEnded` not yet.
    turn_open: bool,
    /// The session's mode, model and effort as the log has them.
    settings: AgentSettings,
    /// The session's background tasks and schedules as the log has them.
    work: AgentWork,
    /// Mode requests the agent has not answered, oldest first, with the
    /// command asking.
    asked_modes: VecDeque<(PermissionMode, Option<CommandId>)>,
    /// Model requests likewise.
    asked_models: VecDeque<(String, Option<CommandId>)>,
    /// Effort requests likewise.
    asked_efforts: VecDeque<(Effort, Option<CommandId>)>,
    /// Task stops the agent has not answered, oldest first, with the
    /// command asking.
    asked_stops: VecDeque<(TaskId, Option<CommandId>)>,
}

/// An approval the mapper logged that no answer has taken yet.
#[derive(Debug)]
struct LoggedApproval {
    request: AgentRequestId,
    id: ApprovalId,
    /// A background task asked, not the turn: the turn's end leaves it.
    of_task: bool,
}

/// What the mapper keeps of a logged call to link its result and approval.
#[derive(Debug)]
struct LoggedCall {
    id: CallId,
    seq: Seq,
    /// The call that started the subagent that made it, as remembered when
    /// it was logged.
    parent: Option<AgentCallId>,
    /// Its result came, or it was named unanswered.
    settled: bool,
    /// The title as logged.
    title: String,
    input: Value,
}

/// A logged call the agent gave no id.
#[derive(Debug)]
struct UnlinkedCall {
    id: CallId,
    seq: Seq,
    parent: Option<AgentCallId>,
}

impl EventMapper {
    /// A mapper for `session_id`, whose log holds `settings` and `work`,
    /// running its agent in session context `context`.
    pub fn new(
        session_id: SessionId,
        context: ContextId,
        settings: AgentSettings,
        work: AgentWork,
    ) -> EventMapper {
        EventMapper {
            session_id,
            context,
            approvals: Vec::new(),
            calls: HashMap::new(),
            unlinked: Vec::new(),
            roots: HashSet::new(),
            live: HashSet::new(),
            turn_open: false,
            settings,
            work,
            asked_modes: VecDeque::new(),
            asked_models: VecDeque::new(),
            asked_efforts: VecDeque::new(),
            asked_stops: VecDeque::new(),
        }
    }

    /// The agent was asked to run in `mode`, for `command` if one asked.
    pub fn asked_mode(&mut self, mode: PermissionMode, command: Option<CommandId>) {
        self.asked_modes.push_back((mode, command));
    }

    /// The agent was asked to run `model`, for `command` if one asked.
    pub fn asked_model(&mut self, model: String, command: Option<CommandId>) {
        self.asked_models.push_back((model, command));
    }

    /// The agent was asked to run at `effort`, for `command` if one asked.
    pub fn asked_effort(&mut self, effort: Effort, command: Option<CommandId>) {
        self.asked_efforts.push_back((effort, command));
    }

    /// The agent was asked to stop `task`, for `command` if one asked.
    pub fn asked_stop(&mut self, task: TaskId, command: Option<CommandId>) {
        self.asked_stops.push_back((task, command));
    }

    /// A task stop the agent was asked for has no answer yet.
    pub fn stops_pending(&self) -> bool {
        !self.asked_stops.is_empty()
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    /// The session context the agent runs in. Everything the mapper logs
    /// is stamped with it.
    pub fn context(&self) -> ContextId {
        self.context
    }

    /// A turn has started and not yet ended.
    pub fn turn_open(&self) -> bool {
        self.turn_open
    }

    /// Approvals logged by this mapper that are still waiting for an answer.
    pub fn pending_approvals(&self) -> impl Iterator<Item = ApprovalId> + '_ {
        self.approvals.iter().map(|a| a.id)
    }

    /// The agent's id for approval `id`, forgetting it: call once when the
    /// log shows `id` resolved, then answer the agent. `None` if `id` is not
    /// this mapper's or was already taken or cancelled.
    pub fn take_approval(&mut self, id: ApprovalId) -> Option<AgentRequestId> {
        let index = self.approvals.iter().position(|a| a.id == id)?;
        Some(self.approvals.remove(index).request)
    }

    /// Records `event` in the log and returns the seqs it appended, in order.
    pub fn apply(
        &mut self,
        log: &EventLog,
        storage: &mut Storage,
        event: AdapterEvent,
    ) -> Result<Vec<Seq>> {
        let mut seqs = Vec::new();
        match event {
            AdapterEvent::TurnStarted { .. } => {
                if !self.turn_open {
                    self.turn_open = true;
                    seqs.push(self.append(
                        log,
                        storage,
                        EventKind::SessionStateChanged(SessionState::AgentWorking),
                    )?);
                }
            }
            AdapterEvent::Text { text, parent } => {
                let parent = self.linked(parent.as_ref()).map(|c| c.id);
                let (text, full) = message(&mut stored(storage), text)?;
                seqs.push(self.append(
                    log,
                    storage,
                    EventKind::AgentMessage { parent, text, full },
                )?);
            }
            AdapterEvent::ToolCall {
                call,
                parent,
                name,
                input,
                display,
            } => {
                let id = CallId::new();
                let (display, input_blob) = call_display(&mut stored(storage), &input, display)?;
                let parent = parent.filter(|p| self.linked(Some(p)).is_some());
                let title = display.title.clone();
                let seq = self.append(
                    log,
                    storage,
                    EventKind::ToolCall(ToolCall {
                        parent: parent.as_ref().map(|p| self.calls[p].id),
                        call: Some(id),
                        name,
                        display,
                        input: input_blob,
                    }),
                )?;
                seqs.push(seq);
                match call {
                    Some(agent_id) => {
                        let logged = LoggedCall {
                            id,
                            seq,
                            parent,
                            settled: false,
                            title,
                            input,
                        };
                        if let Some(earlier) = self.calls.insert(agent_id, logged) {
                            tracing::debug!(call = %earlier.id.0, "agent reused a call id; linking the newer call");
                            // No result can name the earlier one now.
                            if !earlier.settled {
                                self.unlinked.push(UnlinkedCall {
                                    id: earlier.id,
                                    seq: earlier.seq,
                                    parent: earlier.parent,
                                });
                            }
                        }
                    }
                    None => self.unlinked.push(UnlinkedCall { id, seq, parent }),
                }
            }
            AdapterEvent::ToolResult { call, result } => {
                let call = match call.as_ref().and_then(|c| self.calls.get_mut(c)) {
                    Some(logged) => {
                        logged.settled = true;
                        Some(logged.id)
                    }
                    None => {
                        self.linked(call.as_ref());
                        None
                    }
                };
                let result = tool_result(&mut stored(storage), call, result)?;
                seqs.push(self.append(log, storage, EventKind::ToolResult(result))?);
            }
            AdapterEvent::ApprovalRequested {
                request,
                call,
                tool,
                input,
                suggestions,
                task,
            } => {
                let (call, summary, input) = match self.linked(call.as_ref()) {
                    Some(logged) => {
                        let blob = if logged.input == input {
                            None
                        } else {
                            Some(input_blob(&mut stored(storage), &input)?)
                        };
                        (Some(logged.id), logged.title.clone(), blob)
                    }
                    None => {
                        let (summary, shown_whole) = generic_title(&input);
                        let blob = if shown_whole {
                            None
                        } else {
                            Some(input_blob(&mut stored(storage), &input)?)
                        };
                        (None, summary, blob)
                    }
                };
                let id = ApprovalId::new();
                seqs.push(self.append(
                    log,
                    storage,
                    EventKind::ApprovalRequested {
                        id,
                        call,
                        tool,
                        summary,
                        input,
                        suggestions: bounded_suggestions(suggestions),
                    },
                )?);
                self.approvals.push(LoggedApproval {
                    request,
                    id,
                    of_task: task.is_some(),
                });
            }
            AdapterEvent::ApprovalCancelled { request, reason } => {
                match self.approvals.iter().position(|a| a.request == request) {
                    Some(index) => {
                        let id = self.approvals.remove(index).id;
                        seqs.extend(log.cancel_approval(&mut storage.db, id, &reason)?);
                    }
                    None => {
                        tracing::debug!(request = %request.0, "agent cancelled an approval that is not pending");
                    }
                }
            }
            AdapterEvent::TurnUsage(usage) => {
                if self.turn_open && !usage.is_empty() {
                    seqs.push(self.append(log, storage, EventKind::TurnUsage(usage))?);
                } else {
                    tracing::debug!("dropping turn usage outside a turn or with nothing in it");
                }
            }
            // Inside a turn (a foreground subagent) or outside any (a
            // background one): its call is what says whose figures they are.
            AdapterEvent::SubagentUsage { call, usage } => {
                if let Some(logged) = self.linked(Some(&call)) {
                    let kind = EventKind::SubagentUsage(proto::SubagentUsage {
                        call: logged.id,
                        duration_ms: usage.duration_ms,
                        tool_uses: usage.tool_uses,
                        context_tokens: usage.context_tokens,
                    });
                    seqs.push(self.append(log, storage, kind)?);
                }
            }
            AdapterEvent::TurnEnded(outcome) => {
                self.turn_open = false;
                let (reason, state) = match outcome {
                    TurnOutcome::Completed => (TURN_ENDED, SessionState::Idle),
                    TurnOutcome::Interrupted => (TURN_INTERRUPTED, SessionState::Idle),
                    TurnOutcome::Failed(message) => (
                        TURN_ENDED,
                        SessionState::Errored {
                            message: cut_line(&message, REFUSAL_MAX_CHARS).0,
                        },
                    ),
                };
                // A backstop: the adapter cancels the turn's own first.
                self.cancel_pending(log, storage, reason, |a| !a.of_task, &mut seqs)?;
                // The turn's calls: those of no live task.
                let live = self.live.clone();
                let turns = |owner: Option<&AgentCallId>| owner.is_none_or(|o| !live.contains(o));
                self.abandon(log, storage, turns, true, &mut seqs)?;
                seqs.push(self.append(log, storage, EventKind::SessionStateChanged(state))?);
            }
            AdapterEvent::ResumeHandle(handle) => {
                store_resume_handle(&storage.db, self.context, &handle)?;
            }
            AdapterEvent::Grants(grants) => {
                store_grants(&storage.db, self.session_id, &grants)?;
            }
            AdapterEvent::PermissionMode(mode) => {
                if self.settings.mode.current != Some(mode) {
                    let kind = EventKind::PermissionModeChanged {
                        mode,
                        answers: None,
                    };
                    seqs.push(self.append(log, storage, kind)?);
                }
            }
            AdapterEvent::PermissionModeSet(mode) => {
                let answers = self.answer_mode();
                // Restoring what the log has, for no request, says nothing new.
                if answers.is_some() || self.settings.mode.current != Some(mode) {
                    let kind = EventKind::PermissionModeChanged { mode, answers };
                    seqs.push(self.append(log, storage, kind)?);
                }
            }
            AdapterEvent::PermissionModeRefused { mode, reason } => {
                let kind = EventKind::PermissionModeRefused {
                    mode,
                    reason: cut_line(&reason, REFUSAL_MAX_CHARS).0,
                    answers: self.answer_mode(),
                };
                seqs.push(self.append(log, storage, kind)?);
            }
            AdapterEvent::Model(model) => {
                let model = cut_line(&model, MODEL_MAX_CHARS).0;
                if !model.is_empty() && self.settings.model.current.as_ref() != Some(&model) {
                    let kind = EventKind::ModelChanged {
                        model,
                        answers: None,
                    };
                    seqs.push(self.append(log, storage, kind)?);
                }
            }
            AdapterEvent::ModelSet(model) => {
                let model = cut_line(&model, MODEL_MAX_CHARS).0;
                let answers = self.answer_model();
                if answers.is_some() || self.settings.model.current.as_ref() != Some(&model) {
                    let kind = EventKind::ModelChanged { model, answers };
                    seqs.push(self.append(log, storage, kind)?);
                }
            }
            AdapterEvent::ModelRefused { model, reason } => {
                let kind = EventKind::ModelRefused {
                    model: cut_line(&model, MODEL_MAX_CHARS).0,
                    reason: cut_line(&reason, REFUSAL_MAX_CHARS).0,
                    answers: self.answer_model(),
                };
                seqs.push(self.append(log, storage, kind)?);
            }
            AdapterEvent::Effort(effort) => {
                if self.settings.effort.current != Some(effort) {
                    let kind = EventKind::EffortChanged {
                        effort,
                        answers: None,
                    };
                    seqs.push(self.append(log, storage, kind)?);
                }
            }
            AdapterEvent::EffortSet(effort) => {
                let answers = self.answer_effort();
                if answers.is_some() || self.settings.effort.current != Some(effort) {
                    let kind = EventKind::EffortChanged { effort, answers };
                    seqs.push(self.append(log, storage, kind)?);
                }
            }
            AdapterEvent::EffortRefused { effort, reason } => {
                let kind = EventKind::EffortRefused {
                    effort,
                    reason: cut_line(&reason, REFUSAL_MAX_CHARS).0,
                    answers: self.answer_effort(),
                };
                seqs.push(self.append(log, storage, kind)?);
            }
            AdapterEvent::ModelsOffered(_) => {
                tracing::debug!("models offered reach the log through the supervisor");
            }
            AdapterEvent::VersionUnconfirmed { version, confirmed } => {
                let kind = EventKind::AgentVersionUnconfirmed {
                    version: cut_line(&version, AGENT_VERSION_MAX_CHARS).0,
                    confirmed: cut_line(&confirmed, AGENT_VERSION_MAX_CHARS).0,
                };
                seqs.push(self.append(log, storage, kind)?);
            }
            AdapterEvent::BackgroundTasks(tasks) => {
                // The whole report, not the cut set: a task cut from the
                // log still runs its call.
                let live: HashSet<AgentCallId> =
                    tasks.iter().filter_map(|t| t.call.clone()).collect();
                self.roots.extend(live.iter().cloned());
                let left: HashSet<AgentCallId> = self.live.difference(&live).cloned().collect();
                self.live = live;
                if !left.is_empty() {
                    let forget = !self.turn_open;
                    let of_left =
                        |owner: Option<&AgentCallId>| owner.is_some_and(|o| left.contains(o));
                    self.abandon(log, storage, of_left, forget, &mut seqs)?;
                }
                let tasks = cut_tasks(tasks, &self.calls);
                if tasks != self.work.tasks {
                    seqs.push(self.append(
                        log,
                        storage,
                        EventKind::BackgroundTasksChanged { tasks },
                    )?);
                }
            }
            AdapterEvent::Schedules(schedules) => {
                let schedules = cut_schedules(schedules);
                if schedules != self.work.schedules {
                    seqs.push(self.append(
                        log,
                        storage,
                        EventKind::SchedulesChanged { schedules },
                    )?);
                }
            }
            AdapterEvent::TaskStopAnswered { task } => {
                if self.answer_stop().is_none() {
                    tracing::debug!(task, "agent answered a stop nobody asked");
                }
            }
            AdapterEvent::TaskStopRefused { task, reason } => {
                let (task, answers) = match self.answer_stop() {
                    Some(asked) => asked,
                    None => {
                        tracing::debug!(task, "agent refused a stop nobody asked");
                        (TaskId(cut_line(&task, TASK_ID_MAX_CHARS).0), None)
                    }
                };
                let kind = EventKind::TaskStopRefused {
                    task,
                    reason: cut_line(&reason, REFUSAL_MAX_CHARS).0,
                    answers,
                };
                seqs.push(self.append(log, storage, kind)?);
            }
            AdapterEvent::Exited { error, unsent } => {
                self.cancel_pending(log, storage, AGENT_EXITED, |_| true, &mut seqs)?;
                self.refuse_asked(log, storage, &mut seqs)?;
                // The empty set logged below settles them.
                self.asked_stops.clear();
                self.abandon(log, storage, |_| true, true, &mut seqs)?;
                self.calls.clear();
                self.roots.clear();
                self.live.clear();
                // Nothing outlives the process, durable schedules included.
                if !self.work.tasks.is_empty() {
                    let kind = EventKind::BackgroundTasksChanged { tasks: Vec::new() };
                    seqs.push(self.append(log, storage, kind)?);
                }
                if !self.work.schedules.is_empty() {
                    let kind = EventKind::SchedulesChanged {
                        schedules: Vec::new(),
                    };
                    seqs.push(self.append(log, storage, kind)?);
                }
                let exit = match error {
                    Some(error) => Some(format!("agent exited: {error}")),
                    None if self.turn_open => Some("agent exited during a turn".to_owned()),
                    None => None,
                };
                self.turn_open = false;
                // One `Errored` for the exit: the notice comes last and is
                // never cut, the exit's reason giving way to it.
                let message = match (exit, unsent_notice(unsent)) {
                    (Some(exit), Some(notice)) => {
                        let room = REFUSAL_MAX_CHARS.saturating_sub(notice.chars().count() + 2);
                        Some(format!("{}; {notice}", cut_line(&exit, room).0))
                    }
                    (exit, notice) => exit.or(notice),
                };
                if let Some(message) = message {
                    let message = cut_line(&message, REFUSAL_MAX_CHARS).0;
                    seqs.push(self.append(
                        log,
                        storage,
                        EventKind::SessionStateChanged(SessionState::Errored { message }),
                    )?);
                }
            }
        }
        Ok(seqs)
    }

    fn append(&mut self, log: &EventLog, storage: &mut Storage, kind: EventKind) -> Result<Seq> {
        if AgentSettings::folds(&kind) {
            self.settings.apply(None, &kind);
        }
        if AgentWork::folds(&kind) {
            self.work.apply(&kind);
        }
        log.append(&mut storage.db, self.session_id, self.context, kind)
    }

    /// The command of the oldest mode request, now answered.
    fn answer_mode(&mut self) -> Option<CommandId> {
        let asked = self.asked_modes.pop_front();
        if asked.is_none() {
            tracing::debug!("agent answered a mode request nobody asked");
        }
        asked.and_then(|(_, command)| command)
    }

    /// The command of the oldest model request, now answered.
    fn answer_model(&mut self) -> Option<CommandId> {
        let asked = self.asked_models.pop_front();
        if asked.is_none() {
            tracing::debug!("agent answered a model request nobody asked");
        }
        asked.and_then(|(_, command)| command)
    }

    /// The command of the oldest effort request, now answered.
    fn answer_effort(&mut self) -> Option<CommandId> {
        let asked = self.asked_efforts.pop_front();
        if asked.is_none() {
            tracing::debug!("agent answered an effort request nobody asked");
        }
        asked.and_then(|(_, command)| command)
    }

    /// The oldest stop asked, now answered, with its command.
    fn answer_stop(&mut self) -> Option<(TaskId, Option<CommandId>)> {
        self.asked_stops.pop_front()
    }

    /// Refuses every mode, model and effort request the agent left
    /// unanswered.
    fn refuse_asked(
        &mut self,
        log: &EventLog,
        storage: &mut Storage,
        seqs: &mut Vec<Seq>,
    ) -> Result<()> {
        for (mode, answers) in std::mem::take(&mut self.asked_modes) {
            let kind = EventKind::PermissionModeRefused {
                mode,
                reason: AGENT_EXITED.to_owned(),
                answers,
            };
            seqs.push(self.append(log, storage, kind)?);
        }
        for (model, answers) in std::mem::take(&mut self.asked_models) {
            let kind = EventKind::ModelRefused {
                model,
                reason: AGENT_EXITED.to_owned(),
                answers,
            };
            seqs.push(self.append(log, storage, kind)?);
        }
        for (effort, answers) in std::mem::take(&mut self.asked_efforts) {
            let kind = EventKind::EffortRefused {
                effort,
                reason: AGENT_EXITED.to_owned(),
                answers,
            };
            seqs.push(self.append(log, storage, kind)?);
        }
        Ok(())
    }

    /// The call remembered for the agent's `call`, if any.
    fn linked(&self, call: Option<&AgentCallId>) -> Option<&LoggedCall> {
        let call = call?;
        let logged = self.calls.get(call);
        if logged.is_none() {
            tracing::debug!(call = %call.0, "agent named a call not logged or no longer remembered");
        }
        logged
    }

    /// The root that owns a call made under `parent`: the nearest of
    /// `parent` and its ancestors some task runs. `None` for the turn's.
    fn owner<'a>(&'a self, parent: Option<&'a AgentCallId>) -> Option<&'a AgentCallId> {
        let mut at = parent;
        // Bounded: a reused id could close a loop.
        for _ in 0..=self.calls.len() {
            let id = at?;
            if self.roots.contains(id) {
                return Some(id);
            }
            at = self.calls.get(id).and_then(|c| c.parent.as_ref());
        }
        None
    }

    /// Logs one `CallsUnanswered` for the open calls whose owner `owned`
    /// picks, in log order, settling them; nothing if none is open. With
    /// `forget`, the picked calls are forgotten too, roots aside.
    fn abandon(
        &mut self,
        log: &EventLog,
        storage: &mut Storage,
        owned: impl Fn(Option<&AgentCallId>) -> bool,
        forget: bool,
        seqs: &mut Vec<Seq>,
    ) -> Result<()> {
        let picked: Vec<AgentCallId> = self
            .calls
            .iter()
            .filter(|(_, call)| owned(self.owner(call.parent.as_ref())))
            .map(|(id, _)| id.clone())
            .collect();
        let unlinked: Vec<bool> = self
            .unlinked
            .iter()
            .map(|call| owned(self.owner(call.parent.as_ref())))
            .collect();
        let mut named = Vec::new();
        for id in picked {
            let Some(call) = self.calls.get_mut(&id) else {
                continue;
            };
            if !call.settled {
                call.settled = true;
                named.push((call.seq, call.id));
            }
            if forget && !self.roots.contains(&id) {
                self.calls.remove(&id);
            }
        }
        // Named once, an unlinked call has nothing left to settle.
        let mut picks = unlinked.into_iter();
        self.unlinked.retain(|call| {
            let pick = picks.next().unwrap_or(false);
            if pick {
                named.push((call.seq, call.id));
            }
            !pick
        });
        if named.is_empty() {
            return Ok(());
        }
        named.sort();
        let calls = named.into_iter().map(|(_, id)| id).collect();
        seqs.push(self.append(log, storage, EventKind::CallsUnanswered { calls })?);
        Ok(())
    }

    /// Cancels every approval still pending that `which` picks, oldest
    /// first. One already answered in the log stays answered (D5).
    fn cancel_pending(
        &mut self,
        log: &EventLog,
        storage: &mut Storage,
        reason: &str,
        which: impl Fn(&LoggedApproval) -> bool,
        seqs: &mut Vec<Seq>,
    ) -> Result<()> {
        let (cancelled, kept) = std::mem::take(&mut self.approvals)
            .into_iter()
            .partition(which);
        self.approvals = kept;
        for approval in cancelled {
            seqs.extend(log.cancel_approval(&mut storage.db, approval.id, reason)?);
        }
        Ok(())
    }
}

// The bounding helpers below take a blob sink, `put`, for the content they
// leave out of an event: it stores the bytes as a blob and returns what the
// event carries in its place, [`Full::Blob`], or [`Full::Lost`] when the
// content was not kept (the event then has only its cut preview, and says
// so). The live mapper's sink records each blob as it is stored
// ([`Storage::put_blob`]); a caller that must record its blobs later, in one
// transaction with its events, can collect them instead.

/// The live mapper's blob sink: each blob recorded in `storage` as stored.
///
/// A blob that cannot be stored (a full disk, the `blobs` row refused) is
/// [`Full::Lost`], never fatal: the event is still logged, with its cut
/// preview and the marker (its `lines` still counts all of it), so a head
/// tells the user the rest is gone rather than showing the preview as the
/// whole. Failing the supervisor instead would end every running turn for
/// content a device fetches only on demand. The append that follows still
/// fails, and is fatal, when the database itself cannot be written.
fn stored(storage: &Storage) -> impl FnMut(&[u8]) -> Result<Full> + '_ {
    |bytes| match storage.put_blob(bytes) {
        Ok(blob) => Ok(Full::Blob(blob)),
        Err(error) => {
            tracing::error!(%error, bytes = bytes.len(), "cannot keep content as a blob; logging the event without it");
            Ok(Full::Lost)
        }
    }
}

/// The bounded display for a call and, when it does not carry all of the
/// input, the input as a blob.
///
/// A cut body does not make the input a blob: the body's own blob
/// (`Body::full`) holds its whole text, as a cut title's stand-in body does,
/// so a big Write, Edit or shell command is stored once and deleting that
/// blob frees its bytes. Only what no event field holds whole makes one: a
/// field the adapter's display left out (`shows_all_input`), a cut detail, or
/// a cut title beside a body.
pub(crate) fn call_display(
    put: &mut impl FnMut(&[u8]) -> Result<Full>,
    input: &Value,
    display: Option<ToolDisplay>,
) -> Result<(proto::ToolDisplay, Option<Full>)> {
    let (display, carried_whole) = match display {
        Some(display) => {
            let omits_input = !display.shows_all_input;
            let (title, title_cut) = cut_line(&display.title, TITLE_MAX_CHARS);
            let (detail, detail_cut) = match &display.detail {
                Some(detail) => {
                    let (detail, cut) = cut_line(detail, DETAIL_MAX_CHARS);
                    (Some(detail), cut)
                }
                None => (None, false),
            };
            let (body, title_cut) = match display.body {
                Some(body) => (Some(display_body(put, body)?), title_cut),
                None => (title_body(put, &display.title, title_cut)?, false),
            };
            let display = proto::ToolDisplay {
                kind: display.kind,
                title,
                detail,
                body,
            };
            (display, !(omits_input || title_cut || detail_cut))
        }
        None => {
            let (text, whole) = generic_text(input);
            let (title, title_cut) = cut_line(&text, TITLE_MAX_CHARS);
            let body = title_body(put, &text, title_cut)?;
            let display = proto::ToolDisplay {
                kind: ToolKind::Other,
                title,
                detail: None,
                body,
            };
            (display, whole)
        }
    };
    let blob = if carried_whole {
        None
    } else {
        Some(input_blob(put, input)?)
    };
    Ok((display, blob))
}

/// The body standing in for a cut title: its whole text, so a long or
/// multi-line title (a shell command) is never shown in part only.
fn title_body(
    put: &mut impl FnMut(&[u8]) -> Result<Full>,
    title: &str,
    cut: bool,
) -> Result<Option<Body>> {
    if !cut {
        return Ok(None);
    }
    let body = DisplayBody {
        format: BodyFormat::Text,
        text: title.trim().to_owned(),
    };
    display_body(put, body).map(Some)
}

/// A body's preview, with its text as a blob when the preview is shorter.
pub(crate) fn display_body(
    put: &mut impl FnMut(&[u8]) -> Result<Full>,
    body: DisplayBody,
) -> Result<Body> {
    let (preview, cut) = cut_preview(&body.text);
    let full = if cut {
        Some(put(body.text.as_bytes())?)
    } else {
        None
    };
    Ok(Body {
        format: body.format,
        preview,
        lines: line_count(&body.text),
        full,
    })
}

/// A result's preview (of the adapter's preview text, else of the output),
/// with the whole output when the preview does not hold all of it: inline
/// up to [`INLINE_MAX_BYTES`], else as a blob. An adapter's preview nearly
/// always differs from its output (notes for the model, line numbers, a
/// header), so without the inline case nearly every result made a blob.
pub(crate) fn tool_result(
    put: &mut impl FnMut(&[u8]) -> Result<Full>,
    call: Option<CallId>,
    result: ResultDisplay,
) -> Result<ToolResult> {
    let shown = result.preview.as_deref().unwrap_or(&result.output);
    let (preview, cut) = cut_preview(shown);
    let whole = !cut && shown == result.output;
    let lines = line_count(&result.output);
    let output = if whole {
        None
    } else if result.output.len() <= INLINE_MAX_BYTES {
        Some(Full::Inline(result.output))
    } else {
        Some(put(result.output.as_bytes())?)
    };
    Ok(ToolResult {
        call,
        is_error: result.is_error,
        status: result
            .status
            .map(|status| cut_line(&status, STATUS_MAX_CHARS).0),
        preview,
        lines,
        output,
    })
}

/// Title of the generic display for `input`, cut, and whether it shows all
/// of the input (see [`generic_text`]).
fn generic_title(input: &Value) -> (String, bool) {
    let (text, whole) = generic_text(input);
    let (title, cut) = cut_line(&text, TITLE_MAX_CHARS);
    (title, whole && !cut)
}

/// Title text of the generic display for `input`, uncut, and whether it
/// holds all of the input: the input itself if it is a string, else its
/// first string field, else its compact JSON.
fn generic_text(input: &Value) -> (String, bool) {
    match input {
        Value::String(text) => (text.clone(), true),
        Value::Object(fields) => match fields.values().find_map(Value::as_str) {
            Some(text) => (text.to_owned(), fields.len() == 1),
            None => (input.to_string(), true),
        },
        Value::Null => (String::new(), true),
        _ => (input.to_string(), true),
    }
}

/// `input` stored whole: a string as itself, anything else pretty-printed.
pub(crate) fn input_blob(
    put: &mut impl FnMut(&[u8]) -> Result<Full>,
    input: &Value,
) -> Result<Full> {
    let bytes = match input {
        Value::String(text) => text.clone().into_bytes(),
        _ => serde_json::to_vec_pretty(input)?,
    };
    put(&bytes)
}

/// What an `Errored` says of `unsent` messages an agent accepted and exited
/// without beginning ([`AdapterEvent::Exited`]); `None` for none. They are
/// reported, not redelivered: after a stop the user chose to end the work,
/// and after a crash or kill delivery stays at most once (architecture.md
/// "Agent supervisor"), so the user decides whether to send them again.
pub(crate) fn unsent_notice(unsent: usize) -> Option<String> {
    match unsent {
        0 => None,
        1 => Some("1 queued message was not sent to the agent; send it again".to_owned()),
        n => Some(format!(
            "{n} queued messages were not sent to the agent; send them again"
        )),
    }
}

/// The first non-blank line of `text`, trimmed, at most `max_chars` chars,
/// and whether anything but surrounding whitespace was left out. A cut line
/// ends in `…` (within `max_chars`).
pub(crate) fn cut_line(text: &str, max_chars: usize) -> (String, bool) {
    let text = text.trim();
    let line = text.lines().next().unwrap_or("").trim_end();
    let mut out: String = line.chars().take(max_chars).collect();
    let cut = line.len() < text.len() || out.len() < line.len();
    if cut {
        if out.chars().count() >= max_chars {
            out.pop();
        }
        out.push('…');
    }
    (out, cut)
}

/// The first [`TASKS_MAX`] tasks, in the agent's order, titles cut to one
/// line, `call` linked through `calls` (an unknown one is `None`). An entry
/// whose id is too long is dropped: a cut id names nothing.
/// A parent id too long drops the entry's parent link, not the entry: the
/// task still runs, and it is the link that names nothing.
fn cut_tasks(
    tasks: Vec<AgentTask>,
    calls: &HashMap<AgentCallId, LoggedCall>,
) -> Vec<BackgroundTask> {
    let fits = |id: &str| id.chars().count() <= TASK_ID_MAX_CHARS;
    let linked = |call: Option<AgentCallId>| {
        let call = call?;
        let logged = calls.get(&call);
        if logged.is_none() {
            tracing::debug!(call = %call.0, "a task runs a call not logged");
        }
        logged.map(|c| c.id)
    };
    tasks
        .into_iter()
        .filter(|task| fits(&task.id))
        .take(TASKS_MAX)
        .map(|task| BackgroundTask {
            call: linked(task.call),
            id: proto::TaskId(task.id),
            kind: task.kind,
            title: cut_line(&task.title, TITLE_MAX_CHARS).0,
            parent: task.parent.filter(|id| fits(id)).map(proto::TaskId),
            ambient: task.ambient,
        })
        .collect()
}

/// The first [`SCHEDULES_MAX`] schedules, in the agent's order, `when` and
/// prompt cut to one line. An entry whose id is too long is dropped, as in
/// [`cut_tasks`].
fn cut_schedules(schedules: Vec<AgentSchedule>) -> Vec<Schedule> {
    schedules
        .into_iter()
        .filter(|schedule| schedule.id.chars().count() <= TASK_ID_MAX_CHARS)
        .take(SCHEDULES_MAX)
        .map(|schedule| Schedule {
            id: proto::ScheduleId(schedule.id),
            when: cut_line(&schedule.when, SCHEDULE_WHEN_MAX_CHARS).0,
            recurring: schedule.recurring,
            prompt: cut_line(&schedule.prompt, TITLE_MAX_CHARS).0,
            outlives_agent: schedule.outlives_agent,
        })
        .collect()
}

/// The first [`RULE_SUGGESTIONS_MAX`] suggestions, labels cut to one line
/// of [`RULE_LABEL_MAX_CHARS`]. Cutting the end keeps every index valid.
fn bounded_suggestions(mut suggestions: Vec<proto::RuleSuggestion>) -> Vec<proto::RuleSuggestion> {
    suggestions.truncate(RULE_SUGGESTIONS_MAX);
    for suggestion in &mut suggestions {
        suggestion.label = cut_line(&suggestion.label, RULE_LABEL_MAX_CHARS).0;
    }
    suggestions
}

/// The first [`MODELS_MAX`] models, each field cut to one line of its cap
/// and each effort level kept once, first place wins; a model with a blank
/// id is dropped.
pub(crate) fn bounded_models(models: Vec<ModelChoice>) -> Vec<ModelChoice> {
    models
        .into_iter()
        .filter_map(|model| {
            let id = cut_line(&model.id, MODEL_MAX_CHARS).0;
            let label = cut_line(&model.label, MODEL_LABEL_MAX_CHARS).0;
            let detail = model
                .detail
                .map(|detail| cut_line(&detail, MODEL_DETAIL_MAX_CHARS).0)
                .filter(|detail| !detail.is_empty());
            let resolved = model
                .resolved
                .map(|resolved| cut_line(&resolved, MODEL_MAX_CHARS).0)
                .filter(|resolved| !resolved.is_empty());
            let mut efforts = model.efforts;
            let mut seen = HashSet::new();
            efforts.retain(|effort| seen.insert(*effort));
            (!id.is_empty()).then(|| ModelChoice {
                label: if label.is_empty() { id.clone() } else { label },
                id,
                detail,
                resolved,
                efforts,
            })
        })
        .take(MODELS_MAX)
        .collect()
}

/// The head of `text`: at most [`PREVIEW_MAX_LINES`] lines and
/// [`PREVIEW_MAX_BYTES`] bytes, cut on a char boundary, and whether it
/// leaves anything out (a lone trailing newline does not count).
fn cut_preview(text: &str) -> (String, bool) {
    // The newline ending the last line kept, unless it ends the text too.
    let mut end = match text.match_indices('\n').nth(PREVIEW_MAX_LINES - 1) {
        Some((i, _)) if i + 1 < text.len() => i,
        _ => text.len(),
    };
    if end > PREVIEW_MAX_BYTES {
        end = PREVIEW_MAX_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
    }
    let cut = !matches!(&text[end..], "" | "\n");
    (text[..end].to_owned(), cut)
}

/// A message's `text` and `full`: the whole text up to
/// [`MESSAGE_WHOLE_MAX_BYTES`]; past it, its head of at most
/// [`MESSAGE_PREVIEW_MAX_BYTES`] (cut after the last line break in it, if
/// any, else on a char boundary) and the whole as a blob.
pub(crate) fn message(
    put: &mut impl FnMut(&[u8]) -> Result<Full>,
    text: String,
) -> Result<(String, Option<Full>)> {
    if text.len() <= MESSAGE_WHOLE_MAX_BYTES {
        return Ok((text, None));
    }
    let mut end = MESSAGE_PREVIEW_MAX_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    if let Some(at) = text[..end].rfind('\n') {
        end = at + 1;
    }
    let full = put(text.as_bytes())?;
    Ok((text[..end].to_owned(), Some(full)))
}

fn line_count(text: &str) -> u32 {
    u32::try_from(text.lines().count()).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_line_is_cut_on_a_char_boundary_with_an_ellipsis() {
        assert_eq!(cut_line("  ls -la \n", 120), ("ls -la".to_owned(), false));
        let (line, cut) = cut_line(&"é".repeat(300), TITLE_MAX_CHARS);
        assert!(cut);
        assert_eq!(line, format!("{}…", "é".repeat(TITLE_MAX_CHARS - 1)));
        assert_eq!(
            cut_line("\n first\nsecond", 120),
            ("first…".to_owned(), true)
        );
        assert_eq!(cut_line("", 120), (String::new(), false));
    }

    #[test]
    fn a_preview_keeps_the_head_within_lines_and_bytes() {
        let text: String = (0..20).map(|i| format!("line {i}\n")).collect();
        let (preview, cut) = cut_preview(&text);
        assert!(cut);
        assert_eq!(
            preview,
            text.lines()
                .take(PREVIEW_MAX_LINES)
                .collect::<Vec<_>>()
                .join("\n")
        );

        let (preview, cut) = cut_preview(&"é".repeat(400));
        assert!(cut);
        assert_eq!(preview, "é".repeat(PREVIEW_MAX_BYTES / 2));
        let (preview, _) = cut_preview(&format!("x{}", "é".repeat(400)));
        assert_eq!(preview.len(), PREVIEW_MAX_BYTES - 1);

        assert_eq!(cut_preview("a\nb\n"), ("a\nb\n".to_owned(), false));
        assert_eq!(cut_preview(""), (String::new(), false));
        let eight: String = (0..PREVIEW_MAX_LINES).map(|i| format!("{i}\n")).collect();
        assert!(!cut_preview(&eight).1);
    }

    #[test]
    fn a_body_stores_its_full_text_through_the_sink_only_when_cut() {
        let mut stored: Vec<Vec<u8>> = Vec::new();
        let blob = Full::Blob(proto::BlobRef {
            id: proto::BlobId::new(),
            size: 0,
            sha256: proto::ContentHash::of(b""),
        });
        let mut put = |bytes: &[u8]| {
            stored.push(bytes.to_vec());
            Ok(blob.clone())
        };

        let long: String = (0..=PREVIEW_MAX_LINES).map(|i| format!("{i}\n")).collect();
        let body = display_body(
            &mut put,
            DisplayBody {
                format: BodyFormat::Text,
                text: long.clone(),
            },
        )
        .unwrap();
        assert_eq!(body.full, Some(blob.clone()));

        let short = display_body(
            &mut put,
            DisplayBody {
                format: BodyFormat::Text,
                text: "a\nb\n".to_owned(),
            },
        )
        .unwrap();
        assert_eq!(short.full, None);
        assert_eq!(short.preview, "a\nb\n");

        assert_eq!(stored, vec![long.into_bytes()]);
    }

    #[test]
    fn a_small_output_travels_inline_and_only_a_large_one_makes_a_blob() {
        let mut stored: Vec<Vec<u8>> = Vec::new();
        let blob = Full::Blob(proto::BlobRef {
            id: proto::BlobId::new(),
            size: 0,
            sha256: proto::ContentHash::of(b""),
        });
        let mut put = |bytes: &[u8]| {
            stored.push(bytes.to_vec());
            Ok(blob.clone())
        };
        let result = |preview: Option<&str>, output: &str| ResultDisplay {
            is_error: false,
            status: None,
            preview: preview.map(str::to_owned),
            output: output.to_owned(),
        };

        // An edit: an empty preview beside the note to the model.
        let note = "The file a.txt has been updated. file state is current in your context";
        let edit = tool_result(&mut put, None, result(Some(""), note)).unwrap();
        assert_eq!(edit.preview, "");
        assert_eq!(edit.output, Some(Full::Inline(note.to_owned())));

        // Cut, but small enough to carry: the whole of it, inline.
        let long: String = (0..=PREVIEW_MAX_LINES).map(|i| format!("{i}\n")).collect();
        let cut = tool_result(&mut put, None, result(None, &long)).unwrap();
        assert_eq!(cut.output, Some(Full::Inline(long.clone())));
        assert_eq!(cut.lines, line_count(&long));

        // At the threshold still inline; one byte past it, a blob.
        let at = "x".repeat(INLINE_MAX_BYTES);
        let inline = tool_result(&mut put, None, result(Some(""), &at)).unwrap();
        assert_eq!(inline.output, Some(Full::Inline(at)));
        let past = "x".repeat(INLINE_MAX_BYTES + 1);
        let large = tool_result(&mut put, None, result(Some(""), &past)).unwrap();
        assert_eq!(large.output, Some(blob.clone()));

        // The preview holds it all: nothing more.
        let whole = tool_result(&mut put, None, result(None, "ok\n")).unwrap();
        assert_eq!((whole.preview.as_str(), whole.output), ("ok\n", None));

        assert_eq!(stored, vec![past.into_bytes()], "only the large one");
    }

    #[test]
    fn only_a_long_message_is_cut_to_its_head_beside_a_blob() {
        let mut stored: Vec<Vec<u8>> = Vec::new();
        let blob = Full::Blob(proto::BlobRef {
            id: proto::BlobId::new(),
            size: 0,
            sha256: proto::ContentHash::of(b""),
        });
        let mut put = |bytes: &[u8]| {
            stored.push(bytes.to_vec());
            Ok(blob.clone())
        };

        // At the threshold still whole.
        let at = "x".repeat(MESSAGE_WHOLE_MAX_BYTES);
        assert_eq!(message(&mut put, at.clone()).unwrap(), (at, None));

        // Past it: the head ends after the last line break in it.
        let line = format!("{}\n", "y".repeat(99));
        let long = line.repeat(MESSAGE_WHOLE_MAX_BYTES / 100 + 1);
        let (head, full) = message(&mut put, long.clone()).unwrap();
        assert_eq!(full, Some(blob.clone()));
        assert_eq!(head, line.repeat(MESSAGE_PREVIEW_MAX_BYTES / 100));
        assert!(long.starts_with(&head));

        // No line break in it: cut on a char boundary.
        let wide = "é".repeat(MESSAGE_WHOLE_MAX_BYTES);
        let (head, _) = message(&mut put, wide.clone()).unwrap();
        assert_eq!(head, "é".repeat(MESSAGE_PREVIEW_MAX_BYTES / 2));

        assert_eq!(stored, vec![long.into_bytes(), wide.into_bytes()]);
    }

    #[test]
    fn the_generic_title_is_the_first_string_field() {
        assert_eq!(
            generic_text(&json!({ "command": "ls" })),
            ("ls".to_owned(), true)
        );
        assert_eq!(
            generic_text(&json!({ "count": 3, "path": "a.txt", "text": "hi" })),
            ("a.txt".to_owned(), false)
        );
        assert_eq!(
            generic_text(&json!({ "count": 3 })),
            (r#"{"count":3}"#.to_owned(), true)
        );
        assert_eq!(generic_text(&json!("echo")), ("echo".to_owned(), true));
        assert_eq!(generic_text(&Value::Null), (String::new(), true));
    }

    #[test]
    fn bounded_models_keep_each_effort_once_in_order() {
        use skywave_proto::Effort;
        let choice = |id: &str, efforts| ModelChoice {
            id: id.into(),
            label: id.into(),
            detail: None,
            resolved: None,
            efforts,
        };
        let models = bounded_models(vec![
            choice(
                "a",
                vec![
                    Effort::High,
                    Effort::Low,
                    Effort::High,
                    Effort::Max,
                    Effort::Low,
                ],
            ),
            choice("b", Vec::new()),
        ]);
        assert_eq!(models[0].efforts, [Effort::High, Effort::Low, Effort::Max]);
        assert_eq!(models[1].efforts, []);
    }
}
