//! Opening the core for a `Boot`, and closing it again at shutdown.
//!
//! One core per process (D17). rinf restarts the whole Rust `main` on a
//! Flutter hot restart: the old runtime sees `dart_shutdown` while the new
//! one already waits for `Boot`. [`CORE_SLOT`] makes the new core wait until
//! the old one has closed, so two engines never share the data dir.

use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use skywave_core::{Core, CoreError, EngineConfig, ShutdownError, SubscriptionId, ViewState};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tokio::sync::{Mutex, MutexGuard, oneshot};

use crate::dispatcher::{Dispatcher, Report};
use crate::prefs::{self, Prefs};
use crate::signals::{Boot, BootFailed};

/// The core's data dir under the directory Dart sends.
pub const DATA_DIR: &str = "skywave";

/// How long shutdown waits for engine work and a running callback.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Held by whoever has a core open in this process, until it is closed.
static CORE_SLOT: Mutex<()> = Mutex::const_new(());

/// An open core, wired to the hub task: snapshots arrive on `views`,
/// actions go through `dispatcher`, their outcomes arrive on `results`, and
/// the core's own failure ([`Core::on_failure`]) on `failed`.
pub struct Running {
    pub core: Core,
    pub dispatcher: Dispatcher,
    pub views: UnboundedReceiver<ViewState>,
    pub results: UnboundedReceiver<Report>,
    pub failed: oneshot::Receiver<String>,
    /// The head's preferences as read at boot.
    pub prefs: Prefs,
    /// Where they are written back: beside the core's data dir.
    pub prefs_path: PathBuf,
    /// The core's data dir, under which the attachments are stored.
    pub data_dir: PathBuf,
    subscription: SubscriptionId,
    slot: MutexGuard<'static, ()>,
}

/// Opens the core in `<boot.dir>/skywave` under the device name Dart sent,
/// first resetting its replica if Dart asked, subscribes to its snapshots
/// and starts the dispatcher, and reads the head's preferences from
/// `<boot.dir>/skywave-gui.json`. The error is what Dart shows.
pub async fn open(boot: Boot) -> Result<Running, BootFailed> {
    let failed = |message: String| BootFailed {
        message,
        unreadable: false,
    };
    let slot = CORE_SLOT.lock().await;
    let mut opening = Opening {
        core: None,
        dispatcher: None,
    };
    let dir = Path::new(&boot.dir).join(DATA_DIR);
    let data_dir = dir.clone();
    let prefs_path = prefs::path_in(Path::new(&boot.dir));
    let config = EngineConfig {
        device_name: Some(boot.device_name),
        ..EngineConfig::default()
    };
    let reset = boot.reset_replica;
    tracing::info!(dir = %dir.display(), reset, "opening the core");
    // Opening reads SQLite and builds the engine's runtime, which must not
    // be dropped (on failure) on a runtime thread.
    let opened = tokio::task::spawn_blocking(move || {
        if reset {
            Core::reset_replica(&dir)?;
        }
        Core::open_with(&dir, config)
    })
    .await;
    let opened = match opened {
        Ok(opened) => opened,
        // A panic is the hub's, not the data's: it goes on to the caller,
        // which answers `HubFailed`.
        Err(err) if err.is_panic() => std::panic::resume_unwind(err.into_panic()),
        Err(err) => return Err(failed(format!("Opening the data failed: {err}"))),
    };
    let core = match opened {
        Ok(core) => core,
        Err(err @ CoreError::ReplicaUnreadable { .. }) => {
            return Err(BootFailed {
                message: format!("Cannot open the data: {err}"),
                unreadable: true,
            });
        }
        Err(err) => return Err(failed(format!("Cannot open the data: {err}"))),
    };

    // From here on, leaving early (an error, or a panic unwinding) closes
    // the core: `opening` drops after every other local but `slot`.
    opening.core = Some(core.clone());
    let (results_tx, results) = unbounded_channel();
    let dispatcher = Dispatcher::start(core.clone(), results_tx)
        .map_err(|err| failed(format!("Cannot start the dispatcher: {err}")))?;
    opening.dispatcher = Some(dispatcher);
    let (views_tx, views) = unbounded_channel();
    // Runs on the core's `skywave-deliver` thread: hand the snapshot on and
    // nothing else. Once the hub task is gone the send fails, harmlessly.
    let subscription = core.subscribe(move |view| {
        let _ = views_tx.send(view);
    });
    // Runs on the thread that failed; a send never blocks.
    let (core_failed_tx, core_failed) = oneshot::channel();
    core.on_failure(move |message| {
        let _ = core_failed_tx.send(message);
    });
    #[cfg(test)]
    tests::panic_if_asked();
    let Some(dispatcher) = opening.disarm() else {
        // Not reached: it was set above.
        return Err(failed("Cannot start the dispatcher".into()));
    };
    tracing::info!("core open");
    Ok(Running {
        core,
        dispatcher,
        views,
        results,
        failed: core_failed,
        prefs: prefs::load(&prefs_path),
        prefs_path,
        data_dir,
        subscription,
        slot,
    })
}

