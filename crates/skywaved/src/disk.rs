//! Free space on the data dir's file system (architecture.md "Server", D40).
//!
//! Agents share the server's disk with whatever else runs there, and a
//! failed event-log append ends the supervisor. So `serve` keeps a floor of
//! free space (`[disk] min_free`, [`DEFAULT_MIN_FREE`]). Below the floor the
//! disk is *low*: the log refuses commands that start new work
//! (`RejectReason::DiskLow`), the supervisor stops reading its agents'
//! events and the shell runner its runs' output, which pauses them. Once the
//! free space is back at the floor, all of them go on by themselves.
//!
//! **When it is read.** Only where something is about to take space, so an
//! idle server reads nothing and runs no timer:
//! - the API, when a command that starts work arrives;
//! - the supervisor, as it logs what an agent reports, and the shell
//!   runner, as it stores and logs a run's output.
//!
//! Each asks [`Disk::refresh`], which reads the space at most once per
//! [`CHECK_INTERVAL`] and otherwise reuses the last reading: a burst of
//! events costs one read.
//!
//! **While low** nothing writes, so nothing would read the space again: the
//! flip to low starts the one poll left, which reads it every
//! [`CHECK_INTERVAL`] until it is back at the floor, flips the disk back and
//! stops ([`Disk::wait_for`]). It polls because nothing announces free
//! space; what follows is decided by the space it reads. The same loop is
//! the start's wait at a full disk: a disk that fills between reads anyway
//! fails an append, which ends the supervisor (its agents get SIGTERM and
//! their grace) and `serve`, and the next `serve` waits for
//! [`START_MIN_FREE`] before it writes anything, rather than failing on that
//! write at every start.
//!
//! The space is read through [`FreeSpace`], so tests fake it rather than
//! fill a disk.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::watch;
use tokio::time::Instant;

/// Default `[disk] min_free`: the free space below which the disk is low.
pub const DEFAULT_MIN_FREE: u64 = 1 << 30;
/// How old a reading may be and still be used, and how often a low disk is
/// read until it has room.
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
/// it; clones share it. [`Disk::default`] has no source and is never low.
#[derive(Clone, Default)]
pub struct Disk {
    inner: Arc<Inner>,
}

struct Inner {
    /// `None`: nothing to read, never low.
    source: Option<Arc<dyn FreeSpace>>,
    floor: u64,
    low: watch::Sender<bool>,
    /// When the space was last read.
    read_at: Mutex<Option<Instant>>,
    /// The poll of a low disk runs.
    polling: AtomicBool,
    /// Where that poll runs: the runtime the disk was made on, if any,
    /// else the one that finds it low.
    runtime: Option<Handle>,
}

impl Default for Inner {
    fn default() -> Self {
        Inner {
            source: None,
            floor: 0,
            low: watch::Sender::new(false),
            read_at: Mutex::new(None),
            polling: AtomicBool::new(false),
            runtime: None,
        }
    }
}

impl std::fmt::Debug for Disk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Disk")
            .field("floor", &self.inner.floor)
            .field("low", &self.is_low())
            .finish_non_exhaustive()
    }
}

impl Disk {
    /// A disk read from `source`, low below `floor` bytes free (0: never,
    /// and never read). Not read until something asks; the poll of a low
    /// disk runs on the current runtime, if there is one.
    pub fn new(source: Arc<dyn FreeSpace>, floor: u64) -> Disk {
        Disk {
            inner: Arc::new(Inner {
                source: Some(source),
                floor,
                runtime: Handle::try_current().ok(),
                ..Inner::default()
            }),
        }
    }

    /// Whether the disk was low at the last reading. Reads nothing.
    pub fn is_low(&self) -> bool {
        *self.inner.low.borrow()
    }

    /// Watches [`Disk::is_low`]; it changes only when the state flips.
    pub fn subscribe(&self) -> watch::Receiver<bool> {
        self.inner.low.subscribe()
    }

    /// Whether the disk is low, from a reading at most [`CHECK_INTERVAL`]
    /// old: reads the space unless one that recent exists. A low disk is
    /// not read here: its poll reads it. A flip to low starts that poll.
    pub fn refresh(&self) -> bool {
        let inner = &*self.inner;
        if inner.floor == 0 || self.is_low() {
            return self.is_low();
        }
        {
            let read_at = crate::lock(&inner.read_at);
            let now = Instant::now();
            if read_at.is_some_and(|at| now.duration_since(at) < CHECK_INTERVAL) {
                return false;
            }
        }
        self.check()
    }

    /// Reads the space now, however recent the last reading, and records
    /// it; whether the disk is low. [`Disk::refresh`] is the throttled
    /// form everything else uses; tests use this to read on cue.
    pub fn check(&self) -> bool {
        *crate::lock(&self.inner.read_at) = Some(Instant::now());
        if let Some(free) = self.read() {
            self.record(free);
        }
        self.is_low()
    }

    /// Waits until at least `need` bytes are free, reading the space now and
    /// then every [`CHECK_INTERVAL`]; the free space it found. A failed read
    /// ends the wait: an unreadable disk does not hold anything up.
    pub async fn wait_for(&self, need: u64) -> Option<u64> {
        loop {
            let free = self.read()?;
            *crate::lock(&self.inner.read_at) = Some(Instant::now());
            if free >= need {
                return Some(free);
            }
            tokio::time::sleep(CHECK_INTERVAL).await;
        }
    }

