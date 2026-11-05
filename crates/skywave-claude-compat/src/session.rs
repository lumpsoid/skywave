//! One real CLI process, driven through the adapter's own [`Protocol`].
//!
//! Every stdin line is one the adapter would write: the handshake, messages,
//! approvals and requests all come from [`Protocol`], and every stdout line
//! goes through [`Protocol::on_message`], so a scenario checks the
//! [`AdapterEvent`]s skywaved would report as well as the raw lines. Only
//! requests the adapter does not send (to see whether the CLI changed) are
//! written raw, with [`Session::control`].
//!
//! Waiting is bounded, but a bound only turns a hang into a report: each
//! wait ends on a definitive line (a response's request id, a `result`, a
//! task id), never on a quiet spell.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use skywave_proto::Decision;
use skywaved::agent::claude_code::protocol::{Output, Protocol};
use skywaved::agent::claude_code::transcripts::{project_dir, projects_dir};
use skywaved::agent::claude_code::{CliSession, REQUIRED_ARGS};
use skywaved::agent::{AdapterEvent, TurnOutcome};
use tempfile::TempDir;
use uuid::Uuid;

use crate::report::Stop;
use crate::shape::{self, Shape, clip};

/// The flags every fixture was recorded with, after [`REQUIRED_ARGS`] and
/// before the session's flags ([`CliSession::args`]): a cheap model, and
/// none of the user's settings, hooks or MCP servers.
const FIXTURE_ARGS: [&str; 5] = [
    "--model",
    "haiku",
    "--setting-sources",
    "project",
    "--strict-mcp-config",
];

/// How long the handshake may take.
const STARTUP: Duration = Duration::from_secs(60);
/// How long a closed stdin may take to end the process.
const EXIT: Duration = Duration::from_secs(20);

/// What every process of a scenario adds up to: the recording, and what
/// the shape check found.
#[derive(Debug, Default)]
pub struct Recorder {
    /// Fixture records, in the order they happened.
    pub records: Vec<Value>,
    /// Lines with a field the adapter reads that no longer decodes.
    pub broken: Vec<String>,
    pub new_kinds: BTreeSet<String>,
}

pub type Shared = Rc<RefCell<Recorder>>;

/// How to start a process.
#[derive(Debug, Default)]
pub struct Setup {
    /// Flags after the fixture flags (in place of `--setting-sources
    /// project` if they name `--setting-sources` again: the CLI takes the
    /// last).
    pub args: Vec<String>,
    /// Files to create in the scratch workdir: path, content.
    pub files: Vec<(&'static str, &'static str)>,
    /// The conversation the process takes up; a new one by default.
    pub session: Option<CliSession>,
    /// The workdir, shared with another process of the scenario (a resume
    /// must run where its conversation did); a fresh one by default.
    pub workdir: Option<Rc<Workdir>>,
    /// Changes the adapter's `initialize` request before it is written: to
    /// record what the CLI does with a handshake skywaved no longer sends.
    pub initialize: Option<fn(&mut Value)>,
}

/// A scratch workdir. Dropped, it takes the CLI's project dir for it (the
/// transcripts of its conversations) along: nothing could resume them once
/// the dir is gone.
#[derive(Debug)]
pub struct Workdir {
    root: TempDir,
    /// The dir the CLI runs in, canonical: the `cwd` its transcripts carry.
    cwd: PathBuf,
}

impl Workdir {
    /// A fresh scratch dir; the CLI runs in its `subdir` when one is given.
    pub fn new(subdir: Option<&str>) -> Result<Rc<Workdir>, Stop> {
        let root = tempfile::Builder::new()
            .prefix("skywave-bakery-")
            .tempdir()
            .map_err(|e| Stop::Stalled(format!("cannot make a scratch dir: {e}")))?;
        let cwd = match subdir {
            Some(sub) => root.path().join(sub),
            None => root.path().to_owned(),
        };
        std::fs::create_dir_all(&cwd)
            .and_then(|()| std::fs::canonicalize(&cwd))
            .map(|cwd| Rc::new(Workdir { root, cwd }))
            .map_err(|e| Stop::Stalled(format!("cannot make a scratch dir: {e}")))
    }

