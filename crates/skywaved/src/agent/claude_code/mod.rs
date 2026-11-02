//! The Claude Code adapter: drives the `claude` CLI in stream-json mode
//! (docs/agents/claude-code.md, spec §5).
//!
//! This module is the only place that names the agent (D3). One CLI process
//! runs per agent session, in the session's workdir (D14), started as
//!
//! ```text
//! <agent.command> -p --input-format stream-json --output-format stream-json \
//!   --verbose --permission-prompt-tool stdio [<session rules>] \
//!   (--session-id <uuid> | --resume <uuid> |
//!    --resume <parent> --fork-session --session-id <uuid>) <agent.args...>
//! ```
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
//! handshake; a CLI that fails before that is a start error, or a resume
//! error when it does not know the session resumed or forked. The mode,
//! model and effort the [`StartSpec`] names are then asked for, in that
//! order, with control requests, like any later change
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
//! A CLI that stops reading stdin for the write timeout is killed and
//! reported as an `Exited` error, so a wedged process cannot block its
//! session.

pub mod display;
pub mod protocol;
pub mod rules;
pub mod usage;

use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use skywave_proto::{Decision, SessionId};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout, timeout_at};
use tracing::Instrument;
use uuid::Uuid;

use self::protocol::{Output, Protocol};
use super::{
    AdapterEvent, AdapterEvents, AgentAdapter, AgentError, AgentRequestId, AgentSession,
    Conversation, Effort, PermissionMode, StartSpec,
};
use crate::config::AgentConfig;
use crate::process::{self, DEFAULT_TERM_GRACE, Group};

/// Flags every CLI process gets, before the session flag and `agent.args`.
pub const REQUIRED_ARGS: [&str; 8] = [
    "-p",
    "--input-format",
    "stream-json",
    "--output-format",
    "stream-json",
    "--verbose",
    "--permission-prompt-tool",
    "stdio",
];

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
/// or without `--fork-session` (docs/agents/claude-code.md §5).
const UNKNOWN_SESSION: &str = "No conversation found";

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
    /// flags, the flags granting `session_rules`, the session's flags
    /// ([`CliSession::args`]), then `agent.args`.
    pub fn command_args(&self, session: CliSession, session_rules: &[Value]) -> Vec<String> {
        let mut args: Vec<String> = REQUIRED_ARGS.iter().map(|a| (*a).to_owned()).collect();
        args.extend(rules::cli_args(session_rules));
        args.extend(session.args());
        args.extend(self.config.args.iter().cloned());
        args
    }

    fn spawn(
        &self,
        workdir: &Path,
        session: CliSession,
        session_rules: &[Value],
    ) -> Result<Child, AgentError> {
        let mut command = Command::new(&self.config.command);
        command
            .args(self.command_args(session, session_rules))
            .current_dir(workdir)
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
    fn handle_stored(self) -> bool {
        matches!(self, CliSession::Resume(_))
    }

    /// The process takes up a conversation that must exist, so the CLI not
    /// knowing it is a resume error.
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
        let span = tracing::info_span!("agent", session = %spec.session_id.0);
        let child = self.spawn(&spec.workdir, session, &session_rules)?;
        let mut process = Process::new(child, &span, self.write_timeout, self.term_grace)?;
        let mut protocol =
            Protocol::new(session.id(), session.handle_stored()).with_session_rules(session_rules);

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
                    AgentError::Resume(message)
                } else {
                    AgentError::Start(message)
                },
            );
        }

        // Start-time requests are asked like any other: their lines go to
        // the CLI and any answer known at once joins the early events.
        let mut wanted = Vec::new();
        let mut asked = Vec::new();
        if let Some(mode) = spec.mode {
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
        match process.stdout.next_line().await {
            Ok(Some(line)) => out = protocol.on_line(&line),
            Ok(None) | Err(_) => {
                let status = process.wait_exit().await;
                return Err(process.error_text(protocol, status).await);
            }
        }
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
    async fn send_message(&mut self, text: String) -> Result<(), AgentError> {
        self.request(Request::Message(text))
    }

    async fn answer_approval(
        &mut self,
        request: AgentRequestId,
        decision: Decision,
        rule: Option<u8>,
    ) -> Result<(), AgentError> {
        self.request(Request::Answer(request, decision, rule))
    }

    async fn interrupt(&mut self) -> Result<(), AgentError> {
        self.request(Request::Interrupt)
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
    Message(String),
    Answer(AgentRequestId, Decision, Option<u8>),
    Interrupt,
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
    stdout: Lines<BufReader<ChildStdout>>,
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
            stdout: BufReader::new(stdout).lines(),
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

    async fn write_lines(&mut self, lines: &[Value]) -> std::io::Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "stdin closed"))?;
        for line in lines {
            let mut bytes = serde_json::to_vec(line)?;
            bytes.push(b'\n');
            stdin.write_all(&bytes).await?;
        }
        stdin.flush().await
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
    /// The CLI is reaped if it exits in time; until then its group id
    /// cannot be reused.
    async fn kill(&mut self) {
        self.terminate();
        let _ = timeout_at(self.kill_deadline(), async {
            let _ = self.child.wait().await;
            while !self.group.is_empty() {
                tokio::time::sleep(GROUP_POLL).await;
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

    /// The error text for a process that stopped with `status`.
    async fn error_text(
        &mut self,
        protocol: &Protocol,
        status: Option<std::process::ExitStatus>,
    ) -> String {
        self.drain_stderr().await;
        let stderr = self
            .stderr_tail
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if self.wedged {
            return "agent stopped reading its input; killed it".to_owned();
        }
        protocol
            .exit_error()
            .map(str::to_owned)
            .or(stderr)
            .unwrap_or_else(|| match status {
                Some(status) => format!("agent exited ({status})"),
                None => "agent exited".to_owned(),
            })
    }
}

/// Logs the agent's stderr lines and keeps the last one.
async fn read_stderr(stderr: ChildStderr, tail: Arc<Mutex<Option<String>>>) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
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
                line = self.process.stdout.next_line() => match line {
                    Ok(Some(line)) => {
                        let out = self.protocol.on_line(&line);
                        if !self.apply(out).await {
                            return self.abandon().await;
                        }
                    }
                    Ok(None) | Err(_) => break self.exited().await,
                },
                request = self.requests.recv() => {
                    let out = match request {
                        Some(Request::Message(text)) => self.protocol.send_message(text),
                        Some(Request::Answer(request, decision, rule)) => {
                            self.protocol.answer(&request, decision, rule)
                        }
                        Some(Request::Interrupt) => self.protocol.interrupt(),
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
        let _ = self.events.send(AdapterEvent::Exited { error }).await;
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
    async fn exited(&mut self) -> Option<String> {
        let status = match timeout(self.grace_period, self.process.wait_exit()).await {
            Ok(status) => status,
            Err(_) => {
                self.process.kill().await;
                None
            }
        };
        let clean = status.is_some_and(|s| s.success());
        if clean && self.protocol.exit_error().is_none() {
            self.process.drain_stderr().await;
            return None;
        }
        Some(self.process.error_text(&self.protocol, status).await)
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
            match self.process.stdout.next_line().await {
                Ok(Some(line)) => {
                    let out = self.protocol.on_line(&line);
                    self.apply(out).await;
                }
                Ok(None) | Err(_) => return,
            }
        }
    }
}
