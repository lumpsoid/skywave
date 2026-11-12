//! The seam between `skywaved` and a coding agent (architecture.md "Server", D3, D4).
//!
//! An [`AgentAdapter`] starts or resumes one [`AgentSession`] per Skywave
//! session. The session takes the supervisor's requests (send a message,
//! answer an approval, interrupt, shut down) and reports what the agent does
//! as a stream of agent-agnostic [`AdapterEvent`]s. An [`EventMapper`] turns
//! those into log [`EventKind`](skywave_proto::EventKind)s, cutting what an
//! adapter shows to the protocol's caps and moving what was cut into blobs.
//!
//! **Tool calls.** An adapter reports a tool call with its agent's own
//! [`AgentCallId`], the full input and, when it knows the tool, a
//! [`ToolDisplay`] with uncut content; the result and any approval for the
//! call name the same [`AgentCallId`]. Adapters never cut or blob anything:
//! the mapper alone applies `skywave_proto::limits`.
//!
//! Everything agent-specific stays behind this seam: the agent's own
//! approval request ids are opaque [`AgentRequestId`]s, and its resume state
//! (for example its own session id) is an opaque [`ResumeHandle`] persisted
//! per session context in `contexts.agent_state` with
//! [`store_resume_handle`] / [`load_resume_handle`]. A context moved into
//! a new session leaves that session's first context its origin
//! ([`load_origin`]), whose conversation the new one continues as a fork.
//! A session may instead adopt a conversation the agent had outside
//! Skywave, named by the agent's own id: the origin too, continued as
//! [`Conversation::Adopt`], a copy, so the outside conversation is never
//! written. What the agent was
//! granted for the whole Skywave session (for example lasting permission
//! rules) is opaque [`Grants`], persisted per session in
//! `sessions.agent_grants` with [`store_grants`] / [`load_grants`] and given
//! to every start of every context of that session.
//!
//! **Protocol between supervisor and session.**
//! - [`AgentAdapter::start`] runs the [`Conversation`] in `spec`: a fresh
//!   one, the context's own resumed with its stored handle, a fork of
//!   the origin's, or a copy of an outside conversation. The adapter reports the handle to persist as
//!   [`AdapterEvent::ResumeHandle`] once the context's conversation exists
//!   and before the first turn ends (for a fork or an adoption, the copy's handle, as for
//!   a fresh start); a process stopped before any message reports none, so
//!   the context's next start is fresh (or a fork) again. `spec.grants` are the session's grants; when they
//!   change the adapter reports the whole new set as
//!   [`AdapterEvent::Grants`].
//! - A turn begins with a user message and ends with exactly one
//!   [`AdapterEvent::TurnEnded`]. A message sent while a turn runs is queued
//!   by the agent and starts the next turn. What the turn took (time,
//!   tokens, cost, model) may come just before as [`AdapterEvent::TurnUsage`],
//!   per-turn figures: an adapter whose agent reports running totals turns
//!   them into the turn's share.
//! - An approval blocks the agent until [`AgentSession::answer_approval`] (D8).
//!   It may offer lasting rules ([`RuleSuggestion`]s, labelled by the
//!   adapter); an approval can grant one of them by its index, which only
//!   the adapter maps back to what its agent understands.
//!   [`AgentSession::interrupt`] ends the current turn; approvals pending at
//!   that point are reported as [`AdapterEvent::ApprovalCancelled`] (or are
//!   cancelled by the mapper when the turn ends), followed by
//!   `TurnEnded(Interrupted)`. The session stays usable for the next turn.
//!   An agent that does not answer an interrupt is stopped by
//!   [`AgentSession::escalate_interrupt`], the user's repeated interrupt:
//!   it exits with [`INTERRUPT_UNANSWERED`] as its error, like a crash.
//! - [`AgentSession::set_permission_mode`], [`AgentSession::set_model`] and
//!   [`AgentSession::set_effort`] work at any time, mid-turn and with an
//!   approval pending too. Each request gets exactly one answer, in the
//!   order asked: `PermissionModeSet` / `PermissionModeRefused` (`ModelSet`
//!   / `ModelRefused`, `EffortSet` / `EffortRefused`), unless the agent
//!   exits first. What the agent runs with otherwise (at start, at a turn, a
//!   change it made on its own) is reported as `PermissionMode` / `Model` /
//!   `Effort`, as often as the agent says, and the models it offers as
//!   `ModelsOffered`. [`StartSpec`] names the mode, model and effort to
//!   start with; the adapter applies them as requests, so each is answered
//!   the same way.
//! - Work the agent runs outside a turn (background shells, subagents) is
//!   reported as [`AdapterEvent::BackgroundTasks`], the whole live set, as
//!   often as it changes. A subagent task names the call it runs
//!   ([`AgentTask::call`]); the subagent's text and tool calls name that
//!   call as their `parent`, in a turn or outside one. One call may run
//!   again: its task leaves the set and comes back under the same call. An adapter reports the empty set once the agent
//!   is ready, since nothing is reported at start; the mapper logs only
//!   changes.
//! - [`AgentSession::stop_task`] asks the agent to stop one of those tasks,
//!   at any time. Each ask gets exactly one answer, in the order asked:
//!   [`AdapterEvent::TaskStopAnswered`] when the agent took it (the task
//!   leaves a later `BackgroundTasks`; an agent may answer so for a task it
//!   no longer has) or [`AdapterEvent::TaskStopRefused`], unless the agent
//!   exits first.
//! - Wakeups the agent scheduled for itself are reported as
//!   [`AdapterEvent::Schedules`], the whole set, whenever the agent says;
//!   they are information only and never keep the agent running.
//! - Whether the agent is logged in is reported as
//!   [`AdapterEvent::LoggedIn`], whenever the agent says (at start, or a
//!   turn it refused for want of a login), naming which of the two. The
//!   supervisor keeps it in the shared [`LoginState`]; it is not logged.
//! - The stream ends with [`AdapterEvent::Exited`] when the agent stops, on
//!   [`AgentSession::shutdown`] or on its own.
//!
//! **Outside conversations.** What the agent keeps on this machine, for a
//! session to adopt, is read through [`OutsideConversations`], apart from
//! the adapter: a listing of the conversations under the roots, and one
//! conversation's history as [`HistoryItem`]s (the same [`AdapterEvent`]s a
//! live session reports, uncut). [`NoOutsideConversations`] is the source
//! of an agent that keeps none Skywave can read. [`import`] turns a history
//! into the bounded `Imported` events an adopting `CreateSession` logs.
//!
//! The scripted [`fake::FakeAgent`] implements the trait for deterministic
//! tests (and [`fake::FakeOutside`] the source); [`claude_code`] holds the
//! real adapter and source, the only module that names the agent it drives
//! (D3).