/// What [`open`] has built so far, closed if dropped before it is handed on
/// to [`Running`]: a failed open, or a panic in it, must not leave the data
/// dir locked by a core nobody closes (dropping a `Core` only stops it in
/// the background), or the next `Boot` fails with "in use".
struct Opening {
    core: Option<Core>,
    dispatcher: Option<Dispatcher>,
}

impl Opening {
    /// Hands the dispatcher on and lets go of the core: closing both is
    /// [`Running::close`]'s job from now on. `None`, and still armed, if
    /// there is no dispatcher yet.
    fn disarm(&mut self) -> Option<Dispatcher> {
        let dispatcher = self.dispatcher.take()?;
        self.core = None;
        Some(dispatcher)
    }
}

impl Drop for Opening {
    fn drop(&mut self) {
        if let Some(core) = self.core.take() {
            tracing::warn!("closing a core that did not finish opening");
            log_closed(close_core(core, self.dispatcher.take(), None));
        }
    }
}

impl Running {
    /// Stops snapshots, applies every action still queued, then shuts the
    /// core down, on a plain thread: `Core::shutdown` refuses runtime
    /// threads. Blocks the caller until done; failures are logged.
    pub fn close(self) {
        let Running {
            core,
            dispatcher,
            views,
            results,
            failed,
            prefs: _,
            prefs_path: _,
            data_dir: _,
            subscription,
            slot,
        } = self;
        drop((views, results, failed));
        log_closed(close_core(core, Some(dispatcher), Some(subscription)));
        // Only now may another runtime in this process open the core.
        drop(slot);
    }
}

/// Stops `subscription`'s snapshots, lets `dispatcher` apply what is still
/// queued, then shuts `core` down, on a plain thread: `Core::shutdown`
/// refuses runtime threads. Blocks until done.
fn close_core(
    core: Core,
    dispatcher: Option<Dispatcher>,
    subscription: Option<SubscriptionId>,
) -> Result<(), CloseError> {
    on_plain_thread(move || {
        if let Some(subscription) = subscription {
            core.unsubscribe(subscription);
        }
        if let Some(dispatcher) = dispatcher {
            dispatcher.stop();
        }
        core.shutdown(SHUTDOWN_TIMEOUT)
    })?
    .map_err(CloseError::Core)
}

fn log_closed(closed: Result<(), CloseError>) {
    match closed {
        Ok(()) => tracing::info!("core closed"),
        Err(err) => tracing::error!("closing the core: {err}"),
    }
}

/// Why closing the core failed.
#[derive(Debug)]
pub enum CloseError {
    /// No thread could be started to close it on; it was dropped instead,
    /// which stops the engine in the background.
    Spawn(std::io::Error),
    /// The closing thread panicked.
    Panicked,
    Core(ShutdownError),
}

