use rinf::{DartSignal, RustSignal};
use serde::{Deserialize, Serialize};

/// Sent by Dart at app start and to retry a failed boot. `dir` is the app's
/// support directory (`path_provider`); the core keeps its data in
/// `<dir>/skywave`. `device_name` is what pairing shows the server.
/// `reset_replica` first drops the local replica so it syncs again from
/// the server (`Core::reset_replica`), after a [`BootFailed`] that was
/// `unreadable`. The hub keeps the last one, to reopen the core itself
/// after a failure off screen.
#[derive(Debug, Clone, Deserialize, DartSignal)]
pub struct Boot {
    pub dir: String,
    pub device_name: String,
    pub reset_replica: bool,
}

/// The core is open and the hub task runs. Answers every [`Boot`] from then
/// on, without opening the core again.
#[derive(Serialize, RustSignal)]
pub struct Booted;

/// The core could not be opened. Dart shows `message` instead of the startup
/// page and may send [`Boot`] again to retry. `unreadable`: the local data is
/// from another build or damaged, so retrying alone fails again; Dart offers a
/// [`Boot`] with `reset_replica` too.
#[derive(Debug, Serialize, RustSignal)]
pub struct BootFailed {
    pub message: String,
    pub unreadable: bool,
}

/// The hub panicked (its task, or the dispatch thread): `message` is the
/// panic's. The core is closed and nothing the GUI sends is acted on until
/// Dart sends [`Boot`] again, which opens the core afresh and answers
/// [`Booted`] (or [`BootFailed`]). Off screen with the background service
/// holding the link, the hub reopens the core itself, once, and answers
/// [`Booted`] (or [`BootFailed`]) unasked.
///
/// `epoch` counts the failures in this process. Dart answers each at once
/// with `GuiIntent::HubFailedSeen` of the same `epoch`: the intents it sent
/// before that were meant for the head that failed, and the new head drops
/// them.
#[derive(Debug, Serialize, RustSignal)]
pub struct HubFailed {
    pub message: String,
    pub epoch: u32,
}
