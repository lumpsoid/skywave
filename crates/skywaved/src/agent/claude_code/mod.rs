//! The Claude Code adapter: drives the `claude` CLI in stream-json mode
//! (docs/agents/claude-code.md, architecture.md "Server").
//!
//! This module is the only place that names the agent (D3). One CLI process
//! runs per agent session, in the session's workdir (D14), started as
//!
//! ```text
//! <agent.command> -p --input-format stream-json --output-format stream-json \
//!   --verbose --permission-prompt-tool stdio \
//!   --settings '{"cleanupPeriodDays":36500}' \
//!   [--allow-dangerously-skip-permissions] [<session rules>] \
//!   [--permission-mode <mode>] \
//!   (--session-id <uuid> | --resume <uuid> |
//!    --resume <parent> --fork-session --session-id <uuid>) <agent.args...>
//! ```
//!
//! with `DISABLE_AUTOUPDATER=1` in its environment ([`NO_AUTOUPDATE_ENV`]),
//! so the CLI never updates itself under the adapter. The `--settings`
//! ([`KEEP_TRANSCRIPTS_SETTINGS`]) keeps the CLI's own retention sweep from
//! deleting the transcripts a resume needs. A CLI that still
//! reports another version than [`protocol::PINNED_VERSION`] is reported as
//! [`AdapterEvent::VersionUnconfirmed`](super::AdapterEvent::VersionUnconfirmed).
//!
//! A fresh context gets a new UUID as `--session-id`, reported with the first
//! turn as the context's [`ResumeHandle`](super::ResumeHandle)
//! `{"session_id": "<uuid>"}` (a process stopped before any message reports
//! none, as the CLI keeps no conversation for it); a restart passes it back
//! with `--resume`. A context forked from another's conversation resumes the
//! parent with `--fork-session` under a new UUID: the CLI runs the fork as
//! that id, reported like a fresh context's; one stopped before any message
//! reports none, so its next start forks again. A conversation the user had in
//! the CLI outside Skywave is adopted the same way, as a fork of the CLI's
//! session id (so the outside transcript is never written); an id that is not
//! a UUID is a resume error before any process is spawned. Rules granted for
//! the Skywave session that the CLI keeps only in its process are the
//! session's [`Grants`](super::Grants) `{"rules": [...]}`, reported apart from
//! the handle and passed to every later process of the session, fresh or
//! resumed, as flags ([`rules::cli_args`]), so they survive a restart and a
//! new context.
//!
//! [`AgentAdapter::start`] returns after the CLI answers the initialize
//! handshake; a CLI that fails before that is a start error, or
//! [`AgentError::ConversationGone`](super::AgentError::ConversationGone)
//! when it does not know the session resumed or forked (its transcript was
//! deleted). The mode the [`StartSpec`] names is a start flag,
//! `--permission-mode`, replacing any mode the session rules grant: a
//! resumed CLI keeps no mode, and one asked for after the handshake would
//! first report its default as the mode. The initialize answer's
//! `current_permission_mode` answers it. Bypass is no start flag (§3.6); it
//! is asked for like the model and effort, which follow in that order, with
//! control requests, like any later change
//! (docs/agents/claude-code.md §3.7, §3.10), so each gets the CLI's own
//! answer. The CLI handles stdin in order,
//! so the effort is applied after the model switch; the level is a flag that
//! outlasts model switches anyway.
//!
//! The protocol translation lives in [`protocol::Protocol`], which does no
//! I/O; this module runs the process around it. Requests return once queued
//! for the process, like the fake agent's; their effects arrive as events.
//! stderr lines go to the daemon log.
//!
//! The CLI's stored transcripts, outside conversations included, are read
//! by [`transcripts::ClaudeTranscripts`]; [`own_conversations`] names the
//! ones Skywave's contexts run. The account's usage limits are read by
//! [`limits::ClaudeUsageLimits`], with a process of its own.
//!
//! **Shutdown.** [`AgentSession::shutdown`] with a turn that was interrupted
//! waits up to the grace period for its `result`. A turn still running
//! without an interrupt is abandoned: the process is killed at once, without
//! an interrupt (closing stdin would not stop the turn, and interrupting it
//! would cancel its approvals). Otherwise shutdown closes stdin and waits up
//! to the grace period for the process to exit before killing it. The event
//! stream ends with `Exited`.
//!
//! **Killing.** A kill is SIGTERM to the CLI's process group, then SIGKILL
//! for whatever is left after the term grace ([`crate::process`]). The SIGTERM is
//! what stops the CLI's tools: it runs each Bash command in a session of
//! its own, out of the group's reach, and on SIGTERM kills its running
//! command and background tasks and reports them before it exits
//! (docs/agents/claude-code.md §6). A kill on request reads and reports
//! stdout until it ends, so the log gets those reports, which the CLI also
//! keeps in its conversation. On SIGTERM the CLI reports no turn end and
//! cancels no approvals, so an abandoned turn and its approvals stay open.
//! Through the term grace stdout is read into the backlog too, so a CLI
//! writing on its way out is not stuck on a full pipe until its SIGKILL.
//! A CLI that stops reading stdin for the write timeout is killed and
//! reported as an `Exited` error, so a wedged process cannot block its
//! session. A supervisor paused on a low disk stops reading the events
//! (D40): the driver then waits in its event send, with its writes for the
//! request or line at hand already done, and the CLI blocks writing its
//! stdout. No timer runs in that wait: the write timeout covers only a
//! write in progress, so a paused CLI is never killed as wedged, and a
//! request sent meanwhile waits in the driver's queue until the events are
//! read again.
//!
//! **Unanswered interrupts.** The CLI is sent one interrupt per turn. An
//! [`AgentSession::escalate_interrupt`] (the user interrupting again) that
//! arrives while that interrupt is unanswered, the turn's `result` not yet
//! read, kills the process as an abandoned shutdown does and reports
//! [`INTERRUPT_UNANSWERED`](super::INTERRUPT_UNANSWERED) as the `Exited`
//! error. One arriving after the result does nothing: it follows its
//! interrupt on the same request channel, so the turn running then, if any,
//! is a later one (a queued message's).
//!
//! **Output.** stdout is read as raw lines ([`lines`]): bytes that are not
//! UTF-8 are replaced, and a line over [`lines::MAX_LINE`] is dropped. Either
//! way the line is stray output, like one that is not JSON, and reading goes
//! on; only EOF or a read error ends the output.
//!
//! **Exit errors.** A process that exits successfully after the handshake
//! exited cleanly unless the CLI reported an error since the last turn
//! started; stray output does not make it an error. A failed exit is
//! explained by the stderr tail with the exit status, falling back to stray
//! output seen since the handshake or the last turn start
//! ([`protocol::Protocol::exit_error`]).