impl std::fmt::Display for CloseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CloseError::Spawn(err) => write!(f, "cannot start the closing thread: {err}"),
            CloseError::Panicked => write!(f, "the closing thread panicked"),
            CloseError::Core(err) => write!(f, "{err}"),
        }
    }
}

/// Runs `work` on a new thread outside any runtime and waits for it. A
/// blocking-pool thread would not do: it counts as inside the runtime.
fn on_plain_thread<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, CloseError> {
    thread::Builder::new()
        .name("skywave-hub-close".into())
        .spawn(work)
        .map_err(CloseError::Spawn)?
        .join()
        .map_err(|_| CloseError::Panicked)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
pub(crate) mod tests {
    use std::cell::Cell;

    use super::*;

    thread_local! {
        /// Makes the next [`open`] on this thread panic once the core, the
        /// dispatcher and the subscription are all up.
        pub(crate) static PANIC_WHILE_OPENING: Cell<bool> = const { Cell::new(false) };
    }

    pub(super) fn panic_if_asked() {
        if PANIC_WHILE_OPENING.with(|asked| asked.replace(false)) {
            panic!("injected panic while opening");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_from_a_runtime_thread_closes_the_core() {
        let dir = tempfile::tempdir().unwrap();
        let core = Core::open(dir.path()).unwrap();
        // Called here directly, shutdown would refuse.
        assert!(tokio::runtime::Handle::try_current().is_ok());
        close_core(core, None, None).unwrap();
        // Closed for good: the same dir opens again at once.
        let again = Core::open(dir.path()).unwrap();
        close_core(again, None, None).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn open_then_close_releases_the_slot() {
        let dir = tempfile::tempdir().unwrap();
        let running = open(boot_in(dir.path(), false)).await.unwrap();
        assert!(dir.path().join(DATA_DIR).is_dir());
        running.close();
        // A second open (a hot restart's) gets the slot and the same data.
        let running = open(boot_in(dir.path(), false)).await.unwrap();
        running.close();
    }

    /// The preferences sit beside the data dir, so a replica reset keeps
    /// them.
    #[tokio::test(flavor = "current_thread")]
    async fn open_reads_the_preferences_beside_the_data_even_after_a_reset() {
        let dir = tempfile::tempdir().unwrap();
        let running = open(boot_in(dir.path(), false)).await.unwrap();
        assert_eq!(running.prefs, Prefs::default(), "no file yet");
        assert_eq!(running.prefs_path, dir.path().join(prefs::PREFS_FILE));
        assert!(!running.prefs_path.starts_with(dir.path().join(DATA_DIR)));
        let on = Prefs {
            stay_connected: true,
        };
        prefs::save(&running.prefs_path, &on).unwrap();
        running.close();

        let running = open(boot_in(dir.path(), true)).await.unwrap();
        assert_eq!(running.prefs, on);
        running.close();
    }

    fn boot_in(dir: &Path, reset_replica: bool) -> Boot {
        Boot {
            dir: dir.display().to_string(),
            device_name: "test device".into(),
            reset_replica,
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_failed_open_is_a_message() {
        let dir = tempfile::tempdir().unwrap();
        // The data dir's place is taken by a file.
        std::fs::write(dir.path().join(DATA_DIR), b"").unwrap();
        let err = open(boot_in(dir.path(), false)).await.err().unwrap();
        assert!(err.message.starts_with("Cannot open the data"), "{err:?}");
        assert!(!err.unreadable, "a reset would not help");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unreadable_data_fails_as_such_until_a_boot_resets_it() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join(DATA_DIR);
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join(skywave_core::store::DB_FILE), b"not sqlite").unwrap();
        let err = open(boot_in(dir.path(), false)).await.err().unwrap();
        assert!(err.unreadable, "{err:?}");
        // A retry alone fails the same way.
        assert!(
            open(boot_in(dir.path(), false))
                .await
                .err()
                .unwrap()
                .unreadable
        );
        let running = open(boot_in(dir.path(), true)).await.unwrap();
        running.close();
    }
}