pub mod claude_code;
pub mod fake;
pub mod import;
pub(crate) mod mapper;

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use chrono::{DateTime, Utc};
use futures_util::future::BoxFuture;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use skywave_proto::{ContextId, Decision, ImageType, SessionId, SessionOrigin};
pub use skywave_proto::{
    Effort, LimitSeverity, LimitWindow, ModelChoice, PermissionMode, Question, QuestionAnswer,
    QuestionOption, RuleScope, RuleSuggestion, TaskKind, TurnUsage, UsageLimit,
};
use tokio::sync::mpsc;

use crate::db::Db;
use crate::error::{Error, Result};

pub use mapper::EventMapper;
pub(crate) use mapper::{bounded_models, unsent_notice};
pub use skywave_proto::{BodyFormat, ToolKind};

/// The agent's own id for one approval request (e.g. a control request id).
///
/// Chosen by the agent and meaningful only to its adapter; the log names
/// approvals by [`ApprovalId`](skywave_proto::ApprovalId), and the
/// [`EventMapper`] keeps the mapping between the two.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct AgentRequestId(pub String);

impl From<&str> for AgentRequestId {
    fn from(id: &str) -> Self {
        AgentRequestId(id.to_owned())
    }
}

/// The agent's own id for one tool call (e.g. a tool-use id).
///
/// Meaningful only to its adapter; it links a call to its result and its
/// approval. The [`EventMapper`] gives each one a
/// [`CallId`](skywave_proto::CallId) for the log, valid until the turn ends.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct AgentCallId(pub String);

impl From<&str> for AgentCallId {
    fn from(id: &str) -> Self {
        AgentCallId(id.to_owned())
    }
}

/// One background task of the agent, uncut. The ids are the agent's own,
/// opaque; the [`EventMapper`] cuts the rest for the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTask {
    pub id: String,
    pub kind: TaskKind,
    pub title: String,
    /// The id of the subagent task that launched this one, if any.
    pub parent: Option<String>,
    /// For a subagent task, the agent's id of the tool call it runs: the
    /// call its steps name as their `parent`. `None` for other tasks, or
    /// while the adapter does not know it yet.
    pub call: Option<AgentCallId>,
    /// Not activity (see [`skywave_proto::BackgroundTask::ambient`]).
    pub ambient: bool,
}

/// What a finished subagent ran, as its agent reported it; the
/// [`EventMapper`] logs it by the call's [`skywave_proto::CallId`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubagentFigures {
    pub duration_ms: Option<u64>,
    pub tool_uses: Option<u64>,
    /// Its context when it ended, not what it spent.
    pub context_tokens: Option<u64>,
}

/// One scheduled wakeup of the agent: a prompt it runs on its own when
/// `when` comes. Uncut, its id the agent's own and opaque; the
/// [`EventMapper`] cuts the rest for the log. Information only: nothing
/// keeps the agent running for one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSchedule {
    pub id: String,
    /// When it fires, in the agent's own words.
    pub when: String,
    pub recurring: bool,
    pub prompt: String,
    /// Whether it outlives the agent; `None` when the agent does not say.
    pub outlives_agent: Option<bool>,
}

/// How to show a tool call, uncut. Agent-agnostic (D3): the adapter maps its
/// agent's tool onto a [`ToolKind`] and hands over full content; the
/// [`EventMapper`] cuts it to the protocol's caps and stores what was cut.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDisplay {
    pub kind: ToolKind,
    /// Main line, e.g. a path or a command. Only its first line is shown.
    pub title: String,
    /// Optional second line, e.g. a description.
    pub detail: Option<String>,
    /// Content worth showing beneath the title, e.g. a diff, in full.
    pub body: Option<DisplayBody>,
    /// Title, detail and body together show all of the call's input. When
    /// false (the display leaves fields out), or when the mapper cuts the
    /// detail or a title beside a body, the input is kept as a blob. A cut
    /// body is not: its own blob holds it whole.
    pub shows_all_input: bool,
}

/// Full content shown beneath a tool call's title.
#[derive(Debug, Clone, PartialEq)]
pub struct DisplayBody {
    pub format: BodyFormat,
    pub text: String,
}

/// How a tool call ended, uncut.
#[derive(Debug, Clone, PartialEq)]
pub struct ResultDisplay {
    /// The tool failed or was denied.
    pub is_error: bool,
    /// Short outcome, e.g. `exit 1`, `3 matches`; cut to one short line.
    pub status: Option<String>,
    /// What to preview, uncut, when it is not `output` itself, e.g. the
    /// tool's own output without notes meant for the model. `None` previews
    /// `output`. When set and different, `output` is kept whole beside it
    /// (inline when small, else as a blob).
    pub preview: Option<String>,
    /// The whole output.
    pub output: String,
}

