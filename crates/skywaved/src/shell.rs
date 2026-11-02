//! The shell runner: runs the shell commands devices send (`RunShell`) in
//! the daemon, one at a time per session (D35, architecture.md "Server").
//!
//! A sibling of the agent supervisor, not part of it: shell runs share
//! nothing with agent sessions and keep running across `StopSession`,
//! context changes and idle shutdowns. The same pattern, though: its own
//! thread with a current-thread runtime and its own [`Storage`], following
//! the log with [`EventLog::subscribe`] and [`EventLog::read`]. Every shell
//! is spawned from that thread, which lives as long as the daemon:
//! `PR_SET_PDEATHSIG` fires when the *spawning thread* exits.
//!
//! **Queue.** One FIFO per session in log order of `ShellRequested`; at most
//! one run executes per session, sessions run in parallel. A stop for a
//! queued run finishes it `Stopped` without starting it; for the running
//! run it ends the run's process group.
//!
//! **The last status, like `$?`.** A run's `when` is matched against the
//! session's last status ([`EventLog::shell_status`]): set by the newest
//! finished run that was not skipped, from any device; only `Exited 0` is
//! success, and a session with no run counts as success. A run whose `when`
//! does not match is finished `Skipped` without a `ShellStarted`. The
//! command text goes to the shell verbatim; nothing parses it.
//!
//! **At most once, no cursor to persist.** `ShellStarted` is logged (with
//! [`EventLog::shell_event`], which refuses to start a run a stop already
//! names) *before* the shell is spawned. On start the runner reads every
//! unfinished run and the head in one transaction
//! ([`EventLog::unfinished_shell_runs`]): a run that started died with the
//! daemon and is finished `Lost` (a crash between `ShellStarted` and the
//! spawn reads the same, never as a second run); one that never started and
//! has a stop is finished `Stopped`; the rest are still queued and go back
//! into their session's FIFO, or, with the shell disabled, are finished
//! `NotStarted`. An enabled runner then follows the log from that head with
//! an in-memory cursor. A disabled one exits once it has settled them, so no
//! device shows them queued or running forever.
//!
//! **Spawning.** `<program> -c <wrapper> skywave <command> [<rc>]`: the
//! wrapper takes the command from `$1` and shifts it away, sources the rc
//! file if one is configured, runs `eval "$command" 3>&-` in the shell
//! itself (so a `cd` sticks; fd 3 closed, so `cmd &` children cannot hold
//! it), and writes `pwd -P` to fd 3 before it exits with the command's
//! status. stdin is `/dev/null`; stdout and stderr share one pipe, so the
//! output keeps its order. The shell calls `setsid` (no controlling
//! terminal, so a prompt that opens `/dev/tty` fails instead of hanging the
//! run; it also leads its own process group) and gets SIGTERM when its
//! parent dies. Off Linux the daemon is that parent: the shell takes
//! [`die_with_daemon`](crate::process::die_with_daemon) (it cannot also use
//! `isolate`, see [`crate::process`]). On Linux the parent is the run's
//! host, below. A run starts in the session's last
//! known cwd, or its workdir when that is gone; `ShellStarted::cwd` says
//! which. `exit` or `exec` in the command skips the `pwd`: the run's `cwd` is
//! `None` and the session keeps the one before.
//!
//! **The end.** A run is over when the shell exits, not at the output's EOF
//! (a `cmd &` child would hold the pipe open forever). The runner then
//! SIGTERMs the group, SIGKILLs what is left after
//! [`DEFAULT_TERM_GRACE`](crate::process::DEFAULT_TERM_GRACE), takes what the
//! output pipe already holds without waiting for EOF, and logs
//! `ShellFinished`. A process that left the group (`setsid`, a daemonizing
//! tool) survives the run and is not waited for. A stop does the same kill
//! first and ends the run `Stopped` whatever the shell's status.
//!
//! **The run host (Linux).** A `cmd &` child outlives its shell for a
//! moment, and the group is empty only once it is reaped: a zombie still
//! counts. An orphan goes to the nearest subreaper, else PID 1, which may
//! never reap it (a container without `--init`, CI as PID 1); the run would
//! then wait out the grace and leave the zombie. So on Linux every run's
//! shell starts under a host of its own: `skywaved` re-executed
//! ([`ShellConfig::host`]) with [`RUN_HOST`] as its first argument. The host
//! makes itself a subreaper (`PR_SET_CHILD_SUBREAPER`), starts the shell and
//! reaps every child it has with `waitpid(-1)`, which is safe because the
//! host owns nothing else; the daemon itself is no subreaper. The host leads
//! a group of its own (`isolate`), outside the shell's, so the runner's
//! SIGTERM and SIGKILL of the run's group never reach it and it is still
//! there to reap what they kill. It reports on a pipe ([`host`]): the
//! shell's pid, which names the group the runner signals; the shell's raw
//! wait status once it has reaped it; and, by the pipe's EOF at its exit,
//! that the group is empty, believed only when the host's exit is clean. A
//! host that dies (SIGKILL, OOM) leaves the run `Lost` unless stopped, and
//! the runner waits for the group itself as off Linux. The host is the
//! running binary through `/proc/self/exe`, which still execs after the
//! binary is replaced on disk. The host dies with the daemon, and the shell
//! with the host. Elsewhere the daemon spawns the shell itself and polls
//! for the empty group.
//!
//! **Output.** Written as it arrives to `<data_dir>/shell/<run>.out`, at most
//! [`ShellConfig::output_max`] bytes; the rest is discarded and the run is
//! `truncated`. At the end the file becomes a blob unless the preview holds
//! all of it, and is removed. The preview is the output's tail (ANSI
//! stripped, trailing newlines dropped, within the preview caps), and it and
//! the counts cover all output, also what the cap discarded. While the run
//! goes on, `ShellOutput` reports the tail at the [`Progress`] cadence, only
//! when there is new output.
//!
//! **Shutdown** ([`ShellHandle::shutdown`]) kills every running run's group
//! and logs nothing, so the next start settles them `Lost`, as after a
//! crash.
//!
//! **Failure.** An error while following the log (the database full or
//! gone, say) stops the runner the same way: it kills every running run's
//! group, logs nothing and ends with the error, which it logs.
//! [`ShellHandle::failed`] then returns, and `serve` stops the daemon on it
//! and exits non-zero: a daemon whose runner is gone would go on accepting
//! `RunShell` for runs nothing starts and showing killed runs `Running`.
//! The next start settles them from the log like any other.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use skywave_proto::limits::{PREVIEW_MAX_BYTES, PREVIEW_MAX_LINES};
use skywave_proto::{CommandId, DeviceId, Event, EventKind, RunWhen, Seq, SessionId, ShellEnd};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinSet;

