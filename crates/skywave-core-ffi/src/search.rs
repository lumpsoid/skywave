//! Searching the open session's timeline, by the core's rules
//! ([`skywave_core::search`]): the head keeps only the query and which match
//! is current (D2).
//!
//! [`SkywaveCore::start_search`] opens a [`TimelineSearch`] over one
//! session; each [`TimelineSearch::find`] searches the core's current
//! snapshot of it, reading the heads of its downloaded blobs (at most
//! [`HEAD_BYTES`] each) once per search, as the TUI does. A hit names its
//! item by what the head keys items by — a message by its send command,
//! everything else by its seq — never by index, so a result taken from a
//! snapshot a little newer or older than the one on screen still points at
//! the right items; one the head does not show yet it skips.
//! [`search_ranges`] gives where a query matches in a text the head renders,
//! in UTF-16 code units for highlighting.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, Mutex};

use skywave_core as sc;
use skywave_core::search::{self, BlobText, HEAD_BYTES, Query};

use crate::action::parse;
use crate::client::SkywaveCore;
use crate::error::SkywaveError;
use crate::view::id;

/// A search through one session's timeline. Holds the heads of the
/// downloaded blobs it has read until it is closed (Kotlin `close()`), so
/// typing a query reads each file once.
#[derive(uniffi::Object)]
pub struct TimelineSearch {
    core: Arc<SkywaveCore>,
    session: sc::SessionId,
    /// `None` for a blob whose file could not be read.
    blobs: Mutex<HashMap<sc::BlobId, Option<BlobText>>>,
}

/// What a search found in the session's timeline.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SearchResult {
    /// The query searched for, as given.
    pub query: String,
    /// The matching items, oldest first.
    pub hits: Vec<SearchHit>,
    /// Downloaded blobs whose content was searched.
    pub searched_blobs: u64,
    /// Blobs not downloaded, so their content was not searched.
    pub not_downloaded: u64,
    /// Downloaded blobs searched only in their first `head_bytes`.
    pub cut: u64,
    /// How much of a downloaded blob a search reads.
    pub head_bytes: u64,
}

/// A timeline item a query matches.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SearchHit {
    pub item: SearchItem,
    /// The first match in the content of the item's downloaded blobs, if
    /// any; `None` when it matches only in what the timeline shows.
    pub blob: Option<SearchBlobLine>,
}

/// Which timeline item a hit is on, by the ids a head keys items by.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum SearchItem {
    /// A logged item; `command_id` is the send command of a user message.
    Logged {
        seq: u64,
        command_id: Option<String>,
    },
    /// A message from this device not in the log yet.
    Pending { command_id: String },
}

/// A line of a downloaded blob's content where a query matches.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SearchBlobLine {
    pub blob_id: String,
    /// 0 the first line, lines split at `\n`.
    pub line: u64,
}

/// Part of a text in UTF-16 code units, as Kotlin strings index: `start`
/// inclusive, `end` exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct Utf16Range {
    pub start: u64,
    pub end: u64,
}

#[uniffi::export]
impl SkywaveCore {
    /// Starts a search through session `session_id`'s timeline. Close the
    /// result when the search ends, to drop the blob content it read.
    pub fn start_search(
        self: Arc<Self>,
        session_id: String,
    ) -> Result<Arc<TimelineSearch>, SkywaveError> {
        let session = parse("session_id", session_id)?;
        Ok(Arc::new(TimelineSearch {
            core: self,
            session,
            blobs: Mutex::new(HashMap::new()),
        }))
    }
}

#[uniffi::export]
impl TimelineSearch {
    /// The items `query` matches in the core's current snapshot of the
    /// session; nothing when the session is not the open one. Smart case,
    /// as every head searches. Blocking (it may read files): call it off
    /// the main thread. Calls run one at a time.
    pub fn find(&self, query: String) -> Result<SearchResult, SkywaveError> {
        // The core handle goes before any file is read: shutdown waits for it.
        let view = self.core.core()?.view();
        let timeline = match view.screen {
            sc::Screen::Session(session) if session.id == self.session => session.timeline,
            _ => Vec::new(),
        };
        let mut blobs = self
            .blobs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(find(&timeline, query, &mut |blob| {
            blobs
                .entry(blob.id)
                .or_insert_with(|| BlobText::read(blob))
                .clone()
        }))
    }
}

/// Where `query` matches in `text` by the core's rules (smart case, not
/// overlapping, left to right), in UTF-16 code units: for highlighting a
/// text as the head renders it.
#[uniffi::export]
pub fn search_ranges(query: String, text: String) -> Vec<Utf16Range> {
    utf16_ranges(&text, &Query::new(&query).ranges(&text))
}