/// Whatever an adapter needs to resume an agent session after a restart.
///
/// Opaque JSON: only the adapter that produced it reads it. Stored per
/// session context, in `contexts.agent_state`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(transparent)]
pub struct ResumeHandle(pub serde_json::Value);

/// What an agent was granted for the whole Skywave session, across its
/// contexts (for example lasting permission rules).
///
/// Opaque JSON like [`ResumeHandle`]: only the adapter that produced it
/// reads it. Stored per session, in `sessions.agent_grants`, and never
/// shared between sessions.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(transparent)]
pub struct Grants(pub serde_json::Value);

/// Which conversation a starting agent runs.
#[derive(Debug, Clone, PartialEq)]
pub enum Conversation {
    /// A new, empty conversation.
    Fresh,
    /// The context's own conversation, from its handle.
    Resume(ResumeHandle),
    /// A copy of another conversation (the handle of the context this one
    /// continues), which becomes this context's own: the adapter reports
    /// the new conversation's handle as for a fresh start.
    Fork(ResumeHandle),
    /// A copy of a conversation the agent had outside Skywave, named by the
    /// agent's own id; it becomes this context's own, and the adapter
    /// reports its handle as for a fresh start.
    Adopt(String),
}

/// How to start an agent session.
#[derive(Debug, Clone, PartialEq)]
pub struct StartSpec {
    pub session_id: SessionId,
    /// The session context the agent runs in. For logging and the fake's
    /// keying only: adapters must not interpret it (the context's
    /// conversation is `conversation`).
    pub context: ContextId,
    /// Canonical working directory of the session (D14); the agent runs here.
    pub workdir: PathBuf,
    /// The conversation to run in `context`.
    pub conversation: Conversation,
    /// The grants stored for the session, whichever context starts; `None`
    /// until the agent has reported any.
    pub grants: Option<Grants>,
    /// The mode to run in, asked for once the agent is up; `None` leaves
    /// the agent's own.
    pub mode: Option<PermissionMode>,
    /// The model to run, asked for like `mode`.
    pub model: Option<String>,
    /// The reasoning effort to run at, asked for like `mode`.
    pub effort: Option<Effort>,
}

/// How a turn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnOutcome {
    Completed,
    /// Stopped by [`AgentSession::interrupt`].
    Interrupted,
    /// The agent gave up on the turn with an error.
    Failed(String),
}

/// What an agent session reports, in order. Agent-agnostic (D3).
#[derive(Debug, Clone, PartialEq)]
pub enum AdapterEvent {
    /// The agent began working on a turn. `message`: the turn answers the
    /// oldest message sent to the agent whose turn has not started (an
    /// agent begins those turns in sending order); false for a turn the
    /// agent started on its own (a scheduled wakeup, a background job's
    /// report), which answers none. Reported with `message: true` while a
    /// turn is open, the agent took that message up inside the open turn,
    /// whose one `TurnEnded` ends both. The supervisor settles the messages
    /// it owes by this alone, so an adapter must not guess it.
    TurnStarted { message: bool },
    /// Text the agent wrote for the user. `parent` is the call that started
    /// the subagent that wrote it, or `None` for the main agent.
    Text {
        text: String,
        parent: Option<AgentCallId>,
    },
    /// The agent invoked tool `name` with `input`. `call` is the agent's id
    /// for it, if it has one; `parent` the call that started the subagent
    /// that invoked it, or `None` for the main agent; `display` is how to
    /// show it, or `None` for a generic display derived from `input`.
    ToolCall {
        call: Option<AgentCallId>,
        parent: Option<AgentCallId>,
        name: String,
        input: serde_json::Value,
        display: Option<ToolDisplay>,
    },
    /// A tool call ended; `call` names it, if the agent said which.
    ToolResult {
        call: Option<AgentCallId>,
        result: ResultDisplay,
    },
    /// The agent is blocked until [`AgentSession::answer_approval`] for
    /// `request` (D8). `call` is the tool call the approval gates, if known.
    /// `suggestions` are the lasting rules the agent offers to grant with an
    /// approval, labelled uncut; an answer names one by its index.
    ///
    /// `task` is the background task asking, by its [`AgentTask::id`], or
    /// `None` for the current turn's own. A task's approval is not the
    /// turn's: it may come outside any turn and outlives the turn's end. The
    /// adapter cancels it when the task leaves the set, or the agent
    /// withdraws it; a turn's leftover approvals the adapter cancels before
    /// `TurnEnded`.
    ApprovalRequested {
        request: AgentRequestId,
        call: Option<AgentCallId>,
        tool: String,
        input: serde_json::Value,
        suggestions: Vec<RuleSuggestion>,
        task: Option<String>,
    },
    /// The agent asks the user `questions` and is blocked until
    /// [`AgentSession::answer_question`] for `request`, or a denial with
    /// [`AgentSession::answer_approval`] (D8). Otherwise as
    /// `ApprovalRequested`: `call` is the call asking, `task` the
    /// background task, cancelled the same ways. Uncut.
    QuestionAsked {
        request: AgentRequestId,
        call: Option<AgentCallId>,
        questions: Vec<Question>,
        task: Option<String>,
    },
    /// The agent withdrew `request`; an answer to it is no longer read.
    ApprovalCancelled {
        request: AgentRequestId,
        reason: String,
    },
    /// What the turn took, figures for this turn alone; reported at most
    /// once a turn, just before its `TurnEnded`, and only with something in
    /// it. Optional: an agent that measures nothing never sends it.
    TurnUsage(TurnUsage),
    /// The subagent `call` ran finished, and this is what it ran; at most
    /// once a subagent, inside a turn or outside any. Optional: an agent
    /// that reports nothing never sends it, nor for a stopped subagent.
    SubagentUsage {
        call: AgentCallId,
        usage: SubagentFigures,
    },
    /// The current turn is over.
    TurnEnded(TurnOutcome),
    /// The handle to persist for resuming this context; replaces any earlier.
    ResumeHandle(ResumeHandle),
    /// The grants to persist for the session, the whole set; replaces any
    /// earlier.
    Grants(Grants),
    /// The agent runs in `mode`, as it reported unasked (at start, at a
    /// turn, or a change of its own). May repeat what it said before.
    PermissionMode(PermissionMode),
    /// The answer to the oldest unanswered mode request: the agent switched.
    PermissionModeSet(PermissionMode),
    /// The answer to the oldest unanswered mode request: the agent refused
    /// `mode` and keeps the one it had.
    PermissionModeRefused {
        mode: PermissionMode,
        reason: String,
    },
    /// The agent runs `model`, as it reported unasked; may repeat.
    Model(String),
    /// The answer to the oldest unanswered model request: the agent runs
    /// `model` now, as it names it.
    ModelSet(String),
    /// The answer to the oldest unanswered model request: the agent refused
    /// `model` (as asked) and keeps the one it had.
    ModelRefused { model: String, reason: String },
    /// The agent runs at `effort`, as it reported unasked; `None` when its
    /// model takes no effort level. May repeat.
    Effort(Option<Effort>),
    /// The answer to the oldest unanswered effort request: the agent runs at
    /// `effort` now (`None`: its model takes no effort level).
    EffortSet(Option<Effort>),
    /// The answer to the oldest unanswered effort request: the agent refused
    /// `effort` and keeps the one it had.
    EffortRefused { effort: Effort, reason: String },
    /// The models the agent offers, uncut.
    ModelsOffered(Vec<ModelChoice>),
    /// The agent said whether it is logged in, and how it said it
    /// (`source`). Not logged: the supervisor applies it to its
    /// [`LoginState`].
    LoggedIn {
        logged_in: bool,
        source: LoginSource,
    },
    /// The agent runs `version`, not `confirmed`, the version the adapter
    /// was checked against; uncut. At most once per process.
    VersionUnconfirmed { version: String, confirmed: String },
    /// Every live background task, in the agent's order; replaces the set
    /// reported before. May repeat it.
    BackgroundTasks(Vec<AgentTask>),
    /// Every scheduled wakeup, in the agent's order; replaces the set
    /// reported before. May repeat it.
    Schedules(Vec<AgentSchedule>),
    /// The answer to the oldest unanswered stop: the agent took it. The
    /// task leaving the set is the outcome; this only retires the ask.
    TaskStopAnswered { task: String },
    /// The answer to the oldest unanswered stop: the agent did not stop
    /// `task`.
    TaskStopRefused { task: String, reason: String },
    /// The agent stopped; no further events follow. `error` is set when it
    /// stopped abnormally (crash, startup failure). `unsent` counts the
    /// messages it accepted ([`AgentSession::send_message`] returned `Ok`)
    /// but never began a turn for: queued behind a turn when it was stopped,
    /// killed or crashed, or not yet read when it exited. They never run;
    /// skywaved does not redeliver them.
    Exited {
        error: Option<String>,
        unsent: usize,
    },
}

