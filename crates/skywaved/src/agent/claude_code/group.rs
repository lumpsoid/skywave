//! What ties a CLI process, and everything it starts, to its session.
//!
//! Each CLI leads its own process group ([`isolate`]) and every stop ends
//! the whole group: SIGTERM first ([`Group::terminate`]), then SIGKILL
//! ([`Group::kill`]) for whatever is left after a grace.
//!
//! The SIGTERM matters most. The CLI runs each Bash command, foreground or
//! background, in a session of its own (`setsid`), so no group signal
//! reaches it; on SIGTERM the CLI kills those itself before it exits, in
//! well under a second. A CLI that dies without handling a SIGTERM (a
//! SIGKILL, or one that outlives the grace) leaves them running
//! (docs/agents/claude-code.md §6). The group still takes the CLI's
//! children that stay in it. A group of its own also keeps a terminal's
//! Ctrl-C away from the CLI, so the daemon's shutdown stops it in order.
//!
//! On Linux the CLI also gets SIGTERM when the daemon dies without stopping
//! it (`PR_SET_PDEATHSIG`), so it stops its commands and exits, and a
//! restarted daemon does not resume a session another CLI still runs. The
//! signal fires when the *thread* that spawned the CLI exits; the
//! supervisor spawns agents from its own thread, which lives as long as
//! the daemon.

use tokio::process::{Child, Command};

/// Makes the process `command` spawns lead a new process group and, on
/// Linux, die with the daemon.
pub(super) fn isolate(command: &mut Command) {
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let daemon = libc::pid_t::try_from(std::process::id()).unwrap_or(0);
        // SAFETY: the closure runs in the forked child before exec and only
        // calls prctl and getppid, which are async-signal-safe, and builds
        // errors without allocating.
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                // The daemon died before the prctl: no signal will come.
                if libc::getppid() != daemon {
                    return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
                }
                Ok(())
            });
        }
    }
}

/// The process group a spawned CLI leads; killed when dropped.
#[derive(Debug)]
pub(super) struct Group {
    /// `None` once killed, or where groups are not used.
    pgid: Option<i32>,
}

impl Group {
    /// The group `child` leads, after [`isolate`].
    pub(super) fn of(child: &Child) -> Group {
        let pgid = if cfg!(unix) {
            child.id().and_then(|id| i32::try_from(id).ok())
        } else {
            None
        };
        Group { pgid }
    }

    /// SIGTERMs every process in the group; false when there is no group
    /// to signal (killed already, or groups are not used).
    pub(super) fn terminate(&self) -> bool {
        let Some(pgid) = self.pgid else {
            return false;
        };
        #[cfg(unix)]
        signal(pgid, libc::SIGTERM);
        #[cfg(not(unix))]
        let _ = pgid;
        true
    }

    /// Whether nothing is left in the group.
    pub(super) fn is_empty(&self) -> bool {
        let Some(pgid) = self.pgid else {
            return true;
        };
        #[cfg(unix)]
        {
            // SAFETY: signal 0 only checks that the group exists.
            let failed = unsafe { libc::killpg(pgid, 0) } == -1;
            failed && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        }
        #[cfg(not(unix))]
        {
            let _ = pgid;
            true
        }
    }

    /// SIGKILLs every process still in the group, once. Call it before
    /// reaping a live CLI, or after reaping one that exited: while any
    /// process is in the group its id is not reused.
    pub(super) fn kill(&mut self) {
        let Some(pgid) = self.pgid.take() else {
            return;
        };
        #[cfg(unix)]
        signal(pgid, libc::SIGKILL);
        #[cfg(not(unix))]
        let _ = pgid;
    }
}

/// Sends `signal` to process group `pgid`; an empty group is not an error.
#[cfg(unix)]
fn signal(pgid: i32, signal: libc::c_int) {
    // SAFETY: killpg only sends a signal.
    if unsafe { libc::killpg(pgid, signal) } == -1 {
        let error = std::io::Error::last_os_error();
        // ESRCH: nothing was left in the group.
        if error.raw_os_error() != Some(libc::ESRCH) {
            tracing::debug!("cannot signal the agent's process group: {error}");
        }
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        self.kill();
    }
}
