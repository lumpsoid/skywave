//! A scripted agent for deterministic server and sync tests (spec §9).
//!
//! [`FakeAgent`] implements [`AgentAdapter`] without any process. Its
//! [`FakeScript`] is a list of turns; each user message runs the next unused
//! turn of that session, and a message after the last scripted turn runs an
//! echo turn (`Text("echo: <message>")`). Every turn is framed by
//! `TurnStarted` and `TurnEnded`.
//!
//! A turn's [`Step`]s run in order without waiting, except:
//! - [`Step::Approval`] emits `ApprovalRequested` and blocks until the
//!   request is answered, then runs the `on_approve` or `on_deny` steps;
//! - [`Step::Hang`] blocks until interrupted, like a tool that never ends;
//! - [`Step::Wait`] blocks until the test opens a [`Gate`].
//!
//! [`Step::Wake`] schedules a turn the agent starts on its own, with no
//! message, like a CLI's scheduled wakeup: it runs once the session is idle
//! and its time has come, before any queued message, and is lost with the
//! session. [`Step::Later`] is the same without a turn: its events are
//! emitted between turns, like a CLI reporting that a background job
//! ended. A `Later` and a `Wake` due at the same time play in script order,
//! so `[Step::later(t, [no tasks]), Step::wake(t, ..)]` is a task that ends
//! and the turn the agent starts to report it. [`task`] makes an
//! [`AgentTask`] for [`Step::tasks`].
//!
//! An interrupt is seen while blocked, or between two steps; the rest of the
//! turn is dropped, a pending approval is reported `ApprovalCancelled`, and
//! the turn ends `Interrupted`. So a test that interrupts while the agent is
//! blocked gets the same events every run. Like a real agent, an interrupt
//! while idle does nothing and a message sent during a turn is queued for the
//! next one.
//!
//! The fake persists nothing itself but keeps per-session progress in the
//! [`FakeAgent`] (clones share it): the scripted turns run in order across
//! every start of a Skywave session, whatever its context, and every
//! request is recorded and readable with [`FakeAgent::inputs`], each start
//! as [`FakeInput::Started`] naming its context and the grants it got.
//!
//! **Grants.** The fake keeps no grants of its own: a script reports them
//! with [`Step::grants`] (the whole set, as the real adapter does when an
//! approval grants a rule), and the supervisor hands what it stored back in
//! the next start's [`StartSpec::grants`].
//!
//! **Conversations.** Each session context is its own conversation, which,
//! like a real agent's, exists only once it got a message: the fake reports
//! the context's [`ResumeHandle`] ([`FakeAgent::resume_handle`]) as it takes
//! the first message of a fresh start, not at the start itself. Starting a
//! context fresh after it got a message fails ("already exists; resume
//! it"); starting one that never got a message fresh again is fine. A
//! handle for another context is refused. A fork names the handle of a
//! context whose conversation exists, in any session (progress is kept per
//! session, so the lookup crosses them), and is recorded as
//! [`FakeConversation::Fork`] of that context; like a fresh start, it
//! reports the forking context's own handle at its first message. A fork of
//! a context with no conversation is refused, and so is forking into a
//! context that already has one.
//!
//! ```
//! use skywaved::agent::fake::{FakeAgent, FakeScript, Step};
//!
//! let agent = FakeAgent::new(FakeScript::new().turn([
//!     Step::text("Listing files"),
//!     Step::tool_call("shell", serde_json::json!({ "command": "ls" })),
//!     Step::tool_call("write", serde_json::json!({ "path": "a.txt" })).call("c1"),
//!     Step::approval("req-1", "write", serde_json::json!({ "path": "a.txt" }))
//!         .call("c1")
//!         .on_approve([Step::tool_result("written").call("c1")])
//!         .on_deny([Step::text("Not writing it.")]),
//! ]));
//! # let _ = agent;
//! ```
//!
//! Tool steps name no call unless given one with [`Step::call`], and carry
//! no display of their own, so the mapper's generic display applies.
//!
//! **Mode, model and effort.** A request to set any of them is answered at
//! once, idle or mid-turn: `PermissionModeSet` / `ModelSet` / `EffortSet`
//! with what was asked, or a refusal for a mode, model or effort the script
//! refuses ([`FakeScript::refuse_mode`], [`FakeScript::refuse_model`],
//! [`FakeScript::refuse_effort`]). A start naming a mode, model or effort
//! asks for it the same way, after reporting the models the script offers
//! ([`FakeScript::offer_models`]), if any.
//!
//! **Stopping a task.** A stop is answered at once, idle or mid-turn, like
//! the CLI's ack: `TaskStopAnswered`, then, between turns, the last task
//! set the fake reported without that task (nothing when it was not in
//! the set, as the CLI acks an unknown id). [`FakeScript::refuse_stops`]
//! makes it answer every stop `TaskStopRefused` instead.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{Value, json};
use skywave_proto::{ContextId, Decision, SessionId, TaskKind};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use uuid::Uuid;