/// [`search::find`] over `timeline`, as the FFI returns it.
fn find(
    timeline: &[sc::TimelineItem],
    query: String,
    blob_text: &mut dyn FnMut(&sc::BlobView) -> Option<BlobText>,
) -> SearchResult {
    let mut searched = 0u64;
    let found = search::find(timeline, &Query::new(&query), &mut |blob| {
        let text = blob_text(blob);
        searched += u64::from(text.is_some());
        text
    });
    SearchResult {
        query,
        hits: found
            .hits
            .iter()
            .map(|hit| SearchHit {
                item: item(&timeline[hit.index]),
                blob: hit.blob.map(|at| SearchBlobLine {
                    blob_id: id(at.id.0),
                    line: count(at.line),
                }),
            })
            .collect(),
        searched_blobs: searched,
        not_downloaded: count(found.not_downloaded),
        cut: count(found.cut),
        head_bytes: HEAD_BYTES,
    }
}

fn item(item: &sc::TimelineItem) -> SearchItem {
    match item {
        sc::TimelineItem::Pending { command_id, .. } => SearchItem::Pending {
            command_id: id(command_id.0),
        },
        sc::TimelineItem::Logged { seq, entry, .. } => SearchItem::Logged {
            seq: seq.0,
            command_id: match entry {
                sc::Entry::UserMessage { command_id, .. } => Some(id(command_id.0)),
                _ => None,
            },
        },
    }
}