pub mod display;
pub mod limits;
mod lines;
pub mod login;
pub mod protocol;
pub mod rules;
pub mod transcripts;
pub mod usage;

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use skywave_proto::{Decision, SessionId};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout, timeout_at};
use tracing::Instrument;
use uuid::Uuid;

use self::lines::{Line, LineReader, MAX_LINE};
use self::protocol::{Output, Protocol, mode_name};
use super::{
    AdapterEvent, AdapterEvents, AgentAdapter, AgentError, AgentRequestId, AgentSession,
    Conversation, Effort, INTERRUPT_UNANSWERED, PermissionMode, QuestionAnswer, StartSpec,
    UserInput,
};
use crate::config::AgentConfig;
use crate::process::{self, DEFAULT_TERM_GRACE, Group};

/// The CLI conversation ids Skywave's contexts run, each with its
/// session: every stored resume handle of this adapter. A handle this
/// adapter did not make is left out, and so is a moved context's: it is
/// never started again (the session it moved to runs a fork of it), so its
/// conversation is no session's.
pub fn own_conversations(
    conn: &rusqlite::Connection,
) -> crate::error::Result<HashMap<String, SessionId>> {
    let mut statement = conn.prepare_cached(
        "SELECT session_id, agent_state FROM contexts
         WHERE agent_state IS NOT NULL AND moved_to IS NULL",
    )?;
    let rows = statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let mut own = HashMap::new();
    for row in rows {
        let (session, state) = row?;
        let handle = super::ResumeHandle(serde_json::from_str(&state)?);
        if let Ok(id) = protocol::parse_resume_handle(&handle) {
            own.insert(id.to_string(), crate::event_log::parse_id(&session)?);
        }
    }
    Ok(own)
}

/// Flags every CLI process gets, before the session flag and `agent.args`.
pub const REQUIRED_ARGS: [&str; 10] = [
    "-p",
    "--input-format",
    "stream-json",
    "--output-format",
    "stream-json",
    "--verbose",
    "--permission-prompt-tool",
    "stdio",
    "--settings",
    KEEP_TRANSCRIPTS_SETTINGS,
];

/// The `--settings` every CLI process gets: a retention far beyond any
/// session's idle time. A CLI process sweeps transcripts older than
/// `cleanupPeriodDays`, default 30, and `--resume` needs
/// the transcript, so a context idle for a month could not be resumed.
/// Flag settings outrank the user's `~/.claude/settings.json` (only managed
/// policy outranks them) and are never written anywhere, so the user's own
/// files stay untouched; a `claude` the user runs directly still sweeps with
/// the user's value (docs/agents/claude-code.md §5).
pub const KEEP_TRANSCRIPTS_SETTINGS: &str = r#"{"cleanupPeriodDays":36500}"#;

/// The flag a process gets when the server allows bypass
/// (`agent.allow_bypass`): `set_permission_mode` may then switch to
/// `bypassPermissions` mid-session, without the session starting in it as
/// `--dangerously-skip-permissions` would (docs/agents/claude-code.md §3.6).
pub const ALLOW_BYPASS_ARG: &str = "--allow-dangerously-skip-permissions";

