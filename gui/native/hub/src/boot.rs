//! Opening the core for a `Boot`, and closing it again at shutdown.
//!
//! One core per process (D17). rinf restarts the whole Rust `main` on a
//! Flutter hot restart: the old runtime sees `dart_shutdown` while the new
//! one already waits for `Boot`. [`CORE_SLOT`] makes the new core wait until
//! the old one has closed, so two engines never share the data dir.

use std::path::Path;
use std::thread;
use std::time::Duration;

use skywave_core::{Core, EngineConfig, ShutdownError, SubscriptionId, ViewState};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tokio::sync::{Mutex, MutexGuard};

use crate::dispatcher::{Dispatched, Dispatcher};
use crate::signals::Boot;

/// The core's data dir under the directory Dart sends.
pub const DATA_DIR: &str = "skywave";

/// How long shutdown waits for engine work and a running callback.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Held by whoever has a core open in this process, until it is closed.
static CORE_SLOT: Mutex<()> = Mutex::const_new(());

/// An open core, wired to the hub task: snapshots arrive on `views`,
/// actions go through `dispatcher`, their outcomes arrive on `results`.
pub struct Running {
    pub core: Core,
    pub dispatcher: Dispatcher,
    pub views: UnboundedReceiver<ViewState>,
    pub results: UnboundedReceiver<Dispatched>,
    subscription: SubscriptionId,
    slot: MutexGuard<'static, ()>,
}

/// Opens the core in `<boot.dir>/skywave` under the device name Dart sent,
/// subscribes to its snapshots and starts the dispatcher. The error is the
/// message Dart shows.
pub async fn open(boot: Boot) -> Result<Running, String> {
    let slot = CORE_SLOT.lock().await;
    let dir = Path::new(&boot.dir).join(DATA_DIR);
    let config = EngineConfig {
        device_name: Some(boot.device_name),
        ..EngineConfig::default()
    };
    tracing::info!(dir = %dir.display(), "opening the core");
    // Opening reads SQLite and builds the engine's runtime, which must not
    // be dropped (on failure) on a runtime thread.
    let core = tokio::task::spawn_blocking(move || Core::open_with(&dir, config))
        .await
        .map_err(|err| format!("Opening the data failed: {err}"))?
        .map_err(|err| format!("Cannot open the data: {err}"))?;

    let (results_tx, results) = unbounded_channel();
    let dispatcher = match Dispatcher::start(core.clone(), results_tx) {
        Ok(dispatcher) => dispatcher,
        Err(err) => {
            if let Err(close) = shutdown_core(core, SHUTDOWN_TIMEOUT) {
                tracing::error!("closing the core after a failed boot: {close}");
            }
            return Err(format!("Cannot start the dispatcher: {err}"));
        }
    };
    let (views_tx, views) = unbounded_channel();
    // Runs on the core's `skywave-deliver` thread: hand the snapshot on and
    // nothing else. Once the hub task is gone the send fails, harmlessly.
    let subscription = core.subscribe(move |view| {
        let _ = views_tx.send(view);
    });
    tracing::info!("core open");
    Ok(Running {
        core,
        dispatcher,
        views,
        results,
        subscription,
        slot,
    })
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
            subscription,
            slot,
        } = self;
        drop((views, results));
        let closed = on_plain_thread(move || {
            core.unsubscribe(subscription);
            dispatcher.stop();
            core.shutdown(SHUTDOWN_TIMEOUT)
        });
        match closed {
            Ok(Ok(())) => tracing::info!("core closed"),
            Ok(Err(err)) => tracing::error!("closing the core: {}", CloseError::Core(err)),
            Err(err) => tracing::error!("closing the core: {err}"),
        }
        // Only now may another runtime in this process open the core.
        drop(slot);
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

/// `Core::shutdown` on a fresh plain thread, waited for; safe to call from
/// a runtime thread, where `shutdown` itself returns `InsideRuntime`.
pub fn shutdown_core(core: Core, timeout: Duration) -> Result<(), CloseError> {
    on_plain_thread(move || core.shutdown(timeout))?.map_err(CloseError::Core)
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
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_from_a_runtime_thread_closes_the_core() {
        let dir = tempfile::tempdir().unwrap();
        let core = Core::open(dir.path()).unwrap();
        // Called here directly, shutdown would refuse.
        assert!(tokio::runtime::Handle::try_current().is_ok());
        shutdown_core(core, SHUTDOWN_TIMEOUT).unwrap();
        // Closed for good: the same dir opens again at once.
        let again = Core::open(dir.path()).unwrap();
        shutdown_core(again, SHUTDOWN_TIMEOUT).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn open_then_close_releases_the_slot() {
        let dir = tempfile::tempdir().unwrap();
        let boot = || Boot {
            dir: dir.path().display().to_string(),
            device_name: "test device".into(),
        };
        let running = open(boot()).await.unwrap();
        assert!(dir.path().join(DATA_DIR).is_dir());
        running.close();
        // A second open (a hot restart's) gets the slot and the same data.
        let running = open(boot()).await.unwrap();
        running.close();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_failed_open_is_a_message() {
        let dir = tempfile::tempdir().unwrap();
        // The data dir's place is taken by a file.
        std::fs::write(dir.path().join(DATA_DIR), b"").unwrap();
        let err = open(Boot {
            dir: dir.path().display().to_string(),
            device_name: "test device".into(),
        })
        .await
        .err()
        .unwrap();
        assert!(err.starts_with("Cannot open the data"), "{err}");
    }
}