use super::{
    AdapterEvent, AdapterEvents, AgentAdapter, AgentCallId, AgentError, AgentRequestId,
    AgentSchedule, AgentSession, AgentTask, Conversation, Effort, Grants, ModelChoice,
    PermissionMode, ResultDisplay, ResumeHandle, RuleScope, RuleSuggestion, StartSpec, TurnOutcome,
};

/// Capacity of a fake session's event channel.
const EVENT_CAPACITY: usize = 64;

/// The reason a fake reports for an approval it drops on interrupt.
pub const INTERRUPT_REASON: &str = "interrupted";
/// The reason a fake refuses a mode, model, effort or task stop with.
pub const REFUSE_REASON: &str = "refused by the script";

/// Turns a [`FakeAgent`] plays, in order, for every session.
#[derive(Debug, Clone, Default)]
pub struct FakeScript {
    turns: Vec<Vec<Step>>,
    refused_modes: Vec<PermissionMode>,
    refused_models: Vec<String>,
    refused_efforts: Vec<Effort>,
    models: Vec<ModelChoice>,
    refuse_stops: bool,
}

impl FakeScript {
    pub fn new() -> FakeScript {
        FakeScript::default()
    }

    /// Refuses requests for `mode`.
    pub fn refuse_mode(mut self, mode: PermissionMode) -> FakeScript {
        self.refused_modes.push(mode);
        self
    }

    /// Refuses requests for `model`.
    pub fn refuse_model(mut self, model: &str) -> FakeScript {
        self.refused_models.push(model.to_owned());
        self
    }

    /// Refuses requests for `effort`.
    pub fn refuse_effort(mut self, effort: Effort) -> FakeScript {
        self.refused_efforts.push(effort);
        self
    }

    /// Refuses every task stop.
    pub fn refuse_stops(mut self) -> FakeScript {
        self.refuse_stops = true;
        self
    }

    /// Reports `models` as offered whenever a session starts.
    pub fn offer_models(mut self, models: impl IntoIterator<Item = ModelChoice>) -> FakeScript {
        self.models = models.into_iter().collect();
        self
    }

    fn answer_mode(&self, mode: PermissionMode) -> AdapterEvent {
        if self.refused_modes.contains(&mode) {
            AdapterEvent::PermissionModeRefused {
                mode,
                reason: REFUSE_REASON.to_owned(),
            }
        } else {
            AdapterEvent::PermissionModeSet(mode)
        }
    }

    fn answer_model(&self, model: String) -> AdapterEvent {
        if self.refused_models.contains(&model) {
            AdapterEvent::ModelRefused {
                model,
                reason: REFUSE_REASON.to_owned(),
            }
        } else {
            AdapterEvent::ModelSet(model)
        }
    }

    fn answer_effort(&self, effort: Effort) -> AdapterEvent {
        if self.refused_efforts.contains(&effort) {
            AdapterEvent::EffortRefused {
                effort,
                reason: REFUSE_REASON.to_owned(),
            }
        } else {
            AdapterEvent::EffortSet(Some(effort))
        }
    }

    /// Appends a turn made of `steps`.
    pub fn turn(mut self, steps: impl IntoIterator<Item = Step>) -> FakeScript {
        self.turns.push(steps.into_iter().collect());
        self
    }
}

