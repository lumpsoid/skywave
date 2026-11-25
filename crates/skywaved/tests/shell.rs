//! The shell runner with the real `/bin/sh`: commands go through
//! `EventLog::handle` as the HTTPS API would send them. Runs that must wait
//! are held by fifos in the test's own directory, and every wait is driven
//! by the log's notifier with a bound, never by a sleep.
#![cfg(unix)]

use std::ffi::CString;
use std::io::Read;
use std::os::fd::{FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use skywave_proto::{
    BlobRef, Command, CommandId, CommandKind, CommandResult, DeviceId, Event, EventKind,
    RejectReason, RunWhen, Seq, SessionId, ShellEnd,
};
use skywaved::control::{Confirm, Control, Reply, Request, Stop, Work};
use skywaved::disk::{Disk, FreeSpace};
use skywaved::event_log::MAX_SEQ;
use skywaved::event_log::ShellStatus;
use skywaved::shell::{DISABLED_REASON, Progress, ShellConfig, ShellHandle, ShellRunner};
use skywaved::{Activity, EventLog, Storage};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::watch;

#[path = "support/control.rs"]
mod control;
use control::{TestClient, stop_after_work, stop_now};

/// Upper bound for any wait; a passing test never gets near it.
const WAIT: Duration = Duration::from_secs(20);

struct Fixture {
    tmp: TempDir,
    data: PathBuf,
    root: PathBuf,
    storage: Storage,
    log: EventLog,
    /// The first session, working in `root/project`.
    session: SessionId,
}

/// A run's `ShellFinished`, unpacked.
#[derive(Debug, Clone, PartialEq)]
struct Finished {
    by: DeviceId,
    started_at: Option<DateTime<Utc>>,
    command_line: String,
    end: ShellEnd,
    cwd: Option<String>,
    preview: String,
    lines: u32,
    output: Option<BlobRef>,
    truncated: bool,
}

impl Fixture {
    fn new() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let data = tmp.path().join("data");
        let storage = Storage::open(&data).unwrap();
        let log = EventLog::open(&storage.db, vec![root.clone()])
            .unwrap()
            .with_shell(true);
        let mut f = Fixture {
            tmp,
            data,
            root,
            storage,
            log,
            session: SessionId::new(),
        };
        f.session = f.create_session("project");
        f
    }

    /// The test's own scratch directory, for gates and pid files.
    fn dir(&self) -> &Path {
        self.tmp.path()
    }

    /// Canonical path of the first session's workdir.
    fn workdir(&self) -> String {
        self.workdir_of(self.session)
    }

    fn workdir_of(&self, session: SessionId) -> String {
        self.log
            .session(&self.storage.db, session)
            .unwrap()
            .unwrap()
            .workdir
    }

    fn create_session(&mut self, name: &str) -> SessionId {
        let workdir = self.root.join(name);
        std::fs::create_dir_all(&workdir).unwrap();
        let seq = accepted(self.handle(
            DeviceId::new(),
            CommandId::new(),
            CommandKind::CreateSession {
                workdir: workdir.to_str().unwrap().to_owned(),
                conversation: None,
                worktree: None,
                mode: None,
                safety_check: false,
            },
        ));
        self.log
            .read(&self.storage.db, Seq(seq.0 - 1), 1)
            .unwrap()
            .events[0]
            .session_id
    }

    /// A runner config for tests: enabled, progress all but off.
    fn config(&self) -> ShellConfig {
        ShellConfig {
            enabled: true,
            progress: Progress::every(Duration::from_secs(3600)),
            // This test binary is not `skywaved`; runs need it as their host.
            host: Some(PathBuf::from(env!("CARGO_BIN_EXE_skywaved"))),
            ..ShellConfig::default()
        }
    }

    async fn start(&self) -> ShellHandle {
        self.start_with(self.config()).await
    }

    async fn start_with(&self, config: ShellConfig) -> ShellHandle {
        ShellRunner::start(&self.data, self.log.clone(), config)
            .await
            .unwrap()
    }

    fn handle(&mut self, device: DeviceId, id: CommandId, kind: CommandKind) -> CommandResult {
        self.log
            .handle(&mut self.storage.db, device, &Command { id, kind })
            .unwrap()
    }

    /// Runs `command` in the first session, unconditionally.
    fn run(&mut self, command: &str) -> CommandId {
        self.run_in(self.session, RunWhen::Always, command)
    }

    fn run_in(&mut self, session: SessionId, when: RunWhen, command: &str) -> CommandId {
        self.run_as(DeviceId::new(), session, when, command)
    }

    fn run_as(
        &mut self,
        device: DeviceId,
        session: SessionId,
        when: RunWhen,
        command: &str,
    ) -> CommandId {
        let run = CommandId::new();
        accepted(self.handle(
            device,
            run,
            CommandKind::RunShell {
                session_id: session,
                command: command.into(),
                when,
            },
        ));
        run
    }

    fn stop(&mut self, session: SessionId, run: CommandId) -> Seq {
        accepted(self.handle(
            DeviceId::new(),
            CommandId::new(),
            CommandKind::StopShell {
                session_id: session,
                run,
            },
        ))
    }

    fn events(&self) -> Vec<Event> {
        self.log
            .read(&self.storage.db, Seq::ZERO, 5000)
            .unwrap()
            .events
    }

    /// Every event of run `run`, its `ShellRequested` first, with seqs.
    fn run_events(&self, run: CommandId) -> Vec<(Seq, Event)> {
        (1..)
            .map(Seq)
            .zip(self.events())
            .filter(|(_, e)| run_of(e) == Some(run))
            .collect()
    }

    fn run_kinds(&self, run: CommandId) -> Vec<EventKind> {
        self.run_events(run)
            .into_iter()
            .map(|(_, e)| e.kind)
            .collect()
    }

    /// Waits until the log satisfies `done`.
    async fn wait_for(&self, what: &str, done: impl Fn(&[Event]) -> bool) {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let head = self.log.head();
            let events = self.events();
            if done(&events) {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}; log: {events:#?}"
            );
            self.log.wait_after(head, WAIT).await;
        }
    }

    /// Waits for run `run`'s `ShellStarted` and returns its seq and cwd.
    async fn started(&self, run: CommandId) -> (Seq, String) {
        self.wait_for("ShellStarted", |events| {
            events.iter().any(|e| is_started(e, run))
        })
        .await;
        self.run_events(run)
            .into_iter()
            .find_map(|(seq, e)| match e.kind {
                EventKind::ShellStarted { cwd, .. } => Some((seq, cwd)),
                _ => None,
            })
            .unwrap()
    }

    /// Waits for run `run`'s `ShellFinished` and returns it.
    async fn finished(&self, run: CommandId) -> Finished {
        self.wait_for("ShellFinished", |events| {
            events.iter().any(|e| is_finished(e, run))
        })
        .await;
        self.finish_of(run).unwrap()
    }

    fn finish_of(&self, run: CommandId) -> Option<Finished> {
        self.run_kinds(run).into_iter().find_map(|kind| match kind {
            EventKind::ShellFinished {
                by,
                started_at,
                command_line,
                end,
                cwd,
                preview,
                lines,
                output,
                truncated,
                ..
            } => Some(Finished {
                by,
                started_at,
                command_line,
                end,
                cwd,
                preview,
                lines,
                output,
                truncated,
            }),
            _ => None,
        })
    }

    /// The `at` of run `run`'s `ShellStarted`.
    fn started_at(&self, run: CommandId) -> DateTime<Utc> {
        self.run_events(run)
            .into_iter()
            .find(|(_, e)| matches!(e.kind, EventKind::ShellStarted { .. }))
            .expect("the run started")
            .1
            .at
    }

    /// The `at` of run `run`'s `ShellFinished`.
    fn finished_at(&self, run: CommandId) -> DateTime<Utc> {
        self.run_events(run)
            .into_iter()
            .find(|(_, e)| is_finished(e, run))
            .expect("the run finished")
            .1
            .at
    }

    /// Whether run `run` has a `ShellStarted`.
    fn has_started(&self, run: CommandId) -> bool {
        self.events().iter().any(|e| is_started(e, run))
    }

    fn end(&self, run: CommandId) -> Option<ShellEnd> {
        self.finish_of(run).map(|f| f.end)
    }

    fn status(&self, session: SessionId) -> ShellStatus {
        self.log.shell_status(&self.storage.db, session).unwrap()
    }

    fn blob(&self, blob: &BlobRef) -> Vec<u8> {
        let mut content = Vec::new();
        self.storage
            .open_blob(blob.id)
            .unwrap()
            .expect("the blob exists")
            .read_to_end(&mut content)
            .unwrap();
        assert_eq!(
            blob.sha256,
            skywave_proto::ContentHash::of(&content),
            "the reference carries the stored content's hash"
        );
        content
    }

    /// A new fifo in the test's directory.
    fn fifo(&self, name: &str) -> PathBuf {
        let path = self.dir().join(name);
        let c = CString::new(path.to_str().unwrap()).unwrap();
        // SAFETY: `c` is a valid C string for the call.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0, "mkfifo");
        path
    }

    /// Logs what a daemon that crashed would have left: run `command`
    /// requested and, when `started`, its `ShellStarted`, with no runner.
    fn crashed_run(&mut self, command: &str, started: bool) -> CommandId {
        let run = self.run(command);
        if started {
            let cwd = self.workdir();
            self.log
                .shell_event(&mut self.storage.db, EventKind::ShellStarted { run, cwd })
                .unwrap();
        }
        run
    }

    /// Makes every append of an `event_type` event fail as a full disk would, for
    /// every connection to the database, until [`Fixture::disk_has_room`].
    fn disk_full_for(&self, event_type: &str) {
        self.storage
            .db
            .conn()
            .execute_batch(&format!(
                "CREATE TRIGGER disk_full BEFORE INSERT ON events
                 WHEN json_extract(NEW.kind, '$.type') = '{event_type}'
                 BEGIN SELECT RAISE(ABORT, 'database or disk is full'); END;"
            ))
            .unwrap();
    }

    fn disk_has_room(&self) {
        self.storage
            .db
            .conn()
            .execute_batch("DROP TRIGGER disk_full;")
            .unwrap();
    }

    /// Makes recording any blob fail as a full disk would, for every
    /// connection to the database, until [`Fixture::disk_has_room`].
    fn blobs_full(&self) {
        self.storage
            .db
            .conn()
            .execute_batch(
                "CREATE TRIGGER disk_full BEFORE INSERT ON blobs
                 BEGIN SELECT RAISE(ABORT, 'database or disk is full'); END;",
            )
            .unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            let violations = self.storage.db.context_stamp_violations().unwrap();
            assert!(violations.is_empty(), "context stamps: {violations:#?}");
        }
    }
}

