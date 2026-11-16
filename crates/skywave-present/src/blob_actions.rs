//! What a device may do to a blob the server keeps: delete it, or crop it
//! to chosen lines, on the server and on every device. One rule for every
//! head, from a timeline item's content or a storage row.
//!
//! Both are queued like any command ([`skywave_core::Action::DeleteBlob`],
//! [`skywave_core::Action::CropBlob`]) and can't be undone once the server
//! takes them, so a head asks first ([`delete_confirmation`],
//! [`crop_confirmation`]).

use skywave_core::{
    Action, BlobCommand, BlobId, BlobPending, BlobUse, Entry, FullView, ImportedEntry, SessionId,
    ShellRun, ToolOutcome,
};

use crate::format;
use crate::storage::holds_label;

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
        FullView::Blob(_) | FullView::Inline(_) | FullView::Lost | FullView::Deleted { .. } => {
            Vec::new()
        }
    }
}

/// Whether a blob holding `holds` may be cropped.
pub fn croppable(holds: BlobUse) -> bool {
    matches!(holds, BlobUse::ToolOutput | BlobUse::ShellOutput)
}

/// The blob a timeline entry, a shell run or a storage row
/// ([`storage::row_target`](crate::storage::row_target)) acts on: what may
/// be done to it, this device's own pending delete or crop of it, and what
/// the crop page needs to open on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobTarget {
    /// The blob to name: the view's current one, a crop's after a crop.
    pub blob: BlobId,
    /// In the order a head offers them; empty while `pending`.
    pub offers: Vec<BlobAction>,
    /// Shown instead of the offers ([`pending_note`]); discardable while
    /// its `discardable` says so.
    pub pending: Option<BlobPending>,
    pub session: SessionId,
    pub holds: BlobUse,
    /// What the crop page calls it: the item's title or the run's command.
    pub title: String,
}

impl BlobTarget {
    /// The output `full` of an item or run; `None` once the server keeps
    /// it as no blob (lost or deleted): nothing to act on.
    fn of(full: &FullView, holds: BlobUse, session: SessionId, title: String) -> Option<Self> {
        let blob = full.blob()?;
        Some(BlobTarget {
            blob: blob.id,
            offers: item_offers(full, holds),
            pending: blob.pending,
            session,
            holds,
            title,
        })
    }

    /// The action opening the crop page on it; whether that is offered is
    /// [`offers`](Self::offers)'.
    pub fn open_crop(&self) -> Action {
        Action::OpenCrop {
            blob: self.blob,
            session: self.session,
            holds: self.holds,
            title: self.title.clone(),
        }
    }
}

/// The output of a timeline entry of `session`: a tool call's, grouped or
/// not, live or imported, titled as the call or by the result's first
/// line; or a long message's whole text (deleted, never cropped), titled
/// by its first line. `None` for any other entry, or one whose output is
/// no blob.
pub fn entry_target(entry: &Entry, session: SessionId) -> Option<BlobTarget> {
    let first_line = |text: &str| text.lines().next().unwrap_or_default().to_owned();
    let (full, title) = match entry {
        Entry::AgentMessage {
            text,
            full: Some(full),
        }
        | Entry::Imported {
            entry:
                ImportedEntry::UserMessage {
                    text,
                    full: Some(full),
                }
                | ImportedEntry::AgentMessage {
                    text,
                    full: Some(full),
                },
            ..
        } => return BlobTarget::of(full, BlobUse::Message, session, first_line(text)),
        Entry::Tool(view)
        | Entry::Imported {
            entry: ImportedEntry::Tool(view),
            ..
        } => match &view.outcome {
            ToolOutcome::Done {
                output: Some(full), ..
            } => (full, view.title.clone()),
            _ => return None,
        },
        Entry::ToolResult {
            summary,
            output: Some(full),
        }
        | Entry::Imported {
            entry:
                ImportedEntry::ToolResult {
                    summary,
                    output: Some(full),
                },
            ..
        } => (full, first_line(summary)),
        _ => return None,
    };
    BlobTarget::of(full, BlobUse::ToolOutput, session, title)
}

/// A shell run's output, titled by its command.
pub fn run_target(run: &ShellRun, session: SessionId) -> Option<BlobTarget> {
    BlobTarget::of(
        run.output.as_ref()?,
        BlobUse::ShellOutput,
        session,
        run.command.clone(),
    )
}

