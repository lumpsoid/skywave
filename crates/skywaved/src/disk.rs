//! Free space on the data dir's file system (architecture.md "Server", D40).
//!
//! Agents share the server's disk with whatever else runs there, and a
//! failed event-log append ends the supervisor. So `serve` keeps a floor of
//! free space (`[disk] min_free`, [`DEFAULT_MIN_FREE`]) and checks it every
//! [`CHECK_INTERVAL`] ([`Disk::watch`]). Below the floor the disk is *low*:
//! the log refuses commands that start new work (`RejectReason::DiskLow`)
//! and the supervisor stops reading its agents' events, which pauses them
//! (see `supervisor`). Once the free space is back at the floor, both go on
//! by themselves. The check polls because nothing reports free space as it
//! changes; the poll only wakes the check, and what follows is decided by
//! the space it reads.
//!
//! The space is read through [`FreeSpace`], so tests fake it rather than
//! fill a disk.
//!
//! A disk that fills between checks anyway fails an append, which ends the
//! supervisor (its agents get SIGTERM and their grace) and `serve`. The next
//! `serve` waits for [`START_MIN_FREE`] before it writes anything
//! ([`wait_for_room`]), so a full disk does not make it fail on every start.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

/// Default `[disk] min_free`: the free space below which the disk is low.
pub const DEFAULT_MIN_FREE: u64 = 1 << 30;
/// How often `serve` reads the free space.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(5);
/// The free space `serve` waits for before it starts (capped at the floor):
/// enough for a start's own writes (its run's record, the settling of what
/// the last run left), far below the floor.
pub const START_MIN_FREE: u64 = 64 << 20;

/// Where the free space comes from; injectable so tests fake it.
pub trait FreeSpace: Send + Sync + 'static {
    /// Bytes free to the daemon's user on the data dir's file system.
    fn free(&self) -> std::io::Result<u64>;
}

/// The free space of the file system holding `path`, as `statvfs` reports
/// it to an unprivileged user (`f_bavail`).
#[derive(Debug, Clone)]
pub struct FsFreeSpace {
    path: PathBuf,
}

impl FsFreeSpace {
    pub fn new(path: impl Into<PathBuf>) -> FsFreeSpace {
        FsFreeSpace { path: path.into() }
    }
}

impl FreeSpace for FsFreeSpace {
    #[cfg(unix)]
    fn free(&self) -> std::io::Result<u64> {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(self.path.as_os_str().as_bytes())
            .map_err(std::io::Error::other)?;
        // SAFETY: `path` is a valid C string and `stat` a plain struct
        // statvfs fills in.
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } == -1 {
            return Err(std::io::Error::last_os_error());
        }
        #[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
        let free = u64::from(stat.f_bavail).saturating_mul(u64::from(stat.f_frsize));
        Ok(free)
    }

    /// Unknown here: never low.
    #[cfg(not(unix))]
    fn free(&self) -> std::io::Result<u64> {
        Ok(u64::MAX)
    }
}

/// Whether the data dir's disk is low, shared by everything that acts on
/// it. Clones share it; the default is never low until a check says so.
#[derive(Debug, Clone)]
pub struct Disk {
    low: Arc<watch::Sender<bool>>,
}

impl Default for Disk {
    fn default() -> Self {
        Disk {
            low: Arc::new(watch::Sender::new(false)),
        }
    }
}

impl Disk {
    /// Whether the free space was below the floor at the last check.
    pub fn is_low(&self) -> bool {
        *self.low.borrow()
    }

    /// Watches [`Disk::is_low`]; it changes only when the state flips.
    pub fn subscribe(&self) -> watch::Receiver<bool> {
        self.low.subscribe()
    }

