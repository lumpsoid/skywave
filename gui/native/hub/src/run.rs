//! The hub task: boots the core on `Boot`, then runs one `select!` loop
//! that owns the core, the [`Head`], the dispatcher's results and the
//! Dart receivers, until `main` tells it to stop. One task, so everything
//! it owns changes one step at a time, without locks. It decides nothing:
//! each arm hands its input to the head and carries out what comes back.
//!
//! A panic in the task (an index out of range, an overflow: clippy denies
//! only the explicit kinds), in opening the core, on the dispatch thread or
//! on the core's own threads ([`skywave_core::Core::on_failure`]) is caught
//! where it happens: the core is closed, Dart gets `HubFailed`,
//! and the task waits for the next `Boot` to open the core again with a
//! fresh head. Intents Dart sent before it saw `HubFailed` never reach that
//! head ([`Gate`]). Off screen with the background service holding the
//! link, nobody would see the Restart page, so the hub reopens the core
//! itself, once ([`Recovery`]).

use std::future::{Future, poll_fn};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::pin;
use std::task::Poll;

use rinf::{DartSignal, DartSignalPack, RustSignal, SignalReceiver};
use tokio::sync::oneshot;
use tokio::time::{Instant as TokioInstant, sleep_until};

use crate::boot::{self, Running};
use crate::dispatcher::Report;
use crate::head::{Clock, Head, Out};
use crate::lifecycle::{Awake, Holder};
use crate::notify;
use crate::panics::panic_message;
use crate::prefs;
use crate::signals::{
    Boot, Booted, GuiIntent, HubFailed, PlatformEffect, ScreenUpdate, ScreenView,
};

/// Waits for `Boot` and opens the core, answering `Booted`, or
/// `BootFailed` and waiting for the next `Boot`; then runs the loop. After
/// a panic, answers `HubFailed`, then reopens the core itself or waits for
/// `Boot` again, as [`Recovery`] says. Returns once `stop` fires (or its
/// sender goes), with the core closed.
pub async fn boot_then_run(stop: oneshot::Receiver<()>) {
    let mut stop = Stop::new(stop);
    // rinf keeps one receiver per signal type: these, handed on to the
    // loop, are the only ones. Intents sent before the loop are queued.
    let boot = Boot::get_dart_signal_receiver();
    let intents = GuiIntent::get_dart_signal_receiver();
    let mut gate = Gate::default();
    let mut recovery = Recovery::default();
    // A core the hub reopened itself after a failure.
    let mut reopened = None;
    loop {
        let hub = match reopened.take() {
            Some(hub) => hub,
            None => match catching(open_on_boot(&boot, &mut stop)).await {
                Ok(None) => return,
                Ok(Some((hub, booted))) => {
                    recovery.boot = Some(booted);
                    hub
                }
                Err(message) => {
                    tracing::error!("hub panicked while booting: {message}");
                    gate.failed(message).send_signal_to_dart();
                    continue;
                }
            },
        };
        match serve(hub, &boot, &intents, &mut gate, &mut recovery, &mut stop).await {
            Served::Stopped => return,
            Served::Failed(message) => gate.failed(message).send_signal_to_dart(),
        }
        reopened = recover(&mut recovery).await;
    }
}

/// What a hub failure does, from where the app was when it failed. On
/// screen, or off it with nothing holding the link, the user restarts it
/// (the Restart page). Off screen with the background service holding the
/// link, nobody sees that page while the service's notification still
/// says the link is open: the hub reopens the core itself, once. Failing
/// again before the app is back on screen, it stops the service and says
/// so in a notification, rather than restart a failure that repeats.
#[derive(Debug, Default)]
struct Recovery {
    /// The `Boot` that opened the core last; a restart opens it again.
    boot: Option<Boot>,
    /// The holders after the running head's last step, and the ones the
    /// next head starts with.
    holders: Awake,
    /// The hub restarted itself since the app was last on screen.
    restarted: bool,
}

/// What [`Recovery::failed`] decided.
#[derive(Debug)]
enum Recover {
    /// Wait for Dart's `Boot`.
    Wait,
    /// Open the core again with this `Boot`, without Dart asking.
    Restart(Boot),
    /// Stop the service and say the hub stopped; then wait for `Boot`.
    GiveUp,
}

impl Recovery {
    /// The running head's holders after a whole step: on screen, the next
    /// failure may restart again.
    fn stepped(&mut self, holders: Awake) {
        if holders.holds(Holder::Visible) {
            self.restarted = false;
        }
        self.holders = holders;
    }