/// The first of `args` (`agent.args`) that would let the CLI run in
/// bypass on its own, as written: `--dangerously-skip-permissions`,
/// [`ALLOW_BYPASS_ARG`], or `--permission-mode bypassPermissions` (either
/// as two arguments or `=`-joined). Bypass has one switch, the server's
/// `agent.allow_bypass` (D38), so the config refuses these.
pub fn bypass_arg(args: &[String]) -> Option<String> {
    const MODE: &str = "--permission-mode";
    const BYPASS: &str = "bypassPermissions";
    let flag = |arg: &str, name: &str| {
        arg == name
            || arg
                .strip_prefix(name)
                .is_some_and(|rest| rest.starts_with('='))
    };
    args.iter().enumerate().find_map(|(at, arg)| {
        if flag(arg, "--dangerously-skip-permissions") || flag(arg, ALLOW_BYPASS_ARG) {
            return Some(arg.clone());
        }
        if arg.strip_prefix(MODE).and_then(|r| r.strip_prefix('=')) == Some(BYPASS) {
            return Some(arg.clone());
        }
        (arg == MODE && args.get(at + 1).map(String::as_str) == Some(BYPASS))
            .then(|| format!("{MODE} {BYPASS}"))
    })
}

/// The environment variable (name, value) every CLI process gets so it
/// never updates itself: an update under a running server changes the
/// protocol the adapter was confirmed against ([`protocol::PINNED_VERSION`]).
/// It turns off only the automatic updater; `claude update` by hand still
/// works (`DISABLE_UPDATES` would block that too). docs/agents/claude-code.md
/// §1.
pub const NO_AUTOUPDATE_ENV: (&str, &str) = ("DISABLE_AUTOUPDATER", "1");

/// How long shutdown waits for a turn to end, and then for the exit.
pub const DEFAULT_GRACE_PERIOD: Duration = Duration::from_secs(10);
/// How long start waits for the initialize handshake.
pub const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a write may wait for the CLI to read its stdin before the
/// process counts as wedged and is killed.
pub const DEFAULT_WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// Capacity of a session's event channel.
const EVENT_CAPACITY: usize = 256;
/// How long to wait for the rest of stderr once the process has exited.
const STDERR_DRAIN: Duration = Duration::from_secs(1);
/// How often a terminated group is checked for what is left in it.
const GROUP_POLL: Duration = Duration::from_millis(50);
/// What the CLI says when `--resume` names a session it does not have, with
/// or without `--fork-session` (docs/agents/claude-code.md §5): its
/// transcript was deleted, or never existed.
pub const UNKNOWN_SESSION: &str = "No conversation found";

/// Starts Claude Code CLI sessions with the configured command.
#[derive(Debug, Clone)]
pub struct ClaudeCodeAdapter {
    config: AgentConfig,
    grace_period: Duration,
    startup_timeout: Duration,
    write_timeout: Duration,
    term_grace: Duration,
}

impl ClaudeCodeAdapter {
    pub fn new(config: AgentConfig) -> ClaudeCodeAdapter {
        ClaudeCodeAdapter {
            config,
            grace_period: DEFAULT_GRACE_PERIOD,
            startup_timeout: DEFAULT_STARTUP_TIMEOUT,
            write_timeout: DEFAULT_WRITE_TIMEOUT,
            term_grace: DEFAULT_TERM_GRACE,
        }
    }

    /// Sets how long shutdown waits before killing the process.
    pub fn with_grace_period(mut self, grace_period: Duration) -> ClaudeCodeAdapter {
        self.grace_period = grace_period;
        self
    }

    /// Sets how long start waits for the initialize handshake.
    pub fn with_startup_timeout(mut self, startup_timeout: Duration) -> ClaudeCodeAdapter {
        self.startup_timeout = startup_timeout;
        self
    }

    /// Sets how long a write waits for the CLI to read before killing it.
    pub fn with_write_timeout(mut self, write_timeout: Duration) -> ClaudeCodeAdapter {
        self.write_timeout = write_timeout;
        self
    }

    /// Sets how long a killed process group gets between SIGTERM and
    /// SIGKILL.
    pub fn with_term_grace(mut self, term_grace: Duration) -> ClaudeCodeAdapter {
        self.term_grace = term_grace;
        self
    }

    /// The CLI arguments for a process taking up `session`: the required
    /// flags, [`ALLOW_BYPASS_ARG`] when `agent.allow_bypass`, the flags
    /// granting `session_rules`, `--permission-mode <mode>` for a `mode`
    /// (which then replaces any mode the rules grant), the session's flags
    /// ([`CliSession::args`]), then `agent.args`.
    pub fn command_args(
        &self,
        session: CliSession,
        session_rules: &[Value],
        mode: Option<PermissionMode>,
    ) -> Vec<String> {
        let mut args: Vec<String> = REQUIRED_ARGS.iter().map(|a| (*a).to_owned()).collect();
        if self.config.allow_bypass {
            args.push(ALLOW_BYPASS_ARG.to_owned());
        }
        match mode {
            Some(mode) => {
                let granted: Vec<Value> = (session_rules.iter())
                    .filter(|update| !rules::is_mode(update))
                    .cloned()
                    .collect();
                args.extend(rules::cli_args(&granted));
                args.extend(["--permission-mode".to_owned(), mode_name(mode).to_owned()]);
            }
            None => args.extend(rules::cli_args(session_rules)),
        }
        args.extend(session.args());
        args.extend(self.config.args.iter().cloned());
        args
    }