use crate::error::{Error, Result};
use crate::event_log::{ShellAppended, ShellRecovery, ShellRun, shell_command_line, shell_run_in};
use crate::{EventLog, Storage};

/// Default `shell.program`.
pub const DEFAULT_PROGRAM: &str = "/bin/sh";
/// Default `shell.output_max`: 64 MiB.
pub const DEFAULT_OUTPUT_MAX: u64 = 64 << 20;
/// `NotStarted::reason` of a run left queued when the shell is disabled.
pub const DISABLED_REASON: &str = "shell disabled";

/// Events read from the log per batch.
const BATCH: u32 = 500;
/// Raw output bytes kept for the tail preview.
const TAIL_WINDOW: usize = 16 * 1024;

/// How often a finished run's group is checked for stragglers during the
/// grace.
#[cfg(unix)]
const GROUP_POLL: Duration = Duration::from_millis(10);

#[cfg(any(target_os = "linux", target_os = "android"))]
pub mod host;
#[cfg(any(target_os = "linux", target_os = "android"))]
pub use host::RUN_HOST;

/// The `[shell]` table, resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellConfig {
    /// Whether devices may run shell commands. Off by default.
    pub enabled: bool,
    /// The shell; must be POSIX-sh compatible.
    pub program: PathBuf,
    /// A file every run sources first, if any.
    pub rc: Option<PathBuf>,
    /// The most output bytes a run keeps.
    pub output_max: u64,
    /// When `ShellOutput` is logged; not in the config file.
    pub progress: Progress,
    /// The `skywaved` executable a run's host is (Linux only); `None` is the
    /// running one (`/proc/self/exe`, so replacing the binary on disk does
    /// not break runs). Not in the config file: a test process is not
    /// `skywaved`, so tests point it at the built daemon.
    pub host: Option<PathBuf>,
}

impl Default for ShellConfig {
    fn default() -> Self {
        ShellConfig {
            enabled: false,
            program: PathBuf::from(DEFAULT_PROGRAM),
            rc: None,
            output_max: DEFAULT_OUTPUT_MAX,
            progress: Progress::default(),
            host: None,
        }
    }
}

/// When a running shell's progress is checked: at each of `marks` after the
/// start, then every `every`. A check logs a `ShellOutput` only when there
/// is output it has not reported. Backs off because each one is kept in the
/// log and copied to every device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress {
    pub marks: Vec<Duration>,
    pub every: Duration,
}

impl Default for Progress {
    /// 2 s, 5 s, 10 s and 30 s after the start, then every 60 s.
    fn default() -> Self {
        Progress {
            marks: [2, 5, 10, 30].map(Duration::from_secs).to_vec(),
            every: Duration::from_secs(60),
        }
    }
}

impl Progress {
    /// When check `n` (from 0) is due, after the start.
    fn after(&self, n: usize) -> Duration {
        if let Some(mark) = self.marks.get(n) {
            return *mark;
        }
        let last = self.marks.last().copied().unwrap_or_default();
        let past = u32::try_from(n - self.marks.len() + 1).unwrap_or(u32::MAX);
        let every = self.every.max(Duration::from_millis(1));
        last.saturating_add(every.saturating_mul(past))
    }
}

/// A running shell runner. Dropping it stops the runner in the background;
/// [`ShellHandle::shutdown`] stops it and waits.
#[derive(Debug)]
pub struct ShellHandle {
    stop: Option<oneshot::Sender<()>>,
    done: oneshot::Receiver<Result<()>>,
    /// What the runner ended with, once [`ShellHandle::failed`] has seen it.
    ended: Option<Result<()>>,
}

impl ShellHandle {
    /// Kills every running run's process group, logging nothing, and waits
    /// until the runner has stopped. Returns the error it stopped on, if it
    /// failed earlier.
    pub async fn shutdown(mut self) -> Result<()> {
        drop(self.stop.take());
        self.done().await
    }

    /// Waits until the runner ends on its own, as a disabled one does once
    /// it has settled what an earlier run left.
    pub async fn join(mut self) -> Result<()> {
        self.done().await
    }

    /// Returns once the runner has stopped on an error (or panicked); never
    /// while it runs, nor after it ended cleanly as a disabled one does. Its
    /// running runs' groups are killed by then, logging nothing, as at a
    /// shutdown. [`ShellHandle::shutdown`] then returns the error. Cancel
    /// safe.
    pub async fn failed(&mut self) {
        if self.ended.is_none() {
            let result = Self::outcome(&mut self.done).await;
            self.ended = Some(result);
        }
        if matches!(self.ended, Some(Ok(()))) {
            std::future::pending::<()>().await;
        }
    }

    async fn done(&mut self) -> Result<()> {
        match self.ended.take() {
            Some(result) => result,
            None => Self::outcome(&mut self.done).await,
        }
    }

    async fn outcome(done: &mut oneshot::Receiver<Result<()>>) -> Result<()> {
        match done.await {
            Ok(result) => result,
            Err(_) => Err(Error::Shell("shell runner thread panicked".into())),
        }
    }
}

