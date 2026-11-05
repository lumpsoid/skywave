//! One core per data dir: an open [`Core`](crate::Core) holds an exclusive
//! OS lock on [`LOCK_FILE`], so a second client on the same dir is refused
//! with [`LockError::InUse`] instead of racing the first one's SQLite writes
//! and going stale against them.

use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};

/// File in the core's data directory that an open core holds locked.
pub const LOCK_FILE: &str = "core.lock";

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    /// Another core, in this process or another, has the data dir open.
    #[error("another skywave client is using {}", path.display())]
    InUse { path: PathBuf },
    #[error("cannot lock data directory {}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// The exclusive lock on a data dir's [`LOCK_FILE`]. The OS releases it when
/// the file closes: on drop, or when the process exits however it exits, so
/// a crash never leaves the dir locked.
#[derive(Debug)]
pub(crate) struct DataDirLock {
    _file: File,
}

impl DataDirLock {
    /// Creates `data_dir` if needed and takes its lock without waiting.
    /// The lock belongs to the open file (`flock` on Unix), so a second
    /// core in the same process is refused too.
    pub(crate) fn take(data_dir: &Path) -> Result<DataDirLock, LockError> {
        let io = |source| LockError::Io {
            path: data_dir.to_path_buf(),
            source,
        };
        fs::create_dir_all(data_dir).map_err(io)?;
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(data_dir.join(LOCK_FILE))
            .map_err(io)?;
        match try_lock(&file) {
            Ok(()) => Ok(DataDirLock { _file: file }),
            Err(TryLockError::WouldBlock) => Err(LockError::InUse {
                path: data_dir.to_path_buf(),
            }),
            Err(TryLockError::Error(source)) => Err(io(source)),
        }
    }
}

/// `flock(LOCK_EX | LOCK_NB)` straight through libc: std's
/// [`File::try_lock`] answers `Unsupported` on Android (Rust 1.93), which
/// left the app unable to open its data at all. Every Unix takes this path,
/// so the host tests cover the code the phone runs; std's Unix lock is the
/// same `flock`.
#[cfg(unix)]
fn try_lock(file: &File) -> Result<(), TryLockError> {
    use std::os::fd::AsRawFd;
    // SAFETY: the descriptor is open for the whole call; `flock` touches
    // no memory.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock {
        Err(TryLockError::WouldBlock)
    } else {
        Err(TryLockError::Error(error))
    }
}

#[cfg(not(unix))]
fn try_lock(file: &File) -> Result<(), TryLockError> {
    file.try_lock()
}