    /// The command for a process taking up `session` in `workdir`: the
    /// [`command_args`](Self::command_args) and [`NO_AUTOUPDATE_ENV`] on
    /// top of the server's environment.
    pub fn command(
        &self,
        workdir: &Path,
        session: CliSession,
        session_rules: &[Value],
        mode: Option<PermissionMode>,
    ) -> Command {
        let mut command = Command::new(&self.config.command);
        command
            .args(self.command_args(session, session_rules, mode))
            .env(NO_AUTOUPDATE_ENV.0, NO_AUTOUPDATE_ENV.1)
            .current_dir(workdir);
        command
    }

    fn spawn(
        &self,
        workdir: &Path,
        session: CliSession,
        session_rules: &[Value],
        mode: Option<PermissionMode>,
    ) -> Result<Child, AgentError> {
        let mut command = self.command(workdir, session, session_rules, mode);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        process::isolate(&mut command);
        command.spawn().map_err(|e| {
            AgentError::Start(format!(
                "cannot run {} in {}: {e}",
                self.config.command.display(),
                workdir.display()
            ))
        })
    }
}

/// The CLI session a process takes up, from the start's [`Conversation`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliSession {
    /// A new conversation under `id`.
    New(Uuid),
    /// The conversation under `id`.
    Resume(Uuid),
    /// A copy of the conversation `parent`, run under the new `id`.
    Fork { parent: Uuid, id: Uuid },
}

impl CliSession {
    /// The session id the CLI runs as.
    pub fn id(self) -> Uuid {
        match self {
            CliSession::New(id) | CliSession::Resume(id) | CliSession::Fork { id, .. } => id,
        }
    }

    /// The flags naming the session: `--session-id <id>`, `--resume <id>`,
    /// or `--resume <parent> --fork-session --session-id <id>`.
    pub fn args(self) -> Vec<String> {
        match self {
            CliSession::New(id) => vec!["--session-id".to_owned(), id.to_string()],
            CliSession::Resume(id) => vec!["--resume".to_owned(), id.to_string()],
            CliSession::Fork { parent, id } => vec![
                "--resume".to_owned(),
                parent.to_string(),
                "--fork-session".to_owned(),
                "--session-id".to_owned(),
                id.to_string(),
            ],
        }
    }

    /// The handle naming [`Self::id`] is stored already: only a resumed
    /// process runs as a conversation the context had.
    pub fn handle_stored(self) -> bool {
        matches!(self, CliSession::Resume(_))
    }

    /// The process takes up a conversation that must exist, so the CLI not
    /// knowing it means the conversation is gone.
    fn has_parent(self) -> bool {
        matches!(self, CliSession::Resume(_) | CliSession::Fork { .. })
    }
}

impl AgentAdapter for ClaudeCodeAdapter {
    type Session = ClaudeCodeSession;