/// The shell runner; see the [module docs](self).
pub struct ShellRunner {
    log: EventLog,
    storage: Storage,
    config: ShellConfig,
    /// Seq of the last log event handled.
    cursor: Seq,
    queues: HashMap<SessionId, Queue>,
    running: HashMap<CommandId, Running>,
    reports: mpsc::UnboundedSender<Report>,
    reported: mpsc::UnboundedReceiver<Report>,
    tasks: JoinSet<()>,
}

/// One session's runs: the one running and those waiting for it.
#[derive(Default)]
struct Queue {
    running: Option<CommandId>,
    waiting: VecDeque<Queued>,
}

/// A run not started yet.
struct Queued {
    run: CommandId,
    when: RunWhen,
    command: String,
    by: DeviceId,
}

impl From<ShellRun> for Queued {
    fn from(run: ShellRun) -> Queued {
        Queued {
            run: run.run,
            when: run.when,
            command: run.command,
            by: run.by,
        }
    }
}

/// A run whose shell was spawned and has not been reported over.
struct Running {
    session: SessionId,
    by: DeviceId,
    command_line: String,
    started_at: Option<DateTime<Utc>>,
    control: watch::Sender<Control>,
}

/// What the runner asks of a running run's task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Control {
    Run,
    /// A `StopShell`: end the group, report `Stopped`.
    Stop,
    /// The runner is shutting down: kill the group, report nothing.
    Cancel,
}

/// What a running run's task tells the runner.
enum Report {
    Output {
        run: CommandId,
        tail: String,
        lines: u32,
        bytes: u64,
    },
    Done {
        run: CommandId,
        end: ShellEnd,
        cwd: Option<String>,
        output: Capture,
    },
}

enum Wake {
    Stop,
    Log,
    Report(Report),
}

impl ShellRunner {
    /// Starts the runner on its own thread over the data directory at
    /// `data_dir` (opened with [`Storage::open`]; the runner uses its own
    /// connection) and the process's shared `log`.
    ///
    /// Returns once what an earlier run left is settled (see the
    /// [module docs](self)). Enabled, the runner then follows the log until
    /// [`ShellHandle::shutdown`]; disabled, it ends there.
    pub async fn start(
        data_dir: impl Into<PathBuf>,
        log: EventLog,
        config: ShellConfig,
    ) -> Result<ShellHandle> {
        let data_dir = data_dir.into();
        let (ready_tx, ready_rx) = oneshot::channel();
        let (stop_tx, stop_rx) = oneshot::channel();
        let (done_tx, done_rx) = oneshot::channel();
        std::thread::Builder::new()
            .name("skywave-shell".into())
            .spawn(move || {
                let result = run_thread(data_dir, log, config, ready_tx, stop_rx);
                if let Err(error) = &result {
                    tracing::error!(%error, "shell runner stopped");
                }
                let _ = done_tx.send(result);
            })
            .map_err(|e| Error::Shell(format!("cannot spawn thread: {e}")))?;
        match ready_rx.await {
            Ok(Ok(())) => Ok(ShellHandle {
                stop: Some(stop_tx),
                done: done_rx,
                ended: None,
            }),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(Error::Shell("shell runner thread panicked".into())),
        }
    }

    /// Settles what the previous run left; see the module docs.
    fn recover(&mut self) -> Result<()> {
        remove_stale_output(&self.storage.dir.shell_dir());
        let ShellRecovery { head, runs } = self.log.unfinished_shell_runs(&self.storage.db)?;
        self.cursor = head;
        for run in runs {
            if run.started_seq.is_some() {
                let started_at = run.started_at;
                self.finish(unstarted(run.into(), ShellEnd::Lost, started_at))?;
            } else if run.stop_seq.is_some() {
                self.finish(unstarted(run.into(), ShellEnd::Stopped, None))?;
            } else if self.config.enabled {
                self.queues
                    .entry(run.session_id)
                    .or_default()
                    .waiting
                    .push_back(run.into());
            } else {
                let reason = DISABLED_REASON.to_owned();
                self.finish(unstarted(run.into(), ShellEnd::NotStarted { reason }, None))?;
            }
        }
        Ok(())
    }

    /// Follows the log until `stop` or an error, then kills every running
    /// run's group, as a shutdown does, and returns the error, if any.
    async fn run(mut self, stop: oneshot::Receiver<()>) -> Result<()> {
        let result = self.serve(stop).await;
        for running in self.running.values() {
            running.control.send_replace(Control::Cancel);
        }
        while self.tasks.join_next().await.is_some() {}
        result
    }

    async fn serve(&mut self, mut stop: oneshot::Receiver<()>) -> Result<()> {
        let sessions: Vec<SessionId> = self.queues.keys().copied().collect();
        for session in sessions {
            self.pump(session)?;
        }
        let mut head = self.log.subscribe();
        loop {
            head.borrow_and_update();
            if self.log.head() > self.cursor {
                self.follow()?;
                continue;
            }
            let wake = tokio::select! {
                biased;
                _ = &mut stop => Wake::Stop,
                Some(report) = self.reported.recv() => Wake::Report(report),
                _ = head.changed() => Wake::Log,
            };
            match wake {
                Wake::Stop => return Ok(()),
                Wake::Log => {}
                Wake::Report(report) => self.on_report(report)?,
            }
        }
    }

    /// Handles one batch of log events after the cursor.
    fn follow(&mut self) -> Result<()> {
        let batch = self.log.read(&self.storage.db, self.cursor, BATCH)?;
        let mut seq = batch.from;
        for event in batch.events {
            self.on_log_event(event)?;
            self.cursor = seq;
            seq = seq.next();
        }
        Ok(())
    }

    fn on_log_event(&mut self, event: Event) -> Result<()> {
        match event.kind {
            EventKind::ShellRequested { command, when, by } => {
                // The projection refuses a request with no command id.
                let Some(run) = event.command_id else {
                    return Ok(());
                };
                self.queues
                    .entry(event.session_id)
                    .or_default()
                    .waiting
                    .push_back(Queued {
                        run,
                        when,
                        command,
                        by,
                    });
                self.pump(event.session_id)
            }
            EventKind::ShellStopRequested { run, .. } => self.stop(event.session_id, run),
            _ => Ok(()),
        }
    }

