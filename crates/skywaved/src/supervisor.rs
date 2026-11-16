//! The agent supervisor (architecture.md "Server"): runs one agent session per Skywave
//! session that has work, generic over the [`AgentAdapter`].
//!
//! **Contexts.** A Skywave session holds an ordered list of contexts, each
//! its own agent conversation with its own resume handle. At most one agent
//! process runs per session, for one context: the one stamped on the event
//! it was started for ([`Event::context`]), not the session's current
//! context, which a switch logged after that event may already have moved.
//! Everything the process reports is stamped with its context, also what it
//! reports after a switch (its interrupted turn's end, its exit).
//!
//! **Following the log.** The supervisor learns about commands from the log
//! itself, like any other reader: it reads every event after its cursor and
//! reacts to
//! - `UserMessage` → [`AgentSession::send_message`], starting the session's
//!   agent for the message's context first if it is not running (lazily,
//!   and given the session's [`Grants`](crate::agent::Grants); for which
//!   conversation, see **Conversations** below). An agent running another context is stopped first, like
//!   on `SessionStopped`; validated commands never get there, since a
//!   context change already stopped it;
//! - `ApprovalResolved` → [`EventMapper::take_approval`] and
//!   [`AgentSession::answer_approval`]. Command handling already lets only the
//!   first answer in (D5), so the agent hears exactly one answer per request;
//! - `TurnInterrupted` → [`AgentSession::interrupt`], if the agent's current
//!   turn began before the interrupt. The agent then reports
//!   `TurnEnded(Interrupted)` and the mapper cancels leftover approvals and
//!   appends `SessionStateChanged(Idle)`. An interrupt aimed at an earlier
//!   turn (that already ended) is ignored, so it can't stop a later one. A
//!   further `TurnInterrupted` for a turn already handed an interrupt
//!   (command handling accepts one until the turn ends) goes out as
//!   [`AgentSession::escalate_interrupt`]: an agent that has not answered
//!   the first is stopped and exits with
//!   [`INTERRUPT_UNANSWERED`](crate::agent::INTERRUPT_UNANSWERED), which the
//!   mapper logs as `Errored` like any crash; the next `UserMessage`
//!   resumes it.
//! - `SessionStopped` → [`AgentSession::interrupt`] if a turn runs or a
//!   delivered message's turn has not ended, then
//!   [`AgentSession::shutdown`]. The agent reports `TurnEnded(Interrupted)`
//!   (so the mapper appends `SessionStateChanged(Idle)`) and `Exited`. A
//!   message delivered behind that turn never runs: the `Exited` counts it
//!   as unsent and the mapper logs one `Errored` saying so, after the
//!   `Idle`. A session whose agent is not running is left alone. Like an idle
//!   shutdown, the next `UserMessage` resumes the agent.
//! - `ContextStarted` / `ContextSwitched` (stamped with the context they
//!   make current) → exactly what `SessionStopped` does, when the agent runs
//!   another context; nothing when it runs that context or none runs. The
//!   new context is never started here: its next message or setting request
//!   starts it, resumed with its handle or fresh.
//! - `ContextMoved` (stamped with the moved context, in the session it
//!   leaves) → what `SessionStopped` does, if the agent still runs the
//!   moved context. It cannot: a context is moved only when not current,
//!   and the change away from it already stopped its agent. The new
//!   session's `SessionCreated { from }` needs nothing either; the new
//!   session starts lazily like any other.
//! - `PermissionModeRequested` / `ModelRequested` / `EffortRequested` →
//!   [`AgentSession::set_permission_mode`] / [`AgentSession::set_model`] /
//!   [`AgentSession::set_effort`], noted with the mapper so the agent's answer names the command. A
//!   session whose agent is not running is started for it, in the request's
//!   context, as for a message (no message is
//!   sent, so no turn runs) and kept like any idle session. Only the
//!   session's open request goes out, once per agent: one a newer request
//!   replaced before the supervisor reached it is skipped, so requests
//!   never land out of order, and one the agent's start already asked for
//!   is not asked again.
//!   Bypass never reaches the agent while the log does not allow it
//!   ([`EventLog::bypass_allowed`], D38): an open bypass request (accepted
//!   before a restart that turned the opt-in off) is refused here,
//!   `PermissionModeRefused` ([`BYPASS_NOT_ALLOWED`]) answering its
//!   command, without starting an agent; and a start does not ask for a
//!   bypass the log last had, so the agent's own mode is logged instead.
//! - `TaskStopRequested` → [`AgentSession::stop_task`], noted with the
//!   mapper ([`EventMapper::asked_stop`]) so a refusal names the command.
//!   Never starts an agent: with none running, the supervisor logs
//!   `TaskStopRefused` ("agent not running") answering the command, in the
//!   event's context. A stop the agent took shows as the task leaving its
//!   next task set; the busy count and idle clock follow that set as for
//!   any other change.
//!
//! **Starts and stops.** An agent start (the adapter's handshake can take
//! up to a minute) and a stop (the grace periods, tens of seconds) run as
//! tasks beside the supervisor's loop, which goes on following the log and
//! the other agents meanwhile: one session's slow agent never delays
//! another session's answers, interrupts or events. The session is *held*
//! until the start or stop completes: its events read meanwhile wait, in
//! log order, and are handled once the completion is in, as if the start
//! or stop had been awaited where it began. So a stop requested while the
//! agent starts stops it once it has started (and been given the message it
//! started for), a message sent while it stops starts it again after the
//! stop, and a clear, switch or move waits like any other event. The
//! outcome is decided by the completion itself (the agent runs, or the
//! start failed and its trigger is answered with why), never by a timer. A
//! message the agent refuses as closed waits for that agent's stop and goes
//! to a new one, once.
//!
//! **Conversations.** A context starts in its own conversation, resumed
//! with its stored [`ResumeHandle`](crate::agent::ResumeHandle), once it has
//! one. Before that, the first context of a session a context was moved
//! into forks its origin's conversation ([`Conversation::Fork`] with the
//! origin's handle) until its own copy reports a handle, so a start stopped
//! before any message forks again; any other context starts fresh. An
//! origin with no handle refuses the start, logged like any failed start,
//! rather than starting fresh. A handle whose conversation the agent no
//! longer has (its history deleted on the server, say by the agent's own
//! retention sweep) fails the start with
//! [`AgentError::ConversationGone`], whose text tells the user to start a
//! new context. It is logged like any failed start and never turned into a
//! fresh start behind the user's back: the agent would answer without the
//! context's history. Each later message tries the resume again (and gets
//! the same answer), so a history put back on the server resumes. The fork
//! reads a finished conversation: the
//! clear or switch before the move stops the source's agent, and a start
//! that would fork waits (holding its session) while the source session is
//! held, so that agent has exited (its stop drains to `Exited`) and nothing
//! queued in the source still reaches it. Its late events (the interrupted
//! turn's `Idle`, its exit) stay in the source, stamped with the moved
//! context.
//!
//! Likewise the first context of a session created by adoption starts as
//! [`Conversation::Adopt`] with the agent's own id of the outside
//! conversation until its copy reports a handle, so a start stopped before
//! any message adopts again, under a new id, and nothing leaks; an id the
//! agent does not know fails that start, so each message errors and the
//! session stays. Unlike a move, the outside conversation may still be
//! live: a terminal agent may still be writing it, and nothing tells
//! skywaved. The fork reads whatever the conversation held when it was
//! made; later outside turns never reach the copy.
//!
//! **Mode, model and effort.** Every agent starts with the mode, model and
//! effort the session's log wants
//! ([`Setting::wanted`](skywave_proto::Setting::wanted)): an open request,
//! else what the agent last ran with (an effort its model takes none of is
//! not asked for). So a restarted or resumed agent keeps all three, and a start made for a request carries that
//! request out; each is asked for under the open request's command, if
//! any, so its answer settles that request. An agent start that fails
//! refuses the request it was made for, and a message whose agent did not
//! start gets `Errored`, both stamped with the trigger's context. The models an agent offers are
//! logged as `ModelsOffered` only when they differ from the newest list in
//! the whole log.
//!
//! **Login.** What an agent says of its login (`AdapterEvent::LoggedIn`) is
//! not logged: it goes to the [`LoginState`] in the config, shared with
//! whoever reports it. No agent is ever started just to ask. A process
//! reads its login once, as it starts, so the supervisor notes the login
//! successes counted when it begins each start and when it hands over each
//! message (`LoginSeen`). A "logged out" the process reported from its
//! start is dropped once a login has succeeded since the start began; one
//! from a turn it refused, once a login has succeeded since that turn's
//! message was handed over (a turn it started on its own counts from the
//! latest message, else the start). The process may have answered before
//! it saw the login; a refusal of a message handed over after it is fresh.
//! What an agent said while it stopped counts the same way.
//!
//! Everything the agent reports goes through the session's [`EventMapper`].
//! An agent that exits on its own (crash) is logged by the mapper (`Errored`
//! when it was mid-turn, its approvals cancelled) and dropped; the next
//! `UserMessage` starts it again with its resume handle. An agent that has
//! stopped reading but whose `Exited` is not handled yet refuses the
//! message (`AgentError::Closed`): `deliver` stops it, logging its exit, and
//! gives the message to a new one, so it still reaches exactly one agent.
//! Messages it accepted and never began a turn for (queued behind a turn it
//! crashed or was killed in, or not read before it left) are counted by its
//! `Exited` and named in the one `Errored` the mapper logs for the exit
//! ("N queued messages were not sent to the agent; send them again"). The
//! supervisor counts them too, as the messages it still owes (below), and
//! the exit names that many when the agent counted fewer (an event stream
//! that ended without `Exited` counts none). They are reported, never
//! redelivered: delivery is at most once (below), and after a stop the user
//! chose to end the work.
//!
//! **Cursor and delivery.** The cursor (`supervisor_cursor.seq`, the seq of
//! the last event read) is persisted, so a message accepted but not yet
//! read when the daemon stopped reaches the agent after the restart. So is
//! each held session's last handled event (`supervisor_held`): its events
//! after that one, up to the cursor, were read but waited for a start or
//! stop, and the next run handles them first, in order. A message whose
//! agent was still starting is one of them. A `UserMessage` is stored as
//! *owed* (`supervisor_owed`, with the cursor) *before* the message is
//! given to the agent: delivery is **at most once**. A crash between the
//! two loses that message rather than running an instruction twice, which
//! an agent with side effects could not undo; the next start says so with
//! `Errored` (below), so the user can resend it. All of these are written
//! in one transaction.
//!
//! A message stays owed until the log shows it settled, per session: the
//! `AgentWorking` of a turn the adapter reports as a message's
//! (`TurnStarted { message: true }`) settles its oldest owed message (an
//! agent begins the turns of the messages it was handed in order), and an
//! `Errored` reporting it (the agent's exit, a failed delivery) settles it
//! too. A turn the agent starts on its own (a wakeup, or the Claude Code
//! CLI's `system/init` with no turn open) reports `message: false` and
//! settles none, so a message queued behind it stays owed: should the
//! daemon stop or crash during that turn, the restart names it in the lost
//! turn's `Errored`. The agent may instead take the message up inside that
//! turn (`TurnStarted { message: true }` while the turn is open): no
//! `AgentWorking` follows, so the message is settled as that is read. Each settling event is tied to the message it settles
//! by position, never by merely following it: the message is first
//! stored as *starting* (its turn's `AgentWorking` is next) or *settling*
//! (an `Errored` reporting it is next), with the log's last seq
//! (`supervisor_cursor.accounted`), and only such an event in its context
//! after that seq settles it. So a stopped agent's exit, logged after a
//! message that waited for the stop, does not answer that message, nor
//! does a turn the agent started on its own. The rows are rewritten,
//! stamped with the log's last seq, whenever they change; a state event
//! the supervisor logged after the stamp but before the next write is read
//! back from the log on restart (see `lost_messages`), so the gap between
//! logging an event and storing what it settled neither loses nor
//! double-reports a message.
//! Nothing outlives its settlement: a message answered long ago is never
//! reported by a later restart. Answers and interrupts need no such care:
//! after a restart their approvals and turns are already settled (below),
//! so replaying them does nothing.
//!
//! **Lifecycle.** An agent session is started on the first message and kept
//! while it has work: a turn open (also one the agent started on its own,
//! e.g. a scheduled wakeup), a delivered message whose turn has not ended,
//! or a live background task that is not ambient (a shell job, a
//! subagent). The tasks are counted as the agent reports them, before the
//! log cuts the set, so a task the log has no room for still counts.
//! Schedules are not work: the agent is not kept for one, and a schedule
//! that fires while it runs starts a turn, which is. Once idle it is kept
//! warm for [`SupervisorConfig::idle_timeout`] and then shut down; the next
//! message resumes it. The clock starts when the last work ends; a later
//! report of still nothing to do does not restart it.
//!
//! **Daemon start.** Before following the log the supervisor settles what the
//! previous run left open (F13):
//! - every pending approval gets `ApprovalCancelled { reason: "daemon
//!   restarted" }` ([`DAEMON_RESTARTED`]);
//! - a session whose log has calls with neither a result nor a
//!   `CallsUnanswered` naming them gets one `CallsUnanswered` per context
//!   they were logged in, stamped with it, naming them in log order and
//!   logged before the state event below. No mapper survives the daemon, so
//!   the log is the only record of them; an idle session's background
//!   subagent can have some too;
//! - a session whose last state event is `SessionStateChanged(AgentWorking)`
//!   lost its turn with the daemon and gets `SessionStateChanged(Errored)`
//!   ([`TURN_LOST`]);
//! - a session whose last state event is `TurnInterrupted` gets
//!   `SessionStateChanged(Idle)`: its agent stopped with the daemon.
//!   Both are stamped with the context of the state event they settle;
//! - the messages still owed (above), in every session, were taken by no
//!   agent: each session gets one `SessionStateChanged(Errored)` per context
//!   they were sent in, stamped with it, naming them ([`MESSAGE_LOST`] for
//!   one, "N messages lost…" for more). Messages queued behind a turn lost
//!   with the daemon (above, in the same context) are named in that turn's
//!   `Errored` instead ("…; N queued messages were not sent to the agent;
//!   send them again"), so each gets exactly one. They are decided before
//!   anything is logged and stored as settling before they are reported,
//!   so a recovery that dies midway reports none twice. A clean shutdown
//!   that stopped an agent before it began a message's turn leaves the
//!   message owed, and it is settled the same way. A message its session
//!   still held unhandled is not lost: the next run hands it over;
//! - a mode, model or effort request still open whose event the supervisor had
//!   already handled is refused ([`DAEMON_RESTARTED`]): the agent it was
//!   handed to is gone. One after the cursor, or held for its session's
//!   agent, is handled as usual. Settings belong to the session, so the
//!   refusal is stamped with the current context;
//! - a session whose logged background tasks or schedules are not empty gets
//!   the empty set(s), `BackgroundTasksChanged` and/or `SchedulesChanged`:
//!   its agent died with the daemon, and its work with it. They are stamped
//!   with the context of the last such event, the agent's own.
//!
//! [`SupervisorHandle::shutdown`] stops the agents without interrupting them
//! ([`AgentSession::shutdown`] abandons a running turn, so the agent cancels
//! none of its approvals) and does not record their exit. Open turns and
//! approvals stay as they are in the log, and the next start settles them the
//! same way whether the daemon stopped cleanly or crashed. The unsent count
//! that `Exited` carries is dropped with it; the messages it counted stay
//! owed, and the next start reports them (above). A pending approval
//! cannot outlive the daemon: the agent's request for it lives only in the
//! agent process (D8). The agents stop concurrently, together with the stops
//! already under way (recorded as they were requested), so shutdown takes
//! about one agent's grace period however many run (systemd's stop timeout
//! is a bound on the whole of it); what each reported while stopping is
//! logged once all have stopped. A start under way is dropped, which kills
//! its agent: the event it was for, and what waited behind it, stay held
//! for the next run.
//!
//! **A low disk** ([`crate::disk`], D40). The supervisor reads the free
//! space as it logs what an agent reports ([`Disk::refresh`]: at most one
//! reading per check interval, so a burst costs one, and none while no
//! agent reports). While the disk is low it stops reading its agents'
//! events: the event channel an
//! adapter reports on is bounded, so once it is full the agent waits to
//! report (the Claude Code CLI blocks writing its stdout). Nothing is
//! lost and nothing is killed: the agent stays alive and supervised, and
//! when the disk's own poll finds room again the supervisor reads on where
//! it stopped.
//! It goes on following the log meanwhile (the API refuses new work, but
//! what it accepted before still reaches its agent), and control still
//! works. A stop drains the agent as always (`halt`), so it ends with the
//! usual interrupt, grace and SIGTERM, and what it reported is logged. An
//! interrupt or a task stop is sent as usual, and that session alone is
//! read until it settles: until the interrupted turn ends, or the agent
//! answers the stop (`Running::controlled`). No agent is stopped as idle
//! while paused, since its unread events may hold work.
//!
//! **Failure.** An error while following the log (the database full or
//! gone, say) ends the supervisor: its agents are stopped as at a shutdown
//! (the adapter's own stop: an interrupted turn's grace, SIGTERM, then
//! SIGKILL for what outlives the term grace; never a bare SIGKILL, which
//! would leave the commands a CLI started running), nothing more is logged,
//! and the error is logged. A start under way is dropped, as at a shutdown.
//! [`SupervisorHandle::failed`] then returns, and `serve` stops the daemon
//! on it and exits non-zero: a daemon without its supervisor would go on
//! accepting commands no agent hears. The next start settles what it left,
//! as after a crash. What is not such an error: a write lock another
//! connection holds (an admin command, an import) is waited out for
//! [`BUSY_TIMEOUT`](crate::db::BUSY_TIMEOUT); content the mapper cannot
//! store as a blob (a full disk) is left out of its event, which is logged
//! with its cut preview and no blob; and what an agent refuses or fails is
//! that session's error, logged in its log. A failed append stays fatal:
//! skipping one would leave the log without an event the mapper's state
//! counts as logged. Recovery stores no blobs, so a restart while they
//! still cannot be stored settles the log and runs on.
//!
//! **Threading.** SQLite and the mapper block, so the supervisor runs on its
//! own thread with a current-thread runtime and its own [`Storage`]; agent
//! sessions (and any tasks their adapter spawns) live on that runtime. All
//! work is serialized there: agent requests are awaited one at a time, so an
//! adapter must return from them promptly (they only tell the agent). Starts
//! and shutdowns are the exception, run as tasks on the same runtime (see
//! **Starts and stops**); a panic in one ends the supervisor like an error.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::poll_fn;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use futures_util::future::join_all;
use skywave_proto::{
    Attachment, CommandId, ContextId, Decision, Effort, Event, EventKind, ModelChoice,
    PermissionMode, Seq, SessionId, SessionOrigin, SessionState, TaskId,
};
use skywave_safety::Verdict;
use tokio::sync::{oneshot, watch};
use tokio::task::{JoinError, JoinSet};
use tokio::time::Instant;

