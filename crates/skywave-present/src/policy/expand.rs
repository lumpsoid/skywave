//! What an expanded item downloads: expanding asks for the blob behind it,
//! and so does a blob that arrives while it is open (an edit expanded from
//! the approval panel before its result logged gets the result's output
//! once it does).

use std::collections::HashSet;

use skywave_core::{Action, BlobId, Entry, Seq, SessionView, TimelineItem};

use crate::sections::unasked_blobs;

/// Asks for the [`unasked_blobs`] of every expanded item, once per blob:
/// not again on every snapshot while its request is in flight, and not
/// again after a failure, which waits for an explicit retry. A failed blob
/// holds back none of the item's others (a call's failed body, its output
/// still fetched). The blob policy
/// needs no check: under `Eager` the core has queued every blob already, so
/// none is still not downloaded. Which items are expanded is the head's.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExpandFetch {
    /// Blobs asked for because their item was expanded.
    asked: HashSet<BlobId>,
}

impl ExpandFetch {
    pub fn new() -> ExpandFetch {
        ExpandFetch::default()
    }

    /// The user expanded `entry`: the first download it needs, asked again
    /// even if it was asked for before. Its other blobs are asked again by
    /// the next [`ExpandFetch::due`], which the head runs once `entry` is
    /// in its expanded set: a key or tap yields one action.
    pub fn expand(&mut self, entry: &Entry) -> Option<Action> {
        let mut ids = unasked_blobs(entry).into_iter().map(|b| b.id);
        let first = ids.next()?;
        for id in ids {
            self.asked.remove(&id);
        }
        self.asked.insert(first);
        Some(Action::DownloadBlob(first))
    }