    /// The scratch dir itself, the root a listing looks under.
    pub fn root(&self) -> &Path {
        self.root.path()
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Where the CLI keeps this workdir's transcripts, as the adapter
    /// reckons it ([`project_dir`]).
    pub fn project_dir(&self) -> Option<PathBuf> {
        Some(projects_dir()?.join(project_dir(self.cwd.to_str()?)))
    }
}

impl Drop for Workdir {
    fn drop(&mut self) {
        // The dir name holds the scratch dir's random suffix, so it is this
        // workdir's alone.
        if let Some(dir) = self.project_dir() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// One stdout message and the events the adapter made of it.
#[derive(Debug)]
pub struct Seen {
    pub msg: Value,
    pub events: Vec<AdapterEvent>,
}

impl Seen {
    /// `type`, or `system/<subtype>`.
    pub fn is(&self, kind: &str) -> bool {
        let field = |key| self.msg.get(key).and_then(Value::as_str).unwrap_or("");
        match kind.split_once('/') {
            Some(("system", subtype)) => field("type") == "system" && field("subtype") == subtype,
            _ => field("type") == kind,
        }
    }

    pub fn str(&self, pointer: &str) -> Option<&str> {
        self.msg.pointer(pointer).and_then(Value::as_str)
    }

    /// A `control_request` of `subtype`.
    pub fn is_request(&self, subtype: &str) -> bool {
        self.is("control_request") && self.str("/request/subtype") == Some(subtype)
    }
}

enum Line {
    Out(String),
    Err(String),
    Closed,
}

enum Pumped {
    Seen(usize),
    Other,
    Timeout,
    Closed,
}

pub struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<Line>,
    stdout_closed: bool,
    /// The last stderr line, for [`Protocol::exit_error`].
    stderr_tail: Option<String>,
    pub protocol: Protocol,
    /// Every stdout message so far.
    pub seen: Vec<Seen>,
    /// Allow every approval as it comes (as the old drivers did); off, a
    /// scenario answers them itself.
    pub auto_approve: bool,
    recorder: Shared,
    t0: Instant,
    next_raw: u64,
    workdir: Rc<Workdir>,
}

impl Session {
    /// Starts `claude` in the setup's workdir and completes the handshake.
    pub fn start(claude: &str, recorder: Shared, setup: Setup) -> Result<Session, Stop> {
        Session::start_or_refusal(claude, recorder, setup)?
            .map_err(|why| Stop::Stalled(format!("the CLI refused to start: {why}")))
    }

    /// Starts `claude` as [`Session::start`] does; `Ok(Err)` when the CLI
    /// fails before the handshake ends, with the error text the adapter's
    /// start would get (an error `result`, or the exit's).
    pub fn start_or_refusal(
        claude: &str,
        recorder: Shared,
        setup: Setup,
    ) -> Result<Result<Session, String>, Stop> {
        let workdir = match setup.workdir {
            Some(workdir) => workdir,
            None => Workdir::new(None)?,
        };
        for (path, content) in &setup.files {
            let path = workdir.cwd().join(path);
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(|e| Stop::Stalled(e.to_string()))?;
            }
            std::fs::write(&path, content).map_err(|e| Stop::Stalled(e.to_string()))?;
        }
        let session = setup
            .session
            .unwrap_or_else(|| CliSession::New(Uuid::new_v4()));
        let mut args: Vec<String> = REQUIRED_ARGS.iter().map(|a| (*a).to_owned()).collect();
        args.extend(FIXTURE_ARGS.iter().map(|a| (*a).to_owned()));
        args.extend(session.args());
        args.extend(setup.args);

        let mut command = Command::new(claude);
        command
            .args(&args)
            .current_dir(workdir.cwd())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let mut child = command
            .spawn()
            .map_err(|e| Stop::Stalled(format!("cannot start {claude}: {e}")))?;

