use rinf::{RustSignal, SignalPiece};
use serde::{Deserialize, Serialize};

/// The open session's app screen (D65), whole, whenever it changes.
#[derive(Debug, Clone, PartialEq, Serialize, RustSignal)]
pub struct AppScreenUpdate {
    /// The session the screen is of; `None`, and everything empty, when
    /// no session is open.
    pub session_id: Option<String>,
    /// Shown in place of the timeline.
    pub open: bool,
    /// The devices the server last listed.
    pub devices: Vec<AppScreenDevice>,
    /// The device shown, by serial: the one the user picked, else the
    /// first ready one.
    pub device: Option<String>,
    /// Its latest capture.
    pub capture: Option<AppScreenCapture>,
    /// The device's newest requests and device listings, oldest first, at
    /// most 20.
    pub requests: Vec<AppScreenRow>,
    /// The answer to the latest typed text, by the number the GUI gave it.
    pub text_result: Option<AppScreenTextResult>,
}

/// Whether the typed text the GUI numbered `seq` was queued.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, SignalPiece)]
pub struct AppScreenTextResult {
    pub seq: u32,
    /// True once the core took it; false when it was not queued (the
    /// dispatch failed, the aim was refused, or the text was blank).
    pub queued: bool,
}

/// One attached device, as the device list shows it.
#[derive(Debug, Clone, PartialEq, Serialize, SignalPiece)]
pub struct AppScreenDevice {
    pub serial: String,
    /// Its model and serial, or the serial alone.
    pub label: String,
    /// Ready to capture (`state` "device").
    pub ready: bool,
}

/// A device's latest capture, as the screen shows it.
#[derive(Debug, Clone, PartialEq, Serialize, SignalPiece)]
pub struct AppScreenCapture {
    /// The id inputs aim at.
    pub capture_id: String,
    /// The image file once downloaded.
    pub path: Option<String>,
    pub width: u32,
    pub height: u32,
    /// Shown while `path` is null.
    pub note: Option<String>,
}

/// One request of the device's, as the request list shows it.
#[derive(Debug, Clone, PartialEq, Serialize, SignalPiece)]
pub struct AppScreenRow {
    /// The request's id: its row's key.
    pub key: String,
    /// What it asks, in words: `tap 10,20`, `capture`.
    pub label: String,
    /// How it stands: `queued`, `done`, `failed: …`.
    pub state_label: String,
    /// Still waiting for the server.
    pub waiting: bool,
    /// The screen moved on since the capture it aimed at.
    pub stale: bool,
    /// The server refused it for good.
    pub refused: bool,
    /// Taking it back dismisses it (`AppScreenDismiss`).
    pub discardable: bool,
}

/// A hardware key the screen view offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SignalPiece)]
pub enum AppScreenKeyView {
    Back,
    Home,
    Recents,
    Enter,
    Delete,
}
