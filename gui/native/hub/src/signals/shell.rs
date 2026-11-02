use rinf::{RustSignal, SignalPiece};
use serde::Serialize;

use super::timeline::{BlobFacts, OutgoingView};

/// The open session's shell runs: a snapshot, then keyed diffs against it,
/// as the timeline's. One type, so a snapshot and the diffs after it
/// arrive in order.
///
/// A run's key is its id (its `RunShell` command's), the same from the
/// moment this device queues it to its end: a run never changes key.
#[derive(Debug, Clone, PartialEq, Serialize, RustSignal)]
#[expect(
    clippy::large_enum_variant,
    reason = "upserts are most of the traffic; boxing would cost one allocation each"
)]
pub enum ShellUpdate {
    /// Replaces the whole list: a session opened, the app asked
    /// (`AppReady`), or the change was not a keyed diff. `session_id` is
    /// `None`, and `runs` empty, when no session is open.
    Snapshot {
        session_id: Option<String>,
        runs: Vec<ShellRunRow>,
    },
    /// Drop any row keyed `key`, then insert `run` at `index`. Diffs come
    /// with ascending indices after all removals, so applying them in
    /// order rebuilds the list.
    Upserted {
        key: String,
        run: ShellRunRow,
        index: u32,
    },
    /// Drop the row keyed `key`.
    Removed { key: String },
}

/// One shell run as the run list shows it, oldest first; every label is
/// final.
#[derive(Debug, Clone, PartialEq, Serialize, SignalPiece)]
pub struct ShellRunRow {
    /// The run's id: the row's key, and what `Discard`, `EditQueued` and
    /// `StopShellRun` name.
    pub key: String,
    /// As written, with its line breaks; tabs as spaces, other control
    /// characters dropped.
    pub command: String,
    /// The condition it runs on: `if ok ✓`, `if failed ✗`; `None` for one
    /// that always runs. Words, never shell operators.
    pub when_label: Option<String>,
    pub state: ShellRunKind,
    /// How it stands: `queued 40s`, `failed: …`, `running 12s`, `exit 1 ·
    /// 3.2s`, `skipped: previous failed`.
    pub state_label: String,
    /// `by <device>` for another device's run; `None` for this device's.
    pub by_label: Option<String>,
    /// The output's tail (running) or preview (finished), ANSI stripped;
    /// empty before any output.
    pub tail: String,
    /// The output's line count so far, or in all once finished.
    pub lines: u32,
    /// The whole output, once finished, when the preview does not hold
    /// it (`truncated`).
    pub output: Option<BlobFacts>,
    pub truncated: bool,
    /// `ExpandShellRun` asked for its whole output, until `CollapseShellRun`.
    pub expanded: bool,
    /// While `expanded`: the whole output, ANSI stripped, tabs as spaces
    /// and other control characters dropped; of a huge one, its newest
    /// 512 KiB (`whole_note` counts the lines before). `None` while the
    /// `output` blob is not here, or could not be read (`whole_note` says
    /// why). Never sent for a run not expanded.
    pub whole: Option<String>,
    /// What `whole` leaves out and why, e.g. `… 1200 earlier lines not
    /// shown`, or that a running run's whole output comes once it
    /// finishes.
    pub whole_note: Option<String>,
    /// This device's run while it is in the outbox.
    pub outgoing: Option<OutgoingView>,
    /// A stop of it is on its way or logged; it may still run a while.
    pub stopping: bool,
    /// `Discard` and `EditQueued` take it back.
    pub discardable: bool,
    /// Its own stop button does something: it is waiting or running, no
    /// stop is on its way, and it was not refused. `StopShellRun` stops
    /// it (or takes back this device's run the server never saw).
    pub can_stop: bool,
}

/// Where a run is, for its colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, SignalPiece)]
pub enum ShellRunKind {
    /// This device's run not in the log yet.
    Pending,
    /// This device's run refused, or undeliverable: it will never run.
    Refused,
    /// Logged, waiting for its turn.
    Queued,
    Running,
    /// Exited 0.
    Succeeded,
    /// Exited otherwise, killed, lost, or could not start.
    Failed,
    /// A stop ended it.
    Stopped,
    /// Its condition did not match; it never started.
    Skipped,
}
