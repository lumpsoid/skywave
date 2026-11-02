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
//!
//! A process that must run in a session of its own cannot use [`isolate`]:
//! std runs `setpgid` before any `pre_exec` closure, and `setsid` in a
//! process that already leads a group fails with EPERM. Such a process
//! calls `setsid` in its own `pre_exec` and takes [`die_with_daemon`]
//! alone; the session leader also leads a group, so [`Group::of`] still
//! holds. PDEATHSIG fires on the exit of the spawning *thread*, not the
//! daemon, so spawn from a thread that lives as long as the daemon.

use std::time::Duration;

use tokio::process::{Child, Command};

/// How long a killed process group gets between SIGTERM and SIGKILL.
pub(crate) const DEFAULT_TERM_GRACE: Duration = Duration::from_secs(5);

/// Makes the process `command` spawns lead a new process group and, on
/// Linux, die with the daemon.
pub(crate) fn isolate(command: &mut Command) {
    #[cfg(unix)]
    command.process_group(0);
    die_with_daemon(command);
}

/// On Linux, makes the process `command` spawns get SIGTERM when the daemon
/// (strictly, the spawning thread) dies, and fail to spawn if the daemon is
/// gone already. Elsewhere does nothing.
pub(crate) fn die_with_daemon(command: &mut Command) {
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
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let _ = command;
}

/// The process group a spawned CLI leads; killed when dropped.
#[derive(Debug)]
pub(crate) struct Group {
    /// `None` once killed, or where groups are not used.
    pgid: Option<i32>,
}

impl Group {
    /// The group `child` leads, after [`isolate`].
    pub(crate) fn of(child: &Child) -> Group {
        let pgid = if cfg!(unix) {
            child.id().and_then(|id| i32::try_from(id).ok())
        } else {
            None
        };
        Group { pgid }
    }

    /// SIGTERMs every process in the group; false when there is no group
    /// to signal (killed already, or groups are not used).
    pub(crate) fn terminate(&self) -> bool {
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
    pub(crate) fn is_empty(&self) -> bool {
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
    pub(crate) fn kill(&mut self) {
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
pub(crate) fn signal(pgid: i32, signal: libc::c_int) {
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// Spawns `sleep 1000` in a group of its own, and the group that holds it.
    fn spawn_sleep() -> (Child, Group) {
        let mut command = Command::new("sleep");
        command.arg("1000").kill_on_drop(true);
        isolate(&mut command);
        let child = command.spawn().unwrap();
        let group = Group::of(&child);
        (child, group)
    }

    #[tokio::test]
    async fn terminate_ends_the_group() {
        let (mut child, group) = spawn_sleep();
        assert!(!group.is_empty(), "the sleep is in the group");
        assert!(group.terminate());
        // Reaping is the definitive signal: the group lives on as a zombie
        // until then.
        let status = child.wait().await.unwrap();
        assert!(!status.success());
        assert!(group.is_empty());
    }

    #[tokio::test]
    async fn kill_ends_the_group() {
        let (mut child, mut group) = spawn_sleep();
        assert!(!group.is_empty(), "the sleep is in the group");
        let pgid = group.pgid;
        group.kill();
        child.wait().await.unwrap();
        // `kill` forgets the group, so ask the kernel about it directly.
        let left = Group { pgid };
        assert!(left.is_empty());
        std::mem::forget(left);
        assert!(group.is_empty());
        assert!(!group.terminate(), "a killed group is not signalled again");
    }
}