/// One step of a scripted turn.
#[derive(Debug, Clone)]
pub enum Step {
    /// Emits the event as is.
    Emit(AdapterEvent),
    /// Emits `ApprovalRequested` and blocks until it is answered.
    Approval {
        request: AgentRequestId,
        call: Option<AgentCallId>,
        tool: String,
        input: Value,
        suggestions: Vec<RuleSuggestion>,
        on_approve: Vec<Step>,
        on_deny: Vec<Step>,
    },
    /// Blocks until interrupted.
    Hang,
    /// Blocks until the gate is open.
    Wait(Gate),
    /// Ends the turn with `TurnEnded(Failed(message))`.
    Fail(String),
    /// Exits mid-turn: emits `Exited { error }` and ends the session.
    Exit(Option<String>),
    /// Emits nothing; `after` from now the agent starts a turn of `steps`
    /// on its own, with no message, like a scheduled wakeup.
    Wake { after: Duration, steps: Vec<Step> },
    /// Emits nothing; `after` from now, once the agent is idle, it emits
    /// `events` outside any turn, like a background job ending.
    Later {
        after: Duration,
        events: Vec<AdapterEvent>,
    },
}

/// A shell task `id` for [`Step::tasks`]; `ambient` for one that is not
/// activity.
pub fn task(id: &str, ambient: bool) -> AgentTask {
    AgentTask {
        id: id.to_owned(),
        kind: TaskKind::Shell,
        title: format!("task {id}"),
        parent: None,
        ambient,
    }
}

/// A recurring schedule `id` for [`Step::schedules`].
pub fn schedule(id: &str) -> AgentSchedule {
    AgentSchedule {
        id: id.to_owned(),
        when: "*/1 * * * *".to_owned(),
        recurring: true,
        prompt: format!("wake {id}"),
        outlives_agent: None,
    }
}

impl Step {
    pub fn text(text: impl Into<String>) -> Step {
        Step::Emit(AdapterEvent::Text(text.into()))
    }

    pub fn tool_call(name: impl Into<String>, input: Value) -> Step {
        Step::Emit(AdapterEvent::ToolCall {
            call: None,
            name: name.into(),
            input,
            display: None,
        })
    }

    pub fn tool_result(output: impl Into<String>) -> Step {
        Step::tool_outcome(output.into(), false)
    }

    pub fn tool_error(output: impl Into<String>) -> Step {
        Step::tool_outcome(output.into(), true)
    }

    fn tool_outcome(output: String, is_error: bool) -> Step {
        Step::Emit(AdapterEvent::ToolResult {
            call: None,
            result: ResultDisplay {
                is_error,
                status: None,
                preview: None,
                output,
            },
        })
    }

    /// An approval with no follow-up steps; add them with
    /// [`Step::on_approve`] / [`Step::on_deny`].
    pub fn approval(request: &str, tool: impl Into<String>, input: Value) -> Step {
        Step::Approval {
            request: request.into(),
            call: None,
            tool: tool.into(),
            input,
            suggestions: Vec::new(),
            on_approve: Vec::new(),
            on_deny: Vec::new(),
        }
    }

    /// Schedules a turn of `steps` the agent starts on its own `after` from
    /// now ([`Step::Wake`]).
    pub fn wake(after: Duration, steps: impl IntoIterator<Item = Step>) -> Step {
        Step::Wake {
            after,
            steps: steps.into_iter().collect(),
        }
    }

    /// Emits events the agent reports `after` from now, outside any turn
    /// ([`Step::Later`]).
    pub fn later(after: Duration, events: impl IntoIterator<Item = AdapterEvent>) -> Step {
        Step::Later {
            after,
            events: events.into_iter().collect(),
        }
    }

    /// Reports `tasks` as the agent's whole set of background tasks.
    pub fn tasks(tasks: impl IntoIterator<Item = AgentTask>) -> Step {
        Step::Emit(AdapterEvent::BackgroundTasks(tasks.into_iter().collect()))
    }

    /// Reports `schedules` as the agent's whole set of scheduled wakeups.
    pub fn schedules(schedules: impl IntoIterator<Item = AgentSchedule>) -> Step {
        Step::Emit(AdapterEvent::Schedules(schedules.into_iter().collect()))
    }

    /// Reports `grants` as the session's whole set of grants.
    pub fn grants(grants: Value) -> Step {
        Step::Emit(AdapterEvent::Grants(Grants(grants)))
    }

