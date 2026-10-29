//! The Claude Code adapter: drives the `claude` CLI in stream-json mode
//! (docs/agents/claude-code.md, spec §5).
//!
//! This module is the only place that names the agent (D3). One CLI process
//! runs per agent session, in the session's workdir (D14), started as
//!
//! ```text
//! <agent.command> -p --input-format stream-json --output-format stream-json \
//!   --verbose --permission-prompt-tool stdio \
//!   (--session-id <uuid> | --resume <uuid>) <agent.args...>
//! ```
//!
//! A fresh session gets a new UUID as `--session-id`, reported at once as the
//! [`ResumeHandle`] `{"session_id": "<uuid>"}`; a restart passes it back with
//! `--resume`. [`AgentAdapter::start`] returns after the CLI answers the
//! initialize handshake; a CLI that fails before that is a start error, or a
//! resume error when it does not know the session.
//!
//! The protocol translation lives in [`protocol::Protocol`], which does no
//! I/O; this module runs the process around it. Requests return once queued
//! for the process, like the fake agent's; their effects arrive as events.
//! stderr lines go to the daemon log.
//!
//! **Shutdown.** Closing stdin does not stop a running turn, so
//! [`AgentSession::shutdown`] interrupts a running turn and waits up to the
//! grace period for its `result`, then closes stdin and waits up to the
//! grace period for the process to exit before killing it. The event stream
//! ends with `Exited`.

pub mod display;
pub mod protocol;

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
use tokio::time::timeout;
use tracing::Instrument;
use uuid::Uuid;

use self::protocol::{Output, Protocol};
use super::{
    AdapterEvent, AdapterEvents, AgentAdapter, AgentError, AgentRequestId, AgentSession, StartSpec,
};
use crate::config::AgentConfig;

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

/// Capacity of a session's event channel.
const EVENT_CAPACITY: usize = 256;
/// How long to wait for the rest of stderr once the process has exited.
const STDERR_DRAIN: Duration = Duration::from_secs(1);
/// What the CLI says when `--resume` names a session it does not have.
const UNKNOWN_SESSION: &str = "No conversation found";

/// Starts Claude Code CLI sessions with the configured command.
#[derive(Debug, Clone)]
pub struct ClaudeCodeAdapter {
    config: AgentConfig,
    grace_period: Duration,
    startup_timeout: Duration,
}

impl ClaudeCodeAdapter {
    pub fn new(config: AgentConfig) -> ClaudeCodeAdapter {
        ClaudeCodeAdapter {
            config,
            grace_period: DEFAULT_GRACE_PERIOD,
            startup_timeout: DEFAULT_STARTUP_TIMEOUT,
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

    /// The CLI arguments for session `session_id`: the required flags,
    /// `--session-id` (or `--resume` when `resumed`), then `agent.args`.
    pub fn command_args(&self, session_id: Uuid, resumed: bool) -> Vec<String> {
        let mut args: Vec<String> = REQUIRED_ARGS.iter().map(|a| (*a).to_owned()).collect();
        args.push(if resumed { "--resume" } else { "--session-id" }.to_owned());
        args.push(session_id.to_string());
        args.extend(self.config.args.iter().cloned());
        args
    }

    fn spawn(&self, workdir: &Path, session_id: Uuid, resumed: bool) -> Result<Child, AgentError> {
        Command::new(&self.config.command)
            .args(self.command_args(session_id, resumed))
            .current_dir(workdir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                AgentError::Start(format!(
                    "cannot run {} in {}: {e}",
                    self.config.command.display(),
                    workdir.display()
                ))
            })
    }
}

impl AgentAdapter for ClaudeCodeAdapter {
    type Session = ClaudeCodeSession;

    async fn start(
        &self,
        spec: StartSpec,
    ) -> Result<(ClaudeCodeSession, AdapterEvents), AgentError> {
        let (agent_session, resumed) = match &spec.resume {
            Some(handle) => (
                protocol::parse_resume_handle(handle).map_err(AgentError::Resume)?,
                true,
            ),
            None => (Uuid::new_v4(), false),
        };
        let span = tracing::info_span!("agent", session = %spec.session_id.0);
        let mut process = Process::new(self.spawn(&spec.workdir, agent_session, resumed)?, &span)?;
        let mut protocol = Protocol::new(agent_session, resumed);

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
            return Err(if resumed && message.contains(UNKNOWN_SESSION) {
                AgentError::Resume(message)
            } else {
                AgentError::Start(message)
            });
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
    ) -> Result<(), AgentError> {
        self.request(Request::Answer(request, decision))
    }

    async fn interrupt(&mut self) -> Result<(), AgentError> {
        self.request(Request::Interrupt)
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
    Answer(AgentRequestId, Decision),
    Interrupt,
    Shutdown,
}

/// The CLI process and its pipes.
struct Process {
    child: Child,
    /// `None` once closed.
    stdin: Option<ChildStdin>,
    stdout: Lines<BufReader<ChildStdout>>,
    stderr_task: Option<JoinHandle<()>>,
    /// The last non-blank stderr line.
    stderr_tail: Arc<Mutex<Option<String>>>,
}

impl Process {
    fn new(mut child: Child, span: &tracing::Span) -> Result<Process, AgentError> {
        let missing = || AgentError::Start("agent pipes not available".to_owned());
        let stdin = child.stdin.take().ok_or_else(missing)?;
        let stdout = child.stdout.take().ok_or_else(missing)?;
        let stderr = child.stderr.take().ok_or_else(missing)?;
        let stderr_tail = Arc::new(Mutex::new(None));
        let stderr_task =
            tokio::spawn(read_stderr(stderr, Arc::clone(&stderr_tail)).instrument(span.clone()));
        Ok(Process {
            child,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout).lines(),
            stderr_task: Some(stderr_task),
            stderr_tail,
        })
    }

    /// Writes `lines` to stdin, one JSON object per line.
    async fn write(&mut self, lines: &[Value]) -> std::io::Result<()> {
        if lines.is_empty() {
            return Ok(());
        }
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

    /// Waits for the process to exit; `None` if waiting failed.
    async fn wait_exit(&mut self) -> Option<std::process::ExitStatus> {
        self.stdin = None;
        self.child.wait().await.ok()
    }

    /// Kills the process and reaps it.
    async fn kill(&mut self) {
        self.stdin = None;
        if let Err(e) = self.child.kill().await {
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
                        Some(Request::Answer(request, decision)) => {
                            self.protocol.answer(&request, decision)
                        }
                        Some(Request::Interrupt) => self.protocol.interrupt(),
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
        if self.protocol.turn_open() {
            let out = self.protocol.interrupt();
            self.apply(out).await;
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
            self.process.kill().await;
        } else {
            self.process.drain_stderr().await;
        }
        None
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
