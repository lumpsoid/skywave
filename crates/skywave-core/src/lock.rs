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
        match file.try_lock() {
            Ok(()) => Ok(DataDirLock { _file: file }),
            Err(TryLockError::WouldBlock) => Err(LockError::InUse {
                path: data_dir.to_path_buf(),
            }),
            Err(TryLockError::Error(source)) => Err(io(source)),
        }
    }
}
