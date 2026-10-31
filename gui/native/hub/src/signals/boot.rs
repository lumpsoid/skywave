use rinf::{DartSignal, RustSignal};
use serde::{Deserialize, Serialize};

/// Sent by Dart at app start and to retry a failed boot. `dir` is the app's
/// support directory (`path_provider`); the core keeps its data in
/// `<dir>/skywave`. `device_name` is what pairing shows the server.
#[derive(Deserialize, DartSignal)]
pub struct Boot {
    pub dir: String,
    pub device_name: String,
}

/// The core is open and the hub task runs. Answers every [`Boot`] from then
/// on, without opening the core again.
#[derive(Serialize, RustSignal)]
pub struct Booted;

/// The core could not be opened. Dart shows `message` instead of the
/// spinner and may send [`Boot`] again to retry.
#[derive(Serialize, RustSignal)]
pub struct BootFailed {
    pub message: String,
}
