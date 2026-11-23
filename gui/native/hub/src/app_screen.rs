//! The app screen (D65) as [`AppScreenUpdate`]: the open session's devices,
//! the shown device's latest capture and its newest requests, worded by
//! skywave-present. What the user toggled and picked is the head's; this
//! only maps it.

use skywave_core::{AppCapture, BlobState, ScreenDevice, ScreenKey, ScreenRequest, SessionView};
use skywave_present::app_screen::{self as present};

use crate::signals::{
    AppScreenCapture, AppScreenDevice, AppScreenKeyView, AppScreenRow, AppScreenUpdate,
};

/// Most requests the update carries.
const MAX_REQUESTS: usize = 20;

/// The device the screen shows: `selected` when the user picked one, else
/// the default one.
pub fn shown_device(view: &SessionView, selected: Option<&str>) -> Option<String> {
    selected
        .map(str::to_owned)
        .or_else(|| present::default_device(&view.app_screen).map(str::to_owned))
}

/// The app screen of `view`, the open session (`None`: no session is open,
/// and everything is empty), `open` as the head toggled it, and `device`
/// the device the user picked, if any.
pub fn update(view: Option<&SessionView>, open: bool, device: Option<&str>) -> AppScreenUpdate {
    let Some(view) = view else {
        return AppScreenUpdate {
            session_id: None,
            open: false,
            devices: Vec::new(),
            device: None,
            capture: None,
            requests: Vec::new(),
        };
    };
    let app = &view.app_screen;
    let shown = shown_device(view, device);
    let capture = shown
        .as_deref()
        .and_then(|serial| app.latest(serial))
        .map(capture_row);
    let mine: Vec<&ScreenRequest> = app
        .requests
        .iter()
        .filter(|r| {
            r.action
                .device()
                .is_none_or(|serial| Some(serial) == shown.as_deref())
        })
        .collect();
    let skip = mine.len().saturating_sub(MAX_REQUESTS);
    AppScreenUpdate {
        session_id: Some(view.id.0.to_string()),
        open,
        devices: app.devices.iter().map(device_row).collect(),
        device: shown,
        capture,
        requests: mine[skip..].iter().map(|r| request_row(r)).collect(),
    }
}

/// The key a key intent names, as the screen sends it.
pub fn key(key: AppScreenKeyView) -> ScreenKey {
    match key {
        AppScreenKeyView::Back => ScreenKey::Back,
        AppScreenKeyView::Home => ScreenKey::Home,
        AppScreenKeyView::Recents => ScreenKey::Recents,
        AppScreenKeyView::Enter => ScreenKey::Enter,
        AppScreenKeyView::Delete => ScreenKey::Delete,
    }
}

fn device_row(device: &ScreenDevice) -> AppScreenDevice {
    AppScreenDevice {
        serial: device.serial.clone(),
        label: present::device_label(device),
        ready: device.state == "device",
    }
}

fn capture_row(capture: &AppCapture) -> AppScreenCapture {
    AppScreenCapture {
        capture_id: capture.capture.0.to_string(),
        path: capture.content.blob().and_then(|blob| match &blob.state {
            BlobState::Available { path } => Some(path.display().to_string()),
            _ => None,
        }),
        width: capture.width,
        height: capture.height,
        note: present::capture_note(capture),
    }
}

fn request_row(request: &ScreenRequest) -> AppScreenRow {
    AppScreenRow {
        key: request.request.0.to_string(),
        label: present::action_label(&request.action),
        state_label: present::state_label(request),
        waiting: present::waiting(request),
        stale: present::stale(request),
        refused: request.refused(),
        discardable: request.discardable,
    }
}
