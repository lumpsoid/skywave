//! Routes `tracing` events (the hub's and the core's) to stderr, or to
//! logcat on Android, where stderr goes nowhere. Filter with `RUST_LOG`;
//! the default is `info`.

use std::sync::Once;

use tracing_subscriber::EnvFilter;

/// Filter used when `RUST_LOG` is unset or does not parse.
const DEFAULT_FILTER: &str = "info";

static INSTALLED: Once = Once::new();

/// Installs the subscriber; later calls (a hot restart's `main`) do nothing.
pub fn init() {
    INSTALLED.call_once(|| {
        let filter =
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));
        install(filter);
    });
}

#[cfg(not(target_os = "android"))]
fn install(filter: EnvFilter) {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

/// Logcat stamps time, level and tag itself.
#[cfg(target_os = "android")]
fn install(filter: EnvFilter) {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(logcat::MakeLogcatWriter)
        .with_ansi(false)
        .without_time()
        .with_level(false)
        .try_init();
}

/// Writes each event to logcat through liblog.
#[cfg(target_os = "android")]
mod logcat {
    use std::ffi::{CString, c_char, c_int};
    use std::io;

    use tracing::{Level, Metadata};
    use tracing_subscriber::fmt::MakeWriter;

    const TAG: &std::ffi::CStr = c"skywave-gui";

    #[link(name = "log")]
    unsafe extern "C" {
        fn __android_log_write(prio: c_int, tag: *const c_char, text: *const c_char) -> c_int;
    }

    pub struct MakeLogcatWriter;

    impl<'a> MakeWriter<'a> for MakeLogcatWriter {
        type Writer = LogcatWriter;

        fn make_writer(&'a self) -> LogcatWriter {
            LogcatWriter::new(4)
        }

        fn make_writer_for(&'a self, meta: &Metadata<'_>) -> LogcatWriter {
            // `android/log.h` priorities.
            LogcatWriter::new(match *meta.level() {
                Level::TRACE => 2,
                Level::DEBUG => 3,
                Level::INFO => 4,
                Level::WARN => 5,
                Level::ERROR => 6,
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
            text.retain(|&b| b != 0);
            if text.is_empty() {
                return;
            }
            let Ok(text) = CString::new(text) else { return };
            // SAFETY: both pointers are valid NUL-terminated strings that
            // outlive the call; liblog copies them.
            unsafe {
                __android_log_write(self.prio, TAG.as_ptr(), text.as_ptr());
            }
        }
    }
}