/// How an agent said whether it is logged in ([`AdapterEvent::LoggedIn`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginSource {
    /// What the agent reported as it started. It may be older than a login
    /// made since the process started.
    Start,
    /// A turn the agent refused for want of a login. It may be older than a
    /// login made since the turn's message was handed over.
    Turn,
}

/// Whether the agent is logged in, as far as skywaved knows
/// ([`LoginState::status`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LoginStatus {
    /// No session process has said yet, and no login was made.
    #[default]
    Unknown,
    LoggedIn,
    LoggedOut,
}

/// The agent's login as skywaved last learned it, shared by the supervisor
/// (which learns it from session processes) and whoever reports it. Cheap
/// to clone; every clone is the same cell. Nothing starts a process just
/// to find out: it stays [`LoginStatus::Unknown`] until one says.
///
/// It also counts the login successes ([`LoginState::login_succeeded`]),
/// so that a "logged out" older than a success is told apart from a fresh
/// one by position, not by timing. The supervisor keeps a [`LoginSeen`]
/// per process: the count when it began the process's start, and when it
/// handed over each message. A "logged out" the process reported at start
/// ([`LoginSource::Start`]) counts from the start's count; one from a
/// refused turn ([`LoginSource::Turn`]) from the count when that turn's
/// message was handed over (for a turn the agent started on its own, the
/// latest message handed over, else the start). [`LoginState::apply`]
/// drops it when a login succeeded since: the process may have answered
/// before it saw the login. "Logged in" never needs the check.
#[derive(Debug, Clone, Default)]
pub struct LoginState(Arc<Mutex<LoginCell>>);

#[derive(Debug, Default)]
struct LoginCell {
    status: LoginStatus,
    successes: u64,
}

impl LoginState {
    pub fn new() -> LoginState {
        LoginState::default()
    }

    /// Whether the agent is logged in, as last learned.
    pub fn status(&self) -> LoginStatus {
        self.cell().status
    }

    /// The login successes so far.
    pub fn successes(&self) -> u64 {
        self.cell().successes
    }

    /// A login succeeded: the agent is logged in, and every process
    /// started before now reported at start what it no longer is.
    pub fn login_succeeded(&self) {
        let mut cell = self.cell();
        cell.status = LoginStatus::LoggedIn;
        cell.successes += 1;
    }

    /// Applies what a session process said (`logged_in`, said as
    /// `source`), with what it had `seen` of the successes. A "logged out"
    /// is dropped when a login succeeded after what it answers (its start,
    /// or its turn's message); whether it was applied.
    pub fn apply(&self, logged_in: bool, source: LoginSource, seen: &LoginSeen) -> bool {
        let mut cell = self.cell();
        let answers = match source {
            LoginSource::Start => seen.start,
            LoginSource::Turn => seen.turn,
        };
        if !logged_in && answers < cell.successes {
            return false;
        }
        cell.status = if logged_in {
            LoginStatus::LoggedIn
        } else {
            LoginStatus::LoggedOut
        };
        true
    }