use crate::agent::{
    AdapterEvent, AdapterEvents, AgentAdapter, AgentError, AgentRequestId, AgentSession,
    Conversation, EventMapper, ImageInput, LoginSeen, LoginSource, LoginState, StartSpec,
    UserInput, bounded_models, load_grants, load_origin, load_resume_handle, unsent_notice,
};
use crate::db::Db;
use crate::disk::Disk;
use crate::error::{Error, Result};
use crate::event_log::{self, parse_id};
use crate::{EventLog, Storage};

/// Reason of the `ApprovalCancelled` appended on daemon start (architecture.md "Server").
pub const DAEMON_RESTARTED: &str = "daemon restarted";
/// `Errored` message for a session whose turn was running when the daemon
/// stopped.
pub const TURN_LOST: &str = "daemon restarted during a turn";
/// `Errored` message for a message handed to an agent that did not begin
/// its turn before the daemon stopped (at-most-once delivery).
pub const MESSAGE_LOST: &str = "message lost when the server stopped; send it again";
/// Why a task stop is refused when the session has no agent running.
pub const AGENT_NOT_RUNNING: &str = "agent not running";
/// Why a bypass request is refused by the supervisor itself: one accepted
/// while the server allowed bypass, reached after a restart without it.
pub const BYPASS_NOT_ALLOWED: &str = "bypass is not allowed on this server";
/// Default for [`SupervisorConfig::idle_timeout`].
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Events read from the log per batch.
const BATCH: u32 = 500;

/// Tuning for the supervisor.
#[derive(Debug, Clone)]
pub struct SupervisorConfig {
    /// How long an agent session with no work is kept before it is shut
    /// down. The next message resumes it.
    pub idle_timeout: Duration,
    /// Where the supervisor keeps what its agents say of their login
    /// (see **Login** in the module docs); shared with whoever reports it.
    pub login: LoginState,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        SupervisorConfig {
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            login: LoginState::new(),
        }
    }
}

/// Where the supervisor is with the turns it was given, published after
/// each step it takes ([`SupervisorHandle::activity`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Activity {
    /// Seq of the last log event read: handled, or held for its session.
    pub cursor: Seq,
    /// A turn runs or a message is on its way to one: a message is owed, a
    /// session is held while its agent starts or stops, or an agent has a
    /// turn open or owed. Background tasks are not turns.
    pub turns: bool,
}

/// A running supervisor. Dropping it stops the supervisor in the background;
/// [`SupervisorHandle::shutdown`] stops it and waits.
#[derive(Debug)]
pub struct SupervisorHandle {
    stop: Option<oneshot::Sender<()>>,
    done: oneshot::Receiver<Result<()>>,
    activity: watch::Receiver<Activity>,
    /// What the supervisor ended with, once [`SupervisorHandle::failed`] has
    /// seen it.
    ended: Option<Result<()>>,
}

impl SupervisorHandle {
    /// Stops every agent session and the supervisor, and waits until they
    /// have stopped. Returns the error the supervisor stopped on, if it
    /// failed earlier.
    pub async fn shutdown(mut self) -> Result<()> {
        drop(self.stop.take());
        match self.ended.take() {
            Some(result) => result,
            None => Self::outcome(&mut self.done).await,
        }
    }

    /// Returns once the supervisor has stopped on an error (or panicked);
    /// never while it runs, nor after a clean end. Its agent sessions are
    /// gone by then, stopped as at a shutdown and logging nothing.
    /// [`SupervisorHandle::shutdown`] then returns the error.
    /// Cancel safe.
    pub async fn failed(&mut self) {
        if self.ended.is_none() {
            let result = Self::outcome(&mut self.done).await;
            self.ended = Some(result);
        }
        if matches!(self.ended, Some(Ok(()))) {
            std::future::pending::<()>().await;
        }
    }

    /// Watches the supervisor's [`Activity`]. Until its first step it says
    /// a turn may run.
    pub fn activity(&self) -> watch::Receiver<Activity> {
        self.activity.clone()
    }

    async fn outcome(done: &mut oneshot::Receiver<Result<()>>) -> Result<()> {
        match done.await {
            Ok(result) => result,
            Err(_) => Err(Error::Supervisor("supervisor thread panicked".into())),
        }
    }
}

/// The agent supervisor; see the [module docs](self).
pub struct Supervisor<A: AgentAdapter> {
    adapter: Arc<A>,
    log: EventLog,
    storage: Storage,
    config: SupervisorConfig,
    /// Seq of the last log event read: handled, or waiting in [`Held`].
    cursor: Seq,
    /// Each session's messages handed to its agent whose turn has not
    /// started, in handing order; a session owing none has no entry.
    owed: HashMap<SessionId, Vec<Owed>>,
    /// The progress as last persisted.
    stored: Progress,
    running: HashMap<SessionId, Running<A::Session>>,
    /// Sessions whose agent is starting or stopping, and what waits for it.
    held: HashMap<SessionId, Held>,
    /// Agent starts under way.
    starts: JoinSet<(SessionId, Started<A::Session>)>,
    /// Agent stops under way, each with whether its exit is recorded.
    stops: JoinSet<(SessionId, Halted, bool)>,
    /// The models of the newest `ModelsOffered` in the log.
    models: Option<Vec<ModelChoice>>,
    /// Published after every step.
    activity: watch::Sender<Activity>,
}