    /// Stops run `run` of `session`: ends its shell if it runs, finishes it
    /// `Stopped` if it waits. A run that is over is left alone.
    fn stop(&mut self, session: SessionId, run: CommandId) -> Result<()> {
        if let Some(running) = self.running.get(&run) {
            running.control.send_replace(Control::Stop);
            return Ok(());
        }
        let Some(queue) = self.queues.get_mut(&session) else {
            return Ok(());
        };
        let Some(at) = queue.waiting.iter().position(|q| q.run == run) else {
            return Ok(());
        };
        let queued = queue.waiting.remove(at).expect("position is in range");
        self.finish(unstarted(queued, ShellEnd::Stopped, None))
    }

    /// Starts `session`'s next run if none is running, finishing every run
    /// before it that does not start.
    fn pump(&mut self, session: SessionId) -> Result<()> {
        loop {
            let Some(queue) = self.queues.get_mut(&session) else {
                return Ok(());
            };
            if queue.running.is_some() {
                return Ok(());
            }
            let Some(next) = queue.waiting.pop_front() else {
                self.queues.remove(&session);
                return Ok(());
            };
            let db = &self.storage.db;
            let status = self.log.shell_status(db, session)?;
            let matches = match next.when {
                RunWhen::Always => true,
                RunWhen::OnSuccess => status.succeeded(),
                RunWhen::OnFailure => !status.succeeded(),
            };
            if !matches {
                self.finish(unstarted(next, ShellEnd::Skipped, None))?;
                continue;
            }
            let Some(info) = self.log.session(db, session)? else {
                return Err(Error::Corrupt(format!(
                    "shell run {} of unknown session {}",
                    next.run.0, session.0
                )));
            };
            let cwd = status
                .cwd
                .filter(|cwd| Path::new(cwd).is_dir())
                .unwrap_or(info.workdir);
            let started = EventKind::ShellStarted {
                run: next.run,
                cwd: cwd.clone(),
            };
            if let ShellAppended::StoppedInstead(_) =
                self.log.shell_event(&mut self.storage.db, started)?
            {
                continue;
            }
            let started_at =
                shell_run_in(self.storage.db.conn(), next.run)?.and_then(|row| row.run.started_at);
            let (control, controlled) = watch::channel(Control::Run);
            let spawned = exec::spawn(
                Launch {
                    run: next.run,
                    command: &next.command,
                    cwd: Path::new(&cwd),
                    output: self
                        .storage
                        .dir
                        .shell_dir()
                        .join(format!("{}.out", next.run.0)),
                    config: &self.config,
                    control: controlled,
                    reports: self.reports.clone(),
                },
                &mut self.tasks,
            );
            match spawned {
                Ok(()) => {
                    self.running.insert(
                        next.run,
                        Running {
                            session,
                            by: next.by,
                            command_line: shell_command_line(&next.command),
                            started_at,
                            control,
                        },
                    );
                    if let Some(queue) = self.queues.get_mut(&session) {
                        queue.running = Some(next.run);
                    }
                    return Ok(());
                }
                Err(error) => {
                    tracing::warn!(run = %next.run.0, %error, "cannot start shell");
                    let reason = error.to_string();
                    self.finish(unstarted(next, ShellEnd::NotStarted { reason }, started_at))?;
                }
            }
        }
    }

    fn on_report(&mut self, report: Report) -> Result<()> {
        match report {
            Report::Output {
                run,
                tail,
                lines,
                bytes,
            } => {
                if self.running.contains_key(&run) {
                    self.log.shell_event(
                        &mut self.storage.db,
                        EventKind::ShellOutput {
                            run,
                            tail,
                            lines,
                            bytes,
                        },
                    )?;
                }
                Ok(())
            }
            Report::Done {
                run,
                end,
                cwd,
                output,
            } => {
                let Some(running) = self.running.remove(&run) else {
                    return Ok(());
                };
                let preview = output.tail();
                let mut truncated = output.truncated;
                let blob = if output.holds_all(&preview) {
                    None
                } else {
                    match self.put_output(&output.path) {
                        Ok(blob) => Some(blob),
                        Err(error) => {
                            tracing::error!(run = %run.0, %error, "cannot keep shell output");
                            truncated = true;
                            None
                        }
                    }
                };
                remove_file(&output.path);
                self.finish(EventKind::ShellFinished {
                    run,
                    by: running.by,
                    started_at: running.started_at,
                    command_line: running.command_line,
                    end,
                    cwd,
                    preview,
                    lines: output.lines(),
                    output: blob,
                    truncated,
                })?;
                if let Some(queue) = self.queues.get_mut(&running.session) {
                    queue.running = None;
                }
                self.pump(running.session)
            }
        }
    }

    fn put_output(&self, path: &Path) -> Result<skywave_proto::BlobRef> {
        let file = std::fs::File::open(path).map_err(|source| Error::Io {
            context: format!("opening {}", path.display()),
            source,
        })?;
        self.storage.put_blob(file)
    }

    /// Logs `ShellFinished` `kind`.
    fn finish(&mut self, kind: EventKind) -> Result<()> {
        self.log.shell_event(&mut self.storage.db, kind)?;
        Ok(())
    }
}

/// `ShellFinished` for a run that ran no shell (or whose shell the runner
/// lost track of), with no output.
fn unstarted(run: Queued, end: ShellEnd, started_at: Option<DateTime<Utc>>) -> EventKind {
    EventKind::ShellFinished {
        run: run.run,
        by: run.by,
        started_at,
        command_line: shell_command_line(&run.command),
        end,
        cwd: None,
        preview: String::new(),
        lines: 0,
        output: None,
        truncated: false,
    }
}

/// Deletes the output files a previous run left in `dir`.
fn remove_stale_output(dir: &Path) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(dir = %dir.display(), %error, "cannot list shell output");
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "out") {
            remove_file(&path);
        }
    }
}

fn remove_file(path: &Path) {
    if let Err(error) = std::fs::remove_file(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(path = %path.display(), %error, "cannot remove shell output");
    }
}

