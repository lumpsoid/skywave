//! The app screen (D65) as every head words it: each device's capture and
//! its download note, each request's action and state, and the inputs a
//! head may send. An input aims at the device's latest capture as this
//! device knows it, so the server never applies it to a screen that has
//! moved on; when [`input`] gives none, the head tells the user to aim again.

use skywave_core::{
    Action, AppCapture, AppScreenView, BlobState, CommandId, FailReason, FullView, Outgoing,
    RejectReason, ScreenAction, ScreenDevice, ScreenKey, ScreenOutcome, ScreenRequest,
    ScreenRequestState, ScreenShot, SessionView,
};

use crate::format::{count, percent, reject_reason};
use crate::text::{clean, clean_line};

/// A swipe's duration.
pub const SWIPE_MS: u32 = 300;

/// The longest text a request label shows before it is cut with `…`.
const TEXT_LABEL_CHARS: usize = 30;

/// What a user gives the screen, in the capture's pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    Tap {
        x: u32,
        y: u32,
    },
    Swipe {
        from_x: u32,
        from_y: u32,
        to_x: u32,
        to_y: u32,
    },
    Text(String),
    Key(ScreenKey),
}

/// Asks the server which devices are attached.
pub fn devices(view: &SessionView) -> Action {
    screen(view, ScreenAction::Devices)
}

/// Asks the server for a capture of `device`'s screen.
pub fn capture(view: &SessionView, device: &str) -> Action {
    screen(
        view,
        ScreenAction::Capture {
            device: device.to_owned(),
            full: false,
        },
    )
}

/// The input aimed at `aim` on `device`, or `None` when `aim` is not the
/// device's latest capture here, a point is off the image, or text is blank.
pub fn input(view: &SessionView, device: &str, aim: CommandId, input: Input) -> Option<Action> {
    let capture = view.app_screen.latest(device)?;
    if capture.capture != aim {
        return None;
    }
    let inside = |x: u32, y: u32| x < capture.width && y < capture.height;
    let device = device.to_owned();
    let action = match input {
        Input::Tap { x, y } if inside(x, y) => ScreenAction::Tap { device, aim, x, y },
        Input::Swipe {
            from_x,
            from_y,
            to_x,
            to_y,
        } if inside(from_x, from_y) && inside(to_x, to_y) => ScreenAction::Swipe {
            device,
            aim,
            from_x,
            from_y,
            to_x,
            to_y,
            duration_ms: SWIPE_MS,
        },
        Input::Text(text) if !text.trim().is_empty() => ScreenAction::Text { device, aim, text },
        Input::Key(key) => ScreenAction::Key { device, aim, key },
        Input::Tap { .. } | Input::Swipe { .. } | Input::Text(_) => return None,
    };
    Some(screen(view, action))
}

/// The device the screen shows first: the first listed in state "device",
/// else the first listed, else the first captured.
pub fn default_device(app: &AppScreenView) -> Option<&str> {
    app.devices
        .iter()
        .find(|device| device.state == "device")
        .or_else(|| app.devices.first())
        .map(|device| device.serial.as_str())
        .or_else(|| app.captures.first().map(|capture| capture.device.as_str()))
}

/// A device's name in a picker: its model and serial ("Pixel_7 · emulator-5554"), or the serial.
pub fn device_label(device: &ScreenDevice) -> String {
    match &device.model {
        Some(model) => format!("{} · {}", clean_line(model), clean_line(&device.serial)),
        None => clean_line(&device.serial),
    }
}

/// A note in place of the image while it is not on disk: "loading", "loading 40%",
/// "the capture failed to download: {reason}", "capture deleted"; `None` once Available.
pub fn capture_note(capture: &AppCapture) -> Option<String> {
    match &capture.content {
        FullView::Blob(blob) => match &blob.state {
            BlobState::NotDownloaded { .. } | BlobState::Requested { .. } => {
                Some("loading".to_owned())
            }
            BlobState::Downloading { got, size, .. } => {
                Some(format!("loading {}%", percent(*got, *size)))
            }
            BlobState::Failed { reason, .. } => Some(format!(
                "the capture failed to download: {}",
                clean_line(reason)
            )),
            BlobState::Available { .. } => None,
        },
        FullView::Inline(_) => None,
        FullView::Lost => Some("capture not stored".to_owned()),
        FullView::Deleted { .. } => Some("capture deleted".to_owned()),
    }
}

/// A key in words, as a request label and a key picker name it.
pub fn key_label(key: ScreenKey) -> &'static str {
    match key {
        ScreenKey::Back => "back",
        ScreenKey::Home => "home",
        ScreenKey::Recents => "recents",
        ScreenKey::Enter => "enter",
        ScreenKey::Delete => "delete",
        ScreenKey::Power => "power",
        ScreenKey::VolumeUp => "volume up",
        ScreenKey::VolumeDown => "volume down",
    }
}

