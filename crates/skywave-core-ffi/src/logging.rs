//! Routes the core's `tracing` events to the platform log.
//!
//! On Android each event becomes one logcat line under tag [`LOGCAT_TAG`],
//! at the priority matching its level. Elsewhere events go to stderr. The
//! subscriber is installed by the first [`init_logging`] call; later calls
//! do nothing.

use std::sync::OnceLock;

use tracing_subscriber::EnvFilter;

/// Filter used when the caller passes none, or one that does not parse.
pub const DEFAULT_FILTER: &str = "info";

/// Logcat tag every line is written under.
pub const LOGCAT_TAG: &str = "skywave";

static INSTALLED: OnceLock<()> = OnceLock::new();

/// Installs the global `tracing` subscriber: logcat on Android, stderr
/// elsewhere. `filter` is an `EnvFilter` directive such as
/// `"skywave_core=debug,info"`; `None` or an unparsable one means
/// [`DEFAULT_FILTER`]. Only the first call has an effect, and it yields
/// quietly if the host process already set a subscriber.
#[uniffi::export]
pub fn init_logging(filter: Option<String>) {
    INSTALLED.get_or_init(|| {
        let requested = filter.as_deref().unwrap_or(DEFAULT_FILTER);
        let (filter, bad) = match EnvFilter::try_new(requested) {
            Ok(f) => (f, None),
            Err(e) => (EnvFilter::new(DEFAULT_FILTER), Some(e)),
        };
        if install(filter)
            && let Some(e) = bad
        {
            tracing::warn!("ignoring log filter {requested:?} ({e}); using {DEFAULT_FILTER:?}");
        }
    });
}

/// Sets the global subscriber; false if one was already set.
#[cfg(not(target_os = "android"))]
fn install(filter: EnvFilter) -> bool {
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init()
        .is_ok()
}

/// Sets the global subscriber; false if one was already set. Logcat stamps
/// time, level and tag itself, so the line carries only target and message.
#[cfg(target_os = "android")]
fn install(filter: EnvFilter) -> bool {
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(logcat::MakeLogcatWriter)
        .with_ansi(false)
        .without_time()
        .with_level(false)
        .try_init()
        .is_ok()
}

#[cfg(target_os = "android")]
mod logcat {
    use std::ffi::{CString, c_char, c_int};
    use std::io;

    use tracing::{Level, Metadata};
    use tracing_subscriber::fmt::MakeWriter;

    // `android/log.h` priorities.
    const VERBOSE: c_int = 2;
    const DEBUG: c_int = 3;
    const INFO: c_int = 4;
    const WARN: c_int = 5;
    const ERROR: c_int = 6;

    #[link(name = "log")]
    unsafe extern "C" {
        fn __android_log_write(prio: c_int, tag: *const c_char, text: *const c_char) -> c_int;
    }

    /// Hands the fmt layer one [`LogcatWriter`] per event.
    pub struct MakeLogcatWriter;

    impl<'a> MakeWriter<'a> for MakeLogcatWriter {
        type Writer = LogcatWriter;

        fn make_writer(&'a self) -> LogcatWriter {
            LogcatWriter::new(INFO)
        }

        fn make_writer_for(&'a self, meta: &Metadata<'_>) -> LogcatWriter {
            LogcatWriter::new(match *meta.level() {
                Level::TRACE => VERBOSE,
                Level::DEBUG => DEBUG,
                Level::INFO => INFO,
                Level::WARN => WARN,
                Level::ERROR => ERROR,
            })
        }
    }

    /// Buffers one formatted event and writes it to logcat when dropped.
    pub struct LogcatWriter {
        prio: c_int,
        buf: Vec<u8>,
    }

    impl LogcatWriter {
        fn new(prio: c_int) -> Self {
            Self {
                prio,
                buf: Vec::new(),
            }
        }
    }

    impl io::Write for LogcatWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.buf.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Drop for LogcatWriter {
        fn drop(&mut self) {
            let mut text = std::mem::take(&mut self.buf);
            while text.last() == Some(&b'\n') {
                text.pop();
            }
            if text.is_empty() {
                return;
            }
            // An interior NUL would cut the line short; logcat has no use for it.
            text.retain(|&b| b != 0);
            let text = CString::new(text).expect("NULs removed");
            let tag = CString::new(super::LOGCAT_TAG).expect("tag has no NUL");
            // SAFETY: both pointers are valid NUL-terminated strings that
            // outlive the call; liblog copies them.
            unsafe {
                __android_log_write(self.prio, tag.as_ptr(), text.as_ptr());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_logging_is_idempotent_and_tolerates_a_bad_filter() {
        init_logging(Some("skywave_core=loud".into()));
        init_logging(None);
        init_logging(Some("debug".into()));
        tracing::info!("logging initialised");
    }
}
