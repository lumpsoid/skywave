//! `tracing` setup: stderr by default, or append to a file (architecture.md "Testing").
//!
//! The filter comes from `RUST_LOG` (e.g. `skywaved=debug`), defaulting to
//! `info`. A log file is reopened at its path on [`LogFile::reopen`], which
//! `serve` calls on SIGHUP, so logrotate can move it and signal the daemon
//! instead of truncating it under the writer (`copytruncate`).

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

use crate::error::{Error, IoContext, Result};

/// Filter used when `RUST_LOG` is unset.
pub const DEFAULT_FILTER: &str = "info";

/// Installs the global subscriber. Logs go to `file` (appended, no ANSI
/// colours) if given, else to stderr; the returned [`LogFile`] reopens it.
/// Fails if a subscriber is already set.
pub fn init(file: Option<&Path>) -> Result<Option<LogFile>> {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    let (result, log_file) = match file {
        Some(path) => {
            let log_file = LogFile::open(path)?;
            let result = builder
                .with_ansi(false)
                .with_writer(log_file.clone())
                .try_init();
            (result, Some(log_file))
        }
        None => (builder.with_writer(std::io::stderr).try_init(), None),
    };
    result.map_err(|e| Error::Logging(e.to_string()))?;
    Ok(log_file)
}

/// A log file the subscriber appends to, reopenable at its path.
#[derive(Debug, Clone)]
pub struct LogFile {
    shared: Arc<Shared>,
}

#[derive(Debug)]
struct Shared {
    path: PathBuf,
    file: Mutex<File>,
}

impl LogFile {
    /// Opens (creating if needed) `path` for appending.
    pub fn open(path: &Path) -> Result<LogFile> {
        Ok(LogFile {
            shared: Arc::new(Shared {
                path: path.to_owned(),
                file: Mutex::new(append(path)?),
            }),
        })
    }

    /// Opens the file at the path again, creating it if it was moved away,
    /// and writes there from now on. On failure the writer keeps the file
    /// it had.
    pub fn reopen(&self) -> Result<()> {
        let file = append(&self.shared.path)?;
        *self.lock() = file;
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, File> {
        self.shared.file.lock().unwrap_or_else(|p| p.into_inner())
    }
}

fn append(path: &Path) -> Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .context(|| format!("opening log file {}", path.display()))
}

impl<'a> MakeWriter<'a> for LogFile {
    type Writer = Writer<'a>;

    fn make_writer(&'a self) -> Writer<'a> {
        Writer(self.lock())
    }
}

/// The log file, locked for one event.
pub struct Writer<'a>(MutexGuard<'a, File>);

impl Write for Writer<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reopen_writes_to_a_new_file_at_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("skywaved.log");
        let rotated = dir.path().join("skywaved.log.1");
        let log = LogFile::open(&path).unwrap();
        log.make_writer().write_all(b"before\n").unwrap();
        std::fs::rename(&path, &rotated).unwrap();
        log.make_writer().write_all(b"still before\n").unwrap();
        log.reopen().unwrap();
        log.make_writer().write_all(b"after\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(&rotated).unwrap(),
            "before\nstill before\n"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "after\n");
    }
}