/// What [`AgentAdapter::start`] returned.
type Started<S> = Result<(S, AdapterEvents), AgentError>;

/// What the supervisor persists of where it is, besides the log
/// (`supervisor_cursor`, `supervisor_held` and `supervisor_owed`).
#[derive(Debug, Clone, PartialEq)]
struct Progress {
    cursor: Seq,
    /// [`Held::handled`] of every held session.
    held: HashMap<SessionId, Seq>,
    /// [`Supervisor::owed`].
    owed: HashMap<SessionId, Vec<Owed>>,
}

/// A `UserMessage` handed to its session's agent whose turn has not
/// started. It leaves [`Supervisor::owed`] once the agent's
/// `AgentWorking` for it is logged, or an `Errored` naming it is.
#[derive(Debug, Clone, PartialEq)]
struct Owed {
    seq: Seq,
    context: ContextId,
    state: OwedState,
}

/// Where an [`Owed`] message is in being settled; stored with it, so a
/// restart tells which of the supervisor's state events after the rows
/// settle it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OwedState {
    /// Nothing about it is about to be logged.
    Waiting,
    /// Its turn's `AgentWorking` is about to be logged in its context (the
    /// agent reported `TurnStarted { message: true }`): once that is in the
    /// log, it is settled.
    Starting,
    /// An `Errored` reporting it is about to be logged in its context (the
    /// agent's exit, a failed delivery, a restart's): once that is in the
    /// log, it is settled.
    Settling,
}

impl OwedState {
    fn sql(self) -> &'static str {
        match self {
            OwedState::Waiting => "waiting",
            OwedState::Starting => "starting",
            OwedState::Settling => "settling",
        }
    }

    fn from_sql(state: &str) -> Result<OwedState> {
        match state {
            "waiting" => Ok(OwedState::Waiting),
            "starting" => Ok(OwedState::Starting),
            "settling" => Ok(OwedState::Settling),
            other => Err(Error::Corrupt(format!("unknown owed state {other:?}"))),
        }
    }
}

/// A session whose agent is starting or stopping. Its events read
/// meanwhile wait here, in log order, and are handled once the start or
/// stop has completed, so one session's slow agent holds up only its own
/// events.
struct Held {
    /// Seq of the session's last event handled: its events after it, up to
    /// the cursor, are the start's trigger or wait in `queue`. Persisted,
    /// so a restart handles them rather than losing them.
    handled: Seq,
    /// What is under way; `None` while the queue is being handled.
    op: Option<Op>,
    queue: VecDeque<Queued>,
}

impl Held {
    /// Puts `item` back first in line, not handled.
    fn requeue(&mut self, item: Queued) {
        self.handled = self.handled.min(before(item.seq));
        self.queue.push_front(item);
    }
}

/// What a held session waits for.
enum Op {
    /// Its agent starts.
    Starting(Box<Starting>),
    /// Its agent stops.
    Stopping,
    /// Another session to settle (nothing under way or waiting), before
    /// one of this session's contexts forks a conversation of it.
    After(SessionId),
}

/// An agent start under way for `trigger`, a message or setting request,
/// in `trigger`'s context; `mapper` and `asked` are the new agent's.
struct Starting {
    trigger: Queued,
    mapper: EventMapper,
    asked: HashSet<Option<CommandId>>,
    /// [`LoginState::successes`] when the start began.
    logins: u64,
}

/// A log event the supervisor handles.
struct Queued {
    seq: Seq,
    event: Event,
    /// A message an agent refused as closed, once already: it goes to a
    /// new agent, and errors if that one refuses it too.
    retried: bool,
}

/// The seq just before `seq`.
fn before(seq: Seq) -> Seq {
    Seq(seq.0.saturating_sub(1))
}

/// One running agent session: at most one per Skywave session, for one of
/// its contexts ([`Running::context`]).
struct Running<S> {
    session: S,
    events: AdapterEvents,
    mapper: EventMapper,
    /// Seq of the `SessionStateChanged(AgentWorking)` that opened the current
    /// turn; `None` between turns.
    turn_started: Option<Seq>,
    /// The current turn was already handed an interrupt: a later
    /// `TurnInterrupted` escalates. Cleared when the turn ends.
    interrupted: bool,
    /// Messages delivered whose turn has not started yet (an open one is
    /// [`EventMapper::turn_open`]); a turn the agent started on its own
    /// takes none.
    owed_turns: usize,
    /// The live background tasks the agent last reported that are not
    /// ambient, counted before the mapper cuts the set.
    busy_tasks: usize,
    /// When the session last ran out of work; `None` while it has some.
    /// Written only by [`Running::settle_idle`].
    idle_since: Option<Instant>,
    /// The setting requests this agent was asked for, by command.
    asked: HashSet<Option<CommandId>>,
    /// The login successes the agent may have seen, at its start and at
    /// each message handed over ([`LoginState::apply`]).
    login_seen: LoginSeen,
}

impl<S> Running<S> {
    /// The session context the agent was started for. Everything it logs
    /// is stamped with it, also after the session moved to another context.
    fn context(&self) -> ContextId {
        self.mapper.context()
    }

    fn idle_deadline(&self, timeout: Duration) -> Option<Instant> {
        self.idle_since.map(|since| since + timeout)
    }

    /// A turn owed or open, or a busy background task. Schedules are not
    /// work; see the module docs.
    fn has_work(&self) -> bool {
        self.owed_turns > 0 || self.mapper.turn_open() || self.busy_tasks > 0
    }

    /// The user is stopping something in it: its turn was handed an
    /// interrupt that has not ended it yet, or a task stop is unanswered.
    /// Such an agent is read while the supervisor is paused, until that
    /// settles, so the control reaches it and its answer is logged.
    fn controlled(&self) -> bool {
        self.interrupted || self.mapper.stops_pending()
    }

    /// Stops the idle clock while the agent has work and starts it once it
    /// has none. A clock already running keeps its start, so a repeated
    /// report of nothing to do does not push the deadline back.
    fn settle_idle(&mut self) {
        if self.has_work() {
            self.idle_since = None;
        } else if self.idle_since.is_none() {
            self.idle_since = Some(Instant::now());
        }
    }
}

enum Wake<S> {
    Stop,
    Log,
    /// Boxed: an event is far larger than the other wakes.
    Agent(SessionId, Option<Box<AdapterEvent>>),
    Idle,
    /// The disk turned low, or has room again.
    Disk,
    Started(Box<Result<(SessionId, Started<S>), JoinError>>),
    Stopped(Box<Result<(SessionId, Halted, bool), JoinError>>),
}

/// The error for an agent start or stop task that panicked.
fn panicked(error: JoinError) -> Error {
    Error::Supervisor(format!("agent start or stop panicked: {error}"))
}

impl<A: AgentAdapter> Supervisor<A> {
    /// Starts the supervisor on its own thread over the data directory at
    /// `data_dir` (opened with [`Storage::open`]; the supervisor uses its own
    /// connection) and the process's shared `log`.
    ///
    /// Returns once the previous run's leftovers are settled (see the
    /// [module docs](self)); the supervisor then follows the log until
    /// [`SupervisorHandle::shutdown`].
    pub async fn start(
        adapter: A,
        data_dir: impl Into<PathBuf>,
        log: EventLog,
        config: SupervisorConfig,
    ) -> Result<SupervisorHandle> {
        let data_dir = data_dir.into();
        let (ready_tx, ready_rx) = oneshot::channel();
        let (stop_tx, stop_rx) = oneshot::channel();
        let (done_tx, done_rx) = oneshot::channel();
        let (activity_tx, activity_rx) = watch::channel(Activity {
            cursor: Seq::ZERO,
            turns: true,
        });
        std::thread::Builder::new()
            .name("skywave-supervisor".into())
            .spawn(move || {
                let result = run_thread(
                    adapter,
                    data_dir,
                    log,
                    config,
                    activity_tx,
                    ready_tx,
                    stop_rx,
                );
                if let Err(error) = &result {
                    tracing::error!(%error, "agent supervisor stopped");
                }
                let _ = done_tx.send(result);
            })
            .map_err(|e| Error::Supervisor(format!("cannot spawn thread: {e}")))?;
        match ready_rx.await {
            Ok(Ok(())) => Ok(SupervisorHandle {
                stop: Some(stop_tx),
                done: done_rx,
                activity: activity_rx,
                ended: None,
            }),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(Error::Supervisor("supervisor thread panicked".into())),
        }
    }

    /// Settles what the previous run left open; see the module docs.
    fn recover(&mut self) -> Result<()> {
        let db = &mut self.storage.db;
        let progress = load_progress(db)?;
        // Decided before anything below is logged: the log after the stored
        // rows tells which of them an agent took.
        let mut lost = lost_messages(db, &progress)?;
        // Logged once even if this run dies before clearing them: a next
        // run finds their `Errored` after the rows.
        let settling: HashMap<SessionId, Vec<Owed>> = (lost.iter())
            .map(|(&session, contexts)| {
                let rows = (contexts.iter())
                    .flat_map(|(context, seqs)| {
                        seqs.iter().map(|&seq| Owed {
                            seq,
                            context: *context,
                            state: OwedState::Settling,
                        })
                    })
                    .collect();
                (session, rows)
            })
            .collect();
        let owed_any = !progress.owed.is_empty();
        if owed_any {
            store_owed(db, &settling)?;
        }
        for approval in self.log.pending_approvals(db)? {
            self.log
                .cancel_approval(db, approval.id, DAEMON_RESTARTED)?;
        }
        for session in self.log.sessions(db)? {
            // Before the state event, as the mapper names them at a turn's
            // end; an idle session's background subagent can have some too.
            for (context, calls) in self.log.open_calls(db, session.id)? {
                tracing::info!(session = %session.id.0, ?calls, "naming calls lost with the daemon");
                self.log.append(
                    db,
                    session.id,
                    context,
                    EventKind::CallsUnanswered { calls },
                )?;
            }
            // Settled in the context of the turn it settles, which a switch
            // logged after it does not change.
            let Some((kind, context)) = event_log::last_state_event_in(db.conn(), session.id)?
            else {
                continue;
            };
            let settle = match kind {
                EventKind::SessionStateChanged(SessionState::AgentWorking) => {
                    // Messages queued behind the lost turn are named in
                    // its `Errored`, as an agent's exit names them.
                    let queued = lost
                        .get_mut(&session.id)
                        .and_then(|contexts| contexts.remove(&context))
                        .and_then(|seqs| unsent_notice(seqs.len()));
                    let message = match queued {
                        Some(notice) => format!("{TURN_LOST}; {notice}"),
                        None => TURN_LOST.to_owned(),
                    };
                    SessionState::Errored { message }
                }
                EventKind::TurnInterrupted => SessionState::Idle,
                _ => continue,
            };
            tracing::info!(session = %session.id.0, ?settle, "settling session after restart");
            self.log.append(
                db,
                session.id,
                context,
                EventKind::SessionStateChanged(settle),
            )?;
        }
        // After the turns above, whose `Errored` named the messages queued
        // behind them.
        let mut lost: Vec<(SessionId, ContextId, Vec<Seq>)> = (lost.into_iter())
            .flat_map(|(session, contexts)| {
                (contexts.into_iter()).map(move |(context, seqs)| (session, context, seqs))
            })
            .collect();
        lost.sort_by_key(|(_, _, seqs)| seqs.first().copied());
        for (session_id, context, seqs) in lost {
            tracing::info!(session = %session_id.0, ?seqs, "erroring messages lost with the daemon");
            self.log.append(
                db,
                session_id,
                context,
                EventKind::SessionStateChanged(SessionState::Errored {
                    message: messages_lost(seqs.len()),
                }),
            )?;
        }
        if owed_any {
            store_owed(db, &HashMap::new())?;
        }
        // Read but not handled yet: it waited for its session's agent.
        let queued = |session: SessionId, seq: Seq| {
            progress
                .held
                .get(&session)
                .is_some_and(|&handled| seq > handled)
        };
        for session in self.log.sessions(db)? {
            let work = self.log.agent_work(db, session.id)?;
            let mut ended = Vec::new();
            if !work.tasks.is_empty() {
                ended.push(EventKind::BackgroundTasksChanged { tasks: Vec::new() });
            }
            if !work.schedules.is_empty() {
                ended.push(EventKind::SchedulesChanged {
                    schedules: Vec::new(),
                });
            }
            if ended.is_empty() {
                continue;
            }
            // Stamped like the work it ends: the context of the agent that
            // reported it, not one switched to since.
            let Some(context) = event_log::last_work_context_in(db.conn(), session.id)? else {
                continue;
            };
            for kind in ended {
                tracing::info!(session = %session.id.0, ?kind, "ending work lost with the daemon");
                self.log.append(db, session.id, context, kind)?;
            }
        }
        self.cursor = progress.cursor;
        for session in self.log.sessions(db)? {
            let settings = self.log.agent_settings(db, session.id)?;
            let handled = |command| -> Result<bool> {
                Ok(self
                    .log
                    .command_seq(db, command)?
                    .is_some_and(|seq| seq <= self.cursor && !queued(session.id, seq)))
            };
            let mut lost = Vec::new();
            if let Some(request) = &settings.mode.requested
                && handled(request.command)?
            {
                lost.push(EventKind::PermissionModeRefused {
                    mode: request.value,
                    reason: DAEMON_RESTARTED.to_owned(),
                    answers: Some(request.command),
                });
            }
            if let Some(request) = &settings.model.requested
                && handled(request.command)?
            {
                lost.push(EventKind::ModelRefused {
                    model: request.value.clone(),
                    reason: DAEMON_RESTARTED.to_owned(),
                    answers: Some(request.command),
                });
            }
            if let Some(request) = settings.effort.requested_effort()
                && handled(request.command)?
            {
                lost.push(EventKind::EffortRefused {
                    effort: request.value,
                    reason: DAEMON_RESTARTED.to_owned(),
                    answers: Some(request.command),
                });
            }
            for kind in lost {
                tracing::info!(session = %session.id.0, ?kind, "refusing a request lost with the daemon");
                // Settings belong to the session, not to a context.
                self.log.append(db, session.id, session.context, kind)?;
            }
        }
        self.models = self.log.offered_models(db)?;
        self.held = self.held_events(&progress)?;
        self.stored = Progress {
            owed: HashMap::new(),
            ..progress
        };
        Ok(())
    }

