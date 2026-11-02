//! Running the `git` CLI for `GET /v1/workdir/changes` (the parent module's
//! docs, "Changes"): fixed flags and environment, the repo's own filter
//! drivers neutralised, and every child killed at the snapshot's deadline.

use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Instant;

use crate::error::{Error, IoContext};
use crate::process;

/// The `Failed` reason of a snapshot that reached its deadline.
pub(super) const TIMED_OUT: &str = "timed out";
/// Inherited `GIT_*` variables a child keeps: where the user's and the
/// system's config live, and where git's own programs are. Every other one
/// is removed (it could point git at another repository, index or config).
const KEPT_GIT_ENV: [&str; 4] = [
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
    "GIT_CONFIG_NOSYSTEM",
    "GIT_EXEC_PATH",
];
/// stderr bytes kept per child; only its first line is ever shown.
const STDERR_KEPT: usize = 4096;
/// Longest `Failed` reason, in bytes.
const REASON_MAX: usize = 200;

/// How the daemon runs `git`: the program and extra environment for every
/// child. [`Git::default`] runs `git` from `PATH`.
#[derive(Debug, Clone)]
pub struct Git {
    program: PathBuf,
    env: Vec<(OsString, OsString)>,
}

impl Default for Git {
    fn default() -> Git {
        Git::new("git")
    }
}

impl Git {
    /// Runs `program` as git.
    pub fn new(program: impl Into<PathBuf>) -> Git {
        Git {
            program: program.into(),
            env: Vec::new(),
        }
    }

    /// Sets `key` to `value` in every child's environment, after the fixed
    /// variables (so it can override them).
    pub fn with_env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Git {
        self.env.push((key.into(), value.into()));
        self
    }
}

/// Why a git run gave no answer to build on.
#[derive(Debug)]
pub(super) enum GitError {
    /// The domain answer `ChangesResult::Failed { reason }`.
    Failed(String),
    /// A server fault (a 500).
    Fault(Error),
}

impl From<Error> for GitError {
    fn from(e: Error) -> GitError {
        match e {
            Error::WorkdirDeadline => GitError::Failed(TIMED_OUT.into()),
            e => GitError::Fault(e),
        }
    }
}

/// A child that ran to its end.
pub(super) struct Finished<T> {
    pub(super) status: ExitStatus,
    pub(super) stderr: Vec<u8>,
    /// What the reader made of stdout.
    pub(super) out: T,
}

impl<T> Finished<T> {
    /// The first line of stderr, bounded, or the exit status when there is
    /// none.
    pub(super) fn reason(&self) -> String {
        first_line(&self.stderr).unwrap_or_else(|| format!("git exited with {}", self.status))
    }

    /// `out` when the child exited 0, else its [`Finished::reason`].
    pub(super) fn ok(self) -> Result<T, GitError> {
        if self.status.success() {
            Ok(self.out)
        } else {
            Err(GitError::Failed(self.reason()))
        }
    }
}

/// git run in one workdir until one deadline.
pub(super) struct Repo<'a> {
    git: &'a Git,
    dir: &'a Path,
    deadline: Instant,
    /// Names of the filter drivers whose `clean` and `process` are emptied
    /// on every call.
    neutralised: Vec<Vec<u8>>,
}

