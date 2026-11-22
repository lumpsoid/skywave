use rinf::{RustSignal, SignalPiece};
use serde::{Deserialize, Serialize};

use super::timeline::BlobFacts;

/// Lines `first .. first + count` of some content, 0 the first line, as a
/// crop names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SignalPiece)]
pub struct LineSpanView {
    pub first: u32,
    pub count: u32,
}

/// What a blob holds, as `OpenCrop` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SignalPiece)]
pub enum BlobUseView {
    ToolInput,
    ToolBody,
    ToolOutput,
    ApprovalInput,
    ShellOutput,
    Message,
    Attachment,
    ScreenCapture,
}

/// The blob a timeline item's output, a shell run's output or a storage
/// row stands for, and what may be done to it: every field is final, so
/// Dart decides nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct BlobTargetView {
    /// For `DeleteBlob` and `OpenCrop`: the blob a view shows now, a
    /// crop's after a crop.
    pub blob_id: String,
    /// `OpenCrop { blob_id, session_id, holds, title }` is offered.
    pub can_crop: bool,
    /// What to ask before `DeleteBlob`; `None` when a delete is not
    /// offered. Only the confirmed dialog sends it.
    pub delete_question: Option<String>,
    /// This device's own delete or crop of it on its way, or refused:
    /// `Delete queued`, `Crop failed: …`. Shown instead of the offers.
    pub pending_note: Option<String>,
    /// The command `Discard` takes back, while it still can.
    pub discard_id: Option<String>,
    pub session_id: String,
    pub holds: BlobUseView,
    /// What the crop page calls it.
    pub title: String,
}

/// The crop page: which lines of an output to keep. Its lines come a
/// window at a time (`CropLines` asks, [`CropLinesUpdate`] answers).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct CropScreen {
    /// The blob a crop names; a [`CropLinesUpdate`] of another is stale.
    pub blob_id: String,
    pub title: String,
    pub content: CropContentKind,
    /// What the page says instead of lines: not on this device, counting,
    /// unreadable, gone. `None` while `Ready`.
    pub message: Option<String>,
    /// `NotHere`: the blob with where its download stands; `can_download`
    /// offers its one Download button (`DownloadBlob`).
    pub download: Option<BlobFacts>,
    /// The content's line count once `Ready`; 0 otherwise.
    pub lines: u32,
    /// The selection as the core holds it, normalised.
    pub keep: Vec<LineSpanView>,
    /// The ready-made selections worth offering; tapping one sends
    /// `SetCropKeep` with its `keep`.
    pub presets: Vec<CropPresetView>,
    /// `keeps 3 of 120 lines`, once `Ready`.
    pub keeps_label: Option<String>,
    /// The Crop button sends: the selection keeps fewer lines than there
    /// are, and nothing of this device's is pending.
    pub can_crop: bool,
    /// What to ask before `CropBlob`, while `can_crop`.
    pub confirmation: Option<String>,
    /// This device's own delete or crop of the blob, as a target says it.
    pub pending_note: Option<String>,
    /// The command `Discard` takes back, while it still can.
    pub discard_id: Option<String>,
}

/// Where the content stands on this device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, SignalPiece)]
pub enum CropContentKind {
    NotHere,
    Counting,
    Ready,
    Unreadable,
    Gone,
}

/// One ready-made selection: `First 50`, `Last 200`, ….
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct CropPresetView {
    pub label: String,
    pub keep: Vec<LineSpanView>,
}

/// The crop page's lines `from ..` as `CropLines` last asked for them,
/// sent again whenever they or the selection change. Never the whole
/// content: at most the window asked for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, RustSignal)]
pub struct CropLinesUpdate {
    /// The blob they are of ([`CropScreen::blob_id`]).
    pub blob_id: String,
    pub from: u32,
    pub lines: Vec<CropLine>,
    /// Why the lines could not be read; `lines` is then empty.
    pub error: Option<String>,
}

/// One line of the crop page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct CropLine {
    /// 0 the first, as a span names it; shown as `index + 1`.
    pub index: u32,
    /// ANSI stripped, tabs as spaces, other control characters dropped.
    pub text: String,
    /// The selection keeps it.
    pub kept: bool,
}