    /// The events each session held when the previous run last stored its
    /// progress, read again from the log; [`Supervisor::run`] handles them
    /// first.
    fn held_events(&self, progress: &Progress) -> Result<HashMap<SessionId, Held>> {
        let mut held: HashMap<SessionId, Held> = (progress.held.iter())
            .map(|(&id, &handled)| {
                let queue = VecDeque::new();
                (
                    id,
                    Held {
                        handled,
                        op: None,
                        queue,
                    },
                )
            })
            .collect();
        let Some(&from) = progress.held.values().min() else {
            return Ok(held);
        };
        let mut after = from;
        while after < progress.cursor {
            let batch = self.log.read(&self.storage.db, after, BATCH)?;
            if batch.events.is_empty() {
                break;
            }
            let mut seq = batch.from;
            for event in batch.events {
                if seq > progress.cursor {
                    return Ok(held);
                }
                if let Some(session) = held.get_mut(&event.session_id)
                    && seq > session.handled
                    && reacts(&event.kind)
                {
                    let retried = false;
                    session.queue.push_back(Queued {
                        seq,
                        event,
                        retried,
                    });
                }
                after = seq;
                seq = seq.next();
            }
        }
        Ok(held)
    }

    async fn run(mut self, mut stop: oneshot::Receiver<()>) -> Result<()> {
        match self.follow_log(&mut stop).await {
            Ok(()) => self.shut_down().await,
            Err(error) => {
                self.stop_unrecorded().await;
                Err(error)
            }
        }
    }

    /// Follows the log and the agents until `stop`; see the module docs.
    async fn follow_log(&mut self, stop: &mut oneshot::Receiver<()>) -> Result<()> {
        let mut head = self.log.subscribe();
        let mut disk = self.log.disk().subscribe();
        // What the previous run left waiting, before anything newer.
        let replayed: Vec<SessionId> = self.held.keys().copied().collect();
        for id in replayed {
            self.drain(id).await?;
        }
        self.publish_activity();
        loop {
            head.borrow_and_update();
            if self.log.head() > self.cursor {
                self.follow().await?;
                continue;
            }
            let paused = *disk.borrow_and_update();
            // Paused, an agent's unread events may hold work: no idle stop.
            let deadline = if paused {
                None
            } else {
                self.next_idle_deadline()
            };
            let wake = tokio::select! {
                biased;
                _ = &mut *stop => Wake::Stop,
                Some(started) = self.starts.join_next() => Wake::Started(Box::new(started)),
                Some(stopped) = self.stops.join_next() => Wake::Stopped(Box::new(stopped)),
                (id, event) = next_event(&mut self.running, self.log.disk()) => {
                    Wake::Agent(id, event.map(Box::new))
                }
                _ = head.changed() => Wake::Log,
                _ = disk.changed() => Wake::Disk,
                () = sleep_until(deadline) => Wake::Idle,
            };
            match wake {
                Wake::Stop => return Ok(()),
                // The next round reads the agents as the disk now says.
                Wake::Log | Wake::Disk => {}
                Wake::Agent(id, event) => self.on_agent_event(id, event.map(|e| *e)).await?,
                Wake::Idle => self.stop_idle(),
                Wake::Started(started) => {
                    let (id, started) = started.map_err(panicked)?;
                    self.on_started(id, started).await?;
                }
                Wake::Stopped(stopped) => {
                    let (id, halted, map_exit) = stopped.map_err(panicked)?;
                    self.on_stopped(id, halted, map_exit).await?;
                }
            }
            self.store_progress()?;
        }
    }

    /// Stops every agent, those stopping already included, without
    /// interrupting them; see the module docs.
    async fn shut_down(mut self) -> Result<()> {
        let (stopped, halted) = self.halt_all().await;
        for joined in stopped {
            let (id, halted, map_exit) = joined.map_err(panicked)?;
            self.record_stop(id, halted, map_exit)?;
        }
        for (id, halted, map_exit) in halted {
            self.record_stop(id, halted, map_exit)?;
        }
        self.store_progress()
    }

    /// After a failure: stops every agent as a shutdown does, with the
    /// adapter's own stop (for a process, SIGTERM and its grace), and
    /// records nothing: the log is what failed. Dropping the agents instead
    /// would kill them outright, and a CLI killed without its SIGTERM
    /// leaves the commands it started running.
    async fn stop_unrecorded(mut self) {
        tracing::warn!(
            agents = self.running.len() + self.stops.len(),
            "stopping every agent after the failure"
        );
        let _ = self.halt_all().await;
    }

    /// Stops every agent at once, and waits for the stops under way: what
    /// each reported, the stops under way first. A start under way is
    /// dropped, which kills its agent: its trigger and what waited behind
    /// it stay unhandled, for the next run.
    #[allow(clippy::type_complexity)]
    async fn halt_all(
        &mut self,
    ) -> (
        Vec<std::result::Result<(SessionId, Halted, bool), JoinError>>,
        Vec<(SessionId, Halted, bool)>,
    ) {
        self.starts.shutdown().await;
        // All at once: one by one, each agent's grace would add up.
        let halts = std::mem::take(&mut self.running)
            .into_iter()
            .map(|(id, running)| async move { (id, halt(id, running).await, false) });
        let mut stops = std::mem::take(&mut self.stops);
        let under_way = async move {
            let mut stopped = Vec::new();
            while let Some(joined) = stops.join_next().await {
                stopped.push(joined);
            }
            stopped
        };
        let (halted, stopped) = tokio::join!(join_all(halts), under_way);
        (stopped, halted)
    }

    /// Handles one batch of log events after the cursor.
    async fn follow(&mut self) -> Result<()> {
        let batch = self.log.read(&self.storage.db, self.cursor, BATCH)?;
        let mut seq = batch.from;
        for event in batch.events {
            self.cursor = seq;
            self.on_log_event(seq, event).await?;
            seq = seq.next();
        }
        self.store_progress()
    }

    /// Handles the event at `seq`, the cursor, or holds it while its
    /// session's agent starts or stops.
    async fn on_log_event(&mut self, seq: Seq, event: Event) -> Result<()> {
        if !reacts(&event.kind) {
            return Ok(());
        }
        let item = Queued {
            seq,
            event,
            retried: false,
        };
        match self.held.get_mut(&item.event.session_id) {
            Some(held) => held.queue.push_back(item),
            None => self.handle(item).await?,
        }
        Ok(())
    }

    /// Handles `item`, an event for a session no start or stop holds.
    async fn handle(&mut self, item: Queued) -> Result<()> {
        let session_id = item.event.session_id;
        if starts_agent(&item.event.kind)
            && self
                .running
                .get(&session_id)
                .is_some_and(|running| running.context() != item.event.context)
        {
            // Cannot happen while command handling validates stamps and a
            // context change stops the agent (both before the supervisor
            // reaches the event); if it does, the event goes to its own
            // context's agent, once this one has stopped, not silently to
            // another's.
            tracing::warn!(session = %session_id.0, "agent runs another context; stopping it");
            self.stop_on_request(session_id).await?;
            self.held_mut(session_id)?.requeue(item);
            return Ok(());
        }
        let context = item.event.context;
        let command = item.event.command_id;
        match &item.event.kind {
            EventKind::UserMessage { .. } => self.deliver(item).await?,
            &EventKind::ApprovalResolved {
                id, decision, rule, ..
            } => {
                let Some(running) = self.running.get_mut(&session_id) else {
                    return Ok(());
                };
                // `None`: the approval belongs to an agent run that is gone,
                // and was cancelled when it went.
                let Some(request) = running.mapper.take_approval(id) else {
                    return Ok(());
                };
                if let Err(error) = running
                    .session
                    .answer_approval(request, decision, rule)
                    .await
                {
                    tracing::warn!(session = %session_id.0, %error, "cannot answer approval");
                }
            }
            EventKind::QuestionAnswered { id, answers, .. } => {
                let Some(running) = self.running.get_mut(&session_id) else {
                    return Ok(());
                };
                // `None` as for an approval: its agent run is gone.
                let Some(request) = running.mapper.take_approval(*id) else {
                    return Ok(());
                };
                if let Err(error) = running
                    .session
                    .answer_question(request, answers.clone())
                    .await
                {
                    tracing::warn!(session = %session_id.0, %error, "cannot answer question");
                }
            }
            EventKind::TurnInterrupted => {
                let Some(running) = self.running.get_mut(&session_id) else {
                    return Ok(());
                };
                // Aimed at an earlier turn, or before this one began.
                if running
                    .turn_started
                    .is_none_or(|started| started >= item.seq)
                {
                    return Ok(());
                }
                let sent = if running.interrupted {
                    tracing::info!(session = %session_id.0, "interrupted again; escalating");
                    running.session.escalate_interrupt().await
                } else {
                    running.interrupted = true;
                    running.session.interrupt().await
                };
                if let Err(error) = sent {
                    tracing::warn!(session = %session_id.0, %error, "cannot interrupt agent");
                }
            }
            EventKind::SessionStopped => {
                tracing::info!(session = %session_id.0, "stopping agent on request");
                self.stop_on_request(session_id).await?;
            }
            EventKind::WorktreeRemoved { .. } => {
                tracing::info!(
                    session = %session_id.0,
                    "stopping agents: their worktree was removed"
                );
                for id in self.sessions_in_workdir(session_id)? {
                    self.stop_on_request(id).await?;
                }
            }
            EventKind::ContextStarted { .. } | EventKind::ContextSwitched { .. } => {
                // The event is stamped with the context it makes current.
                // That context starts lazily, on its next message or
                // setting request; never here.
                if self
                    .running
                    .get(&session_id)
                    .is_some_and(|running| running.context() != context)
                {
                    tracing::info!(session = %session_id.0, "stopping agent: context changed");
                    self.stop_on_request(session_id).await?;
                }
            }
            EventKind::ContextMoved { .. } => {
                // Stamped with the moved context, which is never current
                // when it moves: the switch away from it already stopped
                // its agent. If one still runs it, stop it rather than let
                // it run a conversation the new session now continues.
                if self
                    .running
                    .get(&session_id)
                    .is_some_and(|running| running.context() == context)
                {
                    tracing::warn!(session = %session_id.0, "agent runs a moved context; stopping it");
                    self.stop_on_request(session_id).await?;
                }
            }
            &EventKind::PermissionModeRequested { mode, .. } => {
                self.set(item, Setting::Mode(mode)).await?;
            }
            EventKind::ModelRequested { model, .. } => {
                let setting = Setting::Model(model.clone());
                self.set(item, setting).await?;
            }
            &EventKind::EffortRequested { effort, .. } => {
                self.set(item, Setting::Effort(effort)).await?;
            }
            EventKind::TaskStopRequested { task, .. } => {
                let task = task.clone();
                self.stop_task(session_id, context, command, task).await?;
            }
            _ => {}
        }
        Ok(())
    }