    /// The hub failed. A restarted head starts with the holders the failed
    /// one had; any other starts on screen, as Dart's `Boot` follows the
    /// user's tap.
    fn failed(&mut self) -> Recover {
        let away = !self.holders.holds(Holder::Visible) && self.holders.holds(Holder::Service);
        let decided = match (away, self.restarted, &self.boot) {
            (false, ..) => Recover::Wait,
            (true, false, Some(boot)) => Recover::Restart(boot.clone()),
            (true, ..) => Recover::GiveUp,
        };
        match decided {
            Recover::Restart(_) => self.restarted = true,
            Recover::Wait | Recover::GiveUp => self.holders = Awake::default(),
        }
        decided
    }
}

/// Carries out what [`Recovery::failed`] decides: a core reopened, or
/// `None` to wait for Dart's `Boot`. A restart that cannot open the core
/// gives up.
async fn recover(recovery: &mut Recovery) -> Option<Running> {
    let boot = match recovery.failed() {
        Recover::Wait => return None,
        Recover::GiveUp => {
            give_up();
            return None;
        }
        Recover::Restart(boot) => boot,
    };
    tracing::warn!("hub failed off screen while the service holds the link: reopening the core");
    match catching(boot::open(boot)).await {
        Ok(Ok(hub)) => {
            Booted.send_signal_to_dart();
            Some(hub)
        }
        Ok(Err(failed)) => {
            tracing::warn!(
                unreadable = failed.unreadable,
                "restart failed: {}",
                failed.message
            );
            failed.send_signal_to_dart();
            give_up();
            None
        }
        Err(message) => {
            tracing::error!("hub panicked while restarting: {message}");
            give_up();
            None
        }
    }
}

/// Stops the background service, whose notification would claim a link
/// that is gone, and says the hub stopped. The Restart page waits for the
/// user; a successful boot takes the notification down.
fn give_up() {
    tracing::error!("hub failed off screen again: stopping the background service");
    PlatformEffect::BackgroundService { run: false }.send_signal_to_dart();
    PlatformEffect::Notify {
        id: notify::STOPPED,
        title: "Skywave stopped".to_owned(),
        body: "An internal error stopped it in the background. Open it to restart.".to_owned(),
        session_id: None,
    }
    .send_signal_to_dart();
}

/// Which intents reach the head. Dart sends intents and `Boot` on separate
/// channels, so only the intents themselves can say which head they were
/// for: after `HubFailed`, Dart answers with `HubFailedSeen` of its epoch,
/// and every intent queued before that marker is dropped.
#[derive(Debug, Default)]
struct Gate {
    /// Failures so far in this process; the last one's is the epoch.
    epoch: u32,
    /// The last failure's marker has not arrived yet.
    closed: bool,
}

impl Gate {
    /// The hub failed with `message`: what Dart is told, under a new epoch,
    /// and intents are dropped until Dart answers it.
    fn failed(&mut self, message: String) -> HubFailed {
        self.epoch = self.epoch.wrapping_add(1);
        self.closed = true;
        HubFailed {
            message,
            epoch: self.epoch,
        }
    }