/// What a run's shell needs to start.
struct Launch<'a> {
    run: CommandId,
    command: &'a str,
    cwd: &'a Path,
    /// The output file, `<data_dir>/shell/<run>.out`.
    output: PathBuf,
    config: &'a ShellConfig,
    control: watch::Receiver<Control>,
    reports: mpsc::UnboundedSender<Report>,
}

/// A run's output as it arrives: written to its file up to the cap, counted,
/// and its last bytes kept for the preview.
struct Capture {
    path: PathBuf,
    /// `None` once a write failed.
    file: Option<std::fs::File>,
    max: u64,
    kept: u64,
    bytes: u64,
    newlines: u64,
    ends_in_newline: bool,
    tail: Vec<u8>,
    truncated: bool,
}

impl Capture {
    fn create(path: PathBuf, max: u64) -> std::io::Result<Capture> {
        let file = std::fs::File::create(&path)?;
        Ok(Capture {
            path,
            file: Some(file),
            max,
            kept: 0,
            bytes: 0,
            newlines: 0,
            ends_in_newline: false,
            tail: Vec::new(),
            truncated: false,
        })
    }

    fn push(&mut self, data: &[u8]) {
        use std::io::Write as _;
        if data.is_empty() {
            return;
        }
        self.bytes += data.len() as u64;
        self.newlines += data.iter().filter(|b| **b == b'\n').count() as u64;
        self.ends_in_newline = data.last() == Some(&b'\n');
        let room = usize::try_from(self.max - self.kept).unwrap_or(usize::MAX);
        let keep = &data[..data.len().min(room)];
        if keep.len() < data.len() {
            self.truncated = true;
        }
        if let Some(file) = &mut self.file
            && !keep.is_empty()
        {
            match file.write_all(keep) {
                Ok(()) => self.kept += keep.len() as u64,
                Err(error) => {
                    tracing::warn!(path = %self.path.display(), %error, "cannot write shell output");
                    self.file = None;
                    self.truncated = true;
                }
            }
        }
        self.tail.extend_from_slice(data);
        if self.tail.len() > 2 * TAIL_WINDOW {
            self.tail.drain(..self.tail.len() - TAIL_WINDOW);
        }
    }

    /// Whether `tail` holds every byte of the output.
    fn whole(&self) -> bool {
        self.bytes == self.tail.len() as u64
    }

    /// The total line count: a last line without a newline counts too.
    fn lines(&self) -> u32 {
        let partial = u64::from(self.bytes > 0 && !self.ends_in_newline);
        u32::try_from(self.newlines + partial).unwrap_or(u32::MAX)
    }

    /// The kept bytes as text, from a line start when the window was cut.
    fn tail_text(&self) -> String {
        let mut raw = &self.tail[..];
        if !self.whole()
            && let Some(newline) = raw.iter().position(|b| *b == b'\n')
        {
            raw = &raw[newline + 1..];
        }
        skywave_proto::strip_ansi(&String::from_utf8_lossy(raw))
    }

    /// The preview: the tail within the preview caps.
    fn tail(&self) -> String {
        preview_tail(&self.tail_text())
    }

    /// Whether `preview` holds the whole output, so no blob is needed: the
    /// raw bytes, trailing newlines aside, are valid UTF-8 and equal it. Any
    /// escape code, `\r` overwrite or invalid byte the preview changed is
    /// kept only by the blob.
    fn holds_all(&self, preview: &str) -> bool {
        !self.truncated
            && self.whole()
            && std::str::from_utf8(&self.tail)
                .is_ok_and(|raw| raw.trim_end_matches('\n') == preview)
    }
}

/// The last [`PREVIEW_MAX_LINES`] lines of `text` within [`PREVIEW_MAX_BYTES`]
/// bytes, cut on a char boundary; trailing newlines are not kept.
fn preview_tail(text: &str) -> String {
    let text = text.trim_end_matches('\n');
    let mut start = text
        .rmatch_indices('\n')
        .nth(PREVIEW_MAX_LINES - 1)
        .map_or(0, |(at, _)| at + 1);
    if text.len() - start > PREVIEW_MAX_BYTES {
        start = text.len() - PREVIEW_MAX_BYTES;
        while !text.is_char_boundary(start) {
            start += 1;
        }
    }
    text[start..].to_owned()
}

fn run_thread(
    data_dir: PathBuf,
    log: EventLog,
    config: ShellConfig,
    ready: oneshot::Sender<Result<()>>,
    stop: oneshot::Receiver<()>,
) -> Result<()> {
    let setup = || -> Result<_> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| Error::Shell(format!("cannot build runtime: {e}")))?;
        let (reports, reported) = mpsc::unbounded_channel();
        let mut runner = ShellRunner {
            log,
            storage: Storage::open(data_dir)?,
            config,
            cursor: Seq::ZERO,
            queues: HashMap::new(),
            running: HashMap::new(),
            reports,
            reported,
            tasks: JoinSet::new(),
        };
        runner.recover()?;
        Ok((runtime, runner))
    };
    let (runtime, runner) = match setup() {
        Ok(parts) => {
            let _ = ready.send(Ok(()));
            parts
        }
        Err(error) => {
            let message = error.to_string();
            let _ = ready.send(Err(error));
            return Err(Error::Shell(message));
        }
    };
    if !runner.config.enabled {
        return Ok(());
    }
    runtime.block_on(runner.run(stop))
}

/// Starting a shell and watching it until it is over.
#[cfg(unix)]
mod exec {
    use std::ffi::OsString;
    use std::io;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    use std::io::Read;
    use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
    use std::os::unix::process::ExitStatusExt;
    use std::path::Path;
    use std::process::{ExitStatus, Stdio};

    use skywave_proto::ShellEnd;
    use tokio::io::AsyncReadExt;
    use tokio::net::unix::pipe;
    use tokio::process::{Child, Command};
    use tokio::sync::watch;
    use tokio::task::JoinSet;
    use tokio::time::Instant;