/// What a head asks before it deletes a blob holding `holds`, naming it as
/// its storage row does ([`holds_label`]).
pub fn delete_confirmation(holds: BlobUse) -> String {
    format!(
        "Delete this {} from the server and from every device? This can't be undone.",
        holds_label(holds)
    )
}

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

    const EVERY_USE: [BlobUse; 6] = [
        BlobUse::ToolInput,
        BlobUse::ToolBody,
        BlobUse::ToolOutput,
        BlobUse::ApprovalInput,
        BlobUse::ShellOutput,
        BlobUse::Message,
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
    fn a_long_messages_blob_may_be_deleted_but_not_cropped() {
        let session = SessionId::new();
        let full = kept(None);
        let id = full.blob().unwrap().id;
        let agent = Entry::AgentMessage {
            text: "# Report\nmore".into(),
            full: Some(full.clone()),
        };
        let imported = Entry::Imported {
            at: chrono::Utc::now(),
            entry: ImportedEntry::AgentMessage {
                text: "# Report\nmore".into(),
                full: Some(full),
            },
        };
        for entry in [agent, imported] {
            let target = entry_target(&entry, session).expect("a target");
            assert_eq!(target.blob, id);
            assert_eq!(target.holds, BlobUse::Message);
            assert_eq!(target.offers, [BlobAction::Delete]);
            assert_eq!(target.title, "# Report");
        }
        // Control: a whole message has nothing to act on.
        let whole = Entry::AgentMessage {
            text: "short".into(),
            full: None,
        };
        assert_eq!(entry_target(&whole, session), None);
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
        let question = |what: &str| {
            format!(
                "Delete this {what} from the server and from every device? This can't be undone."
            )
        };
        for (holds, what) in [
            (BlobUse::ToolInput, "tool input"),
            (BlobUse::ToolBody, "tool diff or body"),
            (BlobUse::ToolOutput, "tool output"),
            (BlobUse::ApprovalInput, "approval input"),
            (BlobUse::ShellOutput, "shell output"),
        ] {
            assert_eq!(delete_confirmation(holds), question(what), "{holds:?}");
        }
        assert_eq!(
            crop_confirmation(3, 120),
            "Keep 3 of 120 lines? Every device keeps only these. This can't be undone."
        );
    }

    fn tool(outcome: ToolOutcome) -> Entry {
        Entry::Tool(Box::new(skywave_core::ToolView {
            call: skywave_core::CallId::new(),
            name: "Bash".into(),
            kind: skywave_core::ToolKind::Shell,
            title: "$ make".into(),
            detail: None,
            body: None,
            input: None,
            approval: None,
            safety_check: None,
            safety_feedback: None,
            shell: None,
            outcome,
            subagent: None,
        }))
    }

    fn done(output: Option<FullView>) -> ToolOutcome {
        ToolOutcome::Done {
            is_error: false,
            status: None,
            preview: "out".into(),
            lines: 40,
            output,
            took: std::time::Duration::from_secs(1),
        }
    }

    fn id_of(full: &FullView) -> BlobId {
        full.blob().unwrap().id
    }

    #[test]
    fn an_entrys_target_is_its_calls_output_titled_as_the_call() {
        let session = SessionId::new();
        let output = kept(None);
        let target = entry_target(&tool(done(Some(output.clone()))), session).unwrap();
        assert_eq!(
            target.open_crop(),
            Action::OpenCrop {
                blob: id_of(&output),
                session,
                holds: BlobUse::ToolOutput,
                title: "$ make".into(),
            }
        );
        assert_eq!(target.offers, [BlobAction::Crop, BlobAction::Delete]);

        // A lone result is titled by its summary's first line, imported too.
        let result = |output| skywave_core::ImportedEntry::ToolResult {
            summary: "12 files\nmore".into(),
            output,
        };
        let imported = Entry::Imported {
            at: chrono::Utc::now(),
            entry: result(Some(output.clone())),
        };
        let target = entry_target(&imported, session).unwrap();
        assert_eq!(
            (target.blob, target.title.as_str()),
            (id_of(&output), "12 files")
        );

        // Nothing to act on: no output, one kept as no blob, still running.
        assert_eq!(entry_target(&tool(done(None)), session), None);
        for gone in [FullView::Lost, FullView::Deleted { by: me() }] {
            assert_eq!(entry_target(&tool(done(Some(gone))), session), None);
        }
        let running = ToolOutcome::Running {
            since: chrono::Utc::now(),
        };
        assert_eq!(entry_target(&tool(running), session), None);
    }

    #[test]
    fn a_runs_target_is_its_output_titled_by_its_command_and_pending_offers_nothing() {
        let session = SessionId::new();
        let waiting = pending(BlobCommand::Delete, Outgoing::Queued);
        let output = kept(Some(waiting));
        let run = skywave_core::ShellRun {
            run: CommandId::new(),
            command: "make test".into(),
            when: skywave_core::RunWhen::Always,
            by: me(),
            state: skywave_core::ShellRunState::Running,
            requested_at: None,
            started_at: None,
            finished_at: None,
            tail_or_preview: String::new(),
            lines: 0,
            output: Some(output.clone()),
            truncated: false,
            outgoing: None,
            discardable: false,
            stopping: false,
            delivery: None,
        };
        let target = run_target(&run, session).unwrap();
        assert_eq!(
            (target.blob, target.holds, target.title.as_str()),
            (id_of(&output), BlobUse::ShellOutput, "make test")
        );
        assert!(target.offers.is_empty());
        assert_eq!(target.pending, Some(waiting));
        let none = skywave_core::ShellRun {
            output: None,
            ..run
        };
        assert_eq!(run_target(&none, session), None);
    }
}
