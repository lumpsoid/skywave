//! The hub task: boots the core on `Boot`, then runs one `select!` loop
//! that owns the core, its snapshots, the dispatcher's results and the
//! `Boot` receiver, until `main` tells it to stop. One task, so everything
//! it owns changes one step at a time, without locks. It decides nothing.

use rinf::{DartSignal, DartSignalPack, RustSignal, SignalReceiver};
use tokio::sync::oneshot;

use crate::boot::{self, Running};
use crate::signals::{Boot, BootFailed, Booted};

/// Waits for `Boot` and opens the core, answering `Booted`, or
/// `BootFailed` and waiting for the next `Boot`; then runs the loop. Returns
/// once `stop` fires (or its sender goes), with the core closed.
pub async fn boot_then_run(mut stop: oneshot::Receiver<()>) {
    // rinf keeps one receiver per signal type: this one, handed on to the
    // loop, answers a later `Boot` too.
    let boot = Boot::get_dart_signal_receiver();
    let running = loop {
        tokio::select! {
            _ = &mut stop => return,
            pack = boot.recv() => {
                let Some(pack) = pack else { return };
                match boot::open(pack.message).await {
                    Ok(running) => {
                        Booted.send_signal_to_dart();
                        break running;
                    }
                    Err(message) => {
                        tracing::warn!("boot failed: {message}");
                        BootFailed { message }.send_signal_to_dart();
                    }
                }
            }
        }
    };
    run(running, boot, stop).await;
}

async fn run(
    mut hub: Running,
    boot: SignalReceiver<DartSignalPack<Boot>>,
    mut stop: oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            _ = &mut stop => break,
            // Dart lost its state (a retry racing the answer): the core is
            // open already, so it is not opened again.
            Some(_) = boot.recv() => Booted.send_signal_to_dart(),
            Some(view) = hub.views.recv() => {
                tracing::debug!(link = ?view.link, queued = view.queued, "snapshot");
            }
            Some(done) = hub.results.recv() => {
                tracing::debug!(tag = done.tag, ok = done.result.is_ok(), "dispatched");
            }
        }
    }
    hub.close();
}