    use super::{Capture, Control, Launch, Report};
    #[cfg(any(target_os = "linux", target_os = "android"))]
    use crate::process::isolate;
    use crate::process::{DEFAULT_TERM_GRACE, Group};

    /// The script `sh -c` runs; see the module docs. `$1` is the command,
    /// `$2` the rc file if any.
    const WRAPPER: &str = r#"__skywave_command=$1
shift
if [ "$#" -gt 0 ]; then
  __skywave_rc=$1
  shift
  . "$__skywave_rc" 3>&-
  unset __skywave_rc
fi
eval "$__skywave_command" 3>&-
__skywave_status=$?
pwd -P >&3
exit "$__skywave_status"
"#;

    /// `$0` of the wrapper.
    const NAME: &str = "skywave";

    /// What every run gets on top of the daemon's environment.
    const ENV: [(&str, &str); 7] = [
        ("TERM", "dumb"),
        ("NO_COLOR", "1"),
        ("PAGER", "cat"),
        ("GIT_PAGER", "cat"),
        ("CI", "1"),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("SKYWAVE_SHELL", "1"),
    ];

    /// Spawns the shell for `launch` from the calling thread, and the task
    /// that watches it into `tasks`. Fails, with nothing left running, when
    /// the shell cannot be started.
    pub(super) fn spawn(launch: Launch<'_>, tasks: &mut JoinSet<()>) -> io::Result<()> {
        let (output, output_w) = io::pipe()?;
        let (cwd, cwd_w) = io::pipe()?;
        let output = pipe::Receiver::from_owned_fd(OwnedFd::from(output))?;
        let cwd = pipe::Receiver::from_owned_fd(OwnedFd::from(cwd))?;
        let args = shell_args(launch.command, launch.config.rc.as_deref());
        let capture = Capture::create(launch.output, launch.config.output_max)?;
        // The write ends go in: kept here, EOF would never come.
        let spawned = Shell::spawn(
            launch.config,
            &args,
            launch.cwd,
            OwnedFd::from(output_w),
            OwnedFd::from(cwd_w),
        );
        let shell = match spawned {
            Ok(shell) => shell,
            Err(error) => {
                super::remove_file(&capture.path);
                return Err(error);
            }
        };
        tasks.spawn(watch_run(
            Watched {
                run: launch.run,
                shell,
                output,
                cwd,
                capture,
                progress: launch.config.progress.clone(),
            },
            launch.control,
            launch.reports,
        ));
        Ok(())
    }

    /// The arguments `<program>` gets; see the module docs.
    fn shell_args(command: &str, rc: Option<&Path>) -> Vec<OsString> {
        let mut args: Vec<OsString> =
            vec!["-c".into(), WRAPPER.into(), NAME.into(), command.into()];
        args.extend(rc.map(OsString::from));
        args
    }

    /// A run's shell as the runner sees it, started under its host
    /// ([`super::host`]): it learns the shell's end and the group's from the
    /// host's report pipe.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    struct Shell {
        /// Not killed on drop: the host must outlive what the runner kills
        /// to reap it, and exits by itself once the group is empty.
        host: Child,
        report: pipe::Receiver,
        /// Read from `report` and not yet parsed.
        pending: Vec<u8>,
        pgid: i32,
        /// The shell's status once known; `None` inside when the host ended
        /// without one.
        ended: Option<Option<ExitStatus>>,
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    impl Shell {
        /// Starts the host, which starts the shell, and waits for its first
        /// report line: the shell's pid, or why it cannot start. It takes a
        /// few milliseconds, and an error comes back as a spawn error does.
        fn spawn(
            config: &super::ShellConfig,
            args: &[OsString],
            cwd: &Path,
            output: OwnedFd,
            cwd_pipe: OwnedFd,
        ) -> io::Result<Shell> {
            let program = match &config.host {
                Some(host) => host.clone(),
                // Not `current_exe`, which reads this link: once the binary
                // is replaced on disk it names "<path> (deleted)". The link
                // itself still execs the running binary.
                None => std::path::PathBuf::from("/proc/self/exe"),
            };
            let (report, report_w) = io::pipe()?;
            let fds = [output.as_raw_fd(), cwd_pipe.as_raw_fd()];
            let mut command = Command::new(program);
            command
                .arg(super::RUN_HOST)
                .arg(fds[0].to_string())
                .arg(fds[1].to_string())
                .arg(&config.program)
                .args(args)
                .current_dir(cwd)
                .envs(ENV)
                .stdin(Stdio::null())
                .stdout(report_w)
                .stderr(Stdio::null())
                .kill_on_drop(false);
            // SAFETY: the closure runs in the forked child before exec and
            // only calls fcntl, which is async-signal-safe, and builds errors
            // without allocating. `io::pipe` makes every end CLOEXEC; the
            // host keeps the two its arguments name.
            unsafe {
                command.pre_exec(move || {
                    for fd in fds {
                        if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                            return Err(io::Error::last_os_error());
                        }
                    }
                    Ok(())
                });
            }
            // A group of its own, outside the shell's session, so no signal
            // to the run's group reaches the host.
            isolate(&mut command);
            let host = command.spawn()?;
            drop(command);
            drop(output);
            drop(cwd_pipe);
            let mut report = report;
            let mut pending = Vec::new();
            let line = loop {
                if let Some(line) = take_line(&mut pending) {
                    break line;
                }
                let mut chunk = [0; 256];
                match report.read(&mut chunk)? {
                    0 => return Err(io::Error::other("the run host ended at its start")),
                    n => pending.extend_from_slice(&chunk[..n]),
                }
            };
            let line = String::from_utf8_lossy(&line);
            if let Some(error) = line.strip_prefix("error ") {
                return Err(io::Error::other(error.to_owned()));
            }
            let pgid = line
                .strip_prefix("pid ")
                .and_then(|pid| pid.parse().ok())
                .ok_or_else(|| io::Error::other(format!("bad run host report: {line}")))?;
            Ok(Shell {
                host,
                report: pipe::Receiver::from_owned_fd(OwnedFd::from(report))?,
                pending,
                pgid,
                ended: None,
            })
        }

