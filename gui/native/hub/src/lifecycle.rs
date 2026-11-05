//! What the app leaving or returning to the screen, and the background
//! service starting or stopping, do to the core. Dart and Kotlin only
//! report them (`AppBackgrounded`, `AppForegrounded`, `BackgroundService`);
//! [`Awake`] decides.
//!
//! On a phone the system may freeze or kill a backgrounded app, so the
//! engine's network loops are paused while nothing holds the link, and the
//! link is retried at once when something holds it again (D21). Two things
//! hold it: the app being on screen ([`Holder::Visible`]) and the Android
//! foreground service running ([`Holder::Service`]), which keeps the process
//! alive off screen. The core is suspended when the last holder lets go and
//! resumed when the first one takes it again; a report that changes nothing
//! calls nothing. A desktop app keeps running behind other windows: its
//! engine is never suspended, whatever the reports.
//!
//! Leaving is treated like quitting on every platform, held or not: the
//! system may kill a phone app within the draft saver's debounce, and a
//! desktop window can be closed from the background. So the unsaved draft
//! is saved and what was seen is marked read at once, both forced, as the
//! TUI does on quit. They go to the dispatcher before the suspend; suspend
//! only pauses the network loops, and a dispatch still writes the store
//! after it.
//!
//! Closing (the hub task stopping, whatever stopped it) saves and marks the
//! same way, without counting on Dart having reported the app leaving
//! first: closing the core applies every action still queued, these last.

use skywave_core::Action;

/// The kind of platform, as far as the app's lifecycle goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Mobile,
    Desktop,
}

impl Target {
    /// The platform this hub was built for.
    pub const BUILT: Target = if cfg!(any(target_os = "android", target_os = "ios")) {
        Target::Mobile
    } else {
        Target::Desktop
    };
}

/// Where the app went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Moved {
    Backgrounded,
    Foregrounded,
    /// The hub is stopping: the core closes next.
    Closed,
}

/// What to do to the core, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum Call {
    /// Save the open session's unsaved draft now, without the debounce.
    SaveDraft,
    /// Mark what the open session shows read now, unthrottled.
    MarkRead,
    /// `Core::suspend`.
    Suspend,
    /// `Core::resume`.
    Resume,
    /// Hand the action to the dispatcher.
    Dispatch(Action),
}

/// What keeps the link up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Holder {
    /// The app is on screen.
    Visible,
    /// The background service runs: Kotlin reported `startForeground`
    /// succeeded, and not yet that it stopped.
    Service,
}

impl Holder {
    fn bit(self) -> u8 {
        1 << self as u8
    }
}

/// Who holds the link, and so whether the core runs. Starts with the app
/// on screen and no service, as every head does, a fresh one after a hub
/// failure too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Awake {
    /// A bit per [`Holder`].
    holders: u8,
    target: Target,
}

impl Default for Awake {
    fn default() -> Awake {
        Awake::new(Target::BUILT)
    }
}

impl Awake {
    pub fn new(target: Target) -> Awake {
        Awake {
            holders: Holder::Visible.bit(),
            target,
        }
    }

    pub fn holds(&self, holder: Holder) -> bool {
        self.holders & holder.bit() != 0
    }

    /// The calls the app's move makes.
    pub fn moved(&mut self, moved: Moved) -> Vec<Call> {
        match moved {
            Moved::Backgrounded => {
                let mut calls = vec![Call::SaveDraft, Call::MarkRead];
                calls.extend(self.set(Holder::Visible, false));
                calls
            }
            Moved::Foregrounded => {
                // Coming on screen while the service held the link: it ran
                // all along, maybe deep in a backoff wait. The user looking
                // is when that wait costs, so it is cut short (a no-op
                // unless the link is waiting). Otherwise the resume retries.
                let retry = self.target == Target::Mobile
                    && !self.holds(Holder::Visible)
                    && self.holds(Holder::Service);
                let mut calls = self.set(Holder::Visible, true);
                if retry {
                    calls.push(Call::Dispatch(Action::RetryNow));
                }
                calls
            }
            // No suspend: the core shuts down right after.
            Moved::Closed => vec![Call::SaveDraft, Call::MarkRead],
        }
    }

    /// The calls Kotlin's definitive report on the background service
    /// makes: `running` once `startForeground` succeeded, not once the
    /// service stopped or was refused.
    pub fn service(&mut self, running: bool) -> Vec<Call> {
        self.set(Holder::Service, running)
    }

