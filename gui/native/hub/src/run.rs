//! The hub task: boots the core on `Boot`, then runs one `select!` loop
//! that owns the core, the [`Head`], the dispatcher's results and the
//! Dart receivers, until `main` tells it to stop. One task, so everything
//! it owns changes one step at a time, without locks. It decides nothing:
//! each arm hands its input to the head and carries out what comes back.
//!
//! A panic in the task (an index out of range, an overflow: clippy denies
//! only the explicit kinds) is caught where it happens: the core is closed,
//! Dart gets `HubFailed`, and the task waits for the next `Boot` to open
//! the core again with a fresh head.

use std::any::Any;
use std::future::{Future, poll_fn};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::pin;
use std::task::Poll;

use rinf::{DartSignal, DartSignalPack, RustSignal, SignalReceiver};
use tokio::sync::oneshot;
use tokio::time::{Instant as TokioInstant, sleep_until};

use crate::boot::{self, Running};
use crate::head::{Clock, Head, Out};
use crate::signals::{Boot, Booted, GuiIntent, HubFailed, ScreenUpdate, ScreenView};

/// Waits for `Boot` and opens the core, answering `Booted`, or
/// `BootFailed` and waiting for the next `Boot`; then runs the loop. After
/// a panic, answers `HubFailed` and waits for `Boot` again. Returns once
/// `stop` fires (or its sender goes), with the core closed.
pub async fn boot_then_run(stop: oneshot::Receiver<()>) {
    let mut stop = Stop::new(stop);
    // rinf keeps one receiver per signal type: these, handed on to the
    // loop, are the only ones. Intents sent before the loop are queued.
    let boot = Boot::get_dart_signal_receiver();
    let intents = GuiIntent::get_dart_signal_receiver();
    loop {
        let failed = match catching(open_on_boot(&boot, &mut stop)).await {
            Ok(None) => return,
            Ok(Some(hub)) => match serve(hub, &boot, &intents, &mut stop).await {
                Served::Stopped => return,
                Served::Failed(failed) => failed,
            },
            Err(message) => {
                tracing::error!("hub panicked while booting: {message}");
                HubFailed { message }
            }
        };
        failed.send_signal_to_dart();
    }
}

/// `main`'s stop, which stays fired once it has: a panic may end the loop
/// after it fired, and the boot that follows must see it too.
struct Stop {
    receiver: oneshot::Receiver<()>,
    fired: bool,
}

impl Stop {
    fn new(receiver: oneshot::Receiver<()>) -> Stop {
        Stop {
            receiver,
            fired: false,
        }
    }

    /// Resolves once `main` stops the hub (or is gone), and at once from
    /// then on. Cancel-safe.
    async fn fired(&mut self) {
        if !self.fired {
            let _ = (&mut self.receiver).await;
            self.fired = true;
        }
    }
}

/// Waits for `Boot` and opens the core, answering `Booted`, or
/// `BootFailed` and waiting for the next `Boot`. `None`: stopped first.
async fn open_on_boot(
    boot: &SignalReceiver<DartSignalPack<Boot>>,
    stop: &mut Stop,
) -> Option<Running> {
    loop {
        tokio::select! {
            () = stop.fired() => return None,
            pack = boot.recv() => {
                match boot::open(pack?.message).await {
                    Ok(running) => {
                        Booted.send_signal_to_dart();
                        return Some(running);
                    }
                    Err(failed) => {
                        tracing::warn!(unreadable = failed.unreadable, "boot failed: {}", failed.message);
                        failed.send_signal_to_dart();
                    }
                }
            }
        }
    }
}

/// How serving an open core ended.
enum Served {
    Stopped,
    Failed(HubFailed),
}

/// Runs the loop over `hub` until `stop` fires, then closes the core. A
/// panic ends the loop where it happens, and the core is closed all the
/// same: actions dispatched before it are applied, but nothing is asked of
/// the head that panicked, so the stop's last draft save and read mark are
/// not made. Closing releases the core's slot and data dir, so the next
/// `Boot` opens it again.
async fn serve(
    mut hub: Running,
    boot: &SignalReceiver<DartSignalPack<Boot>>,
    intents: &SignalReceiver<DartSignalPack<GuiIntent>>,
    stop: &mut Stop,
) -> Served {
    let ran = catching(run(&mut hub, boot, intents, stop)).await;
    hub.close();
    match ran {
        Ok(()) => Served::Stopped,
        Err(message) => {
            tracing::error!("hub panicked: {message}");
            Served::Failed(HubFailed { message })
        }
    }
}