        /// The group the shell leads (`setsid`): its pid is the group's id.
        fn pgid(&self) -> Option<i32> {
            Some(self.pgid)
        }

        fn group(&self) -> Group {
            Group::led_by(self.pgid)
        }

        /// The shell's status, once the host has reaped it. Cancel-safe, and
        /// answers again once known.
        async fn exited(&mut self) -> io::Result<ExitStatus> {
            loop {
                if let Some(ended) = self.ended {
                    return ended.ok_or_else(|| io::Error::other("the run host ended early"));
                }
                if let Some(line) = take_line(&mut self.pending) {
                    let status = String::from_utf8_lossy(&line)
                        .strip_prefix("status ")
                        .and_then(|raw| raw.parse().ok())
                        .map(ExitStatus::from_raw);
                    self.ended = Some(status);
                    continue;
                }
                let mut chunk = [0; 256];
                match self.report.read(&mut chunk).await {
                    Ok(0) | Err(_) => self.ended = Some(None),
                    Ok(n) => self.pending.extend_from_slice(&chunk[..n]),
                }
            }
        }

        /// Returns once the group is empty. Call it after [`Shell::exited`].
        /// The host exits cleanly only then, which closes its report pipe.
        /// But a host that dies (SIGKILL, OOM) closes it too, with the group
        /// maybe still full and nobody to reap it: only its clean exit is
        /// believed, otherwise `group` is polled like off Linux.
        async fn emptied(&mut self, group: &Group) {
            let mut chunk = [0; 256];
            while let Ok(1..) = self.report.read(&mut chunk).await {}
            match self.host.wait().await {
                Ok(status) if status.success() => {}
                ended => {
                    tracing::warn!(?ended, "a shell run's host died");
                    poll_empty(group).await;
                }
            }
        }
    }

    /// A run's shell spawned by the runner itself, off Linux.
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    struct Shell {
        child: Child,
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    impl Shell {
        fn spawn(
            config: &super::ShellConfig,
            args: &[OsString],
            cwd: &Path,
            output: OwnedFd,
            cwd_pipe: OwnedFd,
        ) -> io::Result<Shell> {
            let mut command = Command::new(&config.program);
            command
                .args(args)
                .current_dir(cwd)
                .envs(ENV)
                .stdin(Stdio::null())
                .stdout(output.try_clone()?)
                .stderr(output)
                .kill_on_drop(true);
            let cwd_fd = cwd_pipe.as_raw_fd();
            // SAFETY: the closure runs in the forked child before exec and
            // only calls setsid, dup2 and fcntl, which are async-signal-safe,
            // and builds errors without allocating. `io::pipe` makes both
            // ends CLOEXEC, so the child keeps none of them but its fds 1, 2
            // and 3.
            unsafe {
                command.pre_exec(move || {
                    // A new session, not just a group: no controlling terminal.
                    if libc::setsid() == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    // dup2 onto itself would keep CLOEXEC set.
                    let moved = if cwd_fd == 3 {
                        libc::fcntl(3, libc::F_SETFD, 0)
                    } else {
                        libc::dup2(cwd_fd, 3)
                    };
                    if moved == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            crate::process::die_with_daemon(&mut command);
            let child = command.spawn()?;
            drop(command);
            drop(cwd_pipe);
            Ok(Shell { child })
        }

        /// The group the shell leads (`setsid`): its pid is the group's id.
        fn pgid(&self) -> Option<i32> {
            self.child.id().and_then(|id| i32::try_from(id).ok())
        }

        fn group(&self) -> Group {
            Group::of(&self.child)
        }

        /// The shell's status. Cancel-safe, and answers again once known.
        async fn exited(&mut self) -> io::Result<ExitStatus> {
            self.child.wait().await
        }

        /// Returns once `group` is empty. Call it after [`Shell::exited`]:
        /// until then the shell itself holds the group.
        async fn emptied(&mut self, group: &Group) {
            poll_empty(group).await;
        }
    }

    /// Returns once nothing is left in `group`; the shell must be reaped.
    async fn poll_empty(group: &Group) {
        while !group.is_empty() {
            tokio::time::sleep(super::GROUP_POLL).await;
        }
    }

    /// Takes the first whole line out of `buf`, without its newline.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn take_line(buf: &mut Vec<u8>) -> Option<Vec<u8>> {
        let end = buf.iter().position(|&byte| byte == b'\n')?;
        let line = buf[..end].to_vec();
        buf.drain(..=end);
        Some(line)
    }

    struct Watched {
        run: skywave_proto::CommandId,
        shell: Shell,
        output: pipe::Receiver,
        cwd: pipe::Receiver,
        capture: Capture,
        progress: super::Progress,
    }

    enum Wake {
        Control(Control),
        /// The shell exited; [`end_group`] reaps its status again.
        Exited,
        Read(io::Result<usize>),
        Tick,
    }

    /// Follows one run until its shell exits or it is stopped, then ends
    /// its group and reports it `Done`. Reports nothing when cancelled.
    async fn watch_run(
        mut w: Watched,
        mut control: watch::Receiver<Control>,
        reports: tokio::sync::mpsc::UnboundedSender<Report>,
    ) {
        let mut group = w.shell.group();
        let pgid = w.shell.pgid();
        let start = Instant::now();
        let mut ticks = 0;
        let mut reported = 0;
        let mut buf = vec![0; 64 * 1024];
        let mut output_open = true;
        let (status, stopped) = loop {
            let due = start + w.progress.after(ticks);
            let wake = tokio::select! {
                biased;
                changed = control.changed() => Wake::Control(match changed {
                    Ok(()) => *control.borrow_and_update(),
                    Err(_) => Control::Cancel,
                }),
                _ = w.shell.exited() => Wake::Exited,
                read = w.output.read(&mut buf), if output_open => Wake::Read(read),
                () = tokio::time::sleep_until(due) => Wake::Tick,
            };
            match wake {
                Wake::Control(Control::Run) => {}
                Wake::Control(Control::Stop) => {
                    break (end_group(&mut w.shell, &mut group).await, true);
                }
                Wake::Control(Control::Cancel) => {
                    kill_group(pgid, &mut w.shell, &mut group).await;
                    return;
                }
                Wake::Exited => break (end_group(&mut w.shell, &mut group).await, false),
                Wake::Read(Ok(0) | Err(_)) => output_open = false,
                Wake::Read(Ok(n)) => w.capture.push(&buf[..n]),
                Wake::Tick => {
                    ticks += 1;
                    if w.capture.bytes != reported {
                        reported = w.capture.bytes;
                        let _ = reports.send(Report::Output {
                            run: w.run,
                            tail: w.capture.tail(),
                            lines: w.capture.lines(),
                            bytes: w.capture.bytes,
                        });
                    }
                }
            }
        };
        // What the group wrote before it ended; a process that left the
        // group may hold the pipe open, so no waiting for EOF.
        if output_open {
            drain(w.output.as_fd(), &mut buf, |data| w.capture.push(data));
        }
        let mut cwd = Vec::new();
        drain(w.cwd.as_fd(), &mut buf, |data| cwd.extend_from_slice(data));
        let end = if stopped {
            ShellEnd::Stopped
        } else {
            match status {
                Ok(status) => match (status.code(), status.signal()) {
                    (Some(code), _) => ShellEnd::Exited { code },
                    (None, Some(signal)) => ShellEnd::Signalled { signal },
                    (None, None) => ShellEnd::Lost,
                },
                Err(error) => {
                    tracing::warn!(run = %w.run.0, %error, "cannot wait for shell");
                    ShellEnd::Lost
                }
            }
        };
        let cwd = cwd
            .strip_suffix(b"\n")
            .and_then(|path| String::from_utf8(path.to_vec()).ok())
            .filter(|path| !path.is_empty());
        let _ = reports.send(Report::Done {
            run: w.run,
            end,
            cwd,
            output: w.capture,
        });
    }

    /// SIGTERMs the group, waits for the shell, and SIGKILLs whatever is
    /// left in the group after the grace.
    async fn end_group(shell: &mut Shell, group: &mut Group) -> io::Result<ExitStatus> {
        group.terminate();
        let deadline = Instant::now() + DEFAULT_TERM_GRACE;
        let status = match tokio::time::timeout_at(deadline, shell.exited()).await {
            Ok(status) => status,
            Err(_) => {
                group.kill();
                shell.exited().await
            }
        };
        let _ = tokio::time::timeout_at(deadline, shell.emptied(group)).await;
        group.kill();
        status
    }

    /// For a shutdown: SIGKILLs the group at once, waits for the shell, and
    /// waits until the group is empty, up to the grace, so nothing of the
    /// run is left when the runner has stopped.
    async fn kill_group(pgid: Option<i32>, shell: &mut Shell, group: &mut Group) {
        let deadline = Instant::now() + DEFAULT_TERM_GRACE;
        // Not `Group::kill`, which forgets the group: it is polled below.
        if let Some(pgid) = pgid {
            crate::process::signal(pgid, libc::SIGKILL);
        }
        let _ = shell.exited().await;
        let _ = tokio::time::timeout_at(deadline, shell.emptied(group)).await;
        group.kill();
    }

    /// Reads what `fd` (non-blocking) holds at entry, into `sink`: at most
    /// the bytes `FIONREAD` reports then (one buffer if it cannot say),
    /// stopping early when it would block or reaches EOF. Bounded, since a
    /// process that left the group may write without end, and this runs on
    /// the runner's only thread.
    fn drain(fd: BorrowedFd<'_>, buf: &mut [u8], mut sink: impl FnMut(&[u8])) {
        let mut queued: libc::c_int = 0;
        // SAFETY: FIONREAD writes one c_int to `queued`.
        let asked = unsafe { libc::ioctl(fd.as_raw_fd(), libc::FIONREAD, &mut queued) };
        let mut left = if asked == -1 {
            buf.len()
        } else {
            usize::try_from(queued).unwrap_or(0)
        };
        while left > 0 {
            let want = left.min(buf.len());
            // SAFETY: reads at most `want <= buf.len()` bytes into `buf`.
            let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), want) };
            match usize::try_from(n) {
                Ok(0) => return,
                Ok(n) => {
                    sink(&buf[..n]);
                    left -= n;
                }
                Err(_) if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return,
            }
        }
    }
}