fn count(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// Byte `ranges` of `text` (on char boundaries, in order) as UTF-16 code
/// units, counted in one pass.
fn utf16_ranges(text: &str, ranges: &[Range<usize>]) -> Vec<Utf16Range> {
    let (mut at, mut units) = (0usize, 0u64);
    let mut to = |byte: usize| {
        units += count(text[at..byte].encode_utf16().count());
        at = byte;
        units
    };
    ranges
        .iter()
        .map(|range| Utf16Range {
            start: to(range.start),
            end: to(range.end),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use chrono::DateTime;

    use super::*;

    fn logged(seq: u64, entry: sc::Entry) -> sc::TimelineItem {
        sc::TimelineItem::Logged {
            seq: sc::Seq(seq),
            at: DateTime::UNIX_EPOCH,
            entry,
        }
    }

    fn blob(state: sc::BlobState) -> sc::BlobView {
        sc::BlobView {
            id: sc::BlobId::new(),
            state,
        }
    }

    fn tool(output: Option<sc::BlobView>) -> sc::Entry {
        sc::Entry::Tool(Box::new(sc::ToolView {
            call: sc::CallId::new(),
            name: "Bash".into(),
            kind: sc::ToolKind::Shell,
            title: "$ cargo test".into(),
            detail: Some("in crates/core".into()),
            body: None,
            input: None,
            approval: None,
            outcome: sc::ToolOutcome::Done {
                is_error: true,
                status: Some("exit 101".into()),
                preview: "test failed".into(),
                lines: 1,
                output,
                took: Duration::from_secs(1),
            },
        }))
    }

    struct Fixture {
        timeline: Vec<sc::TimelineItem>,
        message: sc::CommandId,
        pending: sc::CommandId,
        available: sc::BlobId,
        _dir: tempfile::TempDir,
    }

    /// Messages, tool items, a downloaded blob on disk and one not
    /// downloaded.
    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("output");
        std::fs::write(&path, "running 3 tests\nall Needle tests ok\n").unwrap();
        let available = blob(sc::BlobState::Available { path });
        let (message, pending) = (sc::CommandId::new(), sc::CommandId::new());
        let timeline = vec![
            logged(
                1,
                sc::Entry::UserMessage {
                    command_id: message,
                    text: "Run the tests 🚀 then fix the needle".into(),
                    from: sc::DeviceRef {
                        id: sc::DeviceId::new(),
                        name: None,
                        me: true,
                    },
                    outgoing: None,
                    delivery: None,
                },
            ),
            logged(
                2,
                sc::Entry::AgentMessage {
                    text: "Running cargo test now".into(),
                },
            ),
            logged(3, tool(Some(available.clone()))),
            logged(
                4,
                sc::Entry::ToolResult {
                    summary: "big output".into(),
                    output: Some(blob(sc::BlobState::NotDownloaded { size: 10 })),
                },
            ),
            sc::TimelineItem::Pending {
                command_id: pending,
                text: "and the docs".into(),
                outgoing: sc::Outgoing::Queued,
                discardable: true,
                delivery: None,
            },
        ];
        Fixture {
            timeline,
            message,
            pending,
            available: available.id,
            _dir: dir,
        }
    }

    fn search(fixture: &Fixture, query: &str) -> SearchResult {
        find(&fixture.timeline, query.into(), &mut BlobText::read)
    }

    fn seqs(result: &SearchResult) -> Vec<Option<u64>> {
        result
            .hits
            .iter()
            .map(|hit| match hit.item {
                SearchItem::Logged { seq, .. } => Some(seq),
                SearchItem::Pending { .. } => None,
            })
            .collect()
    }

    #[test]
    fn finds_what_the_core_finds() {
        let fixture = fixture();
        for query in [
            "test", "Test", "needle", "Needle", "exit 101", "docs", "🚀", "x",
        ] {
            let ours = search(&fixture, query);
            let core = search::find(&fixture.timeline, &Query::new(query), &mut BlobText::read);
            let expected: Vec<SearchItem> = core
                .hits
                .iter()
                .map(|hit| item(&fixture.timeline[hit.index]))
                .collect();
            let got: Vec<SearchItem> = ours.hits.iter().map(|hit| hit.item.clone()).collect();
            assert_eq!(got, expected, "{query}");
            assert_eq!(ours.not_downloaded, count(core.not_downloaded), "{query}");
            assert_eq!(ours.cut, count(core.cut), "{query}");
            assert_eq!(ours.query, query);
        }
    }

    #[test]
    fn hits_name_items_by_their_head_keys() {
        let fixture = fixture();
        let result = search(&fixture, "the");
        assert_eq!(
            result
                .hits
                .iter()
                .map(|hit| hit.item.clone())
                .collect::<Vec<_>>(),
            [
                SearchItem::Logged {
                    seq: 1,
                    command_id: Some(id(fixture.message.0)),
                },
                SearchItem::Pending {
                    command_id: id(fixture.pending.0),
                },
            ]
        );
    }

    #[test]
    fn searches_downloaded_blobs_and_counts_the_rest() {
        let fixture = fixture();
        let result = search(&fixture, "Needle");
        // Smart case: only the blob has "Needle"; the message says "needle".
        assert_eq!(seqs(&result), [Some(3)]);
        assert_eq!(
            result.hits[0].blob,
            Some(SearchBlobLine {
                blob_id: id(fixture.available.0),
                line: 1,
            })
        );
        assert_eq!((result.searched_blobs, result.not_downloaded), (1, 1));
        assert_eq!((result.cut, result.head_bytes), (0, HEAD_BYTES));
        // Lower case ignores case: the message too, its match not in a blob.
        let result = search(&fixture, "needle");
        assert_eq!(seqs(&result), [Some(1), Some(3)]);
        assert_eq!(result.hits[0].blob, None);
        // A file gone missing is skipped, neither searched nor counted.
        let mut gone = |blob: &sc::BlobView| {
            let mut blob = blob.clone();
            blob.state = sc::BlobState::Available {
                path: Path::new("/nonexistent/skywave/blob").to_owned(),
            };
            BlobText::read(&blob)
        };
        let result = find(&fixture.timeline, "Needle".into(), &mut gone);
        assert!(result.hits.is_empty());
        assert_eq!((result.searched_blobs, result.not_downloaded), (0, 1));
    }

    #[test]
    fn an_empty_query_finds_nothing_and_reads_nothing() {
        let fixture = fixture();
        let result = find(&fixture.timeline, String::new(), &mut |_| panic!("read"));
        assert!(result.hits.is_empty());
        assert_eq!((result.searched_blobs, result.not_downloaded), (0, 0));
    }

    fn slices(text: &str, ranges: &[Utf16Range]) -> Vec<String> {
        let units: Vec<u16> = text.encode_utf16().collect();
        ranges
            .iter()
            .map(|r| String::from_utf16(&units[r.start as usize..r.end as usize]).unwrap())
            .collect()
    }

    #[test]
    fn ranges_are_utf16_code_units() {
        let text = "🚀 Ünïcode and ünïcode 🚀";
        let ranges = search_ranges("ünï".into(), text.into());
        assert_eq!(slices(text, &ranges), ["Ünï", "ünï"]);
        // The rocket is two code units: "Ü" starts at 3.
        assert_eq!(ranges[0], Utf16Range { start: 3, end: 6 });
        let ranges = search_ranges("🚀".into(), text.into());
        assert_eq!(
            ranges,
            [
                Utf16Range { start: 0, end: 2 },
                Utf16Range { start: 23, end: 25 }
            ]
        );
    }

    #[test]
    fn ranges_use_smart_case() {
        let text = "Error, ERROR, error";
        assert_eq!(search_ranges("error".into(), text.into()).len(), 3);
        assert_eq!(
            search_ranges("ERROR".into(), text.into()),
            [Utf16Range { start: 7, end: 12 }]
        );
        assert!(search_ranges(String::new(), text.into()).is_empty());
    }

    #[test]
    fn a_session_that_is_not_open_finds_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let core =
            SkywaveCore::open(dir.path().display().to_string(), Some("test".into())).unwrap();
        let search = core
            .clone()
            .start_search(sc::SessionId::new().0.to_string())
            .unwrap();
        let result = search.find("anything".into()).unwrap();
        assert!(result.hits.is_empty());
        assert!(matches!(
            core.clone().start_search("not an id".into()),
            Err(SkywaveError::InvalidId { .. })
        ));
        core.shutdown().unwrap();
        assert_eq!(search.find("x".into()), Err(SkywaveError::ShutDown));
    }
}
