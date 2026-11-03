use rinf::{DartSignal, RustSignal};
use serde::{Deserialize, Serialize};

/// Sent by Dart at app start and to retry a failed boot. `dir` is the app's
/// support directory (`path_provider`); the core keeps its data in
/// `<dir>/skywave`. `device_name` is what pairing shows the server.
/// `reset_replica` first drops the local replica so it syncs again from
/// the server (`Core::reset_replica`), after a [`BootFailed`] that was
/// `unreadable`.
#[derive(Deserialize, DartSignal)]
pub struct Boot {
    pub dir: String,
    pub device_name: String,
    pub reset_replica: bool,
}

/// The core is open and the hub task runs. Answers every [`Boot`] from then
/// on, without opening the core again.
#[derive(Serialize, RustSignal)]
pub struct Booted;

/// The core could not be opened. Dart shows `message` instead of the
/// spinner and may send [`Boot`] again to retry. `unreadable`: the local
/// data is from another build or damaged, so retrying alone fails again;
/// Dart offers a [`Boot`] with `reset_replica` too.
#[derive(Debug, Serialize, RustSignal)]
pub struct BootFailed {
    pub message: String,
    pub unreadable: bool,
}