    /// The `serve` start's wait: until [`START_MIN_FREE`] (at most the
    /// floor) is free, logging once that it waits. Then reads the disk
    /// against the floor, so a start below it comes up paused.
    pub async fn wait_to_start(&self) {
        let need = START_MIN_FREE.min(self.inner.floor);
        if need == 0 {
            return;
        }
        let free = self.read();
        if free.is_some_and(|free| free < need) {
            tracing::error!(
                free,
                need,
                "the data dir's disk is full: waiting for space before starting"
            );
            self.wait_for(need).await;
            tracing::info!("the data dir's disk has room again: starting");
        }
        self.check();
    }

    fn read(&self) -> Option<u64> {
        let source = self.inner.source.as_ref()?;
        match source.free() {
            Ok(free) => Some(free),
            Err(error) => {
                tracing::warn!(%error, "cannot read the data dir's free space");
                None
            }
        }
    }

    /// Records a reading of `free` bytes, flipping the state and starting
    /// the poll when it turns low.
    fn record(&self, free: u64) {
        let floor = self.inner.floor;
        let low = free < floor;
        let flipped = self.inner.low.send_if_modified(|was| {
            let flipped = *was != low;
            *was = low;
            flipped
        });
        if !flipped {
            return;
        }
        if low {
            tracing::warn!(
                free,
                floor,
                "the data dir's disk is low: agents are paused and new work is refused \
                 until space is freed"
            );
            self.poll();
        } else {
            tracing::info!(free, floor, "the data dir's disk has room again: resuming");
        }
    }

    /// Starts the poll of a low disk, unless it runs: every
    /// [`CHECK_INTERVAL`] until the floor is free, then flips the disk back.
    fn poll(&self) {
        if self.inner.polling.swap(true, Ordering::SeqCst) {
            return;
        }
        let Some(runtime) = (self.inner.runtime.clone()).or_else(|| Handle::try_current().ok())
        else {
            // No runtime to wait on (a synchronous test): it stays low
            // until a reading says otherwise.
            self.inner.polling.store(false, Ordering::SeqCst);
            return;
        };
        let disk = self.clone();
        runtime.spawn(async move {
            tokio::time::sleep(CHECK_INTERVAL).await;
            let free = disk.wait_for(disk.inner.floor).await;
            disk.inner.polling.store(false, Ordering::SeqCst);
            // An unreadable disk counts as one with room.
            disk.record(free.unwrap_or(u64::MAX));
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, AtomicUsize};

    use super::*;

    /// Free space a test sets, counting its reads.
    #[derive(Debug, Default)]
    struct Fake {
        free: AtomicU64,
        reads: AtomicUsize,
    }

    impl Fake {
        fn set(&self, free: u64) {
            self.free.store(free, Ordering::SeqCst);
        }

        fn reads(&self) -> usize {
            self.reads.load(Ordering::SeqCst)
        }
    }

    impl FreeSpace for Fake {
        fn free(&self) -> std::io::Result<u64> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            Ok(self.free.load(Ordering::SeqCst))
        }
    }

    fn disk(floor: u64) -> (Disk, Arc<Fake>) {
        let fake = Arc::new(Fake::default());
        (Disk::new(fake.clone(), floor), fake)
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_is_read_until_asked() {
        let (disk, fake) = disk(100);
        fake.set(0);
        tokio::time::sleep(Duration::from_secs(3600)).await;
        assert_eq!(fake.reads(), 0);
        assert!(!disk.is_low(), "never low before a reading");
    }

    #[tokio::test(start_paused = true)]
    async fn a_burst_costs_one_read() {
        let (disk, fake) = disk(100);
        fake.set(100);
        for _ in 0..50 {
            assert!(!disk.refresh());
        }
        assert_eq!(fake.reads(), 1);
        tokio::time::sleep(CHECK_INTERVAL).await;
        assert!(!disk.refresh());
        assert_eq!(fake.reads(), 2, "an older reading is not reused");
    }

    #[tokio::test(start_paused = true)]
    async fn a_low_disk_is_polled_until_it_has_room_then_not_at_all() {
        let (disk, fake) = disk(100);
        let mut low = disk.subscribe();
        fake.set(99);
        assert!(disk.refresh(), "below the floor is low");
        let reads = fake.reads();
        tokio::time::sleep(CHECK_INTERVAL * 3 + Duration::from_millis(1)).await;
        assert!(disk.is_low());
        assert_eq!(fake.reads(), reads + 3, "read every interval while low");
        assert!(disk.refresh(), "a low disk is left to its poll");
        assert_eq!(fake.reads(), reads + 3);

        fake.set(100);
        low.wait_for(|low| !*low).await.unwrap();
        let reads = fake.reads();
        tokio::time::sleep(Duration::from_secs(3600)).await;
        assert_eq!(fake.reads(), reads, "the poll ends with the room");
        assert!(!disk.refresh(), "at the floor is not low");
    }

    #[tokio::test(start_paused = true)]
    async fn a_zero_floor_is_never_read() {
        let (disk, fake) = disk(0);
        assert!(!disk.refresh());
        disk.wait_to_start().await;
        assert_eq!(fake.reads(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn the_start_waits_for_room_and_comes_up_paused_below_the_floor() {
        let floor = START_MIN_FREE * 2;
        let (disk, fake) = disk(floor);
        let started = {
            let disk = disk.clone();
            tokio::spawn(async move { disk.wait_to_start().await })
        };
        tokio::time::sleep(CHECK_INTERVAL * 10).await;
        assert!(!started.is_finished(), "nothing free: it waits");
        fake.set(START_MIN_FREE);
        started.await.unwrap();
        assert!(disk.is_low(), "room to start, still below the floor");
    }

    #[test]
    fn the_file_system_reports_some_space() {
        let dir = tempfile::tempdir().unwrap();
        let free = FsFreeSpace::new(dir.path()).free().unwrap();
        assert!(free > 0);
    }
}