    fn cell(&self) -> std::sync::MutexGuard<'_, LoginCell> {
        // Every write leaves the cell whole, so a panic elsewhere while it
        // was held spoils nothing.
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The [`LoginState::successes`] one session process may have seen, kept
/// by the supervisor as it starts the process and hands it messages, and
/// as the process starts their turns ([`LoginSeen::observe`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginSeen {
    /// When its start began.
    start: u64,
    /// When each message handed over whose turn has not started was.
    handed: VecDeque<u64>,
    /// When the latest message was handed over, else `start`.
    latest: u64,
    /// What the current (or last) turn answers: its message's count, or
    /// for a turn the agent started on its own, `latest` then.
    turn: u64,
}

impl LoginSeen {
    /// A process whose start began at `successes`.
    pub fn started(successes: u64) -> LoginSeen {
        LoginSeen {
            start: successes,
            handed: VecDeque::new(),
            latest: successes,
            turn: successes,
        }
    }

    /// A message was handed over at `successes`.
    pub fn handed(&mut self, successes: u64) {
        self.handed.push_back(successes);
        self.latest = successes;
    }

    /// Follows the process's `event`, before it is applied: a turn started
    /// for a message (in order, one per message handed over) answers that
    /// message; one it started on its own, the latest message.
    pub fn observe(&mut self, event: &AdapterEvent) {
        match event {
            AdapterEvent::TurnStarted { message: true } => {
                self.turn = self.handed.pop_front().unwrap_or(self.latest);
            }
            AdapterEvent::TurnStarted { message: false } => self.turn = self.latest,
            _ => {}
        }
    }
}

/// Receiving end of an agent session's [`AdapterEvent`] stream.
pub type AdapterEvents = mpsc::Receiver<AdapterEvent>;

/// Why an adapter could not do what the supervisor asked.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    /// The agent has exited; the session takes no more requests.
    #[error("agent session has ended")]
    Closed,
    /// The agent could not be started.
    #[error("cannot start agent: {0}")]
    Start(String),
    /// The resume handle is unusable; starting fresh would lose history.
    #[error("cannot resume agent session: {0}")]
    Resume(String),
    /// The agent no longer has the conversation to resume or fork: its
    /// history is gone on the server, so no retry brings it back and only a
    /// new context goes on. The text says so ([`CONVERSATION_GONE`]), as it
    /// is what the user reads.
    #[error("{CONVERSATION_GONE} (cannot resume agent session: {0})")]
    ConversationGone(String),
    #[error("agent i/o: {0}")]
    Io(#[from] std::io::Error),
}

/// Starts agent sessions. One adapter per daemon, shared by all sessions.
pub trait AgentAdapter: Send + Sync + 'static {
    type Session: AgentSession;

    /// Starts the agent for one session in `spec.conversation` (fresh,
    /// resumed or forked) and returns the session with its event stream.
    /// An unusable handle to resume or fork fails with
    /// [`AgentError::Resume`], and a well-formed one naming a conversation
    /// the agent no longer has with [`AgentError::ConversationGone`]; an
    /// adapter that cannot fork fails a fork with [`AgentError::Start`].
    fn start(
        &self,
        spec: StartSpec,
    ) -> impl Future<Output = Result<(Self::Session, AdapterEvents), AgentError>> + Send;
}

/// The exit error of an agent stopped by
/// [`AgentSession::escalate_interrupt`].
pub const INTERRUPT_UNANSWERED: &str = "agent did not answer the interrupt; stopped it";

/// What the user is told when a context's conversation cannot be resumed
/// because the agent no longer has it ([`AgentError::ConversationGone`]).
pub const CONVERSATION_GONE: &str = "This context's conversation history is gone from the \
    server, so the agent cannot go on with it. Start a new context to continue.";

/// A user message as the supervisor hands it over: the text and its image
/// attachments, each as an open file. The supervisor only opens the files;
/// it runs every session on one runtime, where reading them would stall the
/// others. The adapter reads them ([`UserInput::read`]), and an open file
/// keeps its bytes even if the blob is deleted meanwhile.
#[derive(Debug)]
pub struct UserInput {
    pub text: String,
    pub images: Vec<ImageInput>,
}

/// An image attachment of a [`UserInput`], opened and not read yet.
#[derive(Debug)]
pub struct ImageInput {
    pub media_type: ImageType,
    pub file: std::fs::File,
}

/// What is appended to a message's text for each image that could not be
/// read, so the agent is not told about a picture it cannot see.
pub const IMAGE_REMOVED: &str = "[image removed before delivery]";

/// A user message with its images read: what an adapter writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadInput {
    pub text: String,
    pub images: Vec<Image>,
}

/// An image attachment's bytes, read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub media_type: ImageType,
    pub bytes: Vec<u8>,
}

impl UserInput {
    /// Appends [`IMAGE_REMOVED`] to the text, on a line of its own.
    pub fn note_missing(&mut self) {
        if !self.text.is_empty() {
            self.text.push('\n');
        }
        self.text.push_str(IMAGE_REMOVED);
    }

    /// Reads every image to its end. An image that cannot be read is left
    /// out and noted with [`UserInput::note_missing`]; the rest is
    /// delivered. This blocks on file I/O, so it runs off the async runtime.
    pub fn read(mut self) -> ReadInput {
        let mut images = Vec::with_capacity(self.images.len());
        for ImageInput {
            media_type,
            mut file,
        } in std::mem::take(&mut self.images)
        {
            let mut bytes = Vec::new();
            match file.read_to_end(&mut bytes) {
                Ok(_) => images.push(Image { media_type, bytes }),
                Err(_) => self.note_missing(),
            }
        }
        ReadInput {
            text: self.text,
            images,
        }
    }
}

