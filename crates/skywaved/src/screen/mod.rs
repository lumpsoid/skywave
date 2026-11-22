//! The screen runner (D65): a paired device drives an app on an Android
//! device attached to the server through adb. This module holds what the
//! runner's parts share: the `[screen]` config and the rules an action must
//! meet before the log accepts it. Only the Android backend names adb (D3).

pub mod android;
pub mod backend;
pub mod capture;
mod runner;

pub use runner::{ScreenHandle, ScreenRunner};

use std::path::{Component, Path, PathBuf};

use skywave_proto::{ScreenAction, limits};

/// The adb executable `[screen] adb` defaults to, looked up on `PATH`.
pub const DEFAULT_ADB: &str = "adb";
/// `NotStarted::reason` of a request left queued when the screen runner is
/// disabled.
pub const DISABLED_REASON: &str = "screen disabled";
/// Capture blobs kept per session; the runner prunes the older ones.
pub const KEEP_CAPTURES: usize = 20;

/// Longest swipe the runner performs, in milliseconds.
const SWIPE_MAX_MS: u32 = 10_000;

/// The `[screen]` table, resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenConfig {
    /// Whether devices may drive screens. Off by default.
    pub enabled: bool,
    /// The adb executable: a path, or a bare name looked up on `PATH`.
    pub adb: PathBuf,
}

impl Default for ScreenConfig {
    fn default() -> Self {
        ScreenConfig {
            enabled: false,
            adb: PathBuf::from(DEFAULT_ADB),
        }
    }
}

/// Whether `action` is well formed (`BadScreenInput` when not): the names,
/// text, app ids and APK paths it carries are within their rules.
pub fn valid_action(action: &ScreenAction) -> bool {
    if action.device().is_some_and(|device| !valid_device(device)) {
        return false;
    }
    match action {
        ScreenAction::Devices
        | ScreenAction::Capture { .. }
        | ScreenAction::Tap { .. }
        | ScreenAction::Key { .. } => true,
        ScreenAction::Swipe { duration_ms, .. } => *duration_ms <= SWIPE_MAX_MS,
        ScreenAction::Text { text, .. } => valid_text(text),
        ScreenAction::Install { apk, .. } => valid_apk(apk),
        ScreenAction::Launch { app, .. } | ScreenAction::Stop { app, .. } => valid_app(app),
    }
}