fn accepted(result: CommandResult) -> Seq {
    match result {
        CommandResult::Accepted { seq } => seq,
        other => panic!("expected Accepted, got {other:?}"),
    }
}

fn rejected(result: CommandResult) -> RejectReason {
    match result {
        CommandResult::Rejected { reason } => reason,
        other => panic!("expected Rejected, got {other:?}"),
    }
}

fn run_of(event: &Event) -> Option<CommandId> {
    match &event.kind {
        EventKind::ShellRequested { .. } => event.command_id,
        EventKind::ShellStopRequested { run, .. }
        | EventKind::ShellStarted { run, .. }
        | EventKind::ShellOutput { run, .. }
        | EventKind::ShellFinished { run, .. } => Some(*run),
        _ => None,
    }
}

fn is_started(event: &Event, run: CommandId) -> bool {
    matches!(&event.kind, EventKind::ShellStarted { run: r, .. } if *r == run)
}

fn is_finished(event: &Event, run: CommandId) -> bool {
    matches!(&event.kind, EventKind::ShellFinished { run: r, .. } if *r == run)
}

/// Opens the fifo at `path` for writing and closes it: a `cat` on it sees
/// EOF and goes on.
async fn open_gate(path: &Path) {
    let path = path.to_owned();
    tokio::time::timeout(
        WAIT,
        tokio::task::spawn_blocking(move || std::fs::write(path, b"").unwrap()),
    )
    .await
    .expect("the run reads the gate")
    .unwrap();
}

/// Reads the fifo at `path` until its writer closes it.
async fn read_fifo(path: &Path) -> String {
    let path = path.to_owned();
    tokio::time::timeout(
        WAIT,
        tokio::task::spawn_blocking(move || std::fs::read_to_string(path).unwrap()),
    )
    .await
    .expect("the run writes the fifo")
    .unwrap()
}