/// A text-only message, with no images.
impl From<String> for UserInput {
    fn from(text: String) -> Self {
        UserInput {
            text,
            images: Vec::new(),
        }
    }
}

/// A text-only message, with no images.
impl From<&str> for UserInput {
    fn from(text: &str) -> Self {
        UserInput::from(text.to_owned())
    }
}

/// One running agent session.
///
/// Requests return once the agent has been told; results arrive as
/// [`AdapterEvent`]s. After the agent exits every request fails with
/// [`AgentError::Closed`].
pub trait AgentSession: Send + 'static {
    /// Sends a user message with its images; it starts a turn, or the next
    /// one if a turn runs. The adapter reads the images itself
    /// ([`UserInput::read`]), before the message is written.
    fn send_message(
        &mut self,
        input: UserInput,
    ) -> impl Future<Output = Result<(), AgentError>> + Send;

    /// Answers approval `request`; with an approval, `rule` grants the
    /// request's suggestion with that index too (the log only holds a rule
    /// the request offered, never with a denial). An answer to a request
    /// that is no longer pending is dropped by the adapter.
    fn answer_approval(
        &mut self,
        request: AgentRequestId,
        decision: Decision,
        rule: Option<u8>,
    ) -> impl Future<Output = Result<(), AgentError>> + Send;

    /// Answers question `request` with `answers`, one per question in
    /// order (the log only holds answers that fit). Dropped by the adapter
    /// like an approval's answer once the request is no longer pending.
    fn answer_question(
        &mut self,
        request: AgentRequestId,
        answers: Vec<QuestionAnswer>,
    ) -> impl Future<Output = Result<(), AgentError>> + Send;

    /// Stops the current turn, if any; a no-op when idle.
    fn interrupt(&mut self) -> impl Future<Output = Result<(), AgentError>> + Send;

    /// The user asked again to stop a turn already interrupted; called only
    /// after [`AgentSession::interrupt`] for that turn. If the agent has not
    /// answered that interrupt (the turn has not ended), it is stopped: its
    /// stream ends with [`AdapterEvent::Exited`] carrying
    /// [`INTERRUPT_UNANSWERED`], the turn and its approvals left for the
    /// caller as on a crash. Otherwise it does nothing: a turn running by
    /// then (a queued message's) is a later one, never interrupted by it.
    fn escalate_interrupt(&mut self) -> impl Future<Output = Result<(), AgentError>> + Send;

    /// Asks the agent to run in `mode`; answered by `PermissionModeSet` or
    /// `PermissionModeRefused`.
    fn set_permission_mode(
        &mut self,
        mode: PermissionMode,
    ) -> impl Future<Output = Result<(), AgentError>> + Send;

    /// Asks the agent to run `model`; answered by `ModelSet` or
    /// `ModelRefused`.
    fn set_model(&mut self, model: String) -> impl Future<Output = Result<(), AgentError>> + Send;

    /// Asks the agent to run at `effort`; answered by `EffortSet` or
    /// `EffortRefused`.
    fn set_effort(&mut self, effort: Effort)
    -> impl Future<Output = Result<(), AgentError>> + Send;

    /// Asks the agent to stop background task `task` (an [`AgentTask::id`]);
    /// answered by `TaskStopAnswered` or `TaskStopRefused`.
    fn stop_task(&mut self, task: String) -> impl Future<Output = Result<(), AgentError>> + Send;

    /// Stops the agent; its event stream ends with [`AdapterEvent::Exited`].
    /// Interrupt first to stop a running turn cleanly; a turn still running
    /// without an interrupt is abandoned, not ended, and its approvals are
    /// left for the caller. Messages queued behind the turn are not begun:
    /// the `Exited` counts them as `unsent`.
    fn shutdown(self) -> impl Future<Output = Result<(), AgentError>> + Send;
}

/// The conversations an agent keeps on this machine, those had outside
/// Skywave included, for a session to adopt (`Conversation::Adopt`), and
/// what was said in one before it was adopted.
///
/// Separate from [`AgentAdapter`]: the adapter lives on the supervisor's
/// runtime, while a listing or a history read is served to the API and to
/// command handling, so this is a shared, blocking reader (`Send + Sync`;
/// call it off the async runtime). Every read stays under the server's
/// roots and on the agent's own files, and gives up at its `deadline`.
pub trait OutsideConversations: Send + Sync + 'static {
    /// The newest conversations whose workdir is under `roots`, at most
    /// `max`, newest first, stopping at `deadline` with what it has
    /// (`truncated`). `own` maps the ids of the conversations Skywave's
    /// contexts run to their session, which each listed item carries as
    /// [`OutsideConversation::session`].
    fn list(
        &self,
        roots: &[PathBuf],
        own: &HashMap<String, SessionId>,
        max: usize,
        deadline: Instant,
    ) -> std::result::Result<Listed, SourceError>;

    /// The conversation `id` kept for `workdir` (exactly; the caller has
    /// checked it against the roots), as history items, oldest first.
    fn history(&self, workdir: &Path, id: &str, deadline: Instant) -> HistoryRead;

    /// The `own` map [`OutsideConversations::list`] takes, read from the
    /// contexts' resume handles in `db`: the agent's id of each
    /// conversation a context runs, with that context's session. Only the
    /// agent knows what its handles name; by default none is its own.
    fn own(&self, _db: &Db) -> Result<HashMap<String, SessionId>> {
        Ok(HashMap::new())
    }
}

