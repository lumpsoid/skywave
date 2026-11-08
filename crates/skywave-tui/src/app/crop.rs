//! The crop page's keys and presentation state: the line cursor, how far
//! the list is scrolled, an open `v` range, and the content's line index.
//! The selection itself is the core's ([`Action::SetCropKeep`]); what may
//! be offered and sent is [`skywave_present::crop`]'s.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent};
use skywave_core::{Action, BlobId, BlobState, CropContent, CropView, LineIndex, Screen};
use skywave_present::blob_actions::pending_note;
use skywave_present::crop::{crop_enabled, kept, presets, toggle};

use super::{App, BlobConfirm};

/// The content's line index, opened once per page and file: opening reads
/// the whole file, so a frame only reads the lines it shows.
#[derive(Debug, Clone)]
pub(crate) struct CropIndex {
    pub(crate) path: PathBuf,
    pub(crate) read: Result<Arc<LineIndex>, String>,
}

#[derive(Debug, Clone, Default)]
pub struct CropUi {
    /// The blob the state below belongs to; another page resets it.
    shown: Option<BlobId>,
    /// The line the cursor is on, 0 the first.
    pub cursor: u32,
    /// The first line shown.
    pub top: u32,
    /// Where an open `v` range starts; the next `v` toggles it.
    pub anchor: Option<u32>,
    /// Rows the last frame's list had, for PgUp/PgDn and scrolling.
    pub page: Cell<usize>,
    pub(crate) index: RefCell<Option<CropIndex>>,
}

impl CropUi {
    fn follow(&mut self, view: Option<&CropView>) {
        let blob = view.map(|v| v.blob);
        if self.shown != blob {
            *self = CropUi {
                shown: blob,
                ..CropUi::default()
            };
        }
    }

    /// Puts the cursor on `line`, scrolling so it shows.
    fn move_to(&mut self, line: u32) {
        let page = u32::try_from(self.page.get().max(1)).unwrap_or(u32::MAX);
        self.cursor = line;
        if line < self.top {
            self.top = line;
        } else if line >= self.top.saturating_add(page) {
            self.top = line + 1 - page;
        }
    }

    /// The lines an open range covers with the cursor, first to last.
    pub fn range(&self) -> Option<(u32, u32)> {
        let anchor = self.anchor?;
        Some((anchor.min(self.cursor), anchor.max(self.cursor)))
    }
}

impl App {
    /// Keeps the crop page's state with the page shown.
    pub(super) fn follow_crop(&mut self) {
        let view = match &self.view.screen {
            Screen::Crop(view) => Some(view),
            _ => None,
        };
        self.ui.crop.follow(view);
    }

    pub(super) fn crop_key(&mut self, key: KeyEvent) -> Option<Action> {
        if key.code == KeyCode::Char('q') {
            return self.quit();
        }
        let Screen::Crop(view) = &self.view.screen else {
            return None;
        };
        let ui = &mut self.ui.crop;
        match key.code {
            // An open range goes first; the next Esc leaves.
            KeyCode::Esc if ui.anchor.is_some() => {
                ui.anchor = None;
                return None;
            }
            KeyCode::Esc => return Some(Action::Back),
            KeyCode::Char('d') => {
                return match view.content {
                    CropContent::NotHere(
                        BlobState::NotDownloaded { .. } | BlobState::Failed { .. },
                    ) => Some(Action::DownloadBlob(view.blob)),
                    _ => None,
                };
            }
            _ => {}
        }
        let CropContent::Ready { lines, .. } = view.content else {
            return None;
        };
        let last = lines.saturating_sub(1);
        let page = u32::try_from(ui.page.get().max(1)).unwrap_or(u32::MAX);
        let at = ui.cursor.min(last);
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => ui.move_to(at.saturating_add(1).min(last)),
            KeyCode::Up | KeyCode::Char('k') => ui.move_to(at.saturating_sub(1)),
            KeyCode::PageDown => ui.move_to(at.saturating_add(page).min(last)),
            KeyCode::PageUp => ui.move_to(at.saturating_sub(page)),
            KeyCode::Home | KeyCode::Char('g') => ui.move_to(0),
            KeyCode::End | KeyCode::Char('G') => ui.move_to(last),
            KeyCode::Char(c @ '1'..='9') => {
                let n = c as usize - '1' as usize;
                return presets(lines)
                    .get(n)
                    .map(|preset| Action::SetCropKeep(preset.keep(lines)));
            }
            KeyCode::Char('v') if lines > 0 => match ui.range() {
                None => ui.anchor = Some(at),
                Some((first, last)) => {
                    ui.anchor = None;
                    return Some(Action::SetCropKeep(toggle(&view.keep, first, last)));
                }
            },
            KeyCode::Enter if crop_enabled(view) => {
                self.ui.confirm = Some(BlobConfirm::Crop {
                    blob: view.blob,
                    keep: view.keep.clone(),
                    of: lines,
                });
            }
            KeyCode::Enter => {
                self.ui.notice = Some(match &view.pending {
                    Some(pending) => pending_note(pending),
                    None if view.keep.is_empty() => {
                        "choose the lines to keep first: a number key or v".to_owned()
                    }
                    None if kept(&view.keep) >= u64::from(lines) => {
                        "that keeps every line: nothing to crop".to_owned()
                    }
                    None => "that selection cannot be sent".to_owned(),
                });
            }
            _ => {}
        }
        None
    }
}