/// One line per request: "list devices", "capture", "full capture", "tap 10,20",
/// "swipe 10,20 → 30,40", "type “hello”" (text cut to 30 chars with …), "key back",
/// "install {apk}", "launch {app}", "stop {app}".
pub fn action_label(action: &ScreenAction) -> String {
    match action {
        ScreenAction::Devices => "list devices".to_owned(),
        ScreenAction::Capture { full: false, .. } => "capture".to_owned(),
        ScreenAction::Capture { full: true, .. } => "full capture".to_owned(),
        ScreenAction::Tap { x, y, .. } => format!("tap {x},{y}"),
        ScreenAction::Swipe {
            from_x,
            from_y,
            to_x,
            to_y,
            ..
        } => format!("swipe {from_x},{from_y} → {to_x},{to_y}"),
        ScreenAction::Text { text, .. } => {
            format!("type “{}”", cut(&clean(text), TEXT_LABEL_CHARS))
        }
        ScreenAction::Key { key, .. } => format!("key {}", key_label(*key)),
        ScreenAction::Install { apk, .. } => format!("install {apk}"),
        ScreenAction::Launch { app, .. } => format!("launch {app}"),
        ScreenAction::Stop { app, .. } => format!("stop {app}"),
    }
}

/// Its state: Pending -> "waiting to send" (Queued) / "sending" (Sending);
/// refused -> the refusal text from format.rs (Rejected) or "could not be sent";
/// Queued -> "queued"; Done: Devices -> "1 device" / "{n} devices";
/// Done{Image} -> "done"; Done{Unchanged} -> "done, screen unchanged";
/// Done{shot Failed{reason}} -> "done, capture failed: {reason}";
/// Stale -> "the screen changed since that capture; aim again";
/// Failed{reason} -> "failed: {reason}"; Lost -> "lost: daemon restarted";
/// NotStarted{reason} -> "not started: {reason}".
pub fn state_label(request: &ScreenRequest) -> String {
    if let Some(Outgoing::Failed(reason)) = request.outgoing {
        return match reason {
            FailReason::Rejected(reason) => reject_reason(reason).to_owned(),
            FailReason::Undeliverable | FailReason::AttachmentRefused(_) => {
                "could not be sent".to_owned()
            }
        };
    }
    match &request.state {
        ScreenRequestState::Pending => match request.outgoing {
            Some(Outgoing::Sending) => "sending".to_owned(),
            _ => "waiting to send".to_owned(),
        },
        ScreenRequestState::Queued => "queued".to_owned(),
        ScreenRequestState::Done(outcome) => outcome_label(outcome),
    }
}

/// Stale: refused ScreenStale, or finished Stale.
pub fn stale(request: &ScreenRequest) -> bool {
    matches!(
        request.outgoing,
        Some(Outgoing::Failed(FailReason::Rejected(
            RejectReason::ScreenStale
        )))
    ) || matches!(
        request.state,
        ScreenRequestState::Done(ScreenOutcome::Stale { .. })
    )
}

/// Still waiting: Pending or Queued and not refused.
pub fn waiting(request: &ScreenRequest) -> bool {
    !request.refused()
        && matches!(
            request.state,
            ScreenRequestState::Pending | ScreenRequestState::Queued
        )
}

/// A screen request's state once the server has answered it.
fn outcome_label(outcome: &ScreenOutcome) -> String {
    match outcome {
        ScreenOutcome::Devices { devices } => count(devices.len(), "device", "devices"),
        ScreenOutcome::Done { shot } => match shot {
            ScreenShot::Image(_) => "done".to_owned(),
            ScreenShot::Unchanged { .. } => "done, screen unchanged".to_owned(),
            ScreenShot::Failed { reason } => {
                format!("done, capture failed: {}", clean_line(reason))
            }
        },
        ScreenOutcome::Stale { .. } => reject_reason(RejectReason::ScreenStale).to_owned(),
        ScreenOutcome::Failed { reason } => format!("failed: {}", clean_line(reason)),
        ScreenOutcome::Lost => "lost: daemon restarted".to_owned(),
        ScreenOutcome::NotStarted { reason } => format!("not started: {}", clean_line(reason)),
    }
}

/// A screen action sent in `view`'s session.
fn screen(view: &SessionView, action: ScreenAction) -> Action {
    Action::Screen {
        session_id: view.id,
        action,
    }
}