/// A device serial: non-empty, within the name limit, no whitespace or control.
fn valid_device(device: &str) -> bool {
    !device.is_empty()
        && device.len() <= limits::SCREEN_NAME_MAX_BYTES
        && !device.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// Text to type: non-empty, within the text limit, printable ASCII only.
fn valid_text(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= limits::SCREEN_TEXT_MAX_BYTES
        && text.chars().all(|c| matches!(c, ' '..='~'))
}

/// An app's package id: non-empty, within the name limit, and only ASCII
/// letters, digits and `._/`. `$` is refused: adb joins the launch arguments
/// and the device's `sh -c` expands them.
fn valid_app(app: &str) -> bool {
    !app.is_empty()
        && app.len() <= limits::SCREEN_NAME_MAX_BYTES
        && app
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/'))
}

/// An APK path under the server's allowed names: non-empty, within the name
/// limit, relative, without `..` components, and ending in `.apk`.
fn valid_apk(apk: &str) -> bool {
    let path = Path::new(apk);
    !apk.is_empty()
        && apk.len() <= limits::SCREEN_NAME_MAX_BYTES
        && !path.is_absolute()
        && !path.components().any(|c| matches!(c, Component::ParentDir))
        && apk.ends_with(".apk")
}

#[cfg(test)]
mod tests {
    use skywave_proto::CommandId;

    use super::*;

    fn capture(device: &str) -> ScreenAction {
        ScreenAction::Capture {
            device: device.to_owned(),
            full: false,
        }
    }

    #[test]
    fn accepts_devices_listing() {
        assert!(valid_action(&ScreenAction::Devices));
    }

    #[test]
    fn accepts_a_device_serial() {
        assert!(valid_action(&capture("emulator-5554")));
    }

    #[test]
    fn refuses_an_empty_device() {
        assert!(!valid_action(&capture("")));
    }

    #[test]
    fn refuses_a_long_device() {
        let long = "d".repeat(limits::SCREEN_NAME_MAX_BYTES + 1);
        assert!(!valid_action(&capture(&long)));
    }

    #[test]
    fn refuses_a_device_with_whitespace() {
        assert!(!valid_action(&capture("emu lator")));
    }

    #[test]
    fn accepts_printable_ascii_text() {
        let text = ScreenAction::Text {
            device: "d".to_owned(),
            aim: CommandId::new(),
            text: "hello world ~".to_owned(),
        };
        assert!(valid_action(&text));
    }

    #[test]
    fn refuses_empty_text() {
        let text = ScreenAction::Text {
            device: "d".to_owned(),
            aim: CommandId::new(),
            text: String::new(),
        };
        assert!(!valid_action(&text));
    }

    #[test]
    fn refuses_long_text() {
        let text = ScreenAction::Text {
            device: "d".to_owned(),
            aim: CommandId::new(),
            text: "a".repeat(limits::SCREEN_TEXT_MAX_BYTES + 1),
        };
        assert!(!valid_action(&text));
    }

    #[test]
    fn refuses_non_printable_text() {
        let text = ScreenAction::Text {
            device: "d".to_owned(),
            aim: CommandId::new(),
            text: "tab\there".to_owned(),
        };
        assert!(!valid_action(&text));
    }

    #[test]
    fn accepts_a_swipe_of_at_most_ten_seconds() {
        let swipe = ScreenAction::Swipe {
            device: "d".to_owned(),
            aim: CommandId::new(),
            from_x: 0,
            from_y: 0,
            to_x: 10,
            to_y: 10,
            duration_ms: SWIPE_MAX_MS,
        };
        assert!(valid_action(&swipe));
    }

    #[test]
    fn refuses_a_swipe_over_ten_seconds() {
        let swipe = ScreenAction::Swipe {
            device: "d".to_owned(),
            aim: CommandId::new(),
            from_x: 0,
            from_y: 0,
            to_x: 10,
            to_y: 10,
            duration_ms: SWIPE_MAX_MS + 1,
        };
        assert!(!valid_action(&swipe));
    }

    #[test]
    fn accepts_a_package_id() {
        let launch = ScreenAction::Launch {
            device: "d".to_owned(),
            app: "com.example.app_1x/.Main".to_owned(),
        };
        assert!(valid_action(&launch));
    }

    #[test]
    fn refuses_a_dollar_in_an_app() {
        let launch = ScreenAction::Launch {
            device: "d".to_owned(),
            app: "com.x$Inner/.Main".to_owned(),
        };
        assert!(!valid_action(&launch));
    }

    #[test]
    fn refuses_an_empty_app() {
        let stop = ScreenAction::Stop {
            device: "d".to_owned(),
            app: String::new(),
        };
        assert!(!valid_action(&stop));
    }

    #[test]
    fn refuses_a_long_app() {
        let launch = ScreenAction::Launch {
            device: "d".to_owned(),
            app: "a".repeat(limits::SCREEN_NAME_MAX_BYTES + 1),
        };
        assert!(!valid_action(&launch));
    }

    #[test]
    fn refuses_an_app_with_a_bad_char() {
        let launch = ScreenAction::Launch {
            device: "d".to_owned(),
            app: "com:example".to_owned(),
        };
        assert!(!valid_action(&launch));
    }

    #[test]
    fn accepts_a_relative_apk() {
        let install = ScreenAction::Install {
            device: "d".to_owned(),
            apk: "builds/app-debug.apk".to_owned(),
        };
        assert!(valid_action(&install));
    }

    #[test]
    fn refuses_an_empty_apk() {
        let install = ScreenAction::Install {
            device: "d".to_owned(),
            apk: String::new(),
        };
        assert!(!valid_action(&install));
    }

    #[test]
    fn refuses_a_long_apk() {
        let install = ScreenAction::Install {
            device: "d".to_owned(),
            apk: format!("{}.apk", "a".repeat(limits::SCREEN_NAME_MAX_BYTES)),
        };
        assert!(!valid_action(&install));
    }

    #[test]
    fn refuses_an_absolute_apk() {
        let install = ScreenAction::Install {
            device: "d".to_owned(),
            apk: "/tmp/app.apk".to_owned(),
        };
        assert!(!valid_action(&install));
    }

    #[test]
    fn refuses_an_apk_with_a_parent_component() {
        let install = ScreenAction::Install {
            device: "d".to_owned(),
            apk: "../app.apk".to_owned(),
        };
        assert!(!valid_action(&install));
    }

    #[test]
    fn refuses_a_name_not_ending_in_apk() {
        let install = ScreenAction::Install {
            device: "d".to_owned(),
            apk: "app.zip".to_owned(),
        };
        assert!(!valid_action(&install));
    }
}