    /// The held session `id`, which a start or stop just held.
    fn held_mut(&mut self, id: SessionId) -> Result<&mut Held> {
        self.held
            .get_mut(&id)
            .ok_or_else(|| Error::Supervisor(format!("session {} is not held", id.0)))
    }

    /// Holds session `id` for `op`. A session held first now has handled
    /// everything read so far; the caller lowers that for an event it
    /// puts back.
    fn hold(&mut self, id: SessionId, op: Op) -> &mut Held {
        let cursor = self.cursor;
        let held = self.held.entry(id).or_insert_with(|| Held {
            handled: cursor,
            op: None,
            queue: VecDeque::new(),
        });
        held.op = Some(op);
        held
    }

    /// Handles what waited for session `first`'s start or stop, in order,
    /// until none is left or another start or stop holds the session
    /// again; then what waited for that session to settle.
    async fn drain(&mut self, first: SessionId) -> Result<()> {
        let mut todo = vec![first];
        while let Some(id) = todo.pop() {
            while let Some(held) = self.held.get_mut(&id) {
                if held.op.is_some() {
                    break;
                }
                let Some(item) = held.queue.pop_front() else {
                    self.held.remove(&id);
                    for (&waiter, held) in &mut self.held {
                        if matches!(held.op, Some(Op::After(origin)) if origin == id) {
                            held.op = None;
                            todo.push(waiter);
                        }
                    }
                    break;
                };
                let seq = item.seq;
                self.handle(item).await?;
                if !self.holds(id, seq)
                    && let Some(held) = self.held.get_mut(&id)
                {
                    held.handled = held.handled.max(seq);
                }
            }
        }
        self.store_progress()
    }

    /// Whether session `id` still holds its event at `seq`, unhandled.
    fn holds(&self, id: SessionId, seq: Seq) -> bool {
        self.held.get(&id).is_some_and(|held| {
            held.queue.front().is_some_and(|item| item.seq == seq)
                || matches!(&held.op, Some(Op::Starting(start)) if start.trigger.seq == seq)
        })
    }

    /// Records that session `id` handled its event at `seq`, if it is held.
    fn mark_handled(&mut self, id: SessionId, seq: Seq) {
        if let Some(held) = self.held.get_mut(&id) {
            held.handled = held.handled.max(seq);
        }
    }

    /// Asks the session's agent to stop background `task` on behalf of
    /// `command`, made in `context`. Never starts an agent: one that is not
    /// running has no task to stop, so the stop is refused in `context`.
    async fn stop_task(
        &mut self,
        session_id: SessionId,
        context: ContextId,
        command: Option<CommandId>,
        task: TaskId,
    ) -> Result<()> {
        let Some(running) = self.running.get_mut(&session_id) else {
            let kind = EventKind::TaskStopRefused {
                task,
                reason: AGENT_NOT_RUNNING.to_owned(),
                answers: command,
            };
            self.log
                .append(&mut self.storage.db, session_id, context, kind)?;
            return Ok(());
        };
        running.mapper.asked_stop(task.clone(), command);
        if let Err(error) = running.session.stop_task(task.0).await {
            // The agent is gone; the empty set its `Exited` logs settles it.
            tracing::warn!(session = %session_id.0, %error, "cannot ask the agent to stop a task");
        }
        Ok(())
    }

    /// The sessions whose workdir is `session_id`'s, itself included. A
    /// removed worktree's directory may be shared with sessions that were not
    /// created in it (a plain `CreateSession`, or a context moved there).
    fn sessions_in_workdir(&self, session_id: SessionId) -> Result<Vec<SessionId>> {
        let db = &self.storage.db;
        let Some(workdir) = self.log.session(db, session_id)?.map(|s| s.workdir) else {
            return Ok(vec![session_id]);
        };
        Ok(self
            .log
            .sessions(db)?
            .into_iter()
            .filter(|s| s.workdir == workdir)
            .map(|s| s.id)
            .collect())
    }

    /// Stops the session's agent, if one runs, ending its turn first.
    async fn stop_on_request(&mut self, session_id: SessionId) -> Result<()> {
        let Some(running) = self.running.get_mut(&session_id) else {
            return Ok(());
        };
        // Shutting down mid-turn would abandon the turn and log it as
        // lost; end it as interrupted instead. A delivered message counts
        // too: its turn may have begun before we heard.
        if (running.turn_started.is_some() || running.owed_turns > 0)
            && let Err(error) = running.session.interrupt().await
        {
            tracing::warn!(session = %session_id.0, %error, "cannot interrupt agent");
        }
        self.begin_stop(session_id, true);
        Ok(())
    }

    /// Asks the session's agent for `setting`, the request `item`. An agent
    /// that is not running is started for the request's context, and the
    /// start asks for what the log wants, this request included.
    async fn set(&mut self, item: Queued, setting: Setting) -> Result<()> {
        let session_id = item.event.session_id;
        let context = item.event.context;
        let command = item.event.command_id;
        if matches!(setting, Setting::Mode(PermissionMode::Bypass)) && !self.log.bypass_allowed() {
            // Refused from the config, not left to the agent: only the open
            // request is answered (a replaced one needs no answer), and no
            // agent is started for it.
            let open = self.log.agent_settings(&self.storage.db, session_id)?;
            if command.is_some() && open.mode.requested.as_ref().map(|r| r.command) == command {
                self.refuse_bypass(session_id, context, command)?;
            }
            return Ok(());
        }
        if !self.running.contains_key(&session_id) {
            return self.begin_start(item);
        }
        // Only the open request is carried out, once: a newer one replaced
        // it (and goes out after it), or the start already asked for it.
        let settings = self.log.agent_settings(&self.storage.db, session_id)?;
        let open = match &setting {
            Setting::Mode(_) => settings.mode.requested.as_ref().map(|r| r.command),
            Setting::Model(_) => settings.model.requested.as_ref().map(|r| r.command),
            Setting::Effort(_) => settings.effort.requested_effort().map(|r| r.command),
        };
        let Some(running) = self.running.get_mut(&session_id) else {
            return Ok(());
        };
        if command.is_none() || open != command || running.asked.contains(&command) {
            tracing::debug!(session = %session_id.0, "skipping a setting request already asked or replaced");
            return Ok(());
        }
        running.asked.insert(command);
        let sent = match setting {
            Setting::Mode(mode) => {
                running.mapper.asked_mode(mode, command);
                running.session.set_permission_mode(mode).await
            }
            Setting::Model(model) => {
                running.mapper.asked_model(model.clone(), command);
                running.session.set_model(model).await
            }
            Setting::Effort(effort) => {
                running.mapper.asked_effort(effort, command);
                running.session.set_effort(effort).await
            }
        };
        if let Err(error) = sent {
            // The agent is gone; its `Exited` refuses what it was asked.
            tracing::warn!(session = %session_id.0, %error, "cannot ask the agent for a setting");
        }
        Ok(())
    }

    /// Logs the server's own refusal of a bypass request, `command`, while
    /// the log does not allow bypass. Settings belong to the session; the
    /// refusal is stamped with `context` like any other answer.
    fn refuse_bypass(
        &mut self,
        session_id: SessionId,
        context: ContextId,
        command: Option<CommandId>,
    ) -> Result<()> {
        tracing::info!(session = %session_id.0, "refusing bypass: not allowed on this server");
        let kind = EventKind::PermissionModeRefused {
            mode: PermissionMode::Bypass,
            reason: BYPASS_NOT_ALLOWED.to_owned(),
            answers: command,
        };
        self.log
            .append(&mut self.storage.db, session_id, context, kind)?;
        Ok(())
    }

    /// Gives the message `item` to the session's agent, starting one for
    /// the message's context first if none runs.
    async fn deliver(&mut self, item: Queued) -> Result<()> {
        if self.running.contains_key(&item.event.session_id) {
            self.hand_over(item).await
        } else {
            self.begin_start(item)
        }
    }

    /// Hands the message `item` to the session's running agent. One the
    /// agent refuses as closed waits for that agent to stop and goes to a
    /// new one, once.
    async fn hand_over(&mut self, item: Queued) -> Result<()> {
        let session_id = item.event.session_id;
        let EventKind::UserMessage {
            text, attachments, ..
        } = &item.event.kind
        else {
            return Err(Error::Supervisor("only a message is handed over".into()));
        };
        // Opened before the at-most-once mark: a blob already gone is noted
        // in the text, and the open files pin the bytes of the rest.
        let input = user_input(&self.storage, text, attachments);
        // At most once: stored as handed over before the agent gets it.
        self.owed.entry(session_id).or_default().push(Owed {
            seq: item.seq,
            context: item.event.context,
            state: OwedState::Waiting,
        });
        self.mark_handled(session_id, item.seq);
        self.store_progress()?;
        let Some(running) = self.running.get_mut(&session_id) else {
            return Err(Error::Supervisor("no agent to hand a message to".into()));
        };
        // Read before the agent gets it: a login that succeeds from here
        // on may come after the agent read its own for this message.
        let logins = self.config.login.successes();
        let error = match running.session.send_message(input).await {
            Ok(()) => {
                running.login_seen.handed(logins);
                running.owed_turns += 1;
                running.settle_idle();
                return Ok(());
            }
            Err(error) => error,
        };
        if !item.retried {
            // The agent did not take it: it is not handed over.
            tracing::warn!(session = %session_id.0, %error, "cannot send message; restarting agent");
            self.forget_owed(session_id, |owed| owed.seq == item.seq);
            self.begin_stop(session_id, true);
            let retried = true;
            self.held_mut(session_id)?
                .requeue(Queued { retried, ..item });
            return Ok(());
        }
        let message = format!("cannot deliver message: {error}");
        self.settling(session_id, |owed| owed.seq == item.seq)?;
        self.message_failed(session_id, item.event.context, message)?;
        self.forget_owed(session_id, |owed| owed.seq == item.seq);
        Ok(())
    }

    /// Marks session `id`'s owed messages that `which` picks as settling
    /// and stores that, before the `Errored` reporting them is logged; how
    /// many there are.
    fn settling(&mut self, id: SessionId, which: impl Fn(&Owed) -> bool) -> Result<usize> {
        let Some(owed) = self.owed.get_mut(&id) else {
            return Ok(0);
        };
        let mut count = 0;
        for owed in owed.iter_mut().filter(|owed| which(owed)) {
            owed.state = OwedState::Settling;
            count += 1;
        }
        self.store_progress()?;
        Ok(count)
    }