    /// Offers a lasting rule with an approval step. No effect on other
    /// steps.
    pub fn suggest(mut self, label: &str, scope: RuleScope) -> Step {
        if let Step::Approval { suggestions, .. } = &mut self {
            suggestions.push(RuleSuggestion {
                label: label.to_owned(),
                scope,
            });
        }
        self
    }

    /// Names the agent's call `id` on a tool call, tool result or approval
    /// step, linking them. No effect on other steps.
    pub fn call(mut self, id: &str) -> Step {
        match &mut self {
            Step::Emit(
                AdapterEvent::ToolCall { call, .. } | AdapterEvent::ToolResult { call, .. },
            )
            | Step::Approval { call, .. } => *call = Some(id.into()),
            _ => {}
        }
        self
    }

    /// Steps run after an approval is approved. No effect on other steps.
    pub fn on_approve(mut self, steps: impl IntoIterator<Item = Step>) -> Step {
        if let Step::Approval { on_approve, .. } = &mut self {
            *on_approve = steps.into_iter().collect();
        }
        self
    }

    /// Steps run after an approval is denied. No effect on other steps.
    pub fn on_deny(mut self, steps: impl IntoIterator<Item = Step>) -> Step {
        if let Step::Approval { on_deny, .. } = &mut self {
            *on_deny = steps.into_iter().collect();
        }
        self
    }
}

/// A latch a test opens to let a [`Step::Wait`] continue. Clones share it.
#[derive(Debug, Clone)]
pub struct Gate(Arc<watch::Sender<bool>>);

impl Gate {
    pub fn new() -> Gate {
        Gate(Arc::new(watch::Sender::new(false)))
    }

    /// Opens the gate for good; every waiting and later `Wait` passes.
    pub fn open(&self) {
        self.0.send_replace(true);
    }
}

impl Default for Gate {
    fn default() -> Self {
        Gate::new()
    }
}

/// Which conversation a fake start ran, as recorded in
/// [`FakeInput::Started`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FakeConversation {
    Fresh,
    /// The started context's own.
    Resume,
    /// A copy of this context's conversation.
    Fork(ContextId),
}

/// A request a fake session received, as recorded by [`FakeAgent::inputs`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FakeInput {
    Started {
        conversation: FakeConversation,
        context: ContextId,
        /// The session's grants the start was given.
        grants: Option<Grants>,
    },
    Message(String),
    /// An answer and the rule it grants, if any.
    Answer(AgentRequestId, Decision, Option<u8>),
    Interrupt,
    SetPermissionMode(PermissionMode),
    SetModel(String),
    SetEffort(Effort),
    StopTask(String),
    Shutdown,
}

/// The scripted agent adapter. Clones share the script and session progress.
#[derive(Debug, Clone)]
pub struct FakeAgent {
    script: Arc<FakeScript>,
    shared: Arc<Mutex<HashMap<SessionId, Progress>>>,
}

#[derive(Debug, Default)]
struct Progress {
    /// Index of the next scripted turn.
    next_turn: usize,
    inputs: Vec<FakeInput>,
    /// The session's contexts whose conversation exists: they got a
    /// message.
    conversations: HashSet<ContextId>,
}

impl FakeAgent {
    pub fn new(script: FakeScript) -> FakeAgent {
        FakeAgent {
            script: Arc::new(script),
            shared: Arc::default(),
        }
    }

    /// Everything sessions of `session_id` received, in order.
    pub fn inputs(&self, session_id: SessionId) -> Vec<FakeInput> {
        lock(&self.shared)
            .get(&session_id)
            .map(|p| p.inputs.clone())
            .unwrap_or_default()
    }

    /// The resume handle the fake reports for session context `context`.
    pub fn resume_handle(context: ContextId) -> ResumeHandle {
        ResumeHandle(json!({ "fake_context": context.0 }))
    }

    /// The context `handle` names, if it is one the fake reported.
    fn handle_context(handle: &ResumeHandle) -> Option<ContextId> {
        handle
            .0
            .get("fake_context")
            .and_then(Value::as_str)
            .and_then(|id| Uuid::parse_str(id).ok())
            .map(ContextId)
    }
}

impl AgentAdapter for FakeAgent {
    type Session = FakeSession;

