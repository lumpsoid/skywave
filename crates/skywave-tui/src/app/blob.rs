//! `D` and `K` on an output the server keeps as a blob: delete it, after a
//! y/N question, or open the page that chooses which of its lines to keep.
//! `Del` takes back this device's own pending delete or crop of it. What
//! may be done, and to which blob, is [`skywave_present::blob_actions`]'s
//! call; this turns keys into its actions.

use crossterm::event::KeyCode;
use skywave_core::{Action, BlobId, LineSpan};
use skywave_present::blob_actions::{
    BlobAction, BlobTarget, DELETE_CONFIRMATION, crop_confirmation, pending_note,
};
use skywave_present::crop::kept;

use super::App;

/// A delete or crop the user is asked about first, since neither can be
/// undone; `y` sends it, any other key drops it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobConfirm {
    Delete(BlobId),
    /// Keeping `keep` of the blob's `of` lines.
    Crop {
        blob: BlobId,
        keep: Vec<LineSpan>,
        of: u32,
    },
}

impl BlobConfirm {
    /// The question, in [`skywave_present::blob_actions`]'s words.
    pub fn question(&self) -> String {
        match self {
            BlobConfirm::Delete(_) => DELETE_CONFIRMATION.to_owned(),
            BlobConfirm::Crop { keep, of, .. } => {
                crop_confirmation(u32::try_from(kept(keep)).unwrap_or(u32::MAX), *of)
            }
        }
    }

    fn action(self) -> Action {
        match self {
            BlobConfirm::Delete(blob) => Action::DeleteBlob(blob),
            BlobConfirm::Crop { blob, keep, .. } => Action::CropBlob { blob, keep },
        }
    }
}

/// What `code` does to `target`: `D` asks before deleting, `K` opens the
/// crop page, `Del` takes back a pending delete or crop the core still
/// can. While one is pending, `D` and `K` say how it stands instead.
pub(super) fn key_on(target: BlobTarget, code: KeyCode) -> BlobKey {
    let offered = |action| target.offers.contains(&action);
    match code {
        KeyCode::Char('D') if offered(BlobAction::Delete) => {
            BlobKey::Ask(BlobConfirm::Delete(target.blob))
        }
        KeyCode::Char('K') if offered(BlobAction::Crop) => BlobKey::Send(target.open_crop()),
        KeyCode::Char('D' | 'K') => match &target.pending {
            Some(pending) => BlobKey::Say(pending_note(pending)),
            None => BlobKey::Nothing,
        },
        KeyCode::Delete => match target.pending.filter(|p| p.discardable) {
            Some(pending) => BlobKey::Send(Action::Discard(pending.command)),
            None => BlobKey::Nothing,
        },
        _ => BlobKey::Nothing,
    }
}
/// What a blob key comes to.
pub(super) enum BlobKey {
    Ask(BlobConfirm),
    Send(Action),
    Say(String),
    Nothing,
}

impl App {
    /// Carries out `outcome`: the question opens, the notice shows, or the
    /// action goes.
    pub(super) fn blob_key(&mut self, outcome: BlobKey) -> Option<Action> {
        match outcome {
            BlobKey::Ask(confirm) => self.ui.confirm = Some(confirm),
            BlobKey::Send(action) => return Some(action),
            BlobKey::Say(notice) => self.ui.notice = Some(notice),
            BlobKey::Nothing => {}
        }
        None
    }

    /// Answers the open question, if any: `y` sends it, any other key
    /// drops it. `None` when no question is open, so the key goes on.
    pub(super) fn answer_confirm(&mut self, code: KeyCode) -> Option<Option<Action>> {
        let confirm = self.ui.confirm.take()?;
        Some(matches!(code, KeyCode::Char('y' | 'Y')).then(|| confirm.action()))
    }
}