    /// Drops session `id`'s owed messages that `which` picks: settled, or
    /// not handed over after all.
    fn forget_owed(&mut self, id: SessionId, which: impl Fn(&Owed) -> bool) {
        if let Some(owed) = self.owed.get_mut(&id) {
            owed.retain(|owed| !which(owed));
            if owed.is_empty() {
                self.owed.remove(&id);
            }
        }
    }

    /// Readies session `id`'s agent's `event` for its mapper, whose turn is
    /// open as `turn_open` says. A message's turn about to open settles the
    /// oldest message the agent owes (an agent begins the turns of the
    /// messages it was handed in order): it is marked starting, and that
    /// stored, before its `AgentWorking` is logged. A message the agent
    /// took up inside the open turn (one it started on its own) gets no
    /// `AgentWorking`: it is settled, and that stored, now. A turn the
    /// agent started on its own settles none. An exit reports every message the
    /// agent still owes: they are marked settling, and their count goes
    /// into its `unsent` when the agent counted fewer (an event stream that
    /// ended without `Exited` counts none), so the exit's one `Errored`
    /// names them all.
    fn before_mapping(
        &mut self,
        id: SessionId,
        event: AdapterEvent,
        turn_open: bool,
    ) -> Result<AdapterEvent> {
        match event {
            AdapterEvent::TurnStarted { message: true } => {
                if let Some(owed) = self.owed.get_mut(&id)
                    && let Some(first) =
                        (owed.iter_mut()).find(|owed| owed.state == OwedState::Waiting)
                {
                    if turn_open {
                        let seq = first.seq;
                        self.forget_owed(id, |owed| owed.seq == seq);
                    } else {
                        first.state = OwedState::Starting;
                    }
                    self.store_progress()?;
                }
                Ok(event)
            }
            AdapterEvent::Exited { error, unsent } => {
                let owed = self.settling(id, |_| true)?;
                Ok(AdapterEvent::Exited {
                    error,
                    unsent: unsent.max(owed),
                })
            }
            event => Ok(event),
        }
    }

    /// Keeps [`Supervisor::owed`] in step with what the mapper just logged
    /// for session `id`'s agent, `seqs`: an exit settled every message
    /// owed, a message turn's `AgentWorking` the one marked starting.
    fn after_mapping(&mut self, id: SessionId, exited: bool, message_turn: bool, seqs: &[Seq]) {
        if exited {
            self.forget_owed(id, |_| true);
        } else if message_turn && !seqs.is_empty() {
            self.forget_owed(id, |owed| owed.state == OwedState::Starting);
        }
    }

    /// Logs that the message sent in `context` was not delivered.
    fn message_failed(
        &mut self,
        session_id: SessionId,
        context: ContextId,
        message: String,
    ) -> Result<()> {
        tracing::warn!(session = %session_id.0, %message, "message not delivered");
        self.log.append(
            &mut self.storage.db,
            session_id,
            context,
            EventKind::SessionStateChanged(SessionState::Errored { message }),
        )?;
        Ok(())
    }

    /// Logs that the agent did not start for `trigger`, a message or
    /// setting request, answering it in its context.
    fn start_failed(&mut self, trigger: &Queued, message: String) -> Result<()> {
        let session_id = trigger.event.session_id;
        let context = trigger.event.context;
        let answers = trigger.event.command_id;
        let Some(setting) = Setting::of(&trigger.event.kind) else {
            return self.message_failed(session_id, context, message);
        };
        tracing::warn!(session = %session_id.0, %message, "agent did not start for a setting");
        let reason = format!("agent did not start: {message}");
        let kind = match setting {
            Setting::Mode(mode) => EventKind::PermissionModeRefused {
                mode,
                reason,
                answers,
            },
            Setting::Model(model) => EventKind::ModelRefused {
                model,
                reason,
                answers,
            },
            Setting::Effort(effort) => EventKind::EffortRefused {
                effort,
                reason,
                answers,
            },
        };
        self.log
            .append(&mut self.storage.db, session_id, context, kind)?;
        Ok(())
    }

    /// Starts the session's agent for `trigger`, a message or setting
    /// request, in the trigger's context and that context's
    /// [`Conversation`] (see `conversation`). The start runs in the
    /// background and holds the session until it completes
    /// ([`Supervisor::on_started`]); one that cannot begin is logged as a
    /// failed start of `trigger` now.
    ///
    /// The context is the stamp of the trigger, not the session's current
    /// context: a switch logged after it, and not handled yet, must not hand
    /// it to the new context.
    fn begin_start(&mut self, trigger: Queued) -> Result<()> {
        let session_id = trigger.event.session_id;
        if let Some(origin) = self.forks_from_held(session_id, trigger.event.context)? {
            // It would fork a conversation an agent may still write.
            tracing::info!(session = %session_id.0, origin = %origin.0, "start waits for the session it forks from");
            self.hold(session_id, Op::After(origin)).requeue(trigger);
            return Ok(());
        }
        match self.prepare_start(session_id, trigger.event.context)? {
            Err(message) => self.start_failed(&trigger, message),
            Ok(Prepared {
                spec,
                mapper,
                asked,
            }) => {
                // Read before the process starts: a login that succeeds
                // from here on may come after the process read its own.
                let logins = self.config.login.successes();
                let adapter = Arc::clone(&self.adapter);
                self.starts
                    .spawn(async move { (session_id, adapter.start(spec).await) });
                let seq = trigger.seq;
                let start = Starting {
                    trigger,
                    mapper,
                    asked,
                    logins,
                };
                let held = self.hold(session_id, Op::Starting(Box::new(start)));
                held.handled = held.handled.min(before(seq));
                Ok(())
            }
        }
    }

    /// The session another context of which `context` forks, if that
    /// session is held: the fork must read a finished conversation, so it
    /// waits until nothing is under way or waiting in that session (the
    /// clear or switch before the move stops the source's agent).
    fn forks_from_held(
        &self,
        session_id: SessionId,
        context: ContextId,
    ) -> Result<Option<SessionId>> {
        if load_resume_handle(&self.storage.db, context)?.is_some() {
            return Ok(None);
        }
        Ok(match load_origin(&self.storage.db, context)? {
            Some(SessionOrigin::Context(origin))
                if origin.session != session_id && self.held.contains_key(&origin.session) =>
            {
                Some(origin.session)
            }
            _ => None,
        })
    }

    /// What starting the session's agent for `context` takes: the adapter's
    /// spec, and the new agent's mapper and the setting requests the start
    /// asks for. `Err` carries why the agent cannot start, for the log.
    fn prepare_start(
        &mut self,
        session_id: SessionId,
        context: ContextId,
    ) -> Result<Result<Prepared, String>> {
        let Some(info) = self.log.session(&self.storage.db, session_id)? else {
            return Ok(Err(format!("unknown session {}", session_id.0)));
        };
        let conversation = match conversation(&self.storage.db, context)? {
            Ok(conversation) => conversation,
            Err(message) => {
                tracing::warn!(session = %session_id.0, %message, "agent not started");
                return Ok(Err(message));
            }
        };
        let how = match &conversation {
            Conversation::Fresh => "fresh",
            Conversation::Resume(_) => "resumed",
            Conversation::Fork(_) => "forked",
            Conversation::Adopt(_) => "adopted",
        };
        tracing::info!(session = %session_id.0, context = %context.0, conversation = how, "starting agent");
        let grants = load_grants(&self.storage.db, session_id)?;
        let mut settings = self.log.agent_settings(&self.storage.db, session_id)?;
        // Bypass is never asked for while the server does not allow it: an
        // open request for it (not handled yet) is refused now, and a bypass
        // the agent last ran in is not asked again, so the agent's own mode
        // replaces it in the log.
        let bypass = self.log.bypass_allowed();
        if !bypass
            && let Some(request) = &settings.mode.requested
            && request.value == PermissionMode::Bypass
        {
            self.refuse_bypass(session_id, context, Some(request.command))?;
            settings = self.log.agent_settings(&self.storage.db, session_id)?;
        }
        let mode = (settings.mode.wanted().copied())
            .filter(|&mode| bypass || mode != PermissionMode::Bypass);
        let work = self.log.agent_work(&self.storage.db, session_id)?;
        let model = settings.model.wanted().cloned();
        // An effort the running model takes none of, like an unknown one,
        // asks for nothing.
        let effort = settings.effort.wanted().copied().flatten();
        let spec = StartSpec {
            session_id,
            context,
            workdir: info.workdir.into(),
            conversation,
            grants,
            mode,
            model: model.clone(),
            effort,
        };
        let mut mapper = EventMapper::new(session_id, context, settings.clone(), work);
        let mut asked = HashSet::new();
        if let Some(mode) = mode {
            let command = settings.mode.requested.as_ref().map(|r| r.command);
            mapper.asked_mode(mode, command);
            asked.insert(command);
        }
        if let Some(model) = model {
            let command = settings.model.requested.as_ref().map(|r| r.command);
            mapper.asked_model(model, command);
            asked.insert(command);
        }
        if let Some(effort) = effort {
            let command = settings.effort.requested_effort().map(|r| r.command);
            mapper.asked_effort(effort, command);
            asked.insert(command);
        }
        Ok(Ok(Prepared {
            spec,
            mapper,
            asked,
        }))
    }

    /// Session `id`'s agent start completed: the agent runs and gets the
    /// message it was started for, or the start's trigger is answered with
    /// the failure. Then what waited for the start is handled.
    async fn on_started(&mut self, id: SessionId, started: Started<A::Session>) -> Result<()> {
        let op = self.held_mut(id)?.op.take();
        let Some(Op::Starting(start)) = op else {
            return Err(Error::Supervisor(format!(
                "session {} completed a start it was not making",
                id.0
            )));
        };
        let Starting {
            trigger,
            mapper,
            asked,
            logins,
        } = *start;
        match started {
            Ok((session, events)) => {
                tracing::info!(session = %id.0, "agent started");
                let mut running = Running {
                    session,
                    events,
                    mapper,
                    turn_started: None,
                    interrupted: false,
                    owed_turns: 0,
                    busy_tasks: 0,
                    idle_since: None,
                    asked,
                    login_seen: LoginSeen::started(logins),
                };
                // Nothing to do until a message comes, if one does.
                running.settle_idle();
                self.running.insert(id, running);
                if matches!(trigger.event.kind, EventKind::UserMessage { .. }) {
                    self.hand_over(trigger).await?;
                } else {
                    // The start asked for the setting.
                    self.mark_handled(id, trigger.seq);
                }
            }
            Err(error) => {
                tracing::warn!(session = %id.0, %error, "agent did not start");
                self.start_failed(&trigger, error.to_string())?;
                self.mark_handled(id, trigger.seq);
            }
        }
        self.drain(id).await
    }

    /// Stops session `id`'s agent in the background, holding the session
    /// until it has stopped ([`Supervisor::on_stopped`]). With `map_exit`
    /// false its final `Exited` is not recorded.
    fn begin_stop(&mut self, id: SessionId, map_exit: bool) {
        let Some(running) = self.running.remove(&id) else {
            return;
        };
        self.stops
            .spawn(async move { (id, halt(id, running).await, map_exit) });
        self.hold(id, Op::Stopping);
    }

    /// Session `id`'s agent stopped: records what it reported meanwhile,
    /// then handles what waited for the stop.
    async fn on_stopped(&mut self, id: SessionId, halted: Halted, map_exit: bool) -> Result<()> {
        self.record_stop(id, halted, map_exit)?;
        self.held_mut(id)?.op = None;
        self.drain(id).await
    }

