//! Deleting and cropping a blob from the GUI: the [`BlobTargetView`] a
//! timeline item, shell run or storage row carries, the crop page as a
//! [`CropScreen`], and its lines a window at a time ([`CropLines`]).
//!
//! What may be done, to which blob, and in which words is
//! [`skywave_present::blob_actions`]' and [`skywave_present::crop`]'s;
//! this puts them in Dart's shape.

use std::ops::Range;
use std::path::PathBuf;

use skywave_core::{
    BlobId, BlobUse, BlobView, CropContent, CropView, LineIndex, LineSpan, SessionId, ShellRun,
};
use skywave_present::blob_actions::{
    BlobAction, BlobTarget, crop_confirmation, delete_confirmation, pending_note, run_target,
};
use skywave_present::crop::{crop_enabled, keeps_label, kept, presets};
use skywave_present::format::{blob_size, bytes};
use skywave_present::output::output_text;
use skywave_present::text::{clean, clean_line};

use crate::signals::{
    BlobTargetView, BlobUseView, CropContentKind, CropLine, CropLinesUpdate, CropPresetView,
    CropScreen, LineSpanView, ScreenView,
};
use crate::timeline::blob_facts;

/// Most lines one `CropLines` window carries; a larger ask is cut to it.
pub const CROP_WINDOW_MAX: u32 = 1000;

/// `target` as Dart offers it.
pub fn target_view(target: &BlobTarget) -> BlobTargetView {
    let offered = |action| target.offers.contains(&action);
    BlobTargetView {
        blob_id: target.blob.0.to_string(),
        can_crop: offered(BlobAction::Crop),
        delete_question: offered(BlobAction::Delete).then(|| delete_confirmation(target.holds)),
        pending_note: target.pending.as_ref().map(pending_note),
        discard_id: target
            .pending
            .filter(|p| p.discardable)
            .map(|p| p.command.0.to_string()),
        session_id: target.session.0.to_string(),
        holds: blob_use_view(target.holds),
        title: clean_line(&target.title),
    }
}

/// A shell run's output target, as its row carries it.
pub fn run_target_view(run: &ShellRun, session: SessionId) -> Option<BlobTargetView> {
    run_target(run, session).as_ref().map(target_view)
}

fn blob_use_view(holds: BlobUse) -> BlobUseView {
    match holds {
        BlobUse::ToolInput => BlobUseView::ToolInput,
        BlobUse::ToolBody => BlobUseView::ToolBody,
        BlobUse::ToolOutput => BlobUseView::ToolOutput,
        BlobUse::ApprovalInput => BlobUseView::ApprovalInput,
        BlobUse::ShellOutput => BlobUseView::ShellOutput,
    }
}

fn span_views(spans: &[LineSpan]) -> Vec<LineSpanView> {
    spans
        .iter()
        .map(|s| LineSpanView {
            first: s.first,
            count: s.count,
        })
        .collect()
}

/// The crop page of `view`.
pub fn screen(view: &CropView) -> ScreenView {
    let (content, message, download, lines) = match &view.content {
        CropContent::NotHere(state) => {
            let size =
                blob_size(state).map_or_else(String::new, |size| format!(" {}", bytes(size)));
            let blob = BlobView {
                id: view.blob,
                state: state.clone(),
                change: None,
                pending: None,
            };
            (
                CropContentKind::NotHere,
                Some(format!(
                    "This output isn't on this device. Download{size} to choose lines."
                )),
                Some(blob_facts(&blob)),
                0,
            )
        }
        CropContent::Counting => (
            CropContentKind::Counting,
            Some("Counting its lines…".to_owned()),
            None,
            0,
        ),
        CropContent::Ready { lines, .. } => (CropContentKind::Ready, None, None, *lines),
        CropContent::Unreadable { reason } => (
            CropContentKind::Unreadable,
            Some(format!("Cannot read it here: {}", clean(reason))),
            None,
            0,
        ),
        CropContent::Gone => (
            CropContentKind::Gone,
            Some(
                "This output was deleted or cropped since: there is nothing to crop here."
                    .to_owned(),
            ),
            None,
            0,
        ),
    };
    let ready = content == CropContentKind::Ready;
    let can_crop = crop_enabled(view);
    let kept_lines = kept(&view.keep);
    ScreenView::Crop {
        crop: CropScreen {
            blob_id: view.blob.0.to_string(),
            title: clean_line(&view.title),
            content,
            message,
            download,
            lines,
            keep: span_views(&view.keep),
            presets: presets(lines)
                .into_iter()
                .map(|preset| CropPresetView {
                    label: preset.label(),
                    keep: span_views(&preset.keep(lines)),
                })
                .collect(),
            keeps_label: ready.then(|| keeps_label(kept_lines, lines)),
            can_crop,
            confirmation: can_crop
                .then(|| crop_confirmation(u32::try_from(kept_lines).unwrap_or(u32::MAX), lines)),
            pending_note: view.pending.as_ref().map(pending_note),
            discard_id: view
                .pending
                .filter(|p| p.discardable)
                .map(|p| p.command.0.to_string()),
        },
    }
}