    /// Takes or lets go of `holder`: the core suspends when the last holder
    /// lets go and resumes, with the link retried at once, when the first
    /// takes it again. Only on a phone.
    fn set(&mut self, holder: Holder, held: bool) -> Vec<Call> {
        let was_held = self.holders != 0;
        if held {
            self.holders |= holder.bit();
        } else {
            self.holders &= !holder.bit();
        }
        let is_held = self.holders != 0;
        match (self.target, was_held, is_held) {
            (Target::Desktop, ..) => Vec::new(),
            (Target::Mobile, true, false) => vec![Call::Suspend],
            // Resumed, the link starts over from `Connecting`; the retry
            // cuts short a backoff wait that began before the suspend.
            (Target::Mobile, false, true) => vec![Call::Resume, Call::Dispatch(Action::RetryNow)],
            (Target::Mobile, ..) => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAVE: [Call; 2] = [Call::SaveDraft, Call::MarkRead];

    fn resume() -> Vec<Call> {
        vec![Call::Resume, Call::Dispatch(Action::RetryNow)]
    }

    #[test]
    fn it_starts_on_screen_without_the_service() {
        for target in [Target::Mobile, Target::Desktop] {
            let awake = Awake::new(target);
            assert!(awake.holds(Holder::Visible));
            assert!(!awake.holds(Holder::Service));
        }
        assert_eq!(Awake::default(), Awake::new(Target::BUILT));
    }

    #[test]
    fn mobile_saves_then_suspends_away_and_resumes_then_retries_on_return() {
        let mut awake = Awake::new(Target::Mobile);
        assert_eq!(
            awake.moved(Moved::Backgrounded),
            vec![Call::SaveDraft, Call::MarkRead, Call::Suspend]
        );
        assert_eq!(awake.moved(Moved::Foregrounded), resume());
    }

    #[test]
    fn mobile_with_the_service_running_saves_away_but_does_not_suspend() {
        let mut awake = Awake::new(Target::Mobile);
        assert_eq!(awake.service(true), vec![]);
        assert_eq!(awake.moved(Moved::Backgrounded), SAVE);
    }

    #[test]
    fn the_service_stopping_while_away_suspends() {
        let mut awake = Awake::new(Target::Mobile);
        awake.service(true);
        awake.moved(Moved::Backgrounded);
        assert_eq!(awake.service(false), vec![Call::Suspend]);
    }

    #[test]
    fn the_service_starting_while_away_resumes_then_retries() {
        let mut awake = Awake::new(Target::Mobile);
        awake.moved(Moved::Backgrounded);
        assert_eq!(awake.service(true), resume());
    }

    /// The service kept the link up while away, maybe in a long backoff
    /// wait; the user looking is when that wait costs, so coming on screen
    /// retries at once even though nothing resumes.
    #[test]
    fn foregrounding_while_the_service_holds_retries_without_resuming() {
        let mut awake = Awake::new(Target::Mobile);
        awake.service(true);
        awake.moved(Moved::Backgrounded);
        assert_eq!(
            awake.moved(Moved::Foregrounded),
            vec![Call::Dispatch(Action::RetryNow)]
        );
    }

    #[test]
    fn a_repeated_report_calls_nothing() {
        let mut awake = Awake::new(Target::Mobile);
        assert_eq!(awake.moved(Moved::Foregrounded), vec![], "already shown");
        awake.moved(Moved::Backgrounded);
        assert_eq!(awake.moved(Moved::Foregrounded), resume());
        assert_eq!(awake.moved(Moved::Foregrounded), vec![]);
        assert_eq!(awake.service(true), vec![]);
        assert_eq!(awake.service(true), vec![]);
        awake.moved(Moved::Backgrounded);
        assert_eq!(awake.service(false), vec![Call::Suspend]);
        assert_eq!(awake.service(false), vec![]);
        assert_eq!(awake.moved(Moved::Backgrounded), SAVE, "already away");
    }

    #[test]
    fn desktop_never_suspends_or_resumes() {
        let mut awake = Awake::new(Target::Desktop);
        assert_eq!(awake.moved(Moved::Backgrounded), SAVE);
        assert_eq!(awake.service(true), vec![]);
        assert_eq!(awake.service(false), vec![]);
        assert_eq!(awake.moved(Moved::Backgrounded), SAVE);
        assert_eq!(awake.moved(Moved::Foregrounded), vec![]);
        assert_eq!(awake.service(true), vec![]);
        assert_eq!(awake.moved(Moved::Foregrounded), vec![]);
    }

    #[test]
    fn closing_saves_and_marks_on_every_target() {
        for target in [Target::Mobile, Target::Desktop] {
            for running in [false, true] {
                let mut awake = Awake::new(target);
                awake.service(running);
                assert_eq!(awake.moved(Moved::Closed), SAVE);
            }
        }
    }

    #[test]
    fn the_built_target_follows_the_os() {
        let mobile = cfg!(any(target_os = "android", target_os = "ios"));
        assert_eq!(Target::BUILT == Target::Mobile, mobile);
    }
}