    async fn on_agent_event(&mut self, id: SessionId, event: Option<AdapterEvent>) -> Result<()> {
        // A stream that ends without `Exited` is an agent that went away.
        // It counts nothing unsent; the messages it owes are named anyway.
        let event = event.unwrap_or_else(|| AdapterEvent::Exited {
            error: Some("event stream ended".into()),
            unsent: 0,
        });
        let exited = matches!(event, AdapterEvent::Exited { .. });
        // About to log: a reading up to the check interval old is reused,
        // so a burst costs one. A flip to low pauses the reads from here.
        self.log.disk().refresh();
        if let Some(request) = self.map(id, event)?
            && let Some(running) = self.running.get_mut(&id)
            && let Err(error) = running
                .session
                .answer_approval(request, Decision::Approve, None)
                .await
        {
            tracing::warn!(session = %id.0, %error, "cannot answer approval");
        }
        if exited && let Some(running) = self.running.remove(&id) {
            tracing::info!(session = %id.0, "agent exited");
            // Reap it; it has already stopped.
            let _ = running.session.shutdown().await;
        }
        Ok(())
    }

    /// Records `event` from session `id`'s agent in the log. Returns the
    /// request the safety check approved, which the caller answers once the
    /// log has the approval.
    fn map(&mut self, id: SessionId, event: AdapterEvent) -> Result<Option<AgentRequestId>> {
        if !self.running.contains_key(&id) {
            return Ok(None);
        }
        let turn_open = (self.running.get(&id)).is_some_and(|r| r.mapper.turn_open());
        let event = self.before_mapping(id, event, turn_open)?;
        let safe_reason = self.safe_shell(id, &event)?;
        let Some(running) = self.running.get_mut(&id) else {
            return Ok(None);
        };
        let started = matches!(event, AdapterEvent::TurnStarted { .. });
        let message_turn = matches!(event, AdapterEvent::TurnStarted { message: true });
        let exited = matches!(event, AdapterEvent::Exited { .. });
        let ended = exited || matches!(event, AdapterEvent::TurnEnded(_));
        running.login_seen.observe(&event);
        if let AdapterEvent::LoggedIn { logged_in, source } = event {
            hear_login(
                &self.config.login,
                id,
                logged_in,
                source,
                &running.login_seen,
            );
            return Ok(None);
        }
        if let AdapterEvent::ModelsOffered(models) = event {
            let context = running.context();
            return offer_models(
                &self.log,
                &mut self.storage,
                &mut self.models,
                id,
                context,
                models,
            )
            .map(|()| None);
        }
        if let AdapterEvent::BackgroundTasks(tasks) = &event {
            // Counted uncut: a task the log has no room for still runs.
            running.busy_tasks = tasks.iter().filter(|t| !t.ambient).count();
        }
        let (seqs, answer) = match (safe_reason, event) {
            (
                Some(reason),
                AdapterEvent::ApprovalRequested {
                    request,
                    call,
                    tool,
                    input,
                    ..
                },
            ) => {
                tracing::debug!(session = %id.0, %reason, "safety check approves a shell command");
                let seq = running.mapper.safety_approved(
                    &self.log,
                    &mut self.storage,
                    call,
                    tool,
                    &input,
                    reason,
                )?;
                (vec![seq], Some(request))
            }
            (_, event) => (
                running.mapper.apply(&self.log, &mut self.storage, event)?,
                None,
            ),
        };
        if started && let Some(&seq) = seqs.first() {
            running.turn_started = Some(seq);
            running.interrupted = false;
        }
        if message_turn {
            // Open now: counted by `turn_open` until it ends.
            running.owed_turns = running.owed_turns.saturating_sub(1);
        }
        if ended {
            running.turn_started = None;
            running.interrupted = false;
        }
        // A turn the agent started on its own is work too, as is a task.
        running.settle_idle();
        self.after_mapping(id, exited, message_turn, &seqs);
        Ok(answer)
    }

    /// The reason the safety check approves `event`, an approval of session
    /// `id`'s agent: `Some` only for a shell command, with the check on for
    /// the session, that it proves read-only in the session's workdir. It
    /// never denies; anything else stays with a person.
    fn safe_shell(&self, id: SessionId, event: &AdapterEvent) -> Result<Option<String>> {
        let AdapterEvent::ApprovalRequested {
            shell: Some(command),
            ..
        } = event
        else {
            return Ok(None);
        };
        if !self.log.safety_check_on(&self.storage.db, id)? {
            return Ok(None);
        }
        let Some(session) = self.log.session(&self.storage.db, id)? else {
            return Ok(None);
        };
        match skywave_safety::check(command, Path::new(&session.workdir)) {
            Verdict::Safe { reason } => Ok(Some(reason)),
            Verdict::Ask { why } => {
                tracing::debug!(session = %id.0, %why, "safety check leaves a command to a person");
                Ok(None)
            }
        }
    }

    /// Records what session `id`'s agent reported while it stopped; with
    /// `map_exit` false (daemon shutdown), all but its final `Exited`,
    /// leaving open turns, approvals and the messages it owes for the next
    /// start.
    fn record_stop(&mut self, id: SessionId, halted: Halted, map_exit: bool) -> Result<()> {
        let Halted {
            mut mapper,
            mut events,
            mut login_seen,
        } = halted;
        if !map_exit {
            events.pop();
        }
        // Logged whatever it says: a stop is the user's, and its events are
        // in hand. The reading only lets a low disk pause the others.
        self.log.disk().refresh();
        for event in events {
            login_seen.observe(&event);
            if let AdapterEvent::LoggedIn { logged_in, source } = event {
                hear_login(&self.config.login, id, logged_in, source, &login_seen);
            } else if let AdapterEvent::ModelsOffered(models) = event {
                let context = mapper.context();
                offer_models(
                    &self.log,
                    &mut self.storage,
                    &mut self.models,
                    id,
                    context,
                    models,
                )?;
            } else {
                let event = self.before_mapping(id, event, mapper.turn_open())?;
                let message_turn = matches!(event, AdapterEvent::TurnStarted { message: true });
                let exited = matches!(event, AdapterEvent::Exited { .. });
                let seqs = mapper.apply(&self.log, &mut self.storage, event)?;
                self.after_mapping(id, exited, message_turn, &seqs);
            }
        }
        tracing::info!(session = %id.0, "agent stopped");
        Ok(())
    }

    fn stop_idle(&mut self) {
        let now = Instant::now();
        let timeout = self.config.idle_timeout;
        let idle: Vec<SessionId> = self
            .running
            .iter()
            .filter(|(_, r)| r.idle_deadline(timeout).is_some_and(|d| d <= now))
            .map(|(id, _)| *id)
            .collect();
        for id in idle {
            self.begin_stop(id, true);
        }
    }

    fn next_idle_deadline(&self) -> Option<Instant> {
        self.running
            .values()
            .filter_map(|r| r.idle_deadline(self.config.idle_timeout))
            .min()
    }

    /// Persists the cursor, what each held session has handled and the
    /// messages owed, together, if any changed.
    fn store_progress(&mut self) -> Result<()> {
        let progress = Progress {
            cursor: self.cursor,
            held: (self.held.iter())
                .map(|(&id, held)| (id, held.handled))
                .collect(),
            owed: self.owed.clone(),
        };
        if progress != self.stored {
            store_progress(&mut self.storage.db, &progress)?;
            self.stored = progress;
        }
        self.publish_activity();
        Ok(())
    }

    /// Publishes the [`Activity`] the supervisor is at, if it changed.
    fn publish_activity(&self) {
        let activity = Activity {
            cursor: self.cursor,
            turns: !self.owed.is_empty()
                || !self.held.is_empty()
                || (self.running.values())
                    .any(|running| running.owed_turns > 0 || running.mapper.turn_open()),
        };
        self.activity.send_if_modified(|current| {
            let changed = *current != activity;
            *current = activity;
            changed
        });
    }
}

/// Whether the supervisor acts on an event of this kind.
fn reacts(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::UserMessage { .. }
            | EventKind::ApprovalResolved { .. }
            | EventKind::QuestionAnswered { .. }
            | EventKind::TurnInterrupted
            | EventKind::SessionStopped
            | EventKind::ContextStarted { .. }
            | EventKind::ContextSwitched { .. }
            | EventKind::ContextMoved { .. }
            | EventKind::PermissionModeRequested { .. }
            | EventKind::ModelRequested { .. }
            | EventKind::EffortRequested { .. }
            | EventKind::TaskStopRequested { .. }
    )
}

/// Whether an event of this kind starts the session's agent, if none runs.
fn starts_agent(kind: &EventKind) -> bool {
    matches!(kind, EventKind::UserMessage { .. }) || Setting::of(kind).is_some()
}

/// The input for a message's `text` and `attachments`. Each image is opened,
/// never read: reading blocks, and the supervisor runs every session on one
/// runtime, while an open file keeps its bytes if the blob is deleted after.
/// An image that cannot be opened is noted in the text
/// ([`UserInput::note_missing`]) and the rest is still delivered.
fn user_input(storage: &Storage, text: &str, attachments: &[Attachment]) -> UserInput {
    let mut input = UserInput::from(text);
    for attachment in attachments {
        match storage.open_blob(attachment.blob.id) {
            Ok(Some(file)) => input.images.push(ImageInput {
                media_type: attachment.media_type,
                file: file.into_file(),
            }),
            Ok(None) => input.note_missing(),
            Err(error) => {
                tracing::warn!(
                    blob = %attachment.blob.id.0,
                    %error,
                    "cannot open image; leaving it out"
                );
                input.note_missing();
            }
        }
    }
    input
}

/// What starting an agent takes: the adapter's spec, and the new agent's
/// mapper and the setting requests the start asks for, by command.
struct Prepared {
    spec: StartSpec,
    mapper: EventMapper,
    asked: HashSet<Option<CommandId>>,
}

fn run_thread<A: AgentAdapter>(
    adapter: A,
    data_dir: PathBuf,
    log: EventLog,
    config: SupervisorConfig,
    activity: watch::Sender<Activity>,
    ready: oneshot::Sender<Result<()>>,
    stop: oneshot::Receiver<()>,
) -> Result<()> {
    let setup = || -> Result<_> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| Error::Supervisor(format!("cannot build runtime: {e}")))?;
        let mut supervisor = Supervisor {
            adapter: Arc::new(adapter),
            log,
            storage: Storage::open(data_dir)?,
            config,
            cursor: Seq::ZERO,
            owed: HashMap::new(),
            stored: Progress {
                cursor: Seq::ZERO,
                held: HashMap::new(),
                owed: HashMap::new(),
            },
            running: HashMap::new(),
            held: HashMap::new(),
            starts: JoinSet::new(),
            stops: JoinSet::new(),
            models: None,
            activity,
        };
        supervisor.recover()?;
        Ok((runtime, supervisor))
    };
    let (runtime, supervisor) = match setup() {
        Ok(ready_parts) => {
            let _ = ready.send(Ok(()));
            ready_parts
        }
        Err(error) => {
            let message = error.to_string();
            let _ = ready.send(Err(error));
            return Err(Error::Supervisor(message));
        }
    };
    runtime.block_on(supervisor.run(stop))
}

/// The conversation session context `context` starts in: its own, resumed
/// with its handle; else what its origin names, for the first context of a
/// session a move or an adoption created: a fork of the moved context's
/// conversation, or the outside conversation adopted by the agent's id;
/// else a fresh one. Its own handle comes first: a context that forked or
/// adopted keeps its copy from then on, where forking again would drop
/// every turn it took since. A moved context without a handle is refused
/// (`Err`, for the log) rather than started fresh, which would silently
/// drop the conversation the context continues; command validation moves
/// only a context with a handle, so this is a guard.
fn conversation(db: &Db, context: ContextId) -> Result<Result<Conversation, String>> {
    if let Some(handle) = load_resume_handle(db, context)? {
        return Ok(Ok(Conversation::Resume(handle)));
    }
    Ok(match load_origin(db, context)? {
        None => Ok(Conversation::Fresh),
        Some(SessionOrigin::Agent { conversation }) => Ok(Conversation::Adopt(conversation)),
        Some(SessionOrigin::Context(origin)) => match load_resume_handle(db, origin.context)? {
            Some(handle) => Ok(Conversation::Fork(handle)),
            None => Err(format!(
                "context {} has no conversation to continue",
                origin.context.0
            )),
        },
    })
}

/// A setting a device asked a session's agent for.
enum Setting {
    Mode(PermissionMode),
    Model(String),
    Effort(Effort),
}

