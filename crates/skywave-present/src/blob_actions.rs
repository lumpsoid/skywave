//! What a device may do to a blob the server keeps: delete it, or crop it
//! to chosen lines, on the server and on every device. One rule for every
//! head, from a timeline item's content or a storage row.
//!
//! Both are queued like any command ([`skywave_core::Action::DeleteBlob`],
//! [`skywave_core::Action::CropBlob`]) and can't be undone once the server
//! takes them, so a head asks first ([`DELETE_CONFIRMATION`],
//! [`crop_confirmation`]).

use skywave_core::{BlobCommand, BlobPending, BlobUse, FullView};

use crate::format;

/// One thing a device may do to a blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobAction {
    /// Keep only chosen lines.
    Crop,
    /// Delete it all.
    Delete,
}

/// What may be done to a blob holding `holds`, of every kind, in the order
/// a head offers them: delete anything, crop only a tool's or a shell
/// run's output (their lines are what a crop keeps).
pub fn offers_for(holds: BlobUse) -> Vec<BlobAction> {
    if croppable(holds) {
        vec![BlobAction::Crop, BlobAction::Delete]
    } else {
        vec![BlobAction::Delete]
    }
}

/// What may be done to the content `full` stands for, holding `holds`:
/// nothing unless the server still keeps it as a blob, and nothing while
/// this device's own delete or crop of it is pending (a refusal included,
/// which shows instead: [`pending_note`]). The id to act on is the
/// view's blob, a crop's after a crop.
pub fn item_offers(full: &FullView, holds: BlobUse) -> Vec<BlobAction> {
    match full {
        FullView::Blob(blob) if blob.pending.is_none() => offers_for(holds),
        FullView::Blob(_) | FullView::Lost | FullView::Deleted { .. } => Vec::new(),
    }
}

/// Whether a blob holding `holds` may be cropped.
pub fn croppable(holds: BlobUse) -> bool {
    matches!(holds, BlobUse::ToolOutput | BlobUse::ShellOutput)
}

/// What a head asks before it deletes a blob.
pub const DELETE_CONFIRMATION: &str =
    "Delete this output from the server and from every device? This can't be undone.";

/// What a head asks before it crops a blob of `of` lines to `kept`.
pub fn crop_confirmation(kept: u32, of: u32) -> String {
    format!("Keep {kept} of {of} lines? Every device keeps only these. This can't be undone.")
}

/// This device's pending delete or crop of a blob
/// ([`BlobView::pending`](skywave_core::BlobView::pending)) in words, with
/// why the server refused it. A head offers
/// [`Action::Discard`](skywave_core::Action::Discard) of its `command`
/// while it is `discardable`; once discarded, the blob's offers return.
pub fn pending_note(pending: &BlobPending) -> String {
    let what = match pending.kind {
        BlobCommand::Delete => "Delete",
        BlobCommand::Crop => "Crop",
    };
    format!("{what} {}", format::outgoing_label(&pending.outgoing))
}

#[cfg(test)]
mod tests {
    use skywave_core::{
        BlobChange, BlobId, BlobState, BlobView, CommandId, DeviceId, DeviceRef, FailReason,
        Outgoing, RejectReason, Seq,
    };

    use super::*;

    const EVERY_USE: [BlobUse; 5] = [
        BlobUse::ToolInput,
        BlobUse::ToolBody,
        BlobUse::ToolOutput,
        BlobUse::ApprovalInput,
        BlobUse::ShellOutput,
    ];

    fn me() -> DeviceRef {
        DeviceRef {
            id: DeviceId::new(),
            name: None,
            me: true,
        }
    }

    fn pending(kind: BlobCommand, outgoing: Outgoing) -> BlobPending {
        BlobPending {
            command: CommandId::new(),
            kind,
            outgoing,
            discardable: !matches!(outgoing, Outgoing::Accepted(_)),
        }
    }

    fn kept(pending: Option<BlobPending>) -> FullView {
        FullView::Blob(BlobView {
            id: BlobId::new(),
            state: BlobState::NotDownloaded { size: 10 },
            change: None,
            pending,
        })
    }

    #[test]
    fn only_tool_and_shell_output_offer_a_crop_and_every_blob_a_delete() {
        for holds in EVERY_USE {
            let offers = item_offers(&kept(None), holds);
            assert!(offers.contains(&BlobAction::Delete), "{holds:?}");
            let crop = matches!(holds, BlobUse::ToolOutput | BlobUse::ShellOutput);
            assert_eq!(offers.contains(&BlobAction::Crop), crop, "{holds:?}");
        }
        assert_eq!(
            item_offers(&kept(None), BlobUse::ShellOutput),
            vec![BlobAction::Crop, BlobAction::Delete]
        );
    }

    #[test]
    fn a_cropped_blob_may_be_cropped_again() {
        let cropped = FullView::Blob(BlobView {
            id: BlobId::new(),
            state: BlobState::NotDownloaded { size: 10 },
            change: Some(BlobChange::Cropped {
                kept: 2,
                of: 9,
                by: me(),
            }),
            pending: None,
        });
        assert_eq!(
            item_offers(&cropped, BlobUse::ToolOutput),
            vec![BlobAction::Crop, BlobAction::Delete]
        );
    }

    #[test]
    fn nothing_is_offered_while_a_delete_or_crop_is_pending_or_refused() {
        let refused = Outgoing::Failed(FailReason::Rejected(RejectReason::BlobGone));
        for outgoing in [
            Outgoing::Queued,
            Outgoing::Sending,
            Outgoing::Accepted(Seq(4)),
            refused,
        ] {
            for kind in [BlobCommand::Delete, BlobCommand::Crop] {
                assert_eq!(
                    item_offers(&kept(Some(pending(kind, outgoing))), BlobUse::ToolOutput),
                    Vec::new(),
                    "{kind:?} {outgoing:?}"
                );
            }
        }
        assert_eq!(
            pending_note(&pending(BlobCommand::Delete, refused)),
            "Delete failed: that content was already deleted or cropped"
        );
        assert_eq!(
            pending_note(&pending(BlobCommand::Crop, Outgoing::Queued)),
            "Crop queued"
        );
    }

    #[test]
    fn content_the_server_no_longer_keeps_offers_nothing() {
        for full in [FullView::Lost, FullView::Deleted { by: me() }] {
            for holds in EVERY_USE {
                assert_eq!(item_offers(&full, holds), Vec::new(), "{full:?} {holds:?}");
            }
        }
    }

    #[test]
    fn the_confirmations_say_it_cannot_be_undone() {
        assert_eq!(
            DELETE_CONFIRMATION,
            "Delete this output from the server and from every device? This can't be undone."
        );
        assert_eq!(
            crop_confirmation(3, 120),
            "Keep 3 of 120 lines? Every device keeps only these. This can't be undone."
        );
    }
}
