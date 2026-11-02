//! A run's host on Linux: the process a run's shell starts under, so that
//! the run's orphans are reaped whatever PID 1 does (see the
//! [shell module docs](super)).
//!
//! `skywaved __run-host <out fd> <cwd fd> <program> <args>...` is started by
//! the runner, never by a user. It makes itself a subreaper, starts
//! `<program> <args>...` with the output pipe as stdout and stderr and the
//! cwd pipe as fd 3, and reaps each of its children as it ends: while the
//! shell lives, blocking in `waitpid(-1)`; then, while the shell's group has
//! a member, every [`GROUP_POLL`](super::GROUP_POLL). It exits once the group
//! is empty. The runner signals the group; the host signals nothing.
//!
//! It reports on stdout, a pipe to the runner, one line each:
//! - `pid <pid>` once the shell runs, or `error <message>` when it cannot
//!   start, and the host exits;
//! - `status <wait status>` once it has reaped the shell (the raw status
//!   `waitpid` gives, so a signal death reads as one);
//! - then EOF, at its exit: the group is empty.

use std::ffi::OsString;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitCode, Stdio};

use super::GROUP_POLL;

/// The first argument that makes `skywaved` a run's host. Not a command for
/// users; the daemon's help does not list it.
pub const RUN_HOST: &str = "__run-host";

/// The fd the shell's wrapper writes its cwd to.
const CWD_FD: RawFd = 3;

/// Runs the host on `args`, the arguments after [`RUN_HOST`].
pub fn run(args: impl IntoIterator<Item = OsString>) -> ExitCode {
    let mut report = io::stdout().lock();
    let mut say = |line: std::fmt::Arguments<'_>| {
        // A runner that is gone reads nothing; the host goes on reaping.
        let _ = report.write_fmt(line).and_then(|()| report.flush());
    };
    let shell = match start(args.into_iter()) {
        Ok(shell) => shell,
        Err(error) => {
            say(format_args!("error {error}\n"));
            return ExitCode::FAILURE;
        }
    };
    say(format_args!("pid {shell}\n"));
    // The shell holds its group while it lives: wake for each child that
    // ends, the shell or an orphan.
    loop {
        let mut status = 0;
        // SAFETY: waitpid writes one c_int to `status`.
        let pid = unsafe { libc::waitpid(-1, &mut status, 0) };
        if pid == shell {
            say(format_args!("status {status}\n"));
            break;
        }
        // ECHILD cannot come while the shell lives; the runner reads the
        // missing status as lost.
        if pid == -1 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return ExitCode::FAILURE;
        }
    }
    // Then until the runner's signals have emptied the group.
    loop {
        // SAFETY: waitpid with WNOHANG and no status pointer only reaps.
        while unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) } > 0 {}
        if group_gone(shell) {
            return ExitCode::SUCCESS;
        }
        std::thread::sleep(GROUP_POLL);
    }
}

/// Becomes a subreaper and starts the shell; its pid, which is also its
/// group's id.
fn start(mut args: impl Iterator<Item = OsString>) -> io::Result<libc::pid_t> {
    let out = fd_arg(args.next())?;
    let cwd = fd_arg(args.next())?;
    let program = args
        .next()
        .ok_or_else(|| io::Error::other("no shell program"))?;
    let on: libc::c_ulong = 1;
    // SAFETY: prctl only sets a flag on this process.
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, on) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // The cwd pipe goes to fd 3 here, before the spawn, so the shell
    // inherits it there and nothing std opens for the spawn can take fd 3.
    let out = if out.as_raw_fd() == CWD_FD {
        let moved = out.try_clone()?;
        drop(out);
        moved
    } else {
        out
    };
    set_cloexec(&out, true)?;
    let cwd = if cwd.as_raw_fd() == CWD_FD {
        cwd
    } else {
        // SAFETY: dup2 onto fd 3, which nothing here holds now.
        if unsafe { libc::dup2(cwd.as_raw_fd(), CWD_FD) } == -1 {
            return Err(io::Error::last_os_error());
        }
        drop(cwd);
        // SAFETY: dup2 just opened fd 3; nothing else owns it.
        unsafe { OwnedFd::from_raw_fd(CWD_FD) }
    };
    set_cloexec(&cwd, false)?;
    // SAFETY: getpid only returns this process's id.
    let host = unsafe { libc::getpid() };
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(out);
    // SAFETY: the closure runs in the forked child before exec and only
    // calls setsid, prctl and getppid, which are async-signal-safe, and
    // builds errors without allocating.
    unsafe {
        command.pre_exec(move || {
            // A new session, not just a group: no controlling terminal.
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                return Err(io::Error::last_os_error());
            }
            // The host died before the prctl: no signal will come.
            if libc::getppid() != host {
                return Err(io::Error::from_raw_os_error(libc::ESRCH));
            }
            Ok(())
        });
    }
    // Never waited for through `Child`: the loops in `run` reap it.
    let shell = command.spawn()?;
    libc::pid_t::try_from(shell.id()).map_err(io::Error::other)
}

/// Takes the open fd, above 2, that `arg` names.
fn fd_arg(arg: Option<OsString>) -> io::Result<OwnedFd> {
    let fd = arg
        .and_then(|arg| arg.to_str()?.parse::<RawFd>().ok())
        .filter(|fd| *fd > 2)
        .ok_or_else(|| io::Error::other("bad fd argument"))?;
    // SAFETY: F_GETFD only reads the fd's flags.
    if unsafe { libc::fcntl(fd, libc::F_GETFD) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the runner opened `fd` for this process, which owns it from
    // here on.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn set_cloexec(fd: &OwnedFd, on: bool) -> io::Result<()> {
    let flags = if on { libc::FD_CLOEXEC } else { 0 };
    // SAFETY: F_SETFD only sets the fd's flags.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, flags) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Whether process group `pgid` is gone.
fn group_gone(pgid: libc::pid_t) -> bool {
    // SAFETY: signal 0 only checks that the group exists.
    let failed = unsafe { libc::killpg(pgid, 0) } == -1;
    failed && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}