    async fn start(
        &self,
        spec: StartSpec,
    ) -> Result<(ClaudeCodeSession, AdapterEvents), AgentError> {
        // A fork (or an adoption) gets a new id as a fresh start does: its
        // first turn reports it, and one stopped before any message forks
        // again.
        let session = match &spec.conversation {
            Conversation::Fresh => CliSession::New(Uuid::new_v4()),
            Conversation::Resume(handle) => CliSession::Resume(
                protocol::parse_resume_handle(handle).map_err(AgentError::Resume)?,
            ),
            Conversation::Fork(parent) => CliSession::Fork {
                parent: protocol::parse_resume_handle(parent).map_err(AgentError::Resume)?,
                id: Uuid::new_v4(),
            },
            // `parse_str` also accepts the simple, braced and urn forms; the
            // argv writes the parent hyphenated (the transcript's file
            // name), so all of them work.
            Conversation::Adopt(id) => CliSession::Fork {
                parent: Uuid::parse_str(id).map_err(|_| {
                    AgentError::Resume(format!("not a Claude Code session id: {id}"))
                })?,
                id: Uuid::new_v4(),
            },
        };
        let session_rules = match &spec.grants {
            Some(grants) => rules::parse_grants(grants).map_err(AgentError::Start)?,
            None => Vec::new(),
        };
        // The mode is a start flag, so the CLI never runs in another first;
        // bypass is not one (§3.6) and is asked for after the handshake.
        let (start_mode, asked_mode) = match spec.mode {
            Some(PermissionMode::Bypass) => (None, spec.mode),
            mode => (mode, None),
        };
        let span = tracing::info_span!("agent", session = %spec.session_id.0);
        let child = self.spawn(&spec.workdir, session, &session_rules, start_mode)?;
        let mut process = Process::new(child, &span, self.write_timeout, self.term_grace)?;
        let mut protocol = Protocol::new(session.id(), session.handle_stored())
            .with_session_rules(session_rules)
            .with_start_mode(start_mode);

        let mut early = Vec::new();
        let handshake = handshake(&mut process, &mut protocol, &mut early).instrument(span.clone());
        let failure = match timeout(self.startup_timeout, handshake).await {
            Ok(Ok(())) => None,
            Ok(Err(message)) => Some(message),
            Err(_) => Some(format!(
                "no answer to the initialize handshake within {:?}",
                self.startup_timeout
            )),
        };
        if let Some(message) = failure {
            process.kill().await;
            return Err(
                if session.has_parent() && message.contains(UNKNOWN_SESSION) {
                    AgentError::ConversationGone(message)
                } else {
                    AgentError::Start(message)
                },
            );
        }

        // Start-time requests are asked like any other: their lines go to
        // the CLI and any answer known at once joins the early events.
        let mut wanted = Vec::new();
        let mut asked = Vec::new();
        if let Some(mode) = asked_mode {
            asked.push(protocol.set_permission_mode(mode));
        }
        if let Some(model) = spec.model {
            asked.push(protocol.set_model(model));
        }
        if let Some(effort) = spec.effort {
            asked.push(protocol.set_effort(effort));
        }
        for mut out in asked {
            wanted.append(&mut out.writes);
            early.append(&mut out.events);
        }
        if let Err(e) = process.write(&wanted).await {
            // The process is gone or going; the driver reports its exit.
            tracing::warn!("cannot write to the agent: {e}");
        }

        let (events, events_rx) = mpsc::channel(EVENT_CAPACITY);
        for event in early {
            events.try_send(event).map_err(|_| AgentError::Closed)?;
        }
        let (requests, requests_rx) = mpsc::unbounded_channel();
        let driver = Driver {
            protocol,
            process,
            events,
            requests: requests_rx,
            grace_period: self.grace_period,
        };
        let task = tokio::spawn(driver.run().instrument(span));
        let session = ClaudeCodeSession {
            session_id: spec.session_id,
            requests,
            task,
        };
        Ok((session, events_rx))
    }
}

/// Writes the handshake and reads until the CLI answers it; on failure, the
/// best error text available.
async fn handshake(
    process: &mut Process,
    protocol: &mut Protocol,
    early: &mut Vec<AdapterEvent>,
) -> Result<(), String> {
    let mut out = protocol.start();
    loop {
        early.append(&mut out.events);
        // A CLI that already exited shows up as EOF below, with its error.
        if let Err(e) = process.write(&out.writes).await {
            tracing::debug!("cannot write to the agent during startup: {e}");
        }
        if protocol.ready() {
            return Ok(());
        }
        if let Some(error) = protocol.startup_error() {
            return Err(error.to_owned());
        }
        match process.next_line().await {
            Ok(Some(line)) => out = on_stdout(protocol, line),
            Ok(None) | Err(_) => {
                let status = process.wait_exit().await;
                // Always an error: no exit before the handshake is clean.
                let error = process.exit_error(protocol, status).await;
                return Err(error.unwrap_or_else(|| "agent exited".to_owned()));
            }
        }
    }
}

/// Translates one stdout line.
fn on_stdout(protocol: &mut Protocol, line: Line) -> Output {
    match line {
        Line::Text(text) => protocol.on_line(&text),
        Line::TooLong(len) => protocol.on_oversized_line(len),
    }
}

/// A running Claude Code session.
#[derive(Debug)]
pub struct ClaudeCodeSession {
    session_id: SessionId,
    requests: mpsc::UnboundedSender<Request>,
    task: JoinHandle<()>,
}

impl ClaudeCodeSession {
    /// The Skywave session this agent runs for.
    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    fn request(&self, request: Request) -> Result<(), AgentError> {
        self.requests.send(request).map_err(|_| AgentError::Closed)
    }
}

impl AgentSession for ClaudeCodeSession {
    async fn send_message(&mut self, input: UserInput) -> Result<(), AgentError> {
        self.request(Request::Message(input))
    }

    async fn answer_approval(
        &mut self,
        request: AgentRequestId,
        decision: Decision,
        rule: Option<u8>,
    ) -> Result<(), AgentError> {
        self.request(Request::Answer(request, decision, rule))
    }

    async fn answer_question(
        &mut self,
        request: AgentRequestId,
        answers: Vec<QuestionAnswer>,
    ) -> Result<(), AgentError> {
        self.request(Request::AnswerQuestion(request, answers))
    }

    async fn interrupt(&mut self) -> Result<(), AgentError> {
        self.request(Request::Interrupt)
    }

    async fn escalate_interrupt(&mut self) -> Result<(), AgentError> {
        self.request(Request::EscalateInterrupt)
    }

    async fn set_permission_mode(&mut self, mode: PermissionMode) -> Result<(), AgentError> {
        self.request(Request::Mode(mode))
    }