    /// Reads the free space from `source` once and records whether it is
    /// below `floor`, logging a flip. A failed read keeps the state.
    pub fn check(&self, source: &dyn FreeSpace, floor: u64) {
        let free = match source.free() {
            Ok(free) => free,
            Err(error) => {
                tracing::warn!(%error, "cannot read the data dir's free space");
                return;
            }
        };
        let low = free < floor;
        self.low.send_if_modified(|was| {
            if *was == low {
                return false;
            }
            *was = low;
            if low {
                tracing::warn!(
                    free,
                    floor,
                    "the data dir's disk is low: agents are paused and new work is refused \
                     until space is freed"
                );
            } else {
                tracing::info!(free, floor, "the data dir's disk has room again: resuming");
            }
            true
        });
    }

    /// Checks `source` against `floor` now, then every `interval` on a task
    /// of the current runtime, until the handle is aborted.
    pub fn watch(
        &self,
        source: Arc<dyn FreeSpace>,
        floor: u64,
        interval: Duration,
    ) -> tokio::task::JoinHandle<()> {
        self.check(&*source, floor);
        let disk = self.clone();
        tokio::spawn(async move {
            let mut ticks = tokio::time::interval(interval);
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // The first tick is immediate, and the check above was it.
            ticks.tick().await;
            loop {
                ticks.tick().await;
                disk.check(&*source, floor);
            }
        })
    }
}

/// Waits until `source` reports at least `need` bytes free, reading it
/// every `interval`, and logs once that it waits. `serve` calls it before
/// its first write: a start on a full disk would fail on that write, and a
/// service manager would restart it in a loop. A failed read does not wait.
pub async fn wait_for_room(source: &dyn FreeSpace, need: u64, interval: Duration) {
    let mut told = false;
    loop {
        match source.free() {
            Ok(free) if free < need => {
                if !told {
                    tracing::error!(
                        free,
                        need,
                        "the data dir's disk is full: waiting for space before starting"
                    );
                    told = true;
                }
            }
            Ok(_) => break,
            Err(error) => {
                tracing::warn!(%error, "cannot read the data dir's free space; starting");
                break;
            }
        }
        tokio::time::sleep(interval).await;
    }
    if told {
        tracing::info!("the data dir's disk has room again: starting");
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    /// Free space a test sets.
    #[derive(Debug, Default)]
    struct Fake(AtomicU64);

    impl FreeSpace for Fake {
        fn free(&self) -> std::io::Result<u64> {
            Ok(self.0.load(Ordering::SeqCst))
        }
    }

    #[test]
    fn low_is_below_the_floor() {
        let disk = Disk::default();
        let fake = Fake::default();
        assert!(!disk.is_low(), "never low before a check");
        fake.0.store(99, Ordering::SeqCst);
        disk.check(&fake, 100);
        assert!(disk.is_low());
        fake.0.store(100, Ordering::SeqCst);
        disk.check(&fake, 100);
        assert!(!disk.is_low(), "at the floor is not low");
        fake.0.store(0, Ordering::SeqCst);
        disk.check(&fake, 0);
        assert!(!disk.is_low(), "a zero floor is never low");
    }

    #[tokio::test]
    async fn the_watch_follows_the_space() {
        let disk = Disk::default();
        let fake = Arc::new(Fake::default());
        let mut low = disk.subscribe();
        let task = disk.watch(fake.clone(), 100, Duration::from_millis(5));
        assert!(disk.is_low(), "the first check is made at once");
        fake.0.store(100, Ordering::SeqCst);
        low.wait_for(|low| !*low).await.unwrap();
        fake.0.store(0, Ordering::SeqCst);
        low.wait_for(|low| *low).await.unwrap();
        task.abort();
    }

    #[tokio::test]
    async fn the_start_waits_for_room() {
        let fake = Arc::new(Fake::default());
        let waiting = {
            let fake = fake.clone();
            tokio::spawn(async move {
                wait_for_room(&*fake, 10, Duration::from_millis(5)).await;
            })
        };
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished(), "nothing free: it waits");
        fake.0.store(10, Ordering::SeqCst);
        waiting.await.unwrap();
    }

    #[test]
    fn the_file_system_reports_some_space() {
        let dir = tempfile::tempdir().unwrap();
        let free = FsFreeSpace::new(dir.path()).free().unwrap();
        assert!(free > 0);
    }
}