        let (tx, lines) = mpsc::channel();
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let out_tx = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if out_tx.send(Line::Out(line)).is_err() {
                    return;
                }
            }
            let _ = out_tx.send(Line::Closed);
        });
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if tx.send(Line::Err(line)).is_err() {
                    return;
                }
            }
        });

        let mut session = Session {
            stdin: child.stdin.take(),
            child,
            lines,
            stdout_closed: false,
            stderr_tail: None,
            protocol: Protocol::new(session.id(), session.handle_stored()),
            seen: Vec::new(),
            auto_approve: true,
            recorder,
            t0: Instant::now(),
            next_raw: 0,
            workdir,
        };
        session.note(&format!("argv: claude {}", args.join(" ")));
        let mut start = session.protocol.start();
        if let Some(change) = setup.initialize {
            start.writes.iter_mut().for_each(change);
        }
        session.apply(start);
        let deadline = Instant::now() + STARTUP;
        while !session.protocol.ready() {
            if let Some(error) = session.protocol.startup_error() {
                return Ok(Err(error.to_owned()));
            }
            match session.pump(deadline) {
                Pumped::Timeout => {
                    return Err(Stop::Stalled(format!(
                        "no initialize answer within {}s",
                        STARTUP.as_secs()
                    )));
                }
                Pumped::Closed => return Ok(Err(session.exit_error())),
                Pumped::Seen(_) | Pumped::Other => {}
            }
        }
        Ok(Ok(session))
    }

    /// The dir the CLI runs in.
    pub fn workdir(&self) -> &Path {
        self.workdir.cwd()
    }

    /// A driver remark in the recording.
    pub fn note(&mut self, text: &str) {
        let text = format!("t+{}s {text}", self.t0.elapsed().as_secs());
        self.record(json!({ "dir": "note", "text": text }));
    }

    /// The index the next stdout message will get: wait from here to see
    /// only what comes after a step.
    pub fn mark(&self) -> usize {
        self.seen.len()
    }

    /// Writes what a protocol step asked for.
    pub fn apply(&mut self, out: Output) {
        for line in out.writes {
            self.write(line);
        }
    }

    /// Sends a user message, as the adapter does.
    pub fn send(&mut self, text: &str) {
        let out = self.protocol.send_message(text.to_owned());
        self.apply(out);
    }

    /// Writes `line` as is: a line the adapter does not send.
    pub fn raw(&mut self, line: Value) {
        self.write(line);
    }

    /// Closes the CLI's stdin, as a stopping skywaved does.
    pub fn close_stdin(&mut self) {
        self.note("closing stdin");
        self.stdin = None;
    }

    /// Waits until stdout closes, up to `secs`.
    pub fn wait_closed(&mut self, secs: u64) -> Result<(), Stop> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            match self.pump(deadline) {
                Pumped::Closed => return Ok(()),
                Pumped::Timeout => {
                    return Err(Stop::Stalled(format!("stdout still open after {secs}s")));
                }
                Pumped::Seen(_) | Pumped::Other => {}
            }
        }
    }

    /// Writes a control request the adapter does not send; its request id.
    pub fn control(&mut self, request: Value) -> String {
        self.next_raw += 1;
        let id = format!("compat_{}", self.next_raw);
        self.write(json!({ "type": "control_request", "request_id": id, "request": request }));
        id
    }

    /// The `response` object answering request `id`.
    pub fn response(&mut self, id: &str, secs: u64) -> Result<Value, Stop> {
        let at = self.wait(&format!("answer to request {id}"), secs, 0, |s| {
            s.is("control_response") && s.str("/response/request_id") == Some(id)
        })?;
        Ok(self.seen[at].msg["response"].clone())
    }

    /// Waits for the first message from index `from` on that `found`
    /// accepts; its index.
    pub fn wait(
        &mut self,
        what: &str,
        secs: u64,
        from: usize,
        mut found: impl FnMut(&Seen) -> bool,
    ) -> Result<usize, Stop> {
        if let Some(at) = self.seen.iter().skip(from).position(&mut found) {
            return Ok(from + at);
        }
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            match self.pump(deadline) {
                Pumped::Seen(at) if found(&self.seen[at]) => return Ok(at),
                Pumped::Seen(_) | Pumped::Other => {}
                Pumped::Timeout => {
                    self.note(&format!("gave up waiting for {what}"));
                    return Err(Stop::Stalled(format!("no {what} within {secs}s")));
                }
                Pumped::Closed => {
                    return Err(Stop::Stalled(format!(
                        "the CLI closed stdout before {what}"
                    )));
                }
            }
        }
    }

    /// Waits for the first event from index `from` on that `pick` maps.
    pub fn wait_event<T>(
        &mut self,
        what: &str,
        secs: u64,
        from: usize,
        pick: impl Fn(&AdapterEvent) -> Option<T>,
    ) -> Result<(usize, T), Stop> {
        let at = self.wait(what, secs, from, |s| {
            s.events.iter().any(|e| pick(e).is_some())
        })?;
        let value = self.seen[at]
            .events
            .iter()
            .find_map(&pick)
            .expect("the message matched");
        Ok((at, value))
    }

    /// Waits for the next `TurnEnded` from index `from` on.
    pub fn turn_end(&mut self, from: usize, secs: u64) -> Result<(usize, TurnOutcome), Stop> {
        self.wait_event("end of the turn", secs, from, |e| match e {
            AdapterEvent::TurnEnded(outcome) => Some(outcome.clone()),
            _ => None,
        })
    }

    /// The first event from index `from` on that `pick` maps, with its
    /// message's index.
    pub fn find_event<T>(
        &self,
        from: usize,
        pick: impl Fn(&AdapterEvent) -> Option<T>,
    ) -> Option<(usize, T)> {
        self.seen
            .iter()
            .enumerate()
            .skip(from)
            .find_map(|(at, s)| s.events.iter().find_map(&pick).map(|v| (at, v)))
    }

    /// The first message from index `from` on that `found` accepts.
    pub fn find(&self, from: usize, found: impl Fn(&Seen) -> bool) -> Option<usize> {
        self.seen
            .iter()
            .skip(from)
            .position(found)
            .map(|at| from + at)
    }

    /// Why the CLI ended, once stdout has closed ([`Protocol::exit_error`]).
    ///
    /// Reads the rest of stderr until both pipes have closed (up to
    /// [`EXIT`]), so its last line is known, then takes the status if the
    /// process has already exited; a process still running gives no status,
    /// which `exit_error` reports as not known.
    fn exit_error(&mut self) -> String {
        self.stdin = None;
        let deadline = Instant::now() + EXIT;
        loop {
            let wait = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(wait) {
                Ok(Line::Err(text)) => self.on_stderr(text),
                Ok(Line::Out(_) | Line::Closed) => {}
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
            }
        }
        let status = self.child.try_wait().ok().flatten();
        self.protocol
            .exit_error(self.stderr_tail.as_deref(), status)
            .unwrap_or_else(|| "it exited cleanly".to_owned())
    }

    /// Records a stderr line and keeps it as the tail if it says anything.
    fn on_stderr(&mut self, text: String) {
        self.note(&format!("stderr: {text}"));
        if !text.trim().is_empty() {
            self.stderr_tail = Some(text);
        }
    }

    fn record(&mut self, record: Value) {
        self.recorder.borrow_mut().records.push(record);
    }

    fn write(&mut self, line: Value) {
        self.record(json!({ "dir": "in", "msg": line }));
        let Some(stdin) = self.stdin.as_mut() else {
            return;
        };
        let mut text = line.to_string();
        text.push('\n');
        if let Err(e) = stdin
            .write_all(text.as_bytes())
            .and_then(|()| stdin.flush())
        {
            self.note(&format!("stdin write failed: {e}"));
        }
    }

    /// Reads one line, up to `deadline`, and handles it as the adapter
    /// would.
    fn pump(&mut self, deadline: Instant) -> Pumped {
        if self.stdout_closed {
            return Pumped::Closed;
        }
        let wait = deadline.saturating_duration_since(Instant::now());
        let line = match self.lines.recv_timeout(wait) {
            Ok(line) => line,
            Err(RecvTimeoutError::Timeout) => return Pumped::Timeout,
            Err(RecvTimeoutError::Disconnected) => Line::Closed,
        };
        let text = match line {
            Line::Out(text) => text,
            Line::Err(text) => {
                self.on_stderr(text);
                return Pumped::Other;
            }
            Line::Closed => {
                self.stdout_closed = true;
                self.note("stdout closed");
                return Pumped::Closed;
            }
        };
        if text.trim().is_empty() {
            return Pumped::Other;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&text) else {
            self.note(&format!("non-JSON out: {text}"));
            let mut recorder = self.recorder.borrow_mut();
            recorder
                .broken
                .push(format!("a non-JSON line: {}", clip(&text)));
            return Pumped::Other;
        };
        self.record(json!({ "dir": "out", "msg": msg }));
        match shape::check(&msg) {
            Shape::Fits => {}
            Shape::New(kind) => {
                self.recorder.borrow_mut().new_kinds.insert(kind);
            }
            Shape::Broken(why) => self.recorder.borrow_mut().broken.push(why),
        }
        if msg["type"] == "result" {
            self.note("result");
        }
        let out = self.protocol.on_message(&msg);
        let events = out.events.clone();
        self.apply(out);
        if self.auto_approve {
            for event in &events {
                if let AdapterEvent::ApprovalRequested { request, .. } = event {
                    let answer = self.protocol.answer(request, Decision::Approve, None);
                    self.apply(answer);
                }
            }
        }
        self.seen.push(Seen { msg, events });
        Pumped::Seen(self.seen.len() - 1)
    }
}

impl Drop for Session {
    /// Closes stdin, lets the CLI end on its own for a while, then kills
    /// its whole process group (the background shells too).
    fn drop(&mut self) {
        self.note("closing stdin");
        self.stdin = None;
        let deadline = Instant::now() + EXIT;
        while !matches!(self.pump(deadline), Pumped::Closed | Pumped::Timeout) {}
        #[cfg(unix)]
        if let Ok(pid) = i32::try_from(self.child.id()) {
            // SAFETY: signals the group this process leads; no memory is
            // shared.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
        #[cfg(not(unix))]
        let _ = self.child.kill();
        let status = self
            .child
            .wait()
            .map_or_else(|e| e.to_string(), |s| s.to_string());
        self.note(&status);
    }
}
