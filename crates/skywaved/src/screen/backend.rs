//! The platform a screen runner drives (D65). A backend lists devices,
//! captures their screens and sends them input. The runner is its only
//! caller, so nothing here names a platform tool (D3).

use std::fmt;
use std::path::Path;

use skywave_proto::{ScreenDevice, ScreenKey};

/// A backend call that failed. The string is the reason the runner reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendError(pub String);

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BackendError {}

/// A platform that lists devices, captures their screens and sends them
/// input. The runner runs on one thread and is the only caller, so the
/// futures need not be `Send`.
pub trait ScreenBackend {
    /// The devices the platform can see, with their states.
    fn devices(&mut self) -> impl Future<Output = Result<Vec<ScreenDevice>, BackendError>>;

    /// A PNG of the device's screen.
    fn capture(&mut self, device: &str) -> impl Future<Output = Result<Vec<u8>, BackendError>>;

    /// A tap at pixel `(x, y)` of the screen.
    fn tap(
        &mut self,
        device: &str,
        x: u32,
        y: u32,
    ) -> impl Future<Output = Result<(), BackendError>>;

    /// A swipe from `from` to `to`, in pixels, taking `duration_ms`.
    fn swipe(
        &mut self,
        device: &str,
        from: (u32, u32),
        to: (u32, u32),
        duration_ms: u32,
    ) -> impl Future<Output = Result<(), BackendError>>;

    /// Types `text` into the focused field.
    fn text(&mut self, device: &str, text: &str) -> impl Future<Output = Result<(), BackendError>>;

    /// Presses `key`.
    fn key(
        &mut self,
        device: &str,
        key: ScreenKey,
    ) -> impl Future<Output = Result<(), BackendError>>;

    /// Installs the APK at `apk`, replacing an installed copy.
    fn install(
        &mut self,
        device: &str,
        apk: &Path,
    ) -> impl Future<Output = Result<(), BackendError>>;

    /// Starts `app`: a package id, or a `package/activity` component.
    fn launch(&mut self, device: &str, app: &str)
    -> impl Future<Output = Result<(), BackendError>>;

    /// Stops `app`.
    fn stop(&mut self, device: &str, app: &str) -> impl Future<Output = Result<(), BackendError>>;
}