    async fn start(&self, spec: StartSpec) -> Result<(FakeSession, AdapterEvents), AgentError> {
        let conversation = match &spec.conversation {
            Conversation::Fresh => FakeConversation::Fresh,
            Conversation::Resume(handle) => {
                if FakeAgent::handle_context(handle) != Some(spec.context) {
                    return Err(AgentError::Resume(format!(
                        "handle {} is not for context {}",
                        handle.0, spec.context.0
                    )));
                }
                FakeConversation::Resume
            }
            Conversation::Fork(handle) => {
                let shared = lock(&self.shared);
                match FakeAgent::handle_context(handle) {
                    Some(origin)
                        if shared
                            .values()
                            .any(|progress| progress.conversations.contains(&origin)) =>
                    {
                        FakeConversation::Fork(origin)
                    }
                    _ => {
                        return Err(AgentError::Resume(format!(
                            "handle {} names no conversation to fork",
                            handle.0
                        )));
                    }
                }
            }
        };
        let resumed = conversation == FakeConversation::Resume;
        {
            let mut shared = lock(&self.shared);
            let progress = shared.entry(spec.session_id).or_default();
            if !resumed && progress.conversations.contains(&spec.context) {
                return Err(AgentError::Start(format!(
                    "context {} already exists; resume it",
                    spec.context.0
                )));
            }
            progress.inputs.push(FakeInput::Started {
                conversation,
                context: spec.context,
                grants: spec.grants.clone(),
            });
        }

        let (commands, commands_rx) = mpsc::unbounded_channel();
        let (events, events_rx) = mpsc::channel(EVENT_CAPACITY);
        let mut early = Vec::new();
        if !self.script.models.is_empty() {
            early.push(AdapterEvent::ModelsOffered(self.script.models.clone()));
        }
        {
            let mut shared = lock(&self.shared);
            let inputs = &mut shared.entry(spec.session_id).or_default().inputs;
            if let Some(mode) = spec.mode {
                inputs.push(FakeInput::SetPermissionMode(mode));
                early.push(self.script.answer_mode(mode));
            }
            if let Some(model) = spec.model {
                inputs.push(FakeInput::SetModel(model.clone()));
                early.push(self.script.answer_model(model));
            }
            if let Some(effort) = spec.effort {
                inputs.push(FakeInput::SetEffort(effort));
                early.push(self.script.answer_effort(effort));
            }
        }
        for event in early {
            events.try_send(event).map_err(|_| AgentError::Closed)?;
        }
        let answers = events.clone();
        let runner = Runner {
            session_id: spec.session_id,
            context: spec.context,
            // A resumed conversation exists and its handle is stored; a
            // fork's copy, like a fresh one, exists from its first message.
            handle_reported: resumed,
            script: Arc::clone(&self.script),
            shared: Arc::clone(&self.shared),
            commands: commands_rx,
            events,
            queued: VecDeque::new(),
            wakeups: Vec::new(),
            tasks: Vec::new(),
        };
        let task = tokio::spawn(runner.run());
        let session = FakeSession {
            session_id: spec.session_id,
            script: Arc::clone(&self.script),
            shared: Arc::clone(&self.shared),
            commands,
            answers,
            task,
        };
        Ok((session, events_rx))
    }
}

/// A running fake agent session.
#[derive(Debug)]
pub struct FakeSession {
    session_id: SessionId,
    script: Arc<FakeScript>,
    shared: Arc<Mutex<HashMap<SessionId, Progress>>>,
    commands: mpsc::UnboundedSender<FakeInput>,
    /// Answers to mode and model requests go straight out, idle or not.
    answers: mpsc::Sender<AdapterEvent>,
    task: tokio::task::JoinHandle<()>,
}

impl FakeSession {
    fn request(&self, input: FakeInput) -> Result<(), AgentError> {
        if self.commands.is_closed() {
            return Err(AgentError::Closed);
        }
        lock(&self.shared)
            .entry(self.session_id)
            .or_default()
            .inputs
            .push(input.clone());
        self.commands.send(input).map_err(|_| AgentError::Closed)
    }

    /// Records `input` and sends `answer` to it.
    async fn answer(&self, input: FakeInput, answer: AdapterEvent) -> Result<(), AgentError> {
        if self.commands.is_closed() {
            return Err(AgentError::Closed);
        }
        lock(&self.shared)
            .entry(self.session_id)
            .or_default()
            .inputs
            .push(input);
        self.answers
            .send(answer)
            .await
            .map_err(|_| AgentError::Closed)
    }
}