    async fn set_model(&mut self, model: String) -> Result<(), AgentError> {
        self.request(Request::Model(model))
    }

    async fn set_effort(&mut self, effort: Effort) -> Result<(), AgentError> {
        self.request(Request::Effort(effort))
    }

    async fn stop_task(&mut self, task: String) -> Result<(), AgentError> {
        self.request(Request::StopTask(task))
    }

    async fn shutdown(self) -> Result<(), AgentError> {
        // Already exited on its own: nothing to stop.
        let _ = self.request(Request::Shutdown);
        drop(self.requests);
        self.task
            .await
            .map_err(|e| AgentError::Io(std::io::Error::other(e)))
    }
}

#[derive(Debug)]
enum Request {
    Message(UserInput),
    Answer(AgentRequestId, Decision, Option<u8>),
    AnswerQuestion(AgentRequestId, Vec<QuestionAnswer>),
    Interrupt,
    EscalateInterrupt,
    Mode(PermissionMode),
    Model(String),
    Effort(Effort),
    StopTask(String),
    Shutdown,
}

/// The CLI process and its pipes.
struct Process {
    // Dropped before `child`, so the group is killed while the CLI that
    // leads it is not yet reaped.
    group: Group,
    child: Child,
    /// `None` once closed.
    stdin: Option<ChildStdin>,
    /// Read only through [`Process::next_line`], which hands out the backlog
    /// first.
    stdout: LineReader<BufReader<ChildStdout>>,
    /// What stdout said while a write was pending, its end included, not yet
    /// handed out.
    backlog: VecDeque<std::io::Result<Option<Line>>>,
    /// stdout ended or failed while a write was pending.
    stdout_ended: bool,
    stderr_task: Option<JoinHandle<()>>,
    /// The last non-blank stderr line.
    stderr_tail: Arc<Mutex<Option<String>>>,
    write_timeout: Duration,
    term_grace: Duration,
    /// When the group was sent SIGTERM.
    terminated: Option<Instant>,
    /// The process was killed for not reading its stdin.
    wedged: bool,
}

impl Process {
    fn new(
        mut child: Child,
        span: &tracing::Span,
        write_timeout: Duration,
        term_grace: Duration,
    ) -> Result<Process, AgentError> {
        let missing = || AgentError::Start("agent pipes not available".to_owned());
        let stdin = child.stdin.take().ok_or_else(missing)?;
        let stdout = child.stdout.take().ok_or_else(missing)?;
        let stderr = child.stderr.take().ok_or_else(missing)?;
        let stderr_tail = Arc::new(Mutex::new(None));
        let stderr_task =
            tokio::spawn(read_stderr(stderr, Arc::clone(&stderr_tail)).instrument(span.clone()));
        Ok(Process {
            group: Group::of(&child),
            child,
            stdin: Some(stdin),
            stdout: LineReader::new(BufReader::new(stdout), MAX_LINE),
            backlog: VecDeque::new(),
            stdout_ended: false,
            stderr_task: Some(stderr_task),
            stderr_tail,
            write_timeout,
            term_grace,
            terminated: None,
            wedged: false,
        })
    }

