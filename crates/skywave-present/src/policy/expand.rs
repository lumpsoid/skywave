//! What an expanded item downloads: expanding asks for the blob behind it,
//! and so does a blob that arrives while it is open (an edit expanded from
//! the approval panel before its result logged gets the result's output
//! once it does).

use std::collections::HashSet;

use skywave_core::{Action, BlobId, BlobState, Entry, Seq, SessionView, TimelineItem};

use crate::sections::wanted_blob;

/// Asks for the [`wanted_blob`] of every expanded item, once per blob: not
/// again on every snapshot while its request is in flight, and not again
/// after a failure, which waits for an explicit retry. The blob policy
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

    /// The user expanded `entry`: the download it needs, asked again even
    /// if it was asked for before.
    pub fn expand(&mut self, entry: &Entry) -> Option<Action> {
        let id = wanted(entry)?;
        self.asked.insert(id);
        Some(Action::DownloadBlob(id))
    }

    /// After each snapshot: the downloads the items in `expanded` need that
    /// were not asked for yet, in timeline order.
    pub fn due(&mut self, view: &SessionView, expanded: &HashSet<Seq>) -> Vec<Action> {
        view.timeline
            .iter()
            .filter_map(|item| match item {
                TimelineItem::Logged { seq, entry, .. } if expanded.contains(seq) => wanted(entry),
                _ => None,
            })
            .filter(|id| self.asked.insert(*id))
            .map(Action::DownloadBlob)
            .collect()
    }
}

/// The blob expanding `entry` downloads: present's [`wanted_blob`] while
/// nobody asked for it; a failed one waits for an explicit retry.
fn wanted(entry: &Entry) -> Option<BlobId> {
    wanted_blob(entry)
        .filter(|b| matches!(b.state, BlobState::NotDownloaded { .. }))
        .map(|b| b.id)
}

#[cfg(test)]
mod tests {
    use skywave_core::{BlobView, FullView};

    use super::*;
    use crate::policy::tests::{logged, message, session};

    fn result(id: BlobId, state: BlobState) -> Entry {
        Entry::ToolResult {
            summary: "out".into(),
            output: Some(FullView::Blob(BlobView {
                id,
                state,
                change: None,
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
                paused: false,
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