    /// Whether `intent` goes to the head. A marker never does; the last
    /// failure's opens the gate (an older one, overtaken by a second
    /// failure, does not).
    fn admit(&mut self, intent: &GuiIntent) -> bool {
        match intent {
            GuiIntent::HubFailedSeen { epoch } => {
                if *epoch == self.epoch {
                    self.closed = false;
                }
                false
            }
            _ => !self.closed,
        }
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
/// `BootFailed` and waiting for the next `Boot`. Returns the open core and
/// the `Boot` that opened it; `None`: stopped first.
async fn open_on_boot(
    boot: &SignalReceiver<DartSignalPack<Boot>>,
    stop: &mut Stop,
) -> Option<(Running, Boot)> {
    loop {
        tokio::select! {
            () = stop.fired() => return None,
            pack = boot.recv() => {
                let asked = pack?.message;
                // Reopened later as it was opened now, not reset again.
                let again = Boot { reset_replica: false, ..asked.clone() };
                match boot::open(asked).await {
                    Ok(running) => {
                        Booted.send_signal_to_dart();
                        // Up if the hub gave up off screen; the user has
                        // restarted it now.
                        PlatformEffect::Dismiss { id: notify::STOPPED }.send_signal_to_dart();
                        return Some((running, again));
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
    /// With the panic's message.
    Failed(String),
}

/// Runs the loop over `hub` until `stop` fires, then closes the core. A
/// panic, in the loop, on the dispatch thread or on the core's own threads, ends the loop where it
/// happens, and the core is closed all the same: actions dispatched before
/// it are applied (on the dispatch thread, those before the one that
/// panicked), but nothing is asked of the head, so the stop's last draft
/// save and read mark are not made. Closing releases the core's slot and
/// data dir, so the next `Boot` opens it again.
async fn serve(
    mut hub: Running,
    boot: &SignalReceiver<DartSignalPack<Boot>>,
    intents: &SignalReceiver<DartSignalPack<GuiIntent>>,
    gate: &mut Gate,
    recovery: &mut Recovery,
    stop: &mut Stop,
) -> Served {
    let ran = catching(run(&mut hub, boot, intents, gate, recovery, stop)).await;
    hub.close();
    match ran {
        Ok(Ok(())) => Served::Stopped,
        Ok(Err(message)) => {
            tracing::error!("hub's dispatch thread or core failed: {message}");
            Served::Failed(message)
        }
        Err(message) => {
            tracing::error!("hub panicked: {message}");
            Served::Failed(message)
        }
    }
}

/// The loop. `Err`: the dispatch thread or the core's own threads failed,
/// with this message.
async fn run(
    hub: &mut Running,
    boot: &SignalReceiver<DartSignalPack<Boot>>,
    intents: &SignalReceiver<DartSignalPack<GuiIntent>>,
    gate: &mut Gate,
    recovery: &mut Recovery,
    stop: &mut Stop,
) -> Result<(), String> {
    let mut head = Head::holding(hub.prefs, recovery.holders);
    recovery.stepped(*head.awake());
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
            Some(pack) = intents.recv() => {
                if !gate.admit(&pack.message) {
                    tracing::debug!(intent = ?pack.message, "intent for a failed hub dropped");
                    continue;
                }
                head.on_intent(pack.message, Clock::read())
            }
            Some(mut view) = hub.views.recv() => {
                // Snapshots are whole: only the newest waiting one matters.
                while let Ok(newer) = hub.views.try_recv() {
                    view = newer;
                }
                head.on_view(view, Clock::read())
            }
            // The channel closes only when the dispatch thread ends, which
            // while the hub runs means it panicked: then no action is
            // applied any more, so the hub fails rather than carry on.
            report = hub.results.recv() => match report {
                Some(Report::Done(done)) => head.on_dispatched(done, Clock::read()),
                Some(Report::Panicked { message, .. }) => return Err(message),
                None => return Err("the dispatch thread ended".to_owned()),
            },
            // The core's engine or delivery thread panicked: sync or
            // snapshots may have stopped for good. Closed only with the
            // core, so this branch then just goes quiet.
            Ok(message) = &mut hub.failed => return Err(message),
            () = sleep_until(wake.unwrap_or_else(TokioInstant::now)), if wake.is_some() => {
                head.on_wake(Clock::read())
            }
        };
        #[cfg(test)]
        tests::panic_if_asked();
        perform(outs, hub, &mut shown);
        recovery.stepped(*head.awake());
    }
    // The one way out of the loop: the unsaved draft and read mark are
    // queued now, and closing applies everything queued before shutdown.
    perform(head.on_stop(Clock::read()), hub, &mut shown);
    Ok(())
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

/// Carries out a step's outputs in order: signals to Dart, actions to the
/// dispatcher, suspend and resume to the core (neither blocks: each only
/// flips the engine's flag), preferences to their file (a failed write is
/// logged: the head keeps the choice until the hub stops).
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
            Out::SavePrefs(prefs) => {
                if let Err(err) = prefs::save(&hub.prefs_path, &prefs) {
                    tracing::warn!(path = %hub.prefs_path.display(), "cannot save the preferences: {err}");
                }
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

    use skywave_core::Action;

    use super::*;
    use crate::dispatcher::Dispatcher;
    use crate::lifecycle::{Moved, Target};

    /// A phone's holders off screen, with the service holding the link or not.
    fn away(service: bool) -> Awake {
        let mut awake = Awake::new(Target::Mobile);
        awake.service(service);
        awake.moved(Moved::Backgrounded);
        awake
    }

    /// A phone's holders on screen, the service running.
    fn shown() -> Awake {
        let mut awake = away(true);
        awake.moved(Moved::Foregrounded);
        awake
    }

    fn booted_once(dir: &Path) -> Recovery {
        Recovery {
            boot: Some(boot_in(dir)),
            ..Recovery::default()
        }
    }

    #[test]
    fn a_failure_on_screen_or_with_nothing_holding_waits_for_the_user() {
        let dir = Path::new("/nowhere");
        for holders in [Awake::default(), shown(), away(false)] {
            let mut recovery = booted_once(dir);
            recovery.stepped(holders);
            assert!(matches!(recovery.failed(), Recover::Wait), "{holders:?}");
            assert_eq!(
                recovery.holders,
                Awake::default(),
                "the next head starts on screen"
            );
        }
    }

    #[test]
    fn a_failure_off_screen_under_the_service_restarts_once_then_gives_up() {
        let mut recovery = booted_once(Path::new("/data"));
        recovery.stepped(away(true));
        let Recover::Restart(boot) = recovery.failed() else {
            panic!("not restarted");
        };
        assert_eq!(boot.dir, "/data");
        assert_eq!(
            recovery.holders,
            away(true),
            "the restarted head starts away"
        );

        // The restarted head fails again, before or after a step off screen.
        recovery.stepped(recovery.holders);
        assert!(matches!(recovery.failed(), Recover::GiveUp));
        assert_eq!(recovery.holders, Awake::default());
    }

    #[test]
    fn coming_back_on_screen_allows_another_restart() {
        let mut recovery = booted_once(Path::new("/data"));
        recovery.stepped(away(true));
        assert!(matches!(recovery.failed(), Recover::Restart(_)));
        recovery.stepped(shown());
        recovery.stepped(away(true));
        assert!(matches!(recovery.failed(), Recover::Restart(_)));
    }

    #[test]
    fn a_failure_off_screen_before_any_boot_gives_up() {
        let mut recovery = Recovery::default();
        recovery.stepped(away(true));
        assert!(matches!(recovery.failed(), Recover::GiveUp));
    }

    /// The whole path: a head off screen under the service panics, and the
    /// hub reopens the core without a `Boot`, its head starting away.
    #[tokio::test(flavor = "current_thread")]
    async fn a_hub_failing_off_screen_under_the_service_reopens_the_core_itself() {
        let dir = tempfile::tempdir().unwrap();
        let boot = Boot::get_dart_signal_receiver();
        let intents = GuiIntent::get_dart_signal_receiver();
        let (_running, never) = oneshot::channel();
        let mut stop = Stop::new(never);
        let mut recovery = booted_once(dir.path());
        recovery.holders = away(true);

        let mut gate = Gate::default();
        let hub = reopen(dir.path()).await;
        PANIC_NEXT_STEP.with(|asked| asked.set(true));
        let served = serve(hub, &boot, &intents, &mut gate, &mut recovery, &mut stop).await;
        assert!(matches!(served, Served::Failed(_)));

        let hub = tokio::time::timeout(Duration::from_secs(10), recover(&mut recovery))
            .await
            .expect("the last core released its slot")
            .expect("the core reopened");
        assert_eq!(recovery.holders, away(true));
        let mut stop = stopped();
        let served = serve(hub, &boot, &intents, &mut gate, &mut recovery, &mut stop).await;
        assert!(matches!(served, Served::Stopped));
    }

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
        let mut gate = Gate::default();
        let Served::Failed(message) = serve(
            hub,
            &boot,
            &intents,
            &mut gate,
            &mut Recovery::default(),
            &mut stop,
        )
        .await
        else {
            panic!("the hub stopped without failing");
        };
        assert_eq!(message, "injected panic");

        // The core was closed: the slot is free and the data dir unlocked,
        // and a fresh hub runs over the reopened core until stopped.
        let hub = reopen(dir.path()).await;
        assert!(matches!(
            serve(
                hub,
                &boot,
                &intents,
                &mut gate,
                &mut Recovery::default(),
                &mut stopped()
            )
            .await,
            Served::Stopped
        ));
    }

    /// A stop that has fired already.
    fn stopped() -> Stop {
        let (stopping, stopped) = oneshot::channel();
        stopping.send(()).unwrap();
        Stop::new(stopped)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_panicking_dispatch_fails_the_hub_and_a_reboot_opens_the_core() {
        let dir = tempfile::tempdir().unwrap();
        let boot = Boot::get_dart_signal_receiver();
        let intents = GuiIntent::get_dart_signal_receiver();
        let (_running, never) = oneshot::channel();
        let mut stop = Stop::new(never);

        // The dispatch thread's next action panics.
        let mut hub = reopen(dir.path()).await;
        let (reports_tx, reports) = tokio::sync::mpsc::unbounded_channel();
        let panicking = Dispatcher::start_with(reports_tx, |_| panic!("dispatch blew up")).unwrap();
        std::mem::replace(&mut hub.dispatcher, panicking).stop();
        hub.results = reports;
        hub.dispatcher.send(1, Action::RetryNow);

        let served = tokio::time::timeout(
            Duration::from_secs(10),
            serve(
                hub,
                &boot,
                &intents,
                &mut Gate::default(),
                &mut Recovery::default(),
                &mut stop,
            ),
        )
        .await
        .expect("the hub noticed the dispatch thread's panic");
        let Served::Failed(message) = served else {
            panic!("the hub stopped without failing");
        };
        assert_eq!(message, "dispatch blew up");

        // Closed as after any panic: the core opens again at once.
        let hub = reopen(dir.path()).await;
        assert!(matches!(
            serve(
                hub,
                &boot,
                &intents,
                &mut Gate::default(),
                &mut Recovery::default(),
                &mut stopped()
            )
            .await,
            Served::Stopped
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_failed_core_fails_the_hub_and_a_reboot_opens_the_core() {
        let dir = tempfile::tempdir().unwrap();
        let boot = Boot::get_dart_signal_receiver();
        let intents = GuiIntent::get_dart_signal_receiver();
        let (_running, never) = oneshot::channel();
        let mut stop = Stop::new(never);

        // The core reports a panic on its engine, as its watcher would.
        let mut hub = reopen(dir.path()).await;
        let (failing, failed) = oneshot::channel();
        hub.failed = failed;
        failing
            .send("the sync engine panicked: engine blew up".to_owned())
            .unwrap();

        let served = tokio::time::timeout(
            Duration::from_secs(10),
            serve(
                hub,
                &boot,
                &intents,
                &mut Gate::default(),
                &mut Recovery::default(),
                &mut stop,
            ),
        )
        .await
        .expect("the hub noticed the core's failure");
        let Served::Failed(message) = served else {
            panic!("the hub stopped without failing");
        };
        assert_eq!(message, "the sync engine panicked: engine blew up");

        // Closed as after any panic: the core opens again at once, and a
        // core that never fails serves until stopped.
        let hub = reopen(dir.path()).await;
        assert!(matches!(
            serve(
                hub,
                &boot,
                &intents,
                &mut Gate::default(),
                &mut Recovery::default(),
                &mut stopped()
            )
            .await,
            Served::Stopped
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_panic_while_opening_closes_the_half_open_core() {
        let dir = tempfile::tempdir().unwrap();
        boot::tests::PANIC_WHILE_OPENING.with(|asked| asked.set(true));
        let caught = catching(boot::open(boot_in(dir.path()))).await;
        assert_eq!(caught.err().unwrap(), "injected panic while opening");
        // The core, its dispatcher and its engine were closed before the
        // slot was released: the data dir is not "in use".
        reopen(dir.path()).await.close();
    }

    /// The head's preferences go to `<boot.dir>/skywave-gui.json`, outside
    /// the core's data dir, and the next boot reads them; a write that
    /// fails is logged, not fatal.
    #[tokio::test(flavor = "current_thread")]
    async fn saved_preferences_land_beside_the_data_and_a_failed_write_is_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let hub = reopen(dir.path()).await;
        let on = prefs::Prefs {
            stay_connected: true,
        };
        perform(vec![Out::SavePrefs(on)], &hub, &mut None);
        let file = dir.path().join("skywave-gui.json");
        assert!(file.is_file());
        assert!(
            !dir.path()
                .join(boot::DATA_DIR)
                .join("skywave-gui.json")
                .exists()
        );
        hub.close();
        let hub = reopen(dir.path()).await;
        assert_eq!(hub.prefs, on);

        std::fs::remove_file(&file).unwrap();
        std::fs::create_dir(&file).unwrap();
        std::fs::write(file.join("x"), b"").unwrap();
        perform(vec![Out::SavePrefs(on)], &hub, &mut None);
        hub.close();
    }

    #[test]
    fn intents_sent_before_dart_saw_the_failure_are_dropped() {
        let mut gate = Gate::default();
        // Before any failure everything goes through, markers excepted.
        assert!(gate.admit(&GuiIntent::AppReady));
        assert!(!gate.admit(&GuiIntent::HubFailedSeen { epoch: 0 }));

        assert_eq!(gate.failed("first".into()).epoch, 1);
        assert!(!gate.admit(&GuiIntent::AppForegrounded));
        // A second failure before Dart answered the first: the first's
        // marker does not open the gate.
        assert_eq!(gate.failed("second".into()).epoch, 2);
        assert!(!gate.admit(&GuiIntent::HubFailedSeen { epoch: 1 }));
        assert!(!gate.admit(&GuiIntent::AppBackgrounded));
        // The last one's does, and what follows it is for the new head.
        assert!(!gate.admit(&GuiIntent::HubFailedSeen { epoch: 2 }));
        assert!(gate.admit(&GuiIntent::AppReady));
        assert!(gate.admit(&GuiIntent::AppBackgrounded));
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