impl<'a> Repo<'a> {
    pub(super) fn new(git: &'a Git, dir: &'a Path, deadline: Instant) -> Repo<'a> {
        Repo {
            git,
            dir,
            deadline,
            neutralised: Vec::new(),
        }
    }

    /// Neutralises the filter drivers `names` on every later call.
    pub(super) fn neutralise(&mut self, names: Vec<Vec<u8>>) {
        self.neutralised = names;
    }

    /// `git <fixed flags> <args>` in the workdir, with the fixed environment.
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(&self.git.program);
        command
            .arg("-C")
            .arg(self.dir)
            .args([
                "-c",
                "core.quotePath=false",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "diff.external=",
                "-c",
                "diff.submodule=short",
                "--no-optional-locks",
                "--no-pager",
            ])
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        for (key, _) in std::env::vars_os() {
            if scrubbed(&key) {
                command.env_remove(key);
            }
        }
        command
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("LC_ALL", "C");
        // Command-scope config, as `-c` is, but any driver name fits: `-c`
        // would split a name holding `=`.
        // `required = false` too: a required driver left without a command
        // would fail the diff instead of comparing raw content.
        let neutral = self.neutralised.iter().flat_map(|name| {
            [("clean", ""), ("process", ""), ("required", "false")].map(|(key, value)| {
                let mut full = b"filter.".to_vec();
                full.extend_from_slice(name);
                full.push(b'.');
                full.extend_from_slice(key.as_bytes());
                (OsString::from_vec(full), value)
            })
        });
        let mut count = 0;
        for (i, (key, value)) in neutral.enumerate() {
            command
                .env(format!("GIT_CONFIG_KEY_{i}"), key)
                .env(format!("GIT_CONFIG_VALUE_{i}"), value);
            count = i + 1;
        }
        if count > 0 {
            command.env("GIT_CONFIG_COUNT", count.to_string());
        }
        command.envs(self.git.env.iter().map(|(k, v)| (k, v)));
        command
    }

    /// Runs `git <args>`, handing its stdout to `read`, and waits for it.
    ///
    /// The child leads its own process group, which is SIGKILLed at the
    /// deadline (and when `read` fails), so a filter's children die with it.
    /// `Failed` when git is missing or the deadline came first.
    pub(super) fn run<T>(
        &self,
        args: &[&str],
        read: impl FnOnce(&mut dyn Read) -> io::Result<T>,
    ) -> Result<Finished<T>, GitError> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(GitError::Failed(TIMED_OUT.into()));
        }
        let mut child = match self.command(args).spawn() {
            Ok(child) => child,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(GitError::Failed("git is not installed".into()));
            }
            Err(e) => return Err(GitError::Failed(format!("git could not start: {e}"))),
        };
        let pid = libc::pid_t::try_from(child.id()).expect("a pid fits pid_t");
        let mut stdout = child.stdout.take().expect("stdout is piped");
        let mut stderr = child.stderr.take().expect("stderr is piped");
        let (done, finished) = mpsc::channel::<()>();
        let (out, killed, status, stderr) = std::thread::scope(|scope| {
            let watchdog = scope.spawn(move || match finished.recv_timeout(remaining) {
                Err(RecvTimeoutError::Timeout) => {
                    process::signal(pid, libc::SIGKILL);
                    true
                }
                Ok(()) | Err(RecvTimeoutError::Disconnected) => false,
            });
            let errors = scope.spawn(move || keep_head(&mut stderr, STDERR_KEPT));
            let out = read(&mut stdout);
            if out.is_err() {
                process::signal(pid, libc::SIGKILL);
            }
            drop(stdout);
            // Not reaped yet, so the group id cannot be reused while the
            // watchdog may still signal it.
            wait_unreaped(pid);
            let _ = done.send(());
            let killed = watchdog.join().expect("git watchdog panicked");
            let status = child.wait();
            (
                out,
                killed,
                status,
                errors.join().expect("stderr reader panicked"),
            )
        });
        if killed {
            return Err(GitError::Failed(TIMED_OUT.into()));
        }
        let status = status.context(|| "waiting for git".to_owned())?;
        let out = out.context(|| "reading git's output".to_owned())?;
        Ok(Finished {
            status,
            stderr,
            out,
        })
    }

    /// [`Repo::run`] keeping all of stdout.
    pub(super) fn output(&self, args: &[&str]) -> Result<Finished<Vec<u8>>, GitError> {
        self.run(args, |stdout| {
            let mut all = Vec::new();
            stdout.read_to_end(&mut all)?;
            Ok(all)
        })
    }
}

/// Whether an inherited variable is removed from a child's environment.
fn scrubbed(key: &OsStr) -> bool {
    key.to_str().map_or_else(
        || key.as_encoded_bytes().starts_with(b"GIT_"),
        |key| key.starts_with("GIT_") && !KEPT_GIT_ENV.contains(&key),
    )
}

/// Blocks until `pid` has exited, leaving it a zombie (not reaped).
fn wait_unreaped(pid: libc::pid_t) {
    let Ok(id) = libc::id_t::try_from(pid) else {
        return;
    };
    loop {
        // SAFETY: waitid only writes the zeroed `siginfo_t` it is given.
        let failed = unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            libc::waitid(libc::P_PID, id, &mut info, libc::WEXITED | libc::WNOWAIT) == -1
        };
        if !failed || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return;
        }
    }
}

/// The first `max` bytes of `from`, reading (and dropping) the rest.
fn keep_head(from: &mut impl Read, max: usize) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match from.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let room = max.saturating_sub(kept.len());
                kept.extend_from_slice(&chunk[..n.min(room)]);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    kept
}

/// The first non-empty line of `text`, trimmed and at most [`REASON_MAX`]
/// bytes.
pub(super) fn first_line(text: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(text);
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    let mut end = line.len().min(REASON_MAX);
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    Some(line[..end].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inherited_git_location_env_is_removed_but_config_locations_stay() {
        for key in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_CONFIG_PARAMETERS",
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_KEY_0",
            "GIT_CONFIG_VALUE_0",
            "GIT_NAMESPACE",
            "GIT_LITERAL_PATHSPECS",
        ] {
            assert!(scrubbed(OsStr::new(key)), "{key}");
        }
        for key in ["GIT_CONFIG_GLOBAL", "GIT_CONFIG_NOSYSTEM", "HOME", "PATH"] {
            assert!(!scrubbed(OsStr::new(key)), "{key}");
        }
    }

    #[test]
    fn a_reason_is_the_first_line_bounded() {
        assert_eq!(
            first_line(b"\nfatal: no\nmore").as_deref(),
            Some("fatal: no")
        );
        assert_eq!(first_line(b"  \n").as_deref(), None);
        let long = "\u{e9}".repeat(REASON_MAX);
        assert_eq!(first_line(long.as_bytes()).unwrap().len(), REASON_MAX);
    }
}