impl AgentSession for FakeSession {
    async fn send_message(&mut self, text: String) -> Result<(), AgentError> {
        self.request(FakeInput::Message(text))
    }

    async fn answer_approval(
        &mut self,
        request: AgentRequestId,
        decision: Decision,
        rule: Option<u8>,
    ) -> Result<(), AgentError> {
        self.request(FakeInput::Answer(request, decision, rule))
    }

    async fn interrupt(&mut self) -> Result<(), AgentError> {
        self.request(FakeInput::Interrupt)
    }

    async fn set_permission_mode(&mut self, mode: PermissionMode) -> Result<(), AgentError> {
        let answer = self.script.answer_mode(mode);
        self.answer(FakeInput::SetPermissionMode(mode), answer)
            .await
    }

    async fn set_model(&mut self, model: String) -> Result<(), AgentError> {
        let answer = self.script.answer_model(model.clone());
        self.answer(FakeInput::SetModel(model), answer).await
    }

    async fn set_effort(&mut self, effort: Effort) -> Result<(), AgentError> {
        let answer = self.script.answer_effort(effort);
        self.answer(FakeInput::SetEffort(effort), answer).await
    }

    async fn stop_task(&mut self, task: String) -> Result<(), AgentError> {
        let input = FakeInput::StopTask(task.clone());
        if self.script.refuse_stops {
            let reason = REFUSE_REASON.to_owned();
            return self
                .answer(input, AdapterEvent::TaskStopRefused { task, reason })
                .await;
        }
        let answer = AdapterEvent::TaskStopAnswered { task };
        self.answer(input.clone(), answer).await?;
        // The runner reports the shrunk set between turns.
        self.commands.send(input).map_err(|_| AgentError::Closed)
    }

    async fn shutdown(self) -> Result<(), AgentError> {
        // Already exited on its own: nothing to stop.
        let _ = self.request(FakeInput::Shutdown);
        drop(self.answers);
        self.task
            .await
            .map_err(|e| AgentError::Io(std::io::Error::other(e)))
    }
}

/// How a turn stopped.
enum Flow {
    Ended(TurnOutcome),
    /// The session is over; `Exited` was or must be emitted.
    Exit(Option<String>),
}

/// Why a blocked or running turn stops early.
enum Stop {
    Interrupt,
    Shutdown,
}

impl Stop {
    fn flow(self) -> Flow {
        match self {
            Stop::Interrupt => Flow::Ended(TurnOutcome::Interrupted),
            Stop::Shutdown => Flow::Exit(None),
        }
    }
}

struct Runner {
    session_id: SessionId,
    context: ContextId,
    /// The context's handle has been reported: it was resumed, or this
    /// start took its first message.
    handle_reported: bool,
    script: Arc<FakeScript>,
    shared: Arc<Mutex<HashMap<SessionId, Progress>>>,
    commands: mpsc::UnboundedReceiver<FakeInput>,
    events: mpsc::Sender<AdapterEvent>,
    /// Messages received during a turn, each starting a later turn.
    queued: VecDeque<String>,
    /// What [`Step::Wake`] and [`Step::Later`] scheduled, and stopped
    /// tasks to report, with when each is due.
    wakeups: Vec<(Instant, Wakeup)>,
    /// The last task set emitted, which a stop shrinks.
    tasks: Vec<AgentTask>,
}

/// Something the agent does on its own once idle.
enum Wakeup {
    /// A turn of these steps ([`Step::Wake`]).
    Turn(Vec<Step>),
    /// These events, outside a turn ([`Step::Later`]).
    Report(Vec<AdapterEvent>),
    /// The task set without this stopped task, outside a turn.
    Stopped(String),
}