    /// Writes `lines` to stdin, one JSON object per line. A process that
    /// does not take them within the write timeout is killed: half a line
    /// may have gone out, and a wedged process would hold up its session.
    async fn write(&mut self, lines: &[Value]) -> std::io::Result<()> {
        if lines.is_empty() {
            return Ok(());
        }
        let write_timeout = self.write_timeout;
        match timeout(write_timeout, self.write_lines(lines)).await {
            Ok(written) => written,
            Err(_) => {
                tracing::warn!("agent did not read its input within {write_timeout:?}; killing it");
                self.wedged = true;
                self.kill().await;
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "agent stopped reading its input",
                ))
            }
        }
    }

    /// Writes `lines` while reading stdout into the backlog: a CLI whose
    /// stdout writes block (Node on a pipe) stops reading its stdin once
    /// nobody reads its stdout, and a write longer than a pipe holds would
    /// then wait on it for good.
    async fn write_lines(&mut self, lines: &[Value]) -> std::io::Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "stdin closed"))?;
        let write = async {
            for line in lines {
                let mut bytes = serde_json::to_vec(line)?;
                bytes.push(b'\n');
                stdin.write_all(&bytes).await?;
            }
            stdin.flush().await
        };
        tokio::pin!(write);
        loop {
            tokio::select! {
                written = &mut write => return written,
                line = self.stdout.next_line(), if !self.stdout_ended => {
                    self.stdout_ended = !matches!(line, Ok(Some(_)));
                    self.backlog.push_back(line);
                }
            }
        }
    }

    /// The next stdout line, the backlog's first; `None` at EOF. Cancel
    /// safe, as [`LineReader::next_line`] is.
    async fn next_line(&mut self) -> std::io::Result<Option<Line>> {
        if let Some(line) = self.backlog.pop_front() {
            return line;
        }
        if self.stdout_ended {
            return Ok(None);
        }
        self.stdout.next_line().await
    }

    /// Waits for the process to exit, then kills what it left in its group;
    /// `None` if waiting failed.
    async fn wait_exit(&mut self) -> Option<std::process::ExitStatus> {
        self.stdin = None;
        let status = self.child.wait().await.ok();
        self.kill().await;
        status
    }

    /// SIGTERMs the group, the CLI included, once, then closes stdin: the
    /// CLI handles the signal, not an EOF.
    fn terminate(&mut self) {
        if self.terminated.is_none() && self.group.terminate() {
            self.terminated = Some(Instant::now());
        }
        self.stdin = None;
    }

    /// When whatever is left of a terminated group gets SIGKILL; now if
    /// it was not terminated (groups are not used).
    fn kill_deadline(&self) -> Instant {
        self.terminated
            .map_or_else(Instant::now, |at| at + self.term_grace)
    }

    /// Kills the process with its group and reaps it: SIGTERM unless sent
    /// already, then SIGKILL for whatever is left at the term grace's end.
    /// Stdout is read into the backlog meanwhile: a CLI whose stdout writes
    /// block (Node on a pipe) would otherwise never finish its SIGTERM
    /// handling, and be SIGKILLed at the grace's end. The CLI is reaped if
    /// it exits in time; until then its group id cannot be reused.
    async fn kill(&mut self) {
        self.terminate();
        let _ = timeout_at(self.kill_deadline(), async {
            let mut exited = false;
            loop {
                if exited && self.group.is_empty() {
                    break;
                }
                tokio::select! {
                    _ = self.child.wait(), if !exited => exited = true,
                    () = tokio::time::sleep(GROUP_POLL), if exited => {}
                    line = self.stdout.next_line(), if !self.stdout_ended => {
                        self.stdout_ended = !matches!(line, Ok(Some(_)));
                        self.backlog.push_back(line);
                    }
                }
            }
        })
        .await;
        self.group.kill();
        // Still running where groups are not used.
        if let Ok(None) = self.child.try_wait()
            && let Err(e) = self.child.kill().await
        {
            tracing::debug!("cannot kill the agent: {e}");
        }
        self.drain_stderr().await;
    }

    async fn drain_stderr(&mut self) {
        if let Some(task) = self.stderr_task.take() {
            let _ = timeout(STDERR_DRAIN, task).await;
        }
    }

    /// Why the process stopped with `status`; `None` for a clean exit
    /// ([`Protocol::exit_error`]).
    async fn exit_error(
        &mut self,
        protocol: &Protocol,
        status: Option<std::process::ExitStatus>,
    ) -> Option<String> {
        self.drain_stderr().await;
        if self.wedged {
            return Some("agent stopped reading its input; killed it".to_owned());
        }
        let stderr = self
            .stderr_tail
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        protocol.exit_error(stderr.as_deref(), status)
    }
}

/// Logs the agent's stderr lines and keeps the last one.
async fn read_stderr(stderr: ChildStderr, tail: Arc<Mutex<Option<String>>>) {
    let mut lines = LineReader::new(BufReader::new(stderr), MAX_LINE);
    while let Ok(Some(line)) = lines.next_line().await {
        let Line::Text(line) = line else { continue };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        tracing::warn!("agent stderr: {line}");
        *tail.lock().unwrap_or_else(|p| p.into_inner()) = Some(line.to_owned());
    }
}

/// Owns the process and the protocol for one running session.
struct Driver {
    protocol: Protocol,
    process: Process,
    // Dropped before `events`, so a session that sees its event stream end
    // gets `Closed` from then on.
    requests: mpsc::UnboundedReceiver<Request>,
    events: mpsc::Sender<AdapterEvent>,
    grace_period: Duration,
}

impl Driver {
    async fn run(mut self) {
        let error = loop {
            tokio::select! {
                line = self.process.next_line() => match line {
                    Ok(Some(line)) => {
                        let out = on_stdout(&mut self.protocol, line);
                        if !self.apply(out).await {
                            return self.abandon().await;
                        }
                    }
                    Ok(None) | Err(_) => break self.exited().await,
                },
                request = self.requests.recv() => {
                    let out = match request {
                        // Read off the runtime, before the stdin write: the
                        // write's timeout must not count the reading.
                        Some(Request::Message(input)) => {
                            let read = tokio::task::spawn_blocking(move || input.read())
                                .await
                                .expect("reading a message's images does not panic");
                            self.protocol.send_input(read)
                        }
                        Some(Request::Answer(request, decision, rule)) => {
                            self.protocol.answer(&request, decision, rule)
                        }
                        Some(Request::AnswerQuestion(request, answers)) => {
                            self.protocol.answer_question(&request, &answers)
                        }
                        Some(Request::Interrupt) => self.protocol.interrupt(),
                        // The interrupt is unanswered while its turn's
                        // result has not been read: a definitive signal,
                        // not a timeout.
                        Some(Request::EscalateInterrupt) if self.protocol.interrupting() => {
                            break self.stop_unanswered().await;
                        }
                        // The interrupt it repeats came first on this
                        // channel, so its turn is over: a turn running now
                        // (a queued message's) is not the one interrupted.
                        Some(Request::EscalateInterrupt) => Output::default(),
                        Some(Request::Mode(mode)) => self.protocol.set_permission_mode(mode),
                        Some(Request::Model(model)) => self.protocol.set_model(model),
                        Some(Request::Effort(effort)) => self.protocol.set_effort(effort),
                        Some(Request::StopTask(task)) => self.protocol.stop_task(task),
                        Some(Request::Shutdown) | None => break self.shutdown().await,
                    };
                    if !self.apply(out).await {
                        return self.abandon().await;
                    }
                }
            }
        };
        // Every way out (exit, stop, escalation kill) left the queue as it
        // is: what is still in it never ran.
        let unsent = self.protocol.unsent();
        let _ = self
            .events
            .send(AdapterEvent::Exited { error, unsent })
            .await;
    }