impl Setting {
    /// The setting an event of this kind requests, if it is a request.
    fn of(kind: &EventKind) -> Option<Setting> {
        match kind {
            &EventKind::PermissionModeRequested { mode, .. } => Some(Setting::Mode(mode)),
            EventKind::ModelRequested { model, .. } => Some(Setting::Model(model.clone())),
            &EventKind::EffortRequested { effort, .. } => Some(Setting::Effort(effort)),
            _ => None,
        }
    }
}

/// Logs `models`, offered by session `id`'s agent running in `context`, as
/// `ModelsOffered` when they differ from `known`, the newest list in the
/// log, and makes them so.
fn offer_models(
    log: &EventLog,
    storage: &mut Storage,
    known: &mut Option<Vec<ModelChoice>>,
    id: SessionId,
    context: ContextId,
    models: Vec<ModelChoice>,
) -> Result<()> {
    let models = bounded_models(models);
    if models.is_empty() || known.as_ref() == Some(&models) {
        return Ok(());
    }
    log.append(
        &mut storage.db,
        id,
        context,
        EventKind::ModelsOffered {
            models: models.clone(),
        },
    )?;
    *known = Some(models);
    Ok(())
}

/// Applies what session `id`'s agent said of its login to `login`; `seen`
/// is [`Running::login_seen`].
fn hear_login(
    login: &LoginState,
    id: SessionId,
    logged_in: bool,
    source: LoginSource,
    seen: &LoginSeen,
) {
    if login.apply(logged_in, source, seen) {
        tracing::debug!(session = %id.0, logged_in, ?source, "agent login heard");
    } else {
        tracing::info!(session = %id.0, ?source, "agent reported a logout from before a login; dropped");
    }
}

/// A stopped agent's mapper and what it reported while stopping, ending
/// with `Exited`.
struct Halted {
    mapper: EventMapper,
    events: Vec<AdapterEvent>,
    /// [`Running::login_seen`].
    login_seen: LoginSeen,
}

/// Shuts session `id`'s agent down. Needs nothing of the supervisor, so
/// several can stop at once.
async fn halt<S: AgentSession>(id: SessionId, running: Running<S>) -> Halted {
    let Running {
        session,
        mut events,
        mapper,
        login_seen,
        ..
    } = running;
    // Drain while it stops: an agent may block on a full event channel.
    let drain = async {
        let mut reported = Vec::new();
        while let Some(event) = events.recv().await {
            let exited = matches!(event, AdapterEvent::Exited { .. });
            reported.push(event);
            if exited {
                break;
            }
        }
        reported
    };
    let (stopped, mut reported) = tokio::join!(session.shutdown(), drain);
    if let Err(error) = stopped {
        tracing::warn!(session = %id.0, %error, "agent shutdown failed");
    }
    if !matches!(reported.last(), Some(AdapterEvent::Exited { .. })) {
        reported.push(AdapterEvent::Exited {
            error: None,
            unsent: 0,
        });
    }
    Halted {
        mapper,
        events: reported,
        login_seen,
    }
}

/// The next event from any running agent session; while `disk` is low, only
/// from one the user is stopping something in ([`Running::controlled`]).
/// The disk is read at every poll, so no event is read once it turned low.
/// Pending while none is read.
async fn next_event<S>(
    running: &mut HashMap<SessionId, Running<S>>,
    disk: &Disk,
) -> (SessionId, Option<AdapterEvent>) {
    poll_fn(|cx| {
        let paused = disk.is_low();
        for (id, session) in running.iter_mut() {
            if paused && !session.controlled() {
                continue;
            }
            if let Poll::Ready(event) = session.events.poll_recv(cx) {
                return Poll::Ready((*id, event));
            }
        }
        Poll::Pending
    })
    .await
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// The session's `AgentWorking` and `Errored` after the owed rows were
/// stored ([`lost_messages`]): a range of its newest events, read through
/// `events_session_type`.
pub(crate) const OWED_STATES_SQL: &str = "SELECT context, json_extract(kind, '$.state') FROM events \
     WHERE session_id = ?1 AND seq > ?2 \
     AND json_extract(kind, '$.type') = 'session_state_changed' \
     AND json_extract(kind, '$.state') IN ('agent_working', 'errored') \
     ORDER BY seq";

/// The messages of `progress.owed` no agent took, by session and context,
/// each in handing order. The rows are as the previous run last stored
/// them, with `supervisor_cursor.accounted`, the log's last seq then: the
/// supervisor's own `SessionStateChanged` logged after it in the session
/// settle what the rows do not show yet, each only the rows marked for it.
/// An `AgentWorking` in a context settles the message marked starting
/// there: it was marked, and stored, right before its turn's `AgentWorking`
/// was logged, with nothing logged between, so the first one after the
/// rows is that one. An `Errored` settles those settling in its context:
/// it is the one they were marked for. Anything else is no proof: an
/// `AgentWorking` can open a turn the agent started on its own, `Idle` can
/// end the turn a message was queued behind, and an `Errored` logged
/// before the rows were stored (a stopped agent's, say) answers older
/// messages. So a waiting message is lost whatever follows it. A message
/// its session still held unhandled is not lost: the next run hands it
/// over.
fn lost_messages(
    db: &Db,
    progress: &Progress,
) -> Result<HashMap<SessionId, HashMap<ContextId, Vec<Seq>>>> {
    let accounted: i64 = db.conn().query_row(
        "SELECT accounted FROM supervisor_cursor WHERE id = 1",
        [],
        |r| r.get(0),
    )?;
    let mut lost = HashMap::new();
    for (&session, rows) in &progress.owed {
        let handled = progress.held.get(&session).copied();
        let mut errored = HashSet::new();
        let mut working = HashSet::new();
        let mut states = db.conn().prepare_cached(OWED_STATES_SQL)?;
        let mut states = states.query(rusqlite::params![session.0.to_string(), accounted])?;
        while let Some(row) = states.next()? {
            let context: ContextId = parse_id(&row.get::<_, String>(0)?)?;
            let state: String = row.get(1)?;
            if state == "errored" {
                errored.insert(context);
            } else {
                working.insert(context);
            }
        }
        let mut unsettled: Vec<&Owed> = (rows.iter())
            .filter(|owed| handled.is_none_or(|handled| owed.seq <= handled))
            .filter(|owed| match owed.state {
                OwedState::Waiting => true,
                OwedState::Starting => !working.contains(&owed.context),
                OwedState::Settling => !errored.contains(&owed.context),
            })
            .collect();
        unsettled.sort_by_key(|owed| owed.seq);
        let contexts: &mut HashMap<ContextId, Vec<Seq>> = lost.entry(session).or_default();
        for owed in unsettled {
            contexts.entry(owed.context).or_default().push(owed.seq);
        }
        if contexts.is_empty() {
            lost.remove(&session);
        }
    }
    Ok(lost)
}

/// The `Errored` message for `count` messages lost with the daemon.
fn messages_lost(count: usize) -> String {
    match count {
        1 => MESSAGE_LOST.to_owned(),
        n => format!("{n} messages lost when the server stopped; send them again"),
    }
}

fn load_progress(db: &Db) -> Result<Progress> {
    let cursor: i64 =
        db.conn()
            .query_row("SELECT seq FROM supervisor_cursor WHERE id = 1", [], |r| {
                r.get(0)
            })?;
    let mut held = HashMap::new();
    let mut rows = db
        .conn()
        .prepare_cached("SELECT session_id, seq FROM supervisor_held")?;
    let mut rows = rows.query([])?;
    while let Some(row) = rows.next()? {
        let session: String = row.get(0)?;
        held.insert(parse_id(&session)?, from_sql(row.get(1)?)?);
    }
    let mut owed: HashMap<SessionId, Vec<Owed>> = HashMap::new();
    let mut rows = db.conn().prepare_cached(
        "SELECT seq, session_id, context, state FROM supervisor_owed ORDER BY seq",
    )?;
    let mut rows = rows.query([])?;
    while let Some(row) = rows.next()? {
        let session: String = row.get(1)?;
        let context: String = row.get(2)?;
        let state: String = row.get(3)?;
        owed.entry(parse_id(&session)?).or_default().push(Owed {
            seq: from_sql(row.get(0)?)?,
            context: parse_id(&context)?,
            state: OwedState::from_sql(&state)?,
        });
    }
    Ok(Progress {
        cursor: from_sql(cursor)?,
        held,
        owed,
    })
}

/// Writes `progress` in one transaction: a held session's row is never
/// without the cursor that read its events, nor left behind one; nor an
/// owed message without the cursor past it.
fn store_progress(db: &mut Db, progress: &Progress) -> Result<()> {
    let tx = event_log::begin(db)?;
    tx.prepare_cached("UPDATE supervisor_cursor SET seq = ?1 WHERE id = 1")?
        .execute([to_sql(progress.cursor)?])?;
    tx.execute("DELETE FROM supervisor_held", [])?;
    for (session, &seq) in &progress.held {
        tx.prepare_cached("INSERT INTO supervisor_held (session_id, seq) VALUES (?1, ?2)")?
            .execute(rusqlite::params![session.0.to_string(), to_sql(seq)?])?;
    }
    write_owed(&tx, &progress.owed)?;
    tx.commit()?;
    Ok(())
}

/// Writes the messages `owed` alone; see [`store_progress`].
fn store_owed(db: &mut Db, owed: &HashMap<SessionId, Vec<Owed>>) -> Result<()> {
    let tx = event_log::begin(db)?;
    write_owed(&tx, owed)?;
    tx.commit()?;
    Ok(())
}

/// Replaces the owed rows with `owed`, stamped with the log's last seq:
/// they reflect every state event the supervisor logged up to it, and
/// none after.
fn write_owed(tx: &rusqlite::Transaction<'_>, owed: &HashMap<SessionId, Vec<Owed>>) -> Result<()> {
    tx.execute(
        "UPDATE supervisor_cursor SET accounted = (SELECT COALESCE(MAX(seq), 0) FROM events) \
         WHERE id = 1",
        [],
    )?;
    tx.execute("DELETE FROM supervisor_owed", [])?;
    for (session, rows) in owed {
        for row in rows {
            tx.prepare_cached(
                "INSERT INTO supervisor_owed (seq, session_id, context, state) \
                 VALUES (?1, ?2, ?3, ?4)",
            )?
            .execute(rusqlite::params![
                to_sql(row.seq)?,
                session.0.to_string(),
                row.context.0.to_string(),
                row.state.sql(),
            ])?;
        }
    }
    Ok(())
}

fn from_sql(seq: i64) -> Result<Seq> {
    u64::try_from(seq)
        .map(Seq)
        .map_err(|_| Error::Corrupt(format!("negative supervisor seq {seq}")))
}

fn to_sql(seq: Seq) -> Result<i64> {
    i64::try_from(seq.0).map_err(|_| Error::Corrupt(format!("seq {} too large", seq.0)))
}

#[cfg(test)]
mod tests {
    use skywave_proto::ImageType;

    use super::*;
    use crate::agent::Image;

    #[test]
    fn a_missing_image_is_noted_and_the_rest_is_opened() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::open(tmp.path()).unwrap();
        let gone = storage.put_blob(&b"\x89PNG\r\n\x1a\nA"[..]).unwrap();
        let kept = storage.put_blob(&b"\x89PNG\r\n\x1a\nB"[..]).unwrap();
        std::fs::remove_file(storage.dir.blobs_dir().join(gone.id.0.to_string())).unwrap();
        let attachments = [
            Attachment {
                blob: gone,
                media_type: ImageType::Png,
            },
            Attachment {
                blob: kept,
                media_type: ImageType::Png,
            },
        ];

        let read = user_input(&storage, "look", &attachments).read();

        assert_eq!(read.text, "look\n[image removed before delivery]");
        assert_eq!(
            read.images,
            [Image {
                media_type: ImageType::Png,
                bytes: b"\x89PNG\r\n\x1a\nB".to_vec(),
            }]
        );
    }
}
