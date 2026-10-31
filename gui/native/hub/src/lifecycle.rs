//! What the app leaving or returning to the screen does to the core. Dart
//! only reports it (`AppBackgrounded`, `AppForegrounded`); this decides.
//!
//! On a phone the system may freeze or kill a backgrounded app, so the
//! engine's network loops are paused while away and the link is retried at
//! once on return, as the Android head does (D21). A desktop app keeps
//! running behind other windows: nothing changes there.

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
}

/// What to do to the core, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum Call {
    /// `Core::suspend`.
    Suspend,
    /// `Core::resume`.
    Resume,
    /// Hand the action to the dispatcher.
    Dispatch(Action),
}

/// The calls the app's move makes on `target`.
pub fn calls(moved: Moved, target: Target) -> Vec<Call> {
    match (target, moved) {
        (Target::Desktop, _) => Vec::new(),
        (Target::Mobile, Moved::Backgrounded) => vec![Call::Suspend],
        // Resumed, the link starts over from `Connecting`; the retry cuts
        // short a backoff wait that began before the app went away.
        (Target::Mobile, Moved::Foregrounded) => {
            vec![Call::Resume, Call::Dispatch(Action::RetryNow)]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mobile_suspends_away_and_resumes_then_retries_on_return() {
        assert_eq!(
            calls(Moved::Backgrounded, Target::Mobile),
            vec![Call::Suspend]
        );
        assert_eq!(
            calls(Moved::Foregrounded, Target::Mobile),
            vec![Call::Resume, Call::Dispatch(Action::RetryNow)]
        );
    }

    #[test]
    fn desktop_ignores_both() {
        assert_eq!(calls(Moved::Backgrounded, Target::Desktop), vec![]);
        assert_eq!(calls(Moved::Foregrounded, Target::Desktop), vec![]);
    }

    #[test]
    fn the_built_target_follows_the_os() {
        let mobile = cfg!(any(target_os = "android", target_os = "ios"));
        assert_eq!(Target::BUILT == Target::Mobile, mobile);
    }
}
