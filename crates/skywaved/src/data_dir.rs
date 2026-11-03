//! Layout of the daemon's data directory (architecture.md "Server").
//!
//! ```text
//! <data_dir>/
//!   skywave.db   SQLite database (see `db`)
//!   blobs/       blob files named by BlobId (see `blobs`)
//!   ca/          CA key and certificate (see `identity`)
//!   server/      server key and certificate (see `identity`)
//!   server_id    the stable ServerId (see `identity`)
//!   shell/       output of running shell commands (see `shell`)
//!   serve.lock   held locked by the running `serve` (see `ServeLock`)
//! ```
//!
//! Directories are created with mode 0700 on Unix: `ca/` and `server/` hold
//! private keys.

use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};

use crate::error::{Error, IoContext, Result};

/// File in the data dir that a running `serve` holds locked.
pub const SERVE_LOCK_FILE: &str = "serve.lock";

/// One `serve` per data dir: the exclusive OS lock on [`SERVE_LOCK_FILE`].
/// A second daemon on the same dir, on any address, would otherwise sweep
/// the first one's in-flight blob writes in its startup pass and drive the
/// same sessions' agents. Admin commands (`pair`, `devices`) do not take
/// it: they only touch the database, which SQLite shares.
///
/// The OS releases the lock when the file closes: on drop, or when the
/// process exits however it exits, so a crash never leaves the dir locked.
#[derive(Debug)]
pub struct ServeLock {
    _file: File,
}

/// A data directory whose subdirectories exist.
#[derive(Debug, Clone)]
pub struct DataDir {
    root: PathBuf,
}

impl DataDir {
    /// Takes the serve lock without waiting. The lock belongs to the open
    /// file (`flock` on Unix), so a second take in the same process is
    /// refused too.
    pub fn lock_for_serve(&self) -> Result<ServeLock> {
        let path = self.root.join(SERVE_LOCK_FILE);
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .context(|| format!("opening {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(ServeLock { _file: file }),
            Err(TryLockError::WouldBlock) => Err(Error::DataDirInUse {
                path: self.root.clone(),
            }),
            Err(TryLockError::Error(source)) => Err(Error::Io {
                context: format!("locking {}", path.display()),
                source,
            }),
        }
    }

    /// Creates the directory and its subdirectories if missing.
    pub fn create(root: impl Into<PathBuf>) -> Result<DataDir> {
        let dir = DataDir { root: root.into() };
        for path in [
            dir.root.clone(),
            dir.blobs_dir(),
            dir.ca_dir(),
            dir.server_dir(),
            dir.shell_dir(),
        ] {
            create_private_dir(&path)?;
        }
        Ok(dir)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn db_path(&self) -> PathBuf {
        self.root.join("skywave.db")
    }

    pub fn blobs_dir(&self) -> PathBuf {
        self.root.join("blobs")
    }

    pub fn ca_dir(&self) -> PathBuf {
        self.root.join("ca")
    }

    pub fn server_dir(&self) -> PathBuf {
        self.root.join("server")
    }

    /// Where a running shell command's output is written until it becomes a
    /// blob. Private: output can hold secrets.
    pub fn shell_dir(&self) -> PathBuf {
        self.root.join("shell")
    }
}

fn create_private_dir(path: &Path) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(path)
        .context(|| format!("creating {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_makes_layout_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("nested/data");
        let dir = DataDir::create(&root).unwrap();
        for sub in ["blobs", "ca", "server", "shell"] {
            assert!(root.join(sub).is_dir(), "{sub} missing");
        }
        assert_eq!(dir.db_path(), root.join("skywave.db"));

        std::fs::write(dir.ca_dir().join("keep"), b"x").unwrap();
        DataDir::create(&root).unwrap();
        assert!(dir.ca_dir().join("keep").exists());
    }

    #[test]
    fn a_second_serve_lock_is_refused_until_the_first_drops() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = DataDir::create(tmp.path().join("d")).unwrap();
        let first = dir.lock_for_serve().unwrap();
        // Another handle on the same dir, as a second process would have.
        let again = DataDir::create(dir.root()).unwrap();
        match again.lock_for_serve() {
            Err(Error::DataDirInUse { path }) => assert_eq!(path, dir.root()),
            other => panic!("expected DataDirInUse, got {other:?}"),
        }
        drop(first);
        again.lock_for_serve().unwrap();
    }

    #[test]
    fn admin_commands_work_while_serve_holds_the_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("d");
        let _serving = crate::Storage::open(&root)
            .unwrap()
            .dir
            .lock_for_serve()
            .unwrap();
        // What `skywaved pair` and `devices list` do, beside the daemon.
        let storage = crate::Storage::open(&root).unwrap();
        let issued = crate::pairing::issue_code(
            storage.db.conn(),
            chrono::Utc::now(),
            chrono::Duration::minutes(5),
        );
        assert!(issued.is_ok(), "{issued:?}");
        assert!(crate::devices::list(storage.db.conn()).unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn key_dirs_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = DataDir::create(tmp.path().join("d")).unwrap();
        for path in [dir.ca_dir(), dir.server_dir()] {
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "{} is {mode:o}", path.display());
        }
    }
}