    /// After each snapshot: the downloads the items in `expanded` need that
    /// were not asked for yet, in timeline order.
    pub fn due(&mut self, view: &SessionView, expanded: &HashSet<Seq>) -> Vec<Action> {
        view.timeline
            .iter()
            .flat_map(|item| match item {
                TimelineItem::Logged { seq, entry, .. } if expanded.contains(seq) => {
                    unasked_blobs(entry)
                }
                _ => Vec::new(),
            })
            .map(|blob| blob.id)
            .filter(|id| self.asked.insert(*id))
            .map(Action::DownloadBlob)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use skywave_core::{
        BlobState, BlobView, BodyFormat, BodyView, CallId, FullView, ToolKind, ToolOutcome,
        ToolView,
    };

    use super::*;
    use crate::policy::tests::{logged, message, session};
    use crate::sections::wanted_blob;

    fn blob(id: BlobId, state: BlobState) -> FullView {
        FullView::Blob(BlobView {
            id,
            state,
            change: None,
            pending: None,
        })
    }

    fn failed() -> BlobState {
        BlobState::Failed {
            size: 10,
            reason: "hash mismatch".into(),
        }
    }

    /// A finished write whose body blob is `body` and output blob `output`.
    fn write(body: FullView, output: FullView) -> Entry {
        Entry::Tool(Box::new(ToolView {
            call: CallId::new(),
            name: "Write".into(),
            kind: ToolKind::FileWrite,
            title: "a.txt".into(),
            detail: None,
            body: Some(BodyView {
                format: BodyFormat::Text,
                preview: "x".into(),
                lines: 40,
                full: Some(body),
            }),
            input: None,
            approval: None,
            safety_check: None,
            outcome: ToolOutcome::Done {
                is_error: false,
                status: None,
                preview: "out".into(),
                lines: 40,
                output: Some(output),
                took: std::time::Duration::from_secs(1),
            },
            subagent: None,
        }))
    }

    #[test]
    fn a_failed_blob_holds_back_none_of_its_items_others() {
        let (body, output) = (BlobId::new(), BlobId::new());
        let entry = write(
            blob(body, failed()),
            blob(output, BlobState::NotDownloaded { size: 10 }),
        );
        let mut fetch = ExpandFetch::new();
        assert_eq!(fetch.expand(&entry), Some(Action::DownloadBlob(output)));
        let view = session(vec![logged(1, entry.clone())]);
        assert_eq!(fetch.due(&view, &expanded(&[1])), vec![], "asked already");
        // The open item's output, under a snapshot, is due too.
        let mut fresh = ExpandFetch::new();
        assert_eq!(
            fresh.due(&view, &expanded(&[1])),
            vec![Action::DownloadBlob(output)]
        );
        // An explicit download takes the output first, then retries the
        // body once nothing else is left to ask for.
        assert_eq!(wanted_blob(&entry).map(|b| b.id), Some(output));
        let tried = write(
            blob(body, failed()),
            blob(output, BlobState::Requested { size: 10 }),
        );
        assert_eq!(wanted_blob(&tried).map(|b| b.id), Some(body));
    }

    #[test]
    fn expanding_asks_for_every_blob_the_item_needs() {
        let (body, output) = (BlobId::new(), BlobId::new());
        let state = BlobState::NotDownloaded { size: 10 };
        let entry = write(blob(body, state.clone()), blob(output, state));
        let view = session(vec![logged(1, entry.clone())]);
        let open = expanded(&[1]);
        let mut fetch = ExpandFetch::new();
        assert_eq!(fetch.due(&view, &open).len(), 2, "both, first time");
        // Expanding again asks the first at once, the rest on the next due.
        assert_eq!(fetch.expand(&entry), Some(Action::DownloadBlob(body)));
        assert_eq!(fetch.due(&view, &open), vec![Action::DownloadBlob(output)]);
        assert_eq!(fetch.due(&view, &open), vec![]);
    }

    fn result(id: BlobId, state: BlobState) -> Entry {
        Entry::ToolResult {
            summary: "out".into(),
            output: Some(FullView::Blob(BlobView {
                id,
                state,
                change: None,
                pending: None,
            })),
        }
    }

    fn not_downloaded(id: BlobId) -> Entry {
        result(id, BlobState::NotDownloaded { size: 10 })
    }

    fn expanded(seqs: &[u64]) -> HashSet<Seq> {
        seqs.iter().map(|s| Seq(*s)).collect()
    }

    #[test]
    fn an_expanded_item_asks_for_its_blob_once() {
        let (a, b) = (BlobId::new(), BlobId::new());
        let view = session(vec![
            message(1, "go"),
            logged(2, not_downloaded(a)),
            logged(3, not_downloaded(b)),
        ]);
        let mut fetch = ExpandFetch::new();
        assert_eq!(fetch.due(&view, &expanded(&[])), vec![], "nothing open");
        assert_eq!(
            fetch.due(&view, &expanded(&[1, 3])),
            vec![Action::DownloadBlob(b)],
            "only the open item with a blob"
        );
        // The next snapshot still shows it not downloaded: no repeat.
        assert_eq!(fetch.due(&view, &expanded(&[1, 3])), vec![]);
        assert_eq!(
            fetch.due(&view, &expanded(&[2, 3])),
            vec![Action::DownloadBlob(a)]
        );
    }

    #[test]
    fn a_blob_arriving_under_an_open_item_is_asked_for() {
        let id = BlobId::new();
        let mut fetch = ExpandFetch::new();
        // Expanded before its result logged: nothing to fetch yet.
        let call = Entry::ToolCall {
            name: "Edit".into(),
            summary: "src/lib.rs\n+added".into(),
            input: None,
        };
        assert_eq!(fetch.expand(&call), None);
        let open = expanded(&[2]);
        assert_eq!(
            fetch.due(&session(vec![message(1, "go"), logged(2, call)]), &open),
            vec![]
        );
        // The result's output takes its place.
        let view = session(vec![message(1, "go"), logged(2, not_downloaded(id))]);
        assert_eq!(fetch.due(&view, &open), vec![Action::DownloadBlob(id)]);
        assert_eq!(fetch.due(&view, &open), vec![]);
    }

    #[test]
    fn expanding_again_asks_again() {
        let id = BlobId::new();
        let entry = not_downloaded(id);
        let view = session(vec![logged(1, entry.clone())]);
        let mut fetch = ExpandFetch::new();
        assert_eq!(fetch.expand(&entry), Some(Action::DownloadBlob(id)));
        assert_eq!(fetch.due(&view, &expanded(&[1])), vec![], "asked already");
        assert_eq!(fetch.expand(&entry), Some(Action::DownloadBlob(id)));
    }

    #[test]
    fn only_a_blob_nobody_asked_for_is_fetched() {
        let mut fetch = ExpandFetch::new();
        let states = [
            BlobState::Requested { size: 10 },
            BlobState::Downloading {
                got: 1,
                size: 10,
                paused: None,
            },
            BlobState::Failed {
                size: 10,
                reason: "size mismatch".into(),
            },
            BlobState::Available {
                path: "/nowhere".into(),
            },
        ];
        for state in states {
            let entry = result(BlobId::new(), state);
            assert_eq!(fetch.expand(&entry), None, "{entry:?}");
            let view = session(vec![logged(1, entry)]);
            assert_eq!(fetch.due(&view, &expanded(&[1])), vec![]);
        }
    }
}
