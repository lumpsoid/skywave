//! What the app leaving or returning to the screen does to the core. Dart
//! only reports it (`AppBackgrounded`, `AppForegrounded`); this decides.
//!
//! On a phone the system may freeze or kill a backgrounded app, so the
//! engine's network loops are paused while away and the link is retried at
//! once on return, as the Android head does (D21). A desktop app keeps
//! running behind other windows: its engine is left alone.
//!
//! Leaving is treated like quitting on every platform: the system may kill
//! a phone app within the draft saver's debounce, and a desktop window can
//! be closed from the background. So the unsaved draft is saved and what
//! was seen is marked read at once, both forced, as the TUI does on quit.
//! They go to the dispatcher before the suspend; suspend only pauses the
//! network loops, and a dispatch still writes the store after it.

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

/// The calls the app's move makes on `target`.
pub fn calls(moved: Moved, target: Target) -> Vec<Call> {
    match (target, moved) {
        (Target::Desktop, Moved::Backgrounded) => vec![Call::SaveDraft, Call::MarkRead],
        (Target::Mobile, Moved::Backgrounded) => {
            vec![Call::SaveDraft, Call::MarkRead, Call::Suspend]
        }
        (Target::Desktop, Moved::Foregrounded) => Vec::new(),
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
    fn mobile_saves_then_suspends_away_and_resumes_then_retries_on_return() {
        assert_eq!(
            calls(Moved::Backgrounded, Target::Mobile),
            vec![Call::SaveDraft, Call::MarkRead, Call::Suspend]
        );
        assert_eq!(
            calls(Moved::Foregrounded, Target::Mobile),
            vec![Call::Resume, Call::Dispatch(Action::RetryNow)]
        );
    }

    #[test]
    fn desktop_only_saves_away() {
        assert_eq!(
            calls(Moved::Backgrounded, Target::Desktop),
            vec![Call::SaveDraft, Call::MarkRead]
        );
        assert_eq!(calls(Moved::Foregrounded, Target::Desktop), vec![]);
    }

    #[test]
    fn the_built_target_follows_the_os() {
        let mobile = cfg!(any(target_os = "android", target_os = "ios"));
        assert_eq!(Target::BUILT == Target::Mobile, mobile);
    }
}
