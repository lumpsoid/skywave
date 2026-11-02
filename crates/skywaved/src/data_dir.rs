//! Layout of the daemon's data directory (spec §5).
//!
//! ```text
//! <data_dir>/
//!   skywave.db   SQLite database (see `db`)
//!   blobs/       blob files named by BlobId (see `blobs`)
//!   ca/          CA key and certificate (see `identity`)
//!   server/      server key and certificate (see `identity`)
//!   server_id    the stable ServerId (see `identity`)
//!   shell/       output of running shell commands (see `shell`)
//! ```
//!
//! Directories are created with mode 0700 on Unix: `ca/` and `server/` hold
//! private keys.

use std::path::{Path, PathBuf};

use crate::error::{IoContext, Result};

/// A data directory whose subdirectories exist.
#[derive(Debug, Clone)]
pub struct DataDir {
    root: PathBuf,
}

impl DataDir {
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
