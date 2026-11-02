//! The rinf bridge between the skywave core and the Flutter GUI: turns Dart
//! signals into core actions and core snapshots into Rust signals. It holds
//! no business logic: what the head decides is skywave-present's, driven
//! the way the TUI drives it. Entry point of the Rust side of the app.
//!
//! - [`signals`]: the Dart contract.
//! - `head`: snapshots, intents and results in; signals and actions out.
//! - `link`, `screen`, `timeline`, `search`, `clip`, `intents`: the
//!   mapping, one feature each.
//! - `run`: the hub task's `select!` loop; `boot`, `dispatcher`: opening
//!   the core and dispatching to it in order.
//! - `lifecycle`: what the app leaving and returning to the screen does to
//!   the core, per platform.

mod boot;
mod clip;
pub mod dispatcher;
mod head;
mod intents;
mod lifecycle;
mod link;
mod logging;
mod run;
mod screen;
mod search;
mod shell;
mod signals;
mod timeline;

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
