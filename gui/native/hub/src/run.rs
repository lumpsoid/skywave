//! The hub task: boots the core on `Boot`, then runs one `select!` loop
//! that owns the core, the [`Head`], the dispatcher's results and the
//! Dart receivers, until `main` tells it to stop. One task, so everything
//! it owns changes one step at a time, without locks. It decides nothing:
//! each arm hands its input to the head and carries out what comes back.

use rinf::{DartSignal, DartSignalPack, RustSignal, SignalReceiver};
use tokio::sync::oneshot;
use tokio::time::{Instant as TokioInstant, sleep_until};

use crate::boot::{self, Running};
use crate::head::{Clock, Head, Out};
use crate::signals::{Boot, Booted, GuiIntent, ScreenUpdate, ScreenView};

/// Waits for `Boot` and opens the core, answering `Booted`, or
/// `BootFailed` and waiting for the next `Boot`; then runs the loop. Returns
/// once `stop` fires (or its sender goes), with the core closed.
pub async fn boot_then_run(mut stop: oneshot::Receiver<()>) {
    // rinf keeps one receiver per signal type: these, handed on to the
    // loop, are the only ones. Intents sent before the loop are queued.
    let boot = Boot::get_dart_signal_receiver();
    let intents = GuiIntent::get_dart_signal_receiver();
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
                    Err(failed) => {
                        tracing::warn!(unreadable = failed.unreadable, "boot failed: {}", failed.message);
                        failed.send_signal_to_dart();
                    }
                }
            }
        }
    };
    run(running, boot, intents, stop).await;
}

async fn run(
    mut hub: Running,
    boot: SignalReceiver<DartSignalPack<Boot>>,
    intents: SignalReceiver<DartSignalPack<GuiIntent>>,
    mut stop: oneshot::Receiver<()>,
) {
    let mut head = Head::new();
    let mut shown = None;
    loop {
        // Read afresh each turn: what is due changes with every step.
        let wake = head.next_wake(Clock::read()).map(TokioInstant::from_std);
        let outs = tokio::select! {
            _ = &mut stop => break,
            // Dart lost its state (a retry racing the answer): the core is
            // open already, so it is not opened again.
            Some(_) = boot.recv() => {
                Booted.send_signal_to_dart();
                continue;
            }
            Some(pack) = intents.recv() => head.on_intent(pack.message, Clock::read()),
            Some(mut view) = hub.views.recv() => {
                // Snapshots are whole: only the newest waiting one matters.
                while let Ok(newer) = hub.views.try_recv() {
                    view = newer;
                }
                head.on_view(view, Clock::read())
            }
            Some(done) = hub.results.recv() => head.on_dispatched(done, Clock::read()),
            () = sleep_until(wake.unwrap_or_else(TokioInstant::now)), if wake.is_some() => {
                head.on_wake(Clock::read())
            }
        };
        perform(outs, &hub, &mut shown);
    }
    // The one way out of the loop: the unsaved draft and read mark are
    // queued now, and closing applies everything queued before shutdown.
    perform(head.on_stop(Clock::read()), &hub, &mut shown);
    hub.close();
}

/// Carries out a step's outputs in order: signals to Dart, actions to the
/// dispatcher, suspend and resume to the core (neither blocks: each only
/// flips the engine's flag).
fn perform(outs: Vec<Out>, hub: &Running, shown: &mut Option<&'static str>) {
    for out in outs {
        match out {
            Out::Link(update) => update.send_signal_to_dart(),
            Out::Screen(update) => {
                let kind = screen_kind(&update);
                if *shown != Some(kind) {
                    tracing::info!(screen = kind, "screen update");
                    *shown = Some(kind);
                }
                update.send_signal_to_dart();
            }
            Out::Timeline(update) => update.send_signal_to_dart(),
            Out::Shell(update) => update.send_signal_to_dart(),
            Out::Search(update) => update.send_signal_to_dart(),
            Out::Effect(effect) => effect.send_signal_to_dart(),
            Out::Dispatch(tag, action) => {
                tracing::debug!(tag, ?action, "dispatch");
                hub.dispatcher.send(tag, action);
            }
            Out::Suspend => {
                tracing::info!("app backgrounded: suspending the engine");
                hub.core.suspend();
            }
            Out::Resume => {
                tracing::info!("app foregrounded: resuming the engine");
                hub.core.resume();
            }
        }
    }
}

fn screen_kind(update: &ScreenUpdate) -> &'static str {
    match update.screen {
        ScreenView::Pairing { .. } => "pairing",
        ScreenView::SessionList { .. } => "session list",
        ScreenView::Session { .. } => "session",
        ScreenView::Settings { .. } => "settings",
        ScreenView::Usage { .. } => "usage",
        ScreenView::Workdir { .. } => "workdir",
    }
}