/// One conversation in a [`Listed`].
#[derive(Debug, Clone, PartialEq)]
pub struct OutsideConversation {
    /// The agent's own id, what [`Conversation::Adopt`] names.
    pub id: String,
    /// The directory the conversation ran in, canonical and under a root.
    pub workdir: String,
    /// The title the agent gave it, if any.
    pub title: Option<String>,
    /// The first line of the first prompt, uncut.
    pub first_prompt: Option<String>,
    pub last_active: DateTime<Utc>,
    /// The prompts and the agent's texts in it.
    pub messages: u32,
    /// The Skywave session one of whose contexts runs this conversation.
    pub session: Option<SessionId>,
}

/// What [`OutsideConversations::list`] found.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Listed {
    pub conversations: Vec<OutsideConversation>,
    /// More conversations may exist: the cap or the deadline was reached.
    pub truncated: bool,
}

/// Why [`OutsideConversations::list`] has no listing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SourceError {
    /// This agent cannot list its conversations.
    #[error("the agent's conversations cannot be listed")]
    Unavailable,
    #[error("listing the agent's conversations failed: {0}")]
    Failed(String),
}

/// The usage limits of the account the agent runs as, read when a device
/// asks (`GET /v1/usage-limits`). They belong to the account, not to a
/// session, so this is a shared source the API calls, apart from
/// [`AgentAdapter`].
pub trait UsageLimitsSource: Send + Sync + 'static {
    /// The limits as the agent reports them now, in its order, uncut;
    /// empty when the account is under none. Gives up at `deadline`.
    fn read(
        &self,
        deadline: Instant,
    ) -> BoxFuture<'_, std::result::Result<Vec<UsageLimit>, LimitsError>>;
}

/// Why [`UsageLimitsSource::read`] has no limits.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LimitsError {
    /// This agent cannot report usage limits.
    #[error("the agent cannot report usage limits")]
    Unavailable,
    #[error("{0}")]
    Failed(String),
}

/// A source for an agent that reports no usage limits: every read is
/// [`LimitsError::Unavailable`].
#[derive(Debug, Clone, Copy, Default)]
pub struct NoUsageLimits;

impl UsageLimitsSource for NoUsageLimits {
    fn read(
        &self,
        _deadline: Instant,
    ) -> BoxFuture<'_, std::result::Result<Vec<UsageLimit>, LimitsError>> {
        Box::pin(async { Err(LimitsError::Unavailable) })
    }
}

/// The login the agent runs as, read when a device says `hello`
/// (`login_expires`). Like the usage limits it belongs to the account, not
/// to a session.
pub trait AgentLogin: Send + Sync + 'static {
    /// When the login expires, read now; `None` when it does not expire or
    /// cannot be told. Quick and blocking: a small file read at most.
    fn expires(&self) -> Option<DateTime<Utc>>;

    /// Whether a device may log the agent in (`agent_login_offered`):
    /// false when the agent has no login a device can run, or one the
    /// environment gives it takes precedence over any it would make.
    fn offered(&self) -> bool;

    /// Starts one login attempt: a process of the agent's own, asked for a
    /// URL to sign in at. Gives up at `deadline`, stopping the process.
    fn begin(
        &self,
        deadline: Instant,
    ) -> BoxFuture<'_, std::result::Result<Box<dyn LoginAttempt>, LoginError>>;
}

/// One open login attempt, from [`AgentLogin::begin`]: the user signs in at
/// [`url`](Self::url) and pastes back the code the page shows. Dropping it
/// stops the agent's process, so an attempt never outlives its holder.
pub trait LoginAttempt: Send + Sync {
    /// Where to sign in. Never logged: it carries the attempt's state.
    fn url(&self) -> &str;

    /// Whether `code` is shaped like an answer at all, without asking the
    /// agent; the refusal says what to paste. A refusal leaves the attempt
    /// open.
    fn check(&self, code: &str) -> std::result::Result<(), String>;

    /// Hands the pasted `code` to the agent and waits for its own answer,
    /// with no deadline: the answer is the outcome. `Err` carries the
    /// agent's words. The attempt ends either way.
    fn finish(self: Box<Self>, code: String)
    -> BoxFuture<'static, std::result::Result<(), String>>;
}

/// Leaves the URL out.
impl std::fmt::Debug for dyn LoginAttempt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LoginAttempt")
    }
}

/// Why [`AgentLogin::begin`] has no attempt.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LoginError {
    /// This agent has no login a device can run.
    #[error("the agent has no login a device can run")]
    Unavailable,
    #[error("{0}")]
    Failed(String),
}

/// A login for an agent that says nothing of its expiry and has no login a
/// device can run.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoAgentLogin;

impl AgentLogin for NoAgentLogin {
    fn expires(&self) -> Option<DateTime<Utc>> {
        None
    }

    fn offered(&self) -> bool {
        false
    }

    fn begin(
        &self,
        _deadline: Instant,
    ) -> BoxFuture<'_, std::result::Result<Box<dyn LoginAttempt>, LoginError>> {
        Box::pin(async { Err(LoginError::Unavailable) })
    }
}

/// One step of a conversation's history, as [`OutsideConversations::history`]
/// reads it. `at` is when the agent recorded it, if it did.
#[derive(Debug, Clone, PartialEq)]
pub enum HistoryItem {
    /// A message the user sent, verbatim.
    User {
        at: Option<DateTime<Utc>>,
        text: String,
    },
    /// What the agent did: only [`AdapterEvent::Text`],
    /// [`AdapterEvent::ToolCall`] and [`AdapterEvent::ToolResult`], uncut,
    /// with no `parent` (a subagent's own steps are not read).
    Agent {
        at: Option<DateTime<Utc>>,
        event: AdapterEvent,
    },
}