impl Runner {
    async fn run(mut self) {
        let error = loop {
            if let Some(wakeup) = self.take_due_wakeup() {
                let steps = match wakeup {
                    Wakeup::Turn(steps) => steps,
                    Wakeup::Report(events) => {
                        for event in events {
                            if !self.emit(event).await {
                                return;
                            }
                        }
                        continue;
                    }
                    Wakeup::Stopped(task) => {
                        if self.tasks.iter().any(|live| live.id == task) {
                            let mut tasks = self.tasks.clone();
                            tasks.retain(|live| live.id != task);
                            if !self.emit(AdapterEvent::BackgroundTasks(tasks)).await {
                                return;
                            }
                        }
                        continue;
                    }
                };
                if !self.emit(AdapterEvent::TurnStarted).await {
                    return;
                }
                match self.run_turn(steps).await {
                    Flow::Ended(outcome) => {
                        if !self.emit(AdapterEvent::TurnEnded(outcome)).await {
                            return;
                        }
                    }
                    Flow::Exit(error) => break error,
                }
                continue;
            }
            let text = match self.queued.pop_front() {
                Some(text) => text,
                None => {
                    let next_wakeup = self.wakeups.iter().map(|(due, _)| *due).min();
                    let input = tokio::select! {
                        input = self.commands.recv() => input,
                        () = sleep_until(next_wakeup) => continue,
                    };
                    match input {
                        Some(FakeInput::Message(text)) => text,
                        Some(FakeInput::StopTask(task)) => {
                            self.stopped(task);
                            continue;
                        }
                        // Idle: nothing to interrupt or answer.
                        Some(
                            FakeInput::Interrupt
                            | FakeInput::Answer(..)
                            | FakeInput::Started { .. }
                            | FakeInput::SetPermissionMode(_)
                            | FakeInput::SetModel(_)
                            | FakeInput::SetEffort(_),
                        ) => {
                            continue;
                        }
                        Some(FakeInput::Shutdown) | None => break None,
                    }
                }
            };
            if !self.handle_reported {
                // The conversation exists from its first message on.
                self.handle_reported = true;
                lock(&self.shared)
                    .entry(self.session_id)
                    .or_default()
                    .conversations
                    .insert(self.context);
                let handle = FakeAgent::resume_handle(self.context);
                if !self.emit(AdapterEvent::ResumeHandle(handle)).await {
                    return;
                }
            }
            let steps = self.next_turn(&text);
            if !self.emit(AdapterEvent::TurnStarted).await {
                return;
            }
            match self.run_turn(steps).await {
                Flow::Ended(outcome) => {
                    if !self.emit(AdapterEvent::TurnEnded(outcome)).await {
                        return;
                    }
                }
                Flow::Exit(error) => break error,
            }
        };
        self.emit(AdapterEvent::Exited { error }).await;
    }

    /// The earliest wakeup that is due, if one is; of those due at once,
    /// the first scheduled.
    fn take_due_wakeup(&mut self) -> Option<Wakeup> {
        let now = Instant::now();
        let (index, _) = self
            .wakeups
            .iter()
            .enumerate()
            .filter(|(_, (due, _))| *due <= now)
            .min_by_key(|(_, (due, _))| *due)?;
        Some(self.wakeups.remove(index).1)
    }

    /// Steps of the session's next scripted turn, or an echo of `text`.
    fn next_turn(&self, text: &str) -> Vec<Step> {
        let mut shared = lock(&self.shared);
        let progress = shared.entry(self.session_id).or_default();
        let turn = self.script.turns.get(progress.next_turn).cloned();
        progress.next_turn += 1;
        turn.unwrap_or_else(|| vec![Step::text(format!("echo: {text}"))])
    }

