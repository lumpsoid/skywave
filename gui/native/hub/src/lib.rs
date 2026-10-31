//! The rinf bridge between the skywave core and the Flutter GUI: turns Dart
//! signals into core actions and core snapshots into Rust signals. It holds
//! no business logic. Entry point of the Rust side of the app.

mod boot;
pub mod dispatcher;
mod logging;
mod run;
mod signals;

use rinf::{dart_shutdown, write_interface};
use tokio::sync::oneshot;

write_interface!();

/// Runs until Dart shuts down (app exit, or a hot restart, which starts a
/// fresh `main`), then stops the hub task and waits for it to close the
/// core.
#[tokio::main(flavor = "current_thread")]
async fn main() {
    logging::init();
    let (stop, stopped) = oneshot::channel();
    let hub = tokio::spawn(run::boot_then_run(stopped));
    dart_shutdown().await;
    tracing::info!("dart shut down; closing");
    let _ = stop.send(());
    if let Err(err) = hub.await {
        tracing::error!("hub task ended abnormally: {err}");
    }
}