/// Shells need Unix; elsewhere every run that would start is `NotStarted`.
#[cfg(not(unix))]
mod exec {
    use tokio::task::JoinSet;

    use super::Launch;

    pub(super) fn spawn(launch: Launch<'_>, _: &mut JoinSet<()>) -> std::io::Result<()> {
        let _ = launch;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "shell commands need a Unix server",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_backs_off_then_repeats() {
        let progress = Progress::default();
        let at: Vec<u64> = (0..7).map(|n| progress.after(n).as_secs()).collect();
        assert_eq!(at, [2, 5, 10, 30, 90, 150, 210]);
        let tight = Progress {
            marks: vec![],
            every: Duration::from_millis(10),
        };
        assert_eq!(tight.after(0), Duration::from_millis(10));
        assert_eq!(tight.after(2), Duration::from_millis(30));
    }

    #[test]
    fn the_preview_is_the_tail_within_the_caps() {
        assert_eq!(preview_tail(""), "");
        assert_eq!(preview_tail("hi\n\n"), "hi");
        let lines: String = (1..=12).map(|n| format!("{n}\n")).collect();
        assert_eq!(preview_tail(&lines), "5\n6\n7\n8\n9\n10\n11\n12");
        let long = "é".repeat(PREVIEW_MAX_BYTES);
        let tail = preview_tail(&format!("x{long}"));
        assert_eq!(
            tail,
            "é".repeat(PREVIEW_MAX_BYTES / 2),
            "cut on a char boundary"
        );
    }
}