async fn run(
    hub: &mut Running,
    boot: &SignalReceiver<DartSignalPack<Boot>>,
    intents: &SignalReceiver<DartSignalPack<GuiIntent>>,
    stop: &mut Stop,
) {
    let mut head = Head::new();
    let mut shown = None;
    loop {
        // Read afresh each turn: what is due changes with every step.
        let wake = head.next_wake(Clock::read()).map(TokioInstant::from_std);
        let outs = tokio::select! {
            () = stop.fired() => break,
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
        #[cfg(test)]
        tests::panic_if_asked();
        perform(outs, hub, &mut shown);
    }
    // The one way out of the loop: the unsaved draft and read mark are
    // queued now, and closing applies everything queued before shutdown.
    perform(head.on_stop(Clock::read()), hub, &mut shown);
}

/// Runs `work`, turning a panic in it into the panic's message. The future
/// is dropped where it panicked, so what it borrowed is intact for the
/// caller to close.
async fn catching<T>(work: impl Future<Output = T>) -> Result<T, String> {
    let mut work = pin!(work);
    poll_fn(
        |cx| match catch_unwind(AssertUnwindSafe(|| work.as_mut().poll(cx))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(done)) => Poll::Ready(Ok(done)),
            Err(payload) => Poll::Ready(Err(panic_message(payload.as_ref()))),
        },
    )
    .await
}

/// What `panic!` was given, when it was text, as it nearly always is.
fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_owned()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "a panic without a message".to_owned()
    }
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::cell::Cell;
    use std::path::Path;
    use std::time::Duration;

    use super::*;

    thread_local! {
        /// Makes the hub's next step panic, after the head has taken its
        /// input: the test's own thread, as a current-thread runtime runs
        /// the loop on it.
        static PANIC_NEXT_STEP: Cell<bool> = const { Cell::new(false) };
    }

    pub(super) fn panic_if_asked() {
        if PANIC_NEXT_STEP.with(|asked| asked.replace(false)) {
            panic!("injected panic");
        }
    }

    fn boot_in(dir: &Path) -> Boot {
        Boot {
            dir: dir.display().to_string(),
            device_name: "test device".into(),
            reset_replica: false,
        }
    }

    /// Opens the core in `dir`, failing rather than waiting forever if the
    /// last one still holds the slot.
    async fn reopen(dir: &Path) -> Running {
        tokio::time::timeout(Duration::from_secs(10), boot::open(boot_in(dir)))
            .await
            .expect("the last core released its slot")
            .unwrap()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_panicking_step_fails_the_hub_and_a_reboot_opens_the_core() {
        let dir = tempfile::tempdir().unwrap();
        let boot = Boot::get_dart_signal_receiver();
        let intents = GuiIntent::get_dart_signal_receiver();
        let (_running, never) = oneshot::channel();
        let mut stop = Stop::new(never);

        // The core's first snapshot is the step that panics.
        let hub = reopen(dir.path()).await;
        PANIC_NEXT_STEP.with(|asked| asked.set(true));
        let Served::Failed(failed) = serve(hub, &boot, &intents, &mut stop).await else {
            panic!("the hub stopped without failing");
        };
        assert_eq!(failed.message, "injected panic");

        // The core was closed: the slot is free and the data dir unlocked,
        // and a fresh hub runs over the reopened core until stopped.
        let hub = reopen(dir.path()).await;
        let (stopping, stopped) = oneshot::channel();
        stopping.send(()).unwrap();
        let mut stop = Stop::new(stopped);
        assert!(matches!(
            serve(hub, &boot, &intents, &mut stop).await,
            Served::Stopped
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_stop_stays_fired() {
        let (stopping, stopped) = oneshot::channel();
        let mut stop = Stop::new(stopped);
        stopping.send(()).unwrap();
        stop.fired().await;
        // Polling the receiver again would panic: a boot after a panic
        // during the stop must still see it.
        stop.fired().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_panic_with_a_formatted_message_keeps_it() {
        let caught = catching(async { panic!("index {} out of range", 3) }).await;
        assert_eq!(caught.err().unwrap(), "index 3 out of range");
        assert_eq!(catching(async { 7 }).await.unwrap(), 7);
    }
}