    async fn run_turn(&mut self, steps: Vec<Step>) -> Flow {
        let mut work: VecDeque<Step> = steps.into();
        while let Some(step) = work.pop_front() {
            if let Err(stop) = self.poll_between_steps() {
                return stop.flow();
            }
            match step {
                Step::Emit(event) => {
                    if !self.emit(event).await {
                        return Flow::Exit(None);
                    }
                }
                Step::Approval {
                    request,
                    call,
                    tool,
                    input,
                    suggestions,
                    on_approve,
                    on_deny,
                } => {
                    let asked = AdapterEvent::ApprovalRequested {
                        request: request.clone(),
                        call,
                        tool,
                        input,
                        suggestions,
                    };
                    if !self.emit(asked).await {
                        return Flow::Exit(None);
                    }
                    match self.wait_answer(&request).await {
                        Ok(decision) => {
                            let branch = match decision {
                                Decision::Approve => on_approve,
                                Decision::Deny => on_deny,
                            };
                            for step in branch.into_iter().rev() {
                                work.push_front(step);
                            }
                        }
                        Err(Stop::Interrupt) => {
                            let cancelled = AdapterEvent::ApprovalCancelled {
                                request,
                                reason: INTERRUPT_REASON.to_owned(),
                            };
                            if !self.emit(cancelled).await {
                                return Flow::Exit(None);
                            }
                            return Stop::Interrupt.flow();
                        }
                        Err(stop) => return stop.flow(),
                    }
                }
                Step::Hang => {
                    if let Err(stop) = self.wait_gate(None).await {
                        return stop.flow();
                    }
                }
                Step::Wait(gate) => {
                    if let Err(stop) = self.wait_gate(Some(&gate)).await {
                        return stop.flow();
                    }
                }
                Step::Fail(message) => return Flow::Ended(TurnOutcome::Failed(message)),
                Step::Exit(error) => return Flow::Exit(error),
                Step::Wake { after, steps } => {
                    self.wakeups
                        .push((Instant::now() + after, Wakeup::Turn(steps)));
                }
                Step::Later { after, events } => {
                    self.wakeups
                        .push((Instant::now() + after, Wakeup::Report(events)));
                }
            }
        }
        Flow::Ended(TurnOutcome::Completed)
    }

    /// Takes the requests that arrived while steps ran.
    fn poll_between_steps(&mut self) -> Result<(), Stop> {
        loop {
            match self.commands.try_recv() {
                Ok(input) => self.during_turn(Some(input))?,
                Err(mpsc::error::TryRecvError::Empty) => return Ok(()),
                Err(mpsc::error::TryRecvError::Disconnected) => self.during_turn(None)?,
            }
        }
    }

    /// Blocks until `request` is answered.
    async fn wait_answer(&mut self, request: &AgentRequestId) -> Result<Decision, Stop> {
        loop {
            match self.commands.recv().await {
                Some(FakeInput::Answer(answered, decision, _)) if answered == *request => {
                    return Ok(decision);
                }
                input => self.during_turn(input)?,
            }
        }
    }

    /// Blocks until `gate` opens; with no gate, until the turn is stopped.
    async fn wait_gate(&mut self, gate: Option<&Gate>) -> Result<(), Stop> {
        let mut gate = gate.map(|g| g.0.subscribe());
        loop {
            let opened = async {
                match gate.as_mut() {
                    // The sender lives in the step's `Gate`, so this waits.
                    Some(rx) => {
                        let _ = rx.wait_for(|open| *open).await;
                    }
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                input = self.commands.recv() => self.during_turn(input)?,
                () = opened => return Ok(()),
            }
        }
    }

    /// Handles a request received during a turn: an interrupt or shutdown
    /// stops it, a message is queued for a later turn, an answer that no
    /// step waits for is stale.
    fn during_turn(&mut self, input: Option<FakeInput>) -> Result<(), Stop> {
        match input {
            Some(FakeInput::Interrupt) => Err(Stop::Interrupt),
            Some(FakeInput::Shutdown) | None => Err(Stop::Shutdown),
            Some(FakeInput::Message(text)) => {
                self.queued.push_back(text);
                Ok(())
            }
            Some(FakeInput::StopTask(task)) => {
                self.stopped(task);
                Ok(())
            }
            Some(
                FakeInput::Answer(..)
                | FakeInput::Started { .. }
                | FakeInput::SetPermissionMode(_)
                | FakeInput::SetModel(_)
                | FakeInput::SetEffort(_),
            ) => Ok(()),
        }
    }

    /// Reports the set without stopped `task` as soon as the agent is idle.
    fn stopped(&mut self, task: String) {
        self.wakeups.push((Instant::now(), Wakeup::Stopped(task)));
    }

    /// Sends `event`; false when the receiver is gone and the session over.
    async fn emit(&mut self, event: AdapterEvent) -> bool {
        if let AdapterEvent::BackgroundTasks(tasks) = &event {
            self.tasks.clone_from(tasks);
        }
        self.events.send(event).await.is_ok()
    }
}

/// Sleeps until `deadline`; with none, forever.
async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

fn lock(
    shared: &Mutex<HashMap<SessionId, Progress>>,
) -> MutexGuard<'_, HashMap<SessionId, Progress>> {
    // A panicking test thread must not hide the progress from the others.
    shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