    /// Writes `out`'s lines and reports its events; false when nobody reads
    /// the events any more.
    async fn apply(&mut self, out: Output) -> bool {
        if let Err(e) = self.process.write(&out.writes).await {
            // The process is gone or going; its exit is reported from stdout.
            tracing::warn!("cannot write to the agent: {e}");
        }
        for event in out.events {
            if self.events.send(event).await.is_err() {
                return false;
            }
        }
        true
    }

    /// The event receiver is gone: stop the process without reporting.
    async fn abandon(mut self) {
        self.process.kill().await;
    }

    /// stdout ended on its own: the exit error, if the exit was abnormal.
    ///
    /// The process reads nothing more, so requests are refused from here
    /// on: a message sent now fails with `Closed` and its sender can start
    /// another agent, rather than having it dropped with this one. Messages
    /// accepted before that and not yet read are owed turns the process
    /// never answers ([`Protocol::undelivered`]), counted as unsent like the
    /// queued ones.
    async fn exited(&mut self) -> Option<String> {
        self.refuse_requests();
        let status = match timeout(self.grace_period, self.process.wait_exit()).await {
            Ok(status) => status,
            Err(_) => {
                self.process.kill().await;
                None
            }
        };
        self.process.exit_error(&self.protocol, status).await
    }

    /// Stops the process on request; a requested stop is not an error.
    async fn shutdown(&mut self) -> Option<String> {
        self.protocol.begin_shutdown();
        if self.protocol.turn_open() && !self.protocol.interrupting() {
            // Abandoned: no interrupt, so the CLI cancels nothing and the
            // turn and its approvals stay open in the log.
            tracing::info!("abandoning the running turn; killing the agent");
            self.kill().await;
            return None;
        }
        if self.protocol.turn_open() {
            let grace_period = self.grace_period;
            if timeout(grace_period, self.pump(true)).await.is_err() {
                tracing::warn!("agent turn did not end within {grace_period:?} of the interrupt");
            }
        }
        self.process.stdin = None;
        let grace_period = self.grace_period;
        let exited = timeout(grace_period, async {
            self.pump(false).await;
            self.process.wait_exit().await
        })
        .await;
        if exited.is_err() {
            tracing::warn!("agent did not exit within {grace_period:?}; killing it");
            self.kill().await;
        } else {
            self.process.drain_stderr().await;
        }
        None
    }

    /// The interrupt went unanswered and the user asked again: kills the
    /// process like an abandoned shutdown, reporting why. What the CLI
    /// says as it dies (a result for the turn included) is still reported.
    async fn stop_unanswered(&mut self) -> Option<String> {
        tracing::warn!("agent did not answer the interrupt; killing it");
        self.refuse_requests();
        self.protocol.begin_shutdown();
        self.kill().await;
        Some(INTERRUPT_UNANSWERED.to_owned())
    }

    /// The process will read nothing more: refuses requests from now on
    /// (`Closed`, so the sender can start another agent) and keeps the
    /// messages already accepted as [`Protocol::undelivered`].
    fn refuse_requests(&mut self) {
        self.requests.close();
        while let Ok(request) = self.requests.try_recv() {
            if let Request::Message(input) = request {
                // Read on the runtime thread: this runs once, after the
                // process exited, for messages it never read.
                self.protocol.undelivered(input.read());
            }
        }
    }

    /// Kills the process, reporting what it says until its stdout ends: on
    /// SIGTERM the CLI reports the tools and tasks it stops.
    async fn kill(&mut self) {
        self.process.terminate();
        let _ = timeout_at(self.process.kill_deadline(), self.pump(false)).await;
        self.process.kill().await;
    }

    /// Reads and reports stdout until it ends or, with `until_idle`, until
    /// the current turn is over.
    async fn pump(&mut self, until_idle: bool) {
        loop {
            if until_idle && !self.protocol.turn_open() {
                return;
            }
            match self.process.next_line().await {
                Ok(Some(line)) => {
                    let out = on_stdout(&mut self.protocol, line);
                    self.apply(out).await;
                }
                Ok(None) | Err(_) => return,
            }
        }
    }
}