/// The crop page's line window: the [`LineIndex`] of the content shown,
/// opened once per file (opening reads it whole), and the lines of the
/// window Dart last asked for, read once per window.
///
/// It also keeps the selection this head last sent for the page: a second
/// toggle before the core's view of the first arrives builds on the first,
/// not on the selection the view still shows.
#[derive(Debug, Default)]
pub struct CropLines {
    /// The page the window and the selection are of.
    blob: Option<BlobId>,
    /// The window asked for (`CropLines`), cut to [`CROP_WINDOW_MAX`].
    window: Option<Range<u32>>,
    /// The selection last sent (`SetCropKeep`) for `blob`.
    keep: Option<Vec<LineSpan>>,
    index: Option<(PathBuf, Result<LineIndex, String>)>,
    /// The window's lines as read from `index`'s file.
    read: Option<Read>,
}

/// Lines read from a [`LineIndex`], cleaned.
#[derive(Debug)]
struct Read {
    window: Range<u32>,
    text: Result<Vec<String>, String>,
}

impl CropLines {
    /// Asks for lines `from .. from + count`.
    pub fn ask(&mut self, from: u32, count: u32) {
        self.window = Some(from..from.saturating_add(count.min(CROP_WINDOW_MAX)));
    }

    /// The selection a toggle on `view` starts from: the one this head
    /// sent last, else the view's.
    pub fn keep<'a>(&'a self, view: &'a CropView) -> &'a [LineSpan] {
        match &self.keep {
            Some(keep) if self.blob == Some(view.blob) => keep,
            _ => &view.keep,
        }
    }

    /// `keep` was sent as the selection of the page of `blob`.
    pub fn sent_keep(&mut self, blob: BlobId, keep: Vec<LineSpan>) {
        self.follow(blob);
        self.keep = Some(keep);
    }

    /// Starts over on another page.
    fn follow(&mut self, blob: BlobId) {
        if self.blob != Some(blob) {
            *self = CropLines {
                blob: Some(blob),
                ..CropLines::default()
            };
        }
    }

    /// The window's lines of the page `view`, each marked kept or not;
    /// `None` while there is no window or nothing counted to read.
    pub fn update(&mut self, view: &CropView) -> Option<CropLinesUpdate> {
        self.follow(view.blob);
        let CropContent::Ready { path, lines } = &view.content else {
            return None;
        };
        let asked = self.window.clone()?;
        let window = asked.start.min(*lines)..asked.end.min(*lines);
        if self.index.as_ref().is_none_or(|(p, _)| p != path) {
            let index = LineIndex::open(path)
                .map_err(|err| format!("cannot read {}: {err}", path.display()));
            self.index = Some((path.clone(), index));
            self.read = None;
        }
        // A new file drops what was read of the last.
        let fresh = self.read.as_ref().is_some_and(|r| r.window == window);
        if !fresh {
            let text = match &self.index {
                Some((_, Ok(index))) => index
                    .lines(u64::from(window.start)..u64::from(window.end))
                    .map(|read| {
                        read.iter()
                            .map(|bytes| clean(&output_text(bytes, false)))
                            .collect()
                    })
                    .map_err(|err| format!("cannot read {}: {err}", path.display())),
                Some((_, Err(err))) => Err(err.clone()),
                None => Err("no line index".to_owned()),
            };
            self.read = Some(Read {
                window: window.clone(),
                text,
            });
        }
        let (lines, error) = match &self.read.as_ref()?.text {
            Ok(text) => (
                text.iter()
                    .zip(window.start..)
                    .map(|(text, index)| CropLine {
                        index,
                        text: text.clone(),
                        kept: kept_line(&view.keep, index),
                    })
                    .collect(),
                None,
            ),
            Err(err) => (Vec::new(), Some(clean(err))),
        };
        Some(CropLinesUpdate {
            blob_id: view.blob.0.to_string(),
            from: window.start,
            lines,
            error,
        })
    }

    /// Forgets the window and the file: the page closed.
    pub fn close(&mut self) {
        *self = CropLines::default();
    }
}

/// Whether `spans` keep line `n`.
fn kept_line(spans: &[LineSpan], n: u32) -> bool {
    spans
        .iter()
        .any(|s| n >= s.first && u64::from(n) < u64::from(s.first) + u64::from(s.count))
}