/// The answer of [`OutsideConversations::history`].
#[derive(Debug, Clone, PartialEq)]
pub enum HistoryRead {
    /// The conversation, oldest first.
    Read { items: Vec<HistoryItem> },
    /// The agent keeps no conversation with this id for this workdir.
    NotFound,
    /// It exists but could not be read (too large, an I/O error, the
    /// deadline).
    Failed(String),
    /// This agent cannot read its conversations.
    Unavailable,
}

/// A source for an agent that keeps no conversations Skywave can read:
/// listing is [`SourceError::Unavailable`], history
/// [`HistoryRead::Unavailable`].
#[derive(Debug, Clone, Copy, Default)]
pub struct NoOutsideConversations;

impl OutsideConversations for NoOutsideConversations {
    fn list(
        &self,
        _roots: &[PathBuf],
        _own: &HashMap<String, SessionId>,
        _max: usize,
        _deadline: Instant,
    ) -> std::result::Result<Listed, SourceError> {
        Err(SourceError::Unavailable)
    }

    fn history(&self, _workdir: &Path, _id: &str, _deadline: Instant) -> HistoryRead {
        HistoryRead::Unavailable
    }
}

/// Persists `handle` as session context `context`'s resume handle.
pub fn store_resume_handle(db: &Db, context: ContextId, handle: &ResumeHandle) -> Result<()> {
    let changed = db
        .conn()
        .prepare_cached("UPDATE contexts SET agent_state = ?2 WHERE id = ?1")?
        .execute((context.0.to_string(), serde_json::to_string(handle)?))?;
    if changed == 0 {
        return Err(Error::UnknownContext(context));
    }
    Ok(())
}

/// The resume handle stored for session context `context`; `None` until
/// the agent has reported one.
pub fn load_resume_handle(db: &Db, context: ContextId) -> Result<Option<ResumeHandle>> {
    let row: Option<Option<String>> = db
        .conn()
        .prepare_cached("SELECT agent_state FROM contexts WHERE id = ?1")?
        .query_row([context.0.to_string()], |r| r.get(0))
        .optional()?;
    match row {
        None => Err(Error::UnknownContext(context)),
        Some(json) => Ok(json.as_deref().map(serde_json::from_str).transpose()?),
    }
}

/// What session context `context` continues, its origin: a context of
/// another session moved here, or a conversation the agent had outside
/// Skywave, set on the first context of a session a move or an adoption
/// created; `None` for any other.
pub fn load_origin(db: &Db, context: ContextId) -> Result<Option<SessionOrigin>> {
    let row: Option<Option<String>> = db
        .conn()
        .prepare_cached("SELECT origin FROM contexts WHERE id = ?1")?
        .query_row([context.0.to_string()], |r| r.get(0))
        .optional()?;
    match row {
        None => Err(Error::UnknownContext(context)),
        Some(json) => Ok(json.as_deref().map(serde_json::from_str).transpose()?),
    }
}

/// Persists `grants` as session `session`'s grants, replacing any earlier.
pub fn store_grants(db: &Db, session: SessionId, grants: &Grants) -> Result<()> {
    let changed = db
        .conn()
        .prepare_cached("UPDATE sessions SET agent_grants = ?2 WHERE id = ?1")?
        .execute((session.0.to_string(), serde_json::to_string(grants)?))?;
    if changed == 0 {
        return Err(Error::UnknownSession(session));
    }
    Ok(())
}

/// The grants stored for session `session`; `None` until the agent has
/// reported any.
pub fn load_grants(db: &Db, session: SessionId) -> Result<Option<Grants>> {
    let row: Option<Option<String>> = db
        .conn()
        .prepare_cached("SELECT agent_grants FROM sessions WHERE id = ?1")?
        .query_row([session.0.to_string()], |r| r.get(0))
        .optional()?;
    match row {
        None => Err(Error::UnknownSession(session)),
        Some(json) => Ok(json.as_deref().map(serde_json::from_str).transpose()?),
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;

    use super::*;

    #[test]
    fn read_returns_each_image_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("a.png");
        let jpeg = dir.path().join("b.jpeg");
        std::fs::write(&png, b"png-bytes").unwrap();
        std::fs::write(&jpeg, b"jpeg-bytes").unwrap();
        let input = UserInput {
            text: "look".into(),
            images: vec![
                ImageInput {
                    media_type: ImageType::Png,
                    file: File::open(&png).unwrap(),
                },
                ImageInput {
                    media_type: ImageType::Jpeg,
                    file: File::open(&jpeg).unwrap(),
                },
            ],
        };
        assert_eq!(
            input.read(),
            ReadInput {
                text: "look".into(),
                images: vec![
                    Image {
                        media_type: ImageType::Png,
                        bytes: b"png-bytes".to_vec(),
                    },
                    Image {
                        media_type: ImageType::Jpeg,
                        bytes: b"jpeg-bytes".to_vec(),
                    },
                ],
            }
        );
    }

    #[test]
    fn an_image_that_cannot_be_read_is_left_out_and_noted() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("a.png");
        std::fs::write(&png, b"png-bytes").unwrap();
        // A directory opens, but reading it fails with EISDIR on Linux.
        let input = UserInput {
            text: "look".into(),
            images: vec![
                ImageInput {
                    media_type: ImageType::Jpeg,
                    file: File::open(dir.path()).unwrap(),
                },
                ImageInput {
                    media_type: ImageType::Png,
                    file: File::open(&png).unwrap(),
                },
            ],
        };
        let read = input.read();
        assert_eq!(read.text, "look\n[image removed before delivery]");
        assert_eq!(
            read.images,
            [Image {
                media_type: ImageType::Png,
                bytes: b"png-bytes".to_vec(),
            }]
        );
    }

    #[test]
    fn a_note_on_an_empty_text_is_the_note_alone() {
        let mut input = UserInput::from("");
        input.note_missing();
        assert_eq!(input.text, IMAGE_REMOVED);
    }
}
