//! `tracing` setup: stderr by default, or append to a file (spec §9).
//!
//! The filter comes from `RUST_LOG` (e.g. `skywaved=debug`), defaulting to
//! `info`.

use std::fs::OpenOptions;
use std::path::Path;
use std::sync::Mutex;

use tracing_subscriber::EnvFilter;

use crate::error::{Error, IoContext, Result};

/// Filter used when `RUST_LOG` is unset.
pub const DEFAULT_FILTER: &str = "info";

/// Installs the global subscriber. Logs go to `file` (appended, no ANSI
/// colours) if given, else to stderr. Fails if a subscriber is already set.
pub fn init(file: Option<&Path>) -> Result<()> {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    let result = match file {
        Some(path) => {
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .context(|| format!("opening log file {}", path.display()))?;
            builder
                .with_ansi(false)
                .with_writer(Mutex::new(file))
                .try_init()
        }
        None => builder.with_writer(std::io::stderr).try_init(),
    };
    result.map_err(|e| Error::Logging(e.to_string()))
}