/// Whether process group `pgid` is gone.
fn group_gone(pgid: i32) -> bool {
    // SAFETY: signal 0 only checks that the group exists.
    let failed = unsafe { libc::killpg(pgid, 0) } == -1;
    failed && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

/// Whether process `pid` is gone.
fn process_gone(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    let failed = unsafe { libc::kill(pid, 0) } == -1;
    failed && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

/// The parent of live process `pid`, from `/proc`.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn parent_of(pid: i32) -> i32 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    // `pid (comm) state ppid ...`; comm may hold spaces and parentheses.
    let (_, rest) = stat.rsplit_once(')').unwrap();
    rest.split_whitespace().nth(1).unwrap().parse().unwrap()
}

fn exited(code: i32) -> ShellEnd {
    ShellEnd::Exited { code }
}

// --- one run ------------------------------------------------------------

#[tokio::test]
async fn echo_runs_in_the_workdir_and_reports_its_output() {
    let mut f = Fixture::new();
    let runner = f.start().await;
    let device = DeviceId::new();
    let run = f.run_as(device, f.session, RunWhen::Always, "echo hi");
    let finished = f.finished(run).await;
    let (_, cwd) = f.started(run).await;
    assert_eq!(cwd, f.workdir());
    assert_eq!(
        finished,
        Finished {
            by: device,
            started_at: Some(f.started_at(run)),
            command_line: "echo hi".into(),
            end: exited(0),
            cwd: Some(f.workdir()),
            preview: "hi".into(),
            lines: 1,
            output: None,
            truncated: false,
        }
    );
    let kinds = f.run_kinds(run);
    assert_eq!(kinds.len(), 3, "requested, started, finished: {kinds:#?}");
    runner.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_spawn_really_starts_the_shell() {
    // `setsid` in a process that already leads a group fails with EPERM:
    // combined with a new process group, every run would be `NotStarted`.
    let mut f = Fixture::new();
    let runner = f.start().await;
    let run = f.run("echo started");
    let finished = f.finished(run).await;
    assert_eq!(finished.end, exited(0), "{finished:?}");
    assert_eq!(finished.preview, "started");
    runner.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_rc_file_is_sourced_before_every_run() {
    let mut f = Fixture::new();
    let rc = f.dir().join("rc");
    std::fs::write(&rc, "greet() { echo \"hello $1\"; }\n").unwrap();
    let runner = f
        .start_with(ShellConfig {
            rc: Some(rc),
            ..f.config()
        })
        .await;
    let run = f.run(r#"greet you; echo "[$1][$#]""#);
    let finished = f.finished(run).await;
    assert_eq!(finished.preview, "hello you\n[][0]", "{finished:?}");
    assert_eq!(finished.cwd, Some(f.workdir()));
    runner.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_shell_that_cannot_start_is_not_started() {
    let mut f = Fixture::new();
    let runner = f
        .start_with(ShellConfig {
            program: f.dir().join("no-such-shell"),
            ..f.config()
        })
        .await;
    let run = f.run("echo hi");
    let next = f.run("echo again");
    let finished = f.finished(run).await;
    let ShellEnd::NotStarted { reason } = &finished.end else {
        panic!("expected NotStarted, got {finished:?}");
    };
    assert!(!reason.is_empty());
    // It was logged as started before the spawn failed.
    assert_eq!(finished.started_at, Some(f.started_at(run)));
    // The queue goes on.
    assert!(matches!(
        f.finished(next).await.end,
        ShellEnd::NotStarted { .. }
    ));
    let left: Vec<_> = std::fs::read_dir(f.data.join("shell")).unwrap().collect();
    assert!(left.is_empty(), "no output file is left: {left:?}");
    runner.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_command_text_goes_to_the_shell_verbatim() {
    let mut f = Fixture::new();
    let runner = f.start().await;
    let run = f.run("false || echo y; echo 'a && b'");
    let finished = f.finished(run).await;
    assert_eq!(finished.end, exited(0));
    assert_eq!(finished.preview, "y\na && b");
    assert_eq!(finished.lines, 2);
    runner.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_command_does_not_see_itself_as_an_argument() {
    let mut f = Fixture::new();
    let runner = f.start().await;
    let run = f.run(r#"echo "[$1][$#]""#);
    assert_eq!(f.finished(run).await.preview, "[][0]");
    runner.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_shell_killed_by_a_signal_is_signalled() {
    let mut f = Fixture::new();
    let runner = f.start().await;
    let run = f.run("kill -USR1 $$");
    let finished = f.finished(run).await;
    assert_eq!(
        finished.end,
        ShellEnd::Signalled {
            signal: libc::SIGUSR1
        }
    );
    runner.shutdown().await.unwrap();
}

#[tokio::test]
async fn exit_keeps_its_code_and_skips_the_cwd() {
    let mut f = Fixture::new();
    let runner = f.start().await;
    let into = f.run("mkdir sub && cd sub");
    let sub = format!("{}/sub", f.workdir());
    assert_eq!(f.finished(into).await.cwd.as_deref(), Some(sub.as_str()));
    // It leaves `sub`, but exits before the cwd is taken.
    let exit = f.run("cd .. && exit 3");
    let finished = f.finished(exit).await;
    assert_eq!(finished.end, exited(3));
    assert_eq!(finished.cwd, None);
    // The next run starts where the session last was known to be.
    let pwd = f.run("pwd");
    let finished = f.finished(pwd).await;
    assert_eq!(f.started(pwd).await.1, sub);
    assert_eq!(finished.preview, sub);
    runner.shutdown().await.unwrap();
}

#[tokio::test]
async fn cd_sticks_for_the_next_run_and_falls_back_when_gone() {
    let mut f = Fixture::new();
    let runner = f.start().await;
    let into = f.run("mkdir sub && cd sub");
    let sub = format!("{}/sub", f.workdir());
    assert_eq!(f.finished(into).await.cwd.as_deref(), Some(sub.as_str()));
    let pwd = f.run("pwd");
    assert_eq!(f.finished(pwd).await.preview, sub);
    assert_eq!(f.started(pwd).await.1, sub);

    std::fs::remove_dir(&sub).unwrap();
    let pwd = f.run("pwd");
    let finished = f.finished(pwd).await;
    assert_eq!(
        f.started(pwd).await.1,
        f.workdir(),
        "fell back to the workdir"
    );
    assert_eq!(finished.preview, f.workdir());
    assert_eq!(finished.cwd, Some(f.workdir()));
    runner.shutdown().await.unwrap();
}

#[tokio::test]
async fn fd_3_is_closed_to_the_command() {
    let mut f = Fixture::new();
    let runner = f.start().await;
    let write = f.run("echo x >&3");
    let finished = f.finished(write).await;
    assert!(
        matches!(finished.end, ShellEnd::Exited { code } if code != 0),
        "{finished:?}"
    );
    assert_eq!(finished.cwd, Some(f.workdir()), "the cwd is not corrupted");
    // Neither a child nor a background child holds it: they cannot write a
    // cwd of their own or keep the run from ending.
    let child = f.run("mkdir sub && cd sub && sh -c 'echo /tmp >&3'; { sleep 1000 & } ; true");
    let finished = f.finished(child).await;
    assert_eq!(finished.end, exited(0));
    assert_eq!(finished.cwd, Some(format!("{}/sub", f.workdir())));
    runner.shutdown().await.unwrap();
}

/// Set in the copy of this test binary that [`there_is_no_controlling_terminal`]
/// starts on a pty.
const ON_A_TERMINAL: &str = "SKYWAVE_TEST_ON_A_TERMINAL";

/// `cargo test` has no controlling terminal, so the runner here would pass
/// without its shell's `setsid`. The test reruns itself as a session leader
/// with a pty as its terminal, and runs the shell there: without `setsid`
/// the shell would inherit that terminal and `cat /dev/tty` would block,
/// since nobody ever writes to the pty.
#[tokio::test]
async fn there_is_no_controlling_terminal() {
    if std::env::var_os(ON_A_TERMINAL).is_some() {
        return no_terminal_from_a_terminal().await;
    }
    let (mut master, mut slave) = (-1, -1);
    // SAFETY: openpty writes the two fds; null name, termios and winsize
    // ask for the defaults.
    let opened = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(opened, 0, "openpty: {}", std::io::Error::last_os_error());
    // SAFETY: openpty just opened both fds; nothing else owns them.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "there_is_no_controlling_terminal",
            "--test-threads=1",
        ])
        .env(ON_A_TERMINAL, "1")
        .stdin(slave)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // SAFETY: the closure runs in the forked child before exec and only
    // calls setsid and ioctl, which are async-signal-safe, and builds
    // errors without allocating.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            // Stdin, the pty's slave, becomes the new session's terminal.
            if libc::ioctl(0, libc::TIOCSCTTY, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().unwrap();
    drop(command);
    // The bound only guards a hang: the copy waits at most `WAIT` itself.
    let output = tokio::time::timeout(2 * WAIT, child.wait_with_output())
        .await
        .expect("the run on a terminal ends")
        .unwrap();
    drop(master);
    assert!(
        output.status.success(),
        "the test on a terminal failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    // The copy really ran the test, not nothing.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("1 passed"), "{stdout}");
}

/// [`there_is_no_controlling_terminal`] in the copy whose terminal is a pty.
async fn no_terminal_from_a_terminal() {
    // The copy has a terminal; otherwise the test proves nothing.
    let tty = std::fs::File::open("/dev/tty").expect("the test runs on a terminal");
    drop(tty);
    let mut f = Fixture::new();
    let runner = f.start().await;
    let run = f.run("cat /dev/tty");
    let finished = f.finished(run).await;
    assert!(
        matches!(finished.end, ShellEnd::Exited { code } if code != 0),
        "{finished:?}"
    );
    assert!(finished.preview.contains("/dev/tty"), "{finished:?}");
    runner.shutdown().await.unwrap();
}

// --- queue and status ---------------------------------------------------

#[tokio::test]
async fn runs_queue_per_session_and_sessions_run_in_parallel() {
    let mut f = Fixture::new();
    let other = f.create_session("other");
    let runner = f.start().await;
    let gate = f.fifo("gate");
    let first = f.run(&format!("cat '{}'; echo first", gate.display()));
    let second = f.run("echo second");
    let elsewhere = f.run_in(other, RunWhen::Always, "echo elsewhere");
    // The other session's run, requested last, is over while the first
    // session's first run still waits: the runner has seen `second` too.
    assert_eq!(f.finished(elsewhere).await.end, exited(0));
    assert_eq!(f.started(elsewhere).await.1, f.workdir_of(other));
    assert!(f.has_started(first));
    assert!(!f.has_started(second), "one run at a time per session");

    open_gate(&gate).await;
    assert_eq!(f.finished(second).await.preview, "second");
    assert_eq!(f.finish_of(first).unwrap().preview, "first");
    let first_end = f
        .run_events(first)
        .into_iter()
        .find(|(_, e)| is_finished(e, first))
        .unwrap()
        .0;
    let (second_start, _) = f.started(second).await;
    assert!(first_end < second_start, "second starts after first ends");
    runner.shutdown().await.unwrap();
}

#[tokio::test]
async fn runs_are_unconditional_by_default() {
    let mut f = Fixture::new();
    let runner = f.start().await;
    let failing = f.run("false");
    let after = f.run("echo after");
    assert_eq!(f.finished(after).await.preview, "after");
    assert_eq!(f.end(failing), Some(exited(1)));
    runner.shutdown().await.unwrap();
}

#[tokio::test]
async fn when_follows_the_last_status_like_dollar_question() {
    let mut f = Fixture::new();
    let fresh = f.create_session("fresh");
    let runner = f.start().await;
    let s = f.session;
    let runs = [
        (f.run_in(s, RunWhen::Always, "false"), Some(exited(1))),
        (
            f.run_in(s, RunWhen::OnSuccess, "echo a"),
            Some(ShellEnd::Skipped),
        ),
        (
            f.run_in(s, RunWhen::OnSuccess, "echo b"),
            Some(ShellEnd::Skipped),
        ),
        (f.run_in(s, RunWhen::OnFailure, "echo c"), Some(exited(0))),
        (f.run_in(s, RunWhen::Always, "echo d"), Some(exited(0))),
        (f.run_in(s, RunWhen::Always, "true"), Some(exited(0))),
        (
            f.run_in(s, RunWhen::OnFailure, "echo e"),
            Some(ShellEnd::Skipped),
        ),
        // A session with no run yet counts as success.
        (
            f.run_in(fresh, RunWhen::OnSuccess, "echo f"),
            Some(exited(0)),
        ),
    ];
    for (run, _) in &runs {
        f.finished(*run).await;
    }
    let ends: Vec<_> = runs.iter().map(|(run, _)| f.end(*run)).collect();
    let expected: Vec<_> = runs.iter().map(|(_, end)| end.clone()).collect();
    assert_eq!(ends, expected);
    // A skipped run never starts and has no start time.
    let skipped = runs[1].0;
    assert!(!f.has_started(skipped));
    assert_eq!(f.finish_of(skipped).unwrap().started_at, None);
    assert_eq!(f.finish_of(skipped).unwrap().command_line, "echo a");
    runner.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_stopped_run_counts_as_failure() {
    let mut f = Fixture::new();
    let runner = f.start().await;
    let gate = f.fifo("gate");
    let held = f.run(&format!("cat '{}'", gate.display()));
    f.started(held).await;
    f.stop(f.session, held);
    assert_eq!(f.finished(held).await.end, ShellEnd::Stopped);
    let on_failure = f.run_in(f.session, RunWhen::OnFailure, "echo x");
    assert_eq!(f.finished(on_failure).await.end, exited(0));
    runner.shutdown().await.unwrap();
}

#[tokio::test]
async fn two_devices_share_the_last_status() {
    let mut f = Fixture::new();
    let runner = f.start().await;
    let (laptop, phone) = (DeviceId::new(), DeviceId::new());
    let failing = f.run_as(laptop, f.session, RunWhen::Always, "false");
    let guarded = f.run_as(phone, f.session, RunWhen::OnSuccess, "echo y");
    let finished = f.finished(guarded).await;
    // "Previous" is the other device's run, as in one shared terminal.
    assert_eq!(finished.end, ShellEnd::Skipped);
    assert_eq!(finished.by, phone);
    assert_eq!(f.finish_of(failing).unwrap().by, laptop);
    runner.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_newest_finish_decides_the_status() {
    let mut f = Fixture::new();
    let runner = f.start().await;
    let gate = f.fifo("gate");
    let held = f.run(&format!("cat '{}'", gate.display()));
    let queued = f.run("echo never");
    f.started(held).await;
    // Stopped while the earlier run still runs: it finishes first.
    f.stop(f.session, queued);
    assert_eq!(f.finished(queued).await.end, ShellEnd::Stopped);
    open_gate(&gate).await;
    assert_eq!(f.finished(held).await.end, exited(0));
    // In request order the stop would be newest, and failed; by finish it
    // is the earlier run's success.
    assert!(f.status(f.session).succeeded());
    let later = f.run_in(f.session, RunWhen::OnSuccess, "echo later");
    assert_eq!(f.finished(later).await.end, exited(0));
    runner.shutdown().await.unwrap();
}

// --- stop and the end of a run ------------------------------------------

#[tokio::test]
async fn stop_ends_a_running_shell_and_its_group() {
    let mut f = Fixture::new();
    let runner = f.start().await;
    let gate = f.fifo("gate");
    let pid = f.fifo("pid");
    let run = f.run(&format!(
        "echo $$ > '{}'; cat '{}'",
        pid.display(),
        gate.display()
    ));
    let pgid: i32 = read_fifo(&pid).await.trim().parse().unwrap();
    assert!(!group_gone(pgid), "the shell leads its group");
    f.stop(f.session, run);
    let finished = f.finished(run).await;
    assert_eq!(finished.end, ShellEnd::Stopped);
    assert_eq!(finished.started_at, Some(f.started_at(run)));
    assert!(group_gone(pgid), "nothing is left in the group");
    runner.shutdown().await.unwrap();
}

#[tokio::test]
async fn stop_finishes_a_queued_run_without_starting_it() {
    let mut f = Fixture::new();
    let runner = f.start().await;
    let gate = f.fifo("gate");
    let held = f.run(&format!("cat '{}'", gate.display()));
    let queued = f.run("echo never");
    f.started(held).await;
    f.stop(f.session, queued);
    let finished = f.finished(queued).await;
    assert_eq!(finished.end, ShellEnd::Stopped);
    assert_eq!(finished.started_at, None);
    assert!(!f.has_started(queued));
    assert_eq!(f.end(held), None, "the running run goes on");
    open_gate(&gate).await;
    assert_eq!(f.finished(held).await.end, exited(0));
    assert!(!f.has_started(queued));
    runner.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_background_child_is_killed_with_the_run() {
    let mut f = Fixture::new();
    let runner = f.start().await;
    // The child holds the output pipe: the run ends at the shell's exit,
    // not at EOF.
    let run = f.run("sleep 1000 & echo $!");
    let finished = f.finished(run).await;
    assert_eq!(finished.end, exited(0));
    let pid: i32 = finished.preview.parse().unwrap();
    assert!(process_gone(pid), "the background sleep was killed");
    // By the SIGTERM at the shell's exit, not the SIGKILL after the grace.
    let took = f.finished_at(run) - f.started_at(run);
    assert!(took < chrono::Duration::seconds(4), "took {took}");
    runner.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_process_that_left_the_group_does_not_hold_the_run() {
    let mut f = Fixture::new();
    let runner = f.start().await;
    let pid = f.dir().join("escaped");
    let run = f.run(&format!(
        "p='{}'; mkfifo \"$p\"; \
         setsid sh -c 'echo $$ > \"$1\"; exec sleep 1000' sh \"$p\" </dev/null & \
         read pid < \"$p\"; echo \"$pid\"",
        pid.display()
    ));
    let finished = f.finished(run).await;
    assert_eq!(finished.end, exited(0), "{finished:?}");
    let pid: i32 = finished.preview.parse().unwrap();
    // It survives the run, holding the output pipe: not killed, not waited for.
    let alive = !process_gone(pid);
    // SAFETY: kills the test's own escaped sleep.
    unsafe { libc::kill(pid, libc::SIGKILL) };
    assert!(alive, "a process that left the group survives the run");
    runner.shutdown().await.unwrap();
}

/// A run's orphans go to its host, a subreaper, whatever the host's init
/// does; the host must reap them, or their zombies hold the group, and the
/// run, for the whole 5 s grace. The daemon (this test process) is no
/// subreaper and reaps none of them.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[tokio::test]
async fn the_runs_orphans_are_reaped_by_its_host() {
    let mut f = Fixture::new();
    let runner = f.start().await;
    let mut subreaper: libc::c_int = -1;
    // SAFETY: PR_GET_CHILD_SUBREAPER writes one c_int to `subreaper`.
    let asked = unsafe {
        libc::prctl(
            libc::PR_GET_CHILD_SUBREAPER,
            std::ptr::from_mut(&mut subreaper),
        )
    };
    assert_eq!((asked, subreaper), (0, 0), "the daemon is no subreaper");
    let within = chrono::Duration::milliseconds(2500);

    // Stopped: the sleep is orphaned while the shell runs (its subshell has
    // exited by `ready`), then the SIGTERM ends the shell, the `cat` and it.
    let gate = f.fifo("gate");
    let pid = f.fifo("pid");
    let ready = f.fifo("ready");
    let stopped = f.run(&format!(
        "(sleep 1000 & echo $! > '{}'); echo > '{}'; cat '{}'",
        pid.display(),
        ready.display(),
        gate.display()
    ));
    let sleep: i32 = read_fifo(&pid).await.trim().parse().unwrap();
    read_fifo(&ready).await;
    // The positive control: the orphan went to the run's host, a child of
    // this process, not to whatever reaper the test runs under.
    let host = parent_of(sleep);
    assert_eq!(parent_of(host), i32::try_from(std::process::id()).unwrap());
    let cmdline = std::fs::read(format!("/proc/{host}/cmdline")).unwrap();
    assert_eq!(
        cmdline.split(|&byte| byte == 0).nth(1),
        Some(skywaved::shell::RUN_HOST.as_bytes()),
        "the orphan's parent is the run's host"
    );
    f.stop(f.session, stopped);
    assert_eq!(f.finished(stopped).await.end, ShellEnd::Stopped);
    // A zombie still answers `kill(pid, 0)`: gone means reaped.
    assert!(process_gone(sleep), "the orphaned sleep was reaped");
    let took = f.finished_at(stopped) - f.started_at(stopped);
    assert!(took < within, "stopped run took {took}");

    // Finished: the shell exits first, so its children are orphans before
    // the SIGTERM reaches them.
    let finished = f.run("sleep 0.2 & sleep 1000 & echo $!");
    let end = f.finished(finished).await;
    assert_eq!(end.end, exited(0));
    let sleep: i32 = end.preview.parse().unwrap();
    assert!(process_gone(sleep), "the orphaned sleep was reaped");
    let took = f.finished_at(finished) - f.started_at(finished);
    assert!(took < within, "finished run took {took}");
    runner.shutdown().await.unwrap();
}

/// A host that dies mid-run closes its report pipe without a status, and
/// with the group still full: the run is `Lost`, and the runner ends the
/// group itself, giving a member its grace, and reports only after it.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[tokio::test]
async fn a_run_whose_host_dies_is_lost_and_its_group_ended() {
    let mut f = Fixture::new();
    let runner = f.start().await;
    let gate = f.fifo("gate");
    let pid = f.fifo("pid");
    let member = f.fifo("member");
    let bye = f.dir().join("bye");
    // The member takes a moment over its SIGTERM, then leaves a mark.
    let run = f.run(&format!(
        "sh -c 'trap \"sleep 0.3; echo bye > \\\"$1\\\"; exit\" TERM; \
         echo $$ > \"$2\"; while :; do sleep 0.05; done' sh '{}' '{}' & \
         echo $$ > '{}'; cat '{}'",
        bye.display(),
        member.display(),
        pid.display(),
        gate.display()
    ));
    let shell: i32 = read_fifo(&pid).await.trim().parse().unwrap();
    let member: i32 = read_fifo(&member).await.trim().parse().unwrap();
    let host = parent_of(shell);
    let cmdline = std::fs::read(format!("/proc/{host}/cmdline")).unwrap();
    assert_eq!(
        cmdline.split(|&byte| byte == 0).nth(1),
        Some(skywaved::shell::RUN_HOST.as_bytes()),
        "the shell's parent is its host"
    );
    // SAFETY: kills the test's own run host.
    assert_eq!(unsafe { libc::kill(host, libc::SIGKILL) }, 0);
    let finished = f.finished(run).await;
    assert_eq!(finished.end, ShellEnd::Lost, "{finished:?}");
    // Reported only after the member ended on its own, not killed at once.
    assert_eq!(std::fs::read_to_string(&bye).unwrap(), "bye\n");
    assert!(ended(member), "the member is gone");
    assert!(ended(shell), "the shell is gone");
    runner.shutdown().await.unwrap();
}

/// Whether process `pid` has ended: gone, or a zombie nobody reaped (the
/// test's init may not reap).
#[cfg(any(target_os = "linux", target_os = "android"))]
fn ended(pid: i32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat
            .rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
    }
}

/// Kills the process whose pid is in a file when dropped, so an escaped
/// process is killed even when the test fails.
struct KillPidFile(PathBuf);

impl Drop for KillPidFile {
    fn drop(&mut self) {
        if let Ok(pid) = std::fs::read_to_string(&self.0)
            && let Ok(pid) = pid.trim().parse::<i32>()
        {
            // SAFETY: kills the test's own escaped process.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

#[tokio::test]
async fn an_escaped_writer_does_not_stall_the_runner() {
    let mut f = Fixture::new();
    let runner = f.start().await;
    let fifo = f.dir().join("escaped");
    let pidfile = f.dir().join("escaped.pid");
    let _kill = KillPidFile(pidfile.clone());
    // It keeps the output pipe and never stops writing to it.
    let run = f.run(&format!(
        "p='{}'; mkfifo \"$p\"; \
         setsid sh -c 'echo $$ > \"$1\"; exec yes' sh \"$p\" </dev/null & \
         read pid < \"$p\"; echo \"$pid\" > '{}'",
        fifo.display(),
        pidfile.display()
    ));
    let next = f.run("echo next");
    let finished = f.finished(run).await;
    assert_eq!(finished.end, exited(0), "{finished:?}");
    assert_eq!(f.finished(next).await.preview, "next");
    tokio::time::timeout(WAIT, runner.shutdown())
        .await
        .expect("shutdown returns")
        .unwrap();
}

// --- output -------------------------------------------------------------

#[tokio::test]
async fn output_past_the_cap_is_discarded() {
    let mut f = Fixture::new();
    let runner = f
        .start_with(ShellConfig {
            output_max: 1000,
            ..f.config()
        })
        .await;
    let run = f.run("seq 1 1000");
    let finished = f.finished(run).await;
    let all: String = (1..=1000).map(|n| format!("{n}\n")).collect();
    assert!(finished.truncated);
    assert_eq!(finished.lines, 1000);
    // The preview is the output's real tail, past the cap too.
    let tail: String = (993..=1000).map(|n| format!("{n}\n")).collect();
    assert_eq!(finished.preview, tail.trim_end());
    let blob = finished.output.expect("a blob");
    assert_eq!(blob.size, 1000);
    assert_eq!(f.blob(&blob), all.as_bytes()[..1000]);
    runner.shutdown().await.unwrap();
}

/// Free space a test sets.
struct FakeSpace(AtomicU64);

impl FreeSpace for FakeSpace {
    fn free(&self) -> std::io::Result<u64> {
        Ok(self.0.load(Ordering::SeqCst))
    }
}

/// While the disk is low a running run's output stays in its pipe, so the
/// command blocks once the pipe is full; with room again it is read on and
/// the run ends with all of it.
///
/// "Not finished yet" only means paused next to how long the same run takes
/// with room, so the test first times that on this machine and then holds
/// the paused run many times as long. The output is larger than any pipe
/// (16 pages: 64 KiB to 1 MiB, by page size), so a paused run cannot end.
#[tokio::test]
async fn a_low_disk_pauses_a_runs_output_and_loses_none_of_it() {
    const FLOOR: u64 = 1 << 30;
    const LINES: u32 = 1_000_000;
    let mut f = Fixture::new();
    let space = Arc::new(FakeSpace(AtomicU64::new(FLOOR)));
    let disk = Disk::new(space.clone(), FLOOR);
    f.log = f.log.clone().with_disk(disk.clone());
    let runner = f.start().await;
    let all: String = (1..=LINES).map(|n| format!("{n}\n")).collect();
    let gated_run = |f: &mut Fixture, name: &str| {
        let gate = f.fifo(&format!("{name}-gate"));
        let done = f.dir().join(format!("{name}-done"));
        let run = f.run(&format!(
            "cat '{}'; seq 1 {LINES}; touch '{}'",
            gate.display(),
            done.display()
        ));
        (run, gate, done)
    };

    // With room: how long the run takes here, from the gate to its end.
    let (control, gate, _) = gated_run(&mut f, "control");
    f.started(control).await;
    let start = std::time::Instant::now();
    open_gate(&gate).await;
    let finished = f.finished(control).await;
    let unpaused = start.elapsed();
    assert_eq!(finished.lines, LINES, "the unpaused run takes all of it");

    let (run, gate, done) = gated_run(&mut f, "paused");
    f.started(run).await;
    space.0.store(FLOOR - 1, Ordering::SeqCst);
    assert!(disk.check());
    open_gate(&gate).await;
    let hold = (unpaused * 10).max(Duration::from_secs(1));
    let finished = tokio::time::timeout(hold, f.finished(run)).await;
    assert!(
        finished.is_err(),
        "the run ended while paused (unpaused it took {unpaused:?}): {finished:?}"
    );
    assert!(!done.exists(), "the command blocks on its output");

    space.0.store(FLOOR, Ordering::SeqCst);
    assert!(!disk.check());
    let finished = f.finished(run).await;
    assert_eq!(finished.end, exited(0));
    assert_eq!(finished.lines, LINES);
    assert!(done.exists());
    let blob = finished.output.expect("a blob");
    assert_eq!(f.blob(&blob), all.as_bytes());
    runner.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_blob_is_raw_and_the_preview_a_stripped_tail() {
    let mut f = Fixture::new();
    let runner = f.start().await;
    let run =
        f.run(r"printf '\033[31mred\033[0m\n'; seq 1 20; printf 'bar\r\033[32mdone\033[0m\n'");
    let finished = f.finished(run).await;
    let mut raw = b"\x1b[31mred\x1b[0m\n".to_vec();
    raw.extend((1..=20).flat_map(|n| format!("{n}\n").into_bytes()));
    raw.extend(b"bar\r\x1b[32mdone\x1b[0m\n");
    assert_eq!(f.blob(finished.output.as_ref().expect("a blob")), raw);
    assert_eq!(finished.preview, "14\n15\n16\n17\n18\n19\n20\ndone");
    assert_eq!(finished.lines, 22);
    assert!(!finished.truncated);
    // The output file is gone once the blob holds it.
    let left: Vec<_> = std::fs::read_dir(f.data.join("shell")).unwrap().collect();
    assert!(left.is_empty(), "{left:?}");
    runner.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_output_file_becomes_the_blob_without_a_copy() {
    use std::os::unix::fs::MetadataExt;
    let mut f = Fixture::new();
    let runner = f.start().await;
    // The command reports its own output file's inode as its last line.
    let shell = f.data.join("shell");
    let run = f.run(&format!("seq 1 20; ls -i '{}'/*.out", shell.display()));
    let finished = f.finished(run).await;
    let blob = finished.output.expect("a blob");
    let content = f.blob(&blob);
    assert_eq!(blob.size, content.len() as u64);
    let text = String::from_utf8(content).unwrap();
    let inode: u64 = text
        .lines()
        .last()
        .and_then(|line| line.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no inode in {text:?}"));
    let stored = std::fs::metadata(f.data.join("blobs").join(blob.id.0.to_string())).unwrap();
    assert_eq!(stored.ino(), inode, "the blob is the output file, renamed");
    let left: Vec<_> = std::fs::read_dir(&shell).unwrap().collect();
    assert!(left.is_empty(), "{left:?}");
    runner.shutdown().await.unwrap();
}

#[tokio::test]
async fn output_whose_blob_cannot_be_stored_is_logged_truncated() {
    let mut f = Fixture::new();
    let mut runner = f.start().await;
    f.blobs_full();
    let run = f.run("seq 1 20");
    let finished = f.finished(run).await;
    assert_eq!(finished.end, exited(0));
    assert_eq!(finished.output, None, "the blob could not be stored");
    assert!(finished.truncated, "the event says it holds only the tail");
    assert_eq!(finished.lines, 20);
    assert_eq!(finished.preview, "13\n14\n15\n16\n17\n18\n19\n20");
    let failed = tokio::time::timeout(Duration::from_millis(200), runner.failed()).await;
    assert!(failed.is_err(), "a blob write is no runner failure");

    // Control: the runner goes on, and with room the output is a blob.
    f.disk_has_room();
    let run = f.run("seq 1 20");
    let finished = f.finished(run).await;
    assert!(finished.output.is_some());
    assert!(!finished.truncated);
    runner.shutdown().await.unwrap();
}

#[tokio::test]
async fn short_output_the_preview_changes_still_gets_a_blob() {
    let mut f = Fixture::new();
    let runner = f.start().await;
    // Invalid UTF-8 and escape codes: the preview fits but cannot hold the
    // bytes, so only the blob keeps them.
    let binary = f.run(r"printf '\377\376\n'");
    let colour = f.run(r"printf '\033[1mbold\033[0m\n'");
    let plain = f.run("printf 'plain\\n\\n'");
    let finished = f.finished(binary).await;
    assert_eq!(finished.preview, "\u{FFFD}\u{FFFD}");
    assert_eq!(
        f.blob(finished.output.as_ref().expect("a blob")),
        b"\xff\xfe\n"
    );
    let finished = f.finished(colour).await;
    assert_eq!(finished.preview, "bold");
    assert_eq!(
        f.blob(finished.output.as_ref().expect("a blob")),
        b"\x1b[1mbold\x1b[0m\n"
    );
    // Positive control: text the preview holds, trailing newlines aside.
    let finished = f.finished(plain).await;
    assert_eq!(finished.preview, "plain");
    assert_eq!(finished.output, None);
    runner.shutdown().await.unwrap();
}

#[tokio::test]
async fn progress_reports_the_tail_only_when_there_is_new_output() {
    let mut f = Fixture::new();
    let runner = f
        .start_with(ShellConfig {
            progress: Progress::every(Duration::from_millis(10)),
            ..f.config()
        })
        .await;
    let (one, two) = (f.fifo("one"), f.fifo("two"));
    // The payload sleep spans dozens of checks without new output.
    let run = f.run(&format!(
        "echo a; cat '{}'; sleep 0.3; echo b; cat '{}'",
        one.display(),
        two.display()
    ));
    let output = |f: &Fixture| -> Vec<(String, u32, u64)> {
        f.run_kinds(run)
            .into_iter()
            .filter_map(|k| match k {
                EventKind::ShellOutput {
                    tail, lines, bytes, ..
                } => Some((tail, lines, bytes)),
                _ => None,
            })
            .collect()
    };
    let has_tail = |events: &[Event], want: &str| {
        events.iter().any(|e| {
            matches!(&e.kind, EventKind::ShellOutput { run: r, tail, .. } if *r == run && tail == want)
        })
    };
    f.wait_for("a", |events| has_tail(events, "a")).await;
    open_gate(&one).await;
    f.wait_for("a, b", |events| has_tail(events, "a\nb")).await;
    open_gate(&two).await;
    f.finished(run).await;
    assert_eq!(
        output(&f),
        [("a".into(), 1, 2), ("a\nb".into(), 2, 4)],
        "one report per change"
    );
    runner.shutdown().await.unwrap();
}

// --- restart and shutdown -----------------------------------------------

#[tokio::test]
async fn a_restart_settles_what_the_crash_left_and_runs_the_queue_once() {
    let mut f = Fixture::new();
    let lost = f.crashed_run("make", true);
    let guarded = f.run_in(f.session, RunWhen::OnSuccess, "echo z");
    let always = f.run("echo always");
    let stopped = f.crashed_run("echo stopped", false);
    f.stop(f.session, stopped);
    let stale = f.data.join("shell").join(format!("{}.out", lost.0));
    std::fs::write(&stale, b"half").unwrap();

    let runner = f.start().await;
    assert!(!stale.exists(), "stale output is removed at start");
    let finished = f.finished(lost).await;
    assert_eq!(finished.end, ShellEnd::Lost);
    assert_eq!(finished.started_at, Some(f.started_at(lost)));
    assert_eq!(finished.command_line, "make");
    assert_eq!(f.finished(stopped).await.end, ShellEnd::Stopped);
    assert!(!f.has_started(stopped));
    // Lost is a failure: the "if ok" run is skipped, the unconditional one
    // runs.
    assert_eq!(f.finished(guarded).await.end, ShellEnd::Skipped);
    assert_eq!(f.finished(always).await.preview, "always");
    runner.shutdown().await.unwrap();

    // Another start finds nothing to do: each run ran at most once.
    let head = f.log.head();
    let runner = f.start().await;
    assert_eq!(f.log.head(), head);
    let starts = f
        .run_kinds(always)
        .iter()
        .filter(|k| matches!(k, EventKind::ShellStarted { .. }))
        .count();
    assert_eq!(starts, 1);
    runner.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_disabled_runner_settles_every_leftover_and_ends() {
    let mut f = Fixture::new();
    let lost = f.crashed_run("make", true);
    let queued = f.run("echo queued");
    let stopped = f.crashed_run("echo stopped", false);
    f.stop(f.session, stopped);
    let stale = f.data.join("shell").join(format!("{}.out", lost.0));
    std::fs::write(&stale, b"half").unwrap();

    let runner = f
        .start_with(ShellConfig {
            enabled: false,
            ..f.config()
        })
        .await;
    tokio::time::timeout(WAIT, runner.join())
        .await
        .expect("a disabled runner ends")
        .unwrap();
    assert_eq!(f.end(lost), Some(ShellEnd::Lost));
    assert_eq!(
        f.end(queued),
        Some(ShellEnd::NotStarted {
            reason: DISABLED_REASON.into()
        })
    );
    assert!(!f.has_started(queued));
    assert_eq!(f.end(stopped), Some(ShellEnd::Stopped));
    assert!(!stale.exists());
    assert!(
        f.log
            .unfinished_shell_runs(&f.storage.db)
            .unwrap()
            .runs
            .is_empty()
    );
}

#[tokio::test]
async fn shutdown_kills_runs_and_logs_nothing() {
    let mut f = Fixture::new();
    let runner = f.start().await;
    let gate = f.fifo("gate");
    let pid = f.fifo("pid");
    let run = f.run(&format!(
        "echo $$ > '{}'; cat '{}'",
        pid.display(),
        gate.display()
    ));
    let pgid: i32 = read_fifo(&pid).await.trim().parse().unwrap();
    f.started(run).await;
    let head = f.log.head();
    runner.shutdown().await.unwrap();
    assert_eq!(f.log.head(), head, "shutdown logs nothing");
    assert!(group_gone(pgid), "the run's group was killed");

    // The next start settles it like a crash.
    let runner = f.start().await;
    let finished = f.finished(run).await;
    assert_eq!(finished.end, ShellEnd::Lost);
    assert_eq!(finished.started_at, Some(f.started_at(run)));
    runner.shutdown().await.unwrap();
}

// --- failure ------------------------------------------------------------

#[tokio::test]
async fn a_runner_error_kills_its_runs_and_surfaces() {
    let mut f = Fixture::new();
    let mut runner = f
        .start_with(ShellConfig {
            progress: Progress::every(Duration::from_millis(10)),
            ..f.config()
        })
        .await;
    f.disk_full_for("shell_output");
    let gate = f.fifo("gate");
    let pid = f.fifo("pid");
    let run = f.run(&format!(
        "echo $$ > '{}'; echo out; cat '{}'",
        pid.display(),
        gate.display()
    ));
    let pgid: i32 = read_fifo(&pid).await.trim().parse().unwrap();
    // The first progress report cannot be logged: the runner stops on it.
    tokio::time::timeout(WAIT, runner.failed())
        .await
        .expect("the runner's failure surfaces");
    assert!(group_gone(pgid), "the run's group was killed");
    assert!(f.has_started(run));
    assert_eq!(f.end(run), None, "a failed runner logs nothing more");
    let error = runner.shutdown().await.unwrap_err();
    assert!(error.to_string().contains("disk is full"), "{error}");

    // The next start settles the run from the log.
    f.disk_has_room();
    let runner = f.start().await;
    assert_eq!(f.finished(run).await.end, ShellEnd::Lost);
    runner.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_runner_that_ends_cleanly_never_reports_a_failure() {
    let f = Fixture::new();
    let mut runner = f
        .start_with(ShellConfig {
            enabled: false,
            ..f.config()
        })
        .await;
    // A disabled runner ends at once; `failed` must still not return.
    let failed = tokio::time::timeout(Duration::from_millis(200), runner.failed()).await;
    assert!(failed.is_err(), "a clean end is no failure");
    runner.shutdown().await.unwrap();
}

/// The daemon stops, non-zero and with the error logged, when its shell
/// runner fails, instead of accepting runs nothing would start.
#[tokio::test]
async fn the_daemon_stops_when_its_shell_runner_fails() {
    let mut f = Fixture::new();
    // Queued for the daemon's runner, whose `ShellStarted` cannot be logged.
    let run = f.run("echo never");
    f.disk_full_for("shell_started");
    let log_file = f.dir().join("skywaved.log");
    let config = f.dir().join("skywaved.toml");
    std::fs::write(
        &config,
        format!(
            "listen = \"127.0.0.1:0\"\ndata_dir = {:?}\nroots = [{:?}]\nlog_file = {:?}\n\
             [shell]\nenabled = true\n[disk]\nmin_free = 0\n",
            f.data, f.root, log_file
        ),
    )
    .unwrap();
    let daemon = tokio::process::Command::new(env!("CARGO_BIN_EXE_skywaved"))
        .arg("--config")
        .arg(&config)
        .arg("serve")
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(WAIT, daemon)
        .await
        .expect("the daemon stops on its own")
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("disk is full"), "{stderr}");
    let logged = std::fs::read_to_string(&log_file).unwrap();
    assert!(logged.contains("listening"), "{logged}");
    assert!(logged.contains("shell runner failed"), "{logged}");
    // The daemon's process had a log of its own: take up where it ended.
    f.log = EventLog::open(&f.storage.db, vec![f.root.clone()])
        .unwrap()
        .with_shell(true);
    assert!(!f.has_started(run));

    // Kept queued, not lost: the next start runs it.
    f.disk_has_room();
    let runner = f.start().await;
    assert_eq!(f.finished(run).await.end, exited(0));
    runner.shutdown().await.unwrap();
}

/// Waits until the log file at `path` says `serve` listens.
async fn wait_listening(path: &Path) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while !std::fs::read_to_string(path)
        .unwrap_or_default()
        .contains(" listening addr=")
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "serve never listened"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A Ctrl-C with a run going warns and stops nothing; the next Ctrl-C stops
/// `serve`, and the run with it.
#[tokio::test]
async fn ctrl_c_with_a_shell_run_warns_and_a_second_stops() {
    let mut f = Fixture::new();
    f.run("sleep 30");
    let log_file = f.dir().join("skywaved.log");
    let config = f.dir().join("skywaved.toml");
    std::fs::write(
        &config,
        format!(
            "listen = \"127.0.0.1:0\"\ndata_dir = {:?}\nroots = [{:?}]\nlog_file = {:?}\n\
             [shell]\nenabled = true\n[disk]\nmin_free = 0\n",
            f.data, f.root, log_file
        ),
    )
    .unwrap();
    let mut serve = tokio::process::Command::new(env!("CARGO_BIN_EXE_skywaved"))
        .arg("--config")
        .arg(&config)
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    wait_listening(&log_file).await;
    let pid = libc::pid_t::try_from(serve.id().unwrap()).unwrap();
    // SAFETY: signals a child this test spawned and has not reaped.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGINT) }, 0);

    let mut lines = BufReader::new(serve.stderr.take().unwrap()).lines();
    let mut printed = Vec::new();
    tokio::time::timeout(WAIT, async {
        while let Some(line) = lines.next_line().await.unwrap() {
            let asked = line.contains("press Ctrl-C again");
            printed.push(line);
            if asked {
                break;
            }
        }
    })
    .await
    .expect("serve asks for a second Ctrl-C");
    let (asked, before) = printed.split_last().unwrap();
    assert!(asked.contains("press Ctrl-C again"), "{printed:?}");
    assert!(
        before.iter().any(|l| l.contains("shell run `sleep 30`")),
        "{printed:?}"
    );
    assert!(
        serve.try_wait().unwrap().is_none(),
        "the first Ctrl-C stopped serve"
    );

    // SAFETY: as above.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGINT) }, 0);
    tokio::time::timeout(WAIT, serve.wait())
        .await
        .expect("the second Ctrl-C stops serve")
        .unwrap();
}

// Stopping serve over its control socket (`skywaved::control`), with the
// runs it would lose. No supervisor here: its activity says no turn runs.

impl Fixture {
    /// The control socket over this log, beside a supervisor with no turns.
    fn control(&self) -> (Control, watch::Sender<Activity>) {
        let (activity, watched) = watch::channel(Activity {
            cursor: MAX_SEQ,
            turns: false,
        });
        let control = Control::bind(
            self.storage.dir.control_socket(),
            &self.storage.dir.db_path(),
            self.log.clone(),
            watched,
        )
        .unwrap();
        (control, activity)
    }

    async fn client(&self) -> TestClient {
        TestClient::connect(&self.storage.dir.control_socket()).await
    }

    /// Run `run` of `command` in the first session, as the control socket
    /// lists it.
    fn listed(&self, run: CommandId, command: &str, started: bool) -> Work {
        Work::Shell {
            run,
            session: self.session,
            workdir: self.workdir(),
            command: command.into(),
            started,
        }
    }

    fn try_run(&mut self, command: &str) -> CommandResult {
        self.handle(
            DeviceId::new(),
            CommandId::new(),
            CommandKind::RunShell {
                session_id: self.session,
                command: command.into(),
                when: RunWhen::Always,
            },
        )
    }
}

#[tokio::test]
async fn a_stop_now_with_nothing_running_needs_no_confirmation() {
    let f = Fixture::new();
    let (mut control, _activity) = f.control();
    let mut client = f.client().await;
    assert_eq!(
        client.ask(stop_now(Confirm::Nothing)).await,
        Reply::Stopping { restart: false }
    );
    assert_eq!(control.stopped().await, Stop { restart: false });
}

#[tokio::test]
async fn a_stop_now_needs_every_running_shell_run_confirmed() {
    let mut f = Fixture::new();
    let runner = f.start().await;
    let (mut control, _activity) = f.control();
    let gate = f.fifo("gate");
    let command = format!("cat '{}'", gate.display());
    let held = f.run(&command);
    f.started(held).await;
    let running = f.listed(held, &command, true);

    let mut client = f.client().await;
    assert_eq!(
        client.ask(stop_now(Confirm::Nothing)).await,
        Reply::Busy {
            work: vec![running.clone()]
        }
    );
    // Asked for since: the confirmation does not cover it.
    let later = f.run("echo later");
    let Reply::Busy { work } = client.ask(stop_now(Confirm::Seen(vec![running]))).await else {
        panic!("refused again");
    };
    assert_eq!(work.len(), 2, "{work:?}");
    assert!(
        work[1].same(&f.listed(later, "echo later", false)),
        "{work:?}"
    );

    assert_eq!(
        client.ask(stop_now(Confirm::Seen(work))).await,
        Reply::Stopping { restart: false }
    );
    assert_eq!(control.stopped().await, Stop { restart: false });
    assert_eq!(rejected(f.try_run("echo no")), RejectReason::ServerStopping);
    runner.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_forced_stop_now_takes_whatever_runs() {
    let mut f = Fixture::new();
    let runner = f.start().await;
    let (mut control, _activity) = f.control();
    let gate = f.fifo("gate");
    let held = f.run(&format!("cat '{}'", gate.display()));
    f.started(held).await;
    let mut client = f.client().await;
    assert_eq!(
        client.ask(stop_now(Confirm::Force)).await,
        Reply::Stopping { restart: false }
    );
    assert_eq!(control.stopped().await, Stop { restart: false });
    runner.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_stop_after_work_waits_for_the_shell_runs_and_refuses_new_ones() {
    let mut f = Fixture::new();
    let runner = f.start().await;
    let (mut control, _activity) = f.control();
    let gate = f.fifo("gate");
    let command = format!("cat '{}'", gate.display());
    let held = f.run(&command);
    f.started(held).await;

    let mut client = f.client().await;
    assert_eq!(
        client.ask(stop_after_work(true)).await,
        Reply::Waiting {
            work: vec![f.listed(held, &command, true)]
        }
    );
    assert_eq!(rejected(f.try_run("echo no")), RejectReason::ServerStopping);

    open_gate(&gate).await;
    assert_eq!(f.finished(held).await.end, exited(0));
    assert!(client.stopping().await);
    assert_eq!(control.stopped().await, Stop { restart: true });
    runner.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_cancelled_stop_takes_work_again() {
    let mut f = Fixture::new();
    let runner = f.start().await;
    let (_control, _activity) = f.control();
    let gate = f.fifo("gate");
    let command = format!("cat '{}'", gate.display());
    let held = f.run(&command);
    f.started(held).await;

    let mut waiting = f.client().await;
    assert!(matches!(
        waiting.ask(stop_after_work(false)).await,
        Reply::Waiting { .. }
    ));
    let mut other = f.client().await;
    assert_eq!(other.ask(Request::Cancel).await, Reply::Cancelled);
    assert_eq!(waiting.next().await, Reply::Cancelled);
    assert_eq!(other.ask(Request::Cancel).await, Reply::NothingToCancel);
    accepted(f.try_run("echo yes"));

    open_gate(&gate).await;
    f.finished(held).await;
    runner.shutdown().await.unwrap();
}