/// The first `max` characters of `text`, with `…` when it is longer.
fn cut(text: &str, max: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use skywave_core::{CaptureFormat, DeviceId, DeviceRef};

    use super::*;
    use crate::policy::tests::session;

    fn capture_of(device: &str, capture: CommandId) -> AppCapture {
        AppCapture {
            device: device.to_owned(),
            capture,
            format: CaptureFormat::Png,
            width: 1080,
            height: 2400,
            screen_width: 1080,
            screen_height: 2400,
            content: FullView::Lost,
            taken_at: Utc.timestamp_opt(0, 0).unwrap(),
        }
    }

    fn view_with(captures: Vec<AppCapture>) -> SessionView {
        let mut view = session(Vec::new());
        view.app_screen = AppScreenView {
            captures,
            ..AppScreenView::default()
        };
        view
    }

    fn request(state: ScreenRequestState, outgoing: Option<Outgoing>) -> ScreenRequest {
        ScreenRequest {
            request: CommandId::new(),
            action: ScreenAction::Devices,
            by: DeviceRef {
                id: DeviceId::new(),
                name: None,
                me: true,
            },
            state,
            requested_at: None,
            finished_at: None,
            outgoing,
            discardable: false,
            delivery: None,
        }
    }

    #[test]
    fn an_input_aims_at_the_latest_capture_only() {
        let (c1, c2) = (CommandId::new(), CommandId::new());
        let view = view_with(vec![capture_of("emulator-5554", c2)]);
        assert!(matches!(
            input(&view, "emulator-5554", c2, Input::Tap { x: 10, y: 20 }),
            Some(Action::Screen {
                action: ScreenAction::Tap { .. },
                ..
            })
        ));
        assert_eq!(
            input(&view, "emulator-5554", c1, Input::Tap { x: 10, y: 20 }),
            None
        );
        assert_eq!(
            input(&view, "emulator-9999", c2, Input::Tap { x: 10, y: 20 }),
            None
        );
    }

    #[test]
    fn a_point_off_the_image_is_not_sent() {
        let aim = CommandId::new();
        let view = view_with(vec![capture_of("emulator-5554", aim)]);
        assert_eq!(
            input(&view, "emulator-5554", aim, Input::Tap { x: 1080, y: 0 }),
            None
        );
        assert_eq!(
            input(&view, "emulator-5554", aim, Input::Tap { x: 0, y: 2400 }),
            None
        );
        assert!(input(&view, "emulator-5554", aim, Input::Tap { x: 1079, y: 2399 }).is_some());
    }

    #[test]
    fn blank_text_is_not_sent() {
        let aim = CommandId::new();
        let view = view_with(vec![capture_of("emulator-5554", aim)]);
        assert_eq!(
            input(&view, "emulator-5554", aim, Input::Text(String::new())),
            None
        );
        assert_eq!(
            input(&view, "emulator-5554", aim, Input::Text(" \t ".to_owned())),
            None
        );
        assert!(input(&view, "emulator-5554", aim, Input::Text(" hi ".to_owned())).is_some());
    }

    #[test]
    fn a_swipe_takes_the_fixed_duration() {
        let aim = CommandId::new();
        let view = view_with(vec![capture_of("emulator-5554", aim)]);
        let swipe = Input::Swipe {
            from_x: 1,
            from_y: 2,
            to_x: 3,
            to_y: 4,
        };
        assert!(matches!(
            input(&view, "emulator-5554", aim, swipe),
            Some(Action::Screen {
                action: ScreenAction::Swipe { duration_ms, .. },
                ..
            }) if duration_ms == SWIPE_MS
        ));
        assert_eq!(SWIPE_MS, 300);
    }

    #[test]
    fn stale_covers_a_refusal_and_a_stale_outcome() {
        let refused = request(
            ScreenRequestState::Pending,
            Some(Outgoing::Failed(FailReason::Rejected(
                RejectReason::ScreenStale,
            ))),
        );
        assert!(stale(&refused));
        let finished = request(
            ScreenRequestState::Done(ScreenOutcome::Stale { latest: None }),
            None,
        );
        assert!(stale(&finished));
        let other_refusal = request(
            ScreenRequestState::Pending,
            Some(Outgoing::Failed(FailReason::Rejected(
                RejectReason::BadScreenInput,
            ))),
        );
        assert!(!stale(&other_refusal));
        let lost = request(ScreenRequestState::Done(ScreenOutcome::Lost), None);
        assert!(!stale(&lost));
    }

    #[test]
    fn labels_name_the_action() {
        let aim = CommandId::new();
        let device = "emulator-5554".to_owned();
        assert_eq!(
            action_label(&ScreenAction::Tap {
                device: device.clone(),
                aim,
                x: 10,
                y: 20,
            }),
            "tap 10,20"
        );
        assert_eq!(
            action_label(&ScreenAction::Swipe {
                device: device.clone(),
                aim,
                from_x: 10,
                from_y: 20,
                to_x: 30,
                to_y: 40,
                duration_ms: SWIPE_MS,
            }),
            "swipe 10,20 → 30,40"
        );
        assert_eq!(
            action_label(&ScreenAction::Text {
                device: device.clone(),
                aim,
                text: "a".repeat(40),
            }),
            format!("type “{}…”", "a".repeat(30))
        );
        assert_eq!(
            action_label(&ScreenAction::Key {
                device,
                aim,
                key: ScreenKey::Back,
            }),
            "key back"
        );
    }
}
