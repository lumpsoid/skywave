//! A search through the open session's timeline. What matches is the
//! core's ([`skywave_core::search`]); the query, which match is current,
//! stepping round the ends, and the downloaded blobs read so far are kept
//! here. Where a match is on screen is the head's.

use std::collections::HashMap;
use std::ops::Range;

use skywave_core::search::{self, BlobLine, BlobText, Found, HEAD_BYTES, Hit};
use skywave_core::{BlobId, BlobView, Query, Seq, SessionView, TimelineItem};

use crate::format::bytes;
use crate::sections::{ItemKey, find, match_hidden};
use crate::utf16::byte_ranges_to_utf16;

/// Where a [`TimelineSearch::jump`] landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Jump {
    /// The match's timeline index.
    pub index: usize,
    /// The match's item.
    pub key: ItemKey,
    /// The jump went round an end of the timeline.
    pub wrapped: bool,
    /// The item matches in the content of one of its downloaded blobs.
    pub in_blob: bool,
    /// The item matches only in what it shows expanded: the head expands
    /// this logged item to show the match.
    pub expand: Option<Seq>,
}

/// A search through a session's timeline: the query, its matches, and the
/// current one. Reading a downloaded blob is the head's: the methods that
/// search take `read`, called once per blob (e.g. [`BlobText::read`]).
#[derive(Debug, Clone)]
pub struct TimelineSearch {
    /// The query; change it through [`TimelineSearch::edit`].
    pub query: Query,
    /// The match jumped to last, by key so it stays put as items arrive.
    pub current: Option<ItemKey>,
    /// The last jump went round an end of the timeline; a head says so
    /// until the next input and then clears it.
    pub wrapped: bool,
    /// The matching items, oldest first, and how much blob content went
    /// unsearched.
    found: Found,
    /// The heads of the downloaded blobs read so far (at most
    /// [`HEAD_BYTES`] each), so a search reads each file once; `None` for
    /// one that could not be read.
    blob_cache: HashMap<BlobId, Option<BlobText>>,
}

impl Default for TimelineSearch {
    fn default() -> Self {
        TimelineSearch {
            query: Query::new(""),
            current: None,
            wrapped: false,
            found: Found::default(),
            blob_cache: HashMap::new(),
        }
    }
}

impl TimelineSearch {
    /// An empty search.
    pub fn new() -> TimelineSearch {
        TimelineSearch::default()
    }

    /// The matching items, oldest first.
    pub fn hits(&self) -> &[Hit] {
        &self.found.hits
    }

    /// The match on the item at timeline index `index`, if it matches.
    pub fn hit(&self, index: usize) -> Option<&Hit> {
        let at = self
            .found
            .hits
            .binary_search_by_key(&index, |hit| hit.index)
            .ok()?;
        self.found.hits.get(at)
    }

    /// What the search read of a downloaded blob, if it did.
    pub fn blob_text(&self, id: BlobId) -> Option<&BlobText> {
        self.blob_cache.get(&id)?.as_ref()
    }

    /// Finds the matches in `view`, reading the downloaded blobs not read
    /// yet with `read`; a blob that became downloaded since is read now.
    fn find(&mut self, view: &SessionView, mut read: impl FnMut(&BlobView) -> Option<BlobText>) {
        let cache = &mut self.blob_cache;
        self.found = search::find(&view.timeline, &self.query, &mut |blob| {
            cache.entry(blob.id).or_insert_with(|| read(blob)).clone()
        });
    }

    /// Edits the query and finds its matches again; no match is current.
    pub fn edit(
        &mut self,
        view: &SessionView,
        edit: impl FnOnce(&mut String),
        read: impl FnMut(&BlobView) -> Option<BlobText>,
    ) {
        let mut text = self.query.as_str().to_owned();
        edit(&mut text);
        self.query = Query::new(&text);
        self.find(view, read);
        self.current = None;
    }

    /// Finds the matches in a new snapshot. The current match stays on its
    /// item; if that is gone, the match nearest `old`, its index in the
    /// snapshot before, is current.
    pub fn refresh(
        &mut self,
        view: &SessionView,
        old: Option<usize>,
        read: impl FnMut(&BlobView) -> Option<BlobText>,
    ) {
        self.find(view, read);
        let Some(key) = self.current else {
            return;
        };
        let index = find(view, key).or_else(|| {
            let old = old?;
            self.hits()
                .iter()
                .map(|hit| hit.index)
                .min_by_key(|&i| i.abs_diff(old))
        });
        self.current = index.and_then(|i| view.timeline.get(i)).map(ItemKey::of);
    }

    /// Jumps to the next older (`Some(true)`) or newer match from the
    /// current one, round the ends of the timeline; with no current match,
    /// or `None`, to the nearest one at or above (newer: below) timeline
    /// index `from`, where the user is. `None` when nothing matches.
    pub fn jump(&mut self, view: &SessionView, older: Option<bool>, from: usize) -> Option<Jump> {
        let hits: Vec<usize> = self.hits().iter().map(|hit| hit.index).collect();
        let (&oldest, &newest) = (hits.first()?, hits.last()?);
        let current = self
            .current
            .and_then(|key| find(view, key))
            .filter(|_| older.is_some());
        let newer = older == Some(false);
        let target = match current {
            Some(at) if newer => hits.iter().find(|&&i| i > at),
            Some(at) => hits.iter().rev().find(|&&i| i < at),
            None if newer => hits.iter().find(|&&i| i >= from),
            None => hits.iter().rev().find(|&&i| i <= from),
        };
        let wrapped = target.is_none();
        let index = target
            .copied()
            .unwrap_or(if newer { oldest } else { newest });
        let in_blob = self.hit(index).and_then(|hit| hit.blob).is_some();
        let item = view.timeline.get(index)?;
        let expand = match item {
            TimelineItem::Logged { seq, entry, .. } if match_hidden(entry, &self.query) => {
                Some(*seq)
            }
            _ => None,
        };
        let key = ItemKey::of(item);
        self.current = Some(key);
        self.wrapped = wrapped;
        Some(Jump {
            index,
            key,
            wrapped,
            in_blob,
            expand,
        })
    }

    /// The current match's place among the matches, 1 the oldest.
    pub fn position(&self, view: &SessionView) -> Option<usize> {
        let index = find(view, self.current?)?;
        Some(self.hits().iter().position(|hit| hit.index == index)? + 1)
    }

    /// The current match's line in its blob, with its timeline index.
    pub fn current_line(&self, view: &SessionView) -> Option<(usize, BlobLine)> {
        let index = find(view, self.current?)?;
        Some((index, self.hit(index)?.blob?))
    }

    /// Where the query matches in `text`, as UTF-16 code units, for heads
    /// whose strings index that way.
    pub fn ranges_utf16(&self, text: &str) -> Vec<Range<u64>> {
        byte_ranges_to_utf16(text, &self.query.ranges(text))
    }

    /// What the search could not look through, if anything: `3 outputs not
    /// downloaded`, `1 output searched only in its first 1.0 MiB`.
    pub fn partial(&self) -> Option<String> {
        let outputs = |n: usize| if n == 1 { "output" } else { "outputs" };
        let mut out = Vec::new();
        match self.found.not_downloaded {
            0 => {}
            n => out.push(format!("{n} {} not downloaded", outputs(n))),
        }
        match self.found.cut {
            0 => {}
            n => out.push(format!(
                "{n} {} searched only in {} first {}",
                outputs(n),
                if n == 1 { "its" } else { "their" },
                bytes(HEAD_BYTES)
            )),
        }
        (!out.is_empty()).then(|| out.join(" · "))
    }

    /// How the search stands: `3 of 17`, `17 matches` before a jump, `no
    /// matches`.
    pub fn count(&self, view: &SessionView) -> String {
        match (self.hits().len(), self.position(view)) {
            (0, _) => "no matches".to_owned(),
            (of, Some(at)) => format!("{at} of {of}"),
            (1, None) => "1 match".to_owned(),
            (of, None) => format!("{of} matches"),
        }
    }

    /// The query and how it stands: `/foo · 3 of 17`.
    pub fn status(&self, view: &SessionView) -> String {
        format!("/{} · {}", self.query.as_str(), self.count(view))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use skywave_core::{BlobState, Entry};

    use super::*;
    use crate::policy::tests::{logged, message, session};

    fn agent(seq: u64, text: &str) -> TimelineItem {
        logged(
            seq,
            Entry::AgentMessage {
                text: text.to_owned(),
            },
        )
    }

    fn no_blobs(_: &BlobView) -> Option<BlobText> {
        None
    }

    fn typed(search: &mut TimelineSearch, view: &SessionView, query: &str) {
        search.edit(view, |q| q.push_str(query), no_blobs);
    }

    #[test]
    fn n_and_shift_n_wrap_round_the_ends() {
        let view = session(vec![
            agent(1, "needle one"),
            agent(2, "hay"),
            agent(3, "needle two"),
        ]);
        let mut search = TimelineSearch::new();
        typed(&mut search, &view, "needle");
        assert_eq!(search.count(&view), "2 matches");

        // From the bottom: the nearest at or above.
        let jump = search.jump(&view, None, 2).unwrap();
        assert_eq!((jump.index, jump.wrapped), (2, false));
        assert_eq!(search.status(&view), "/needle · 2 of 2");
        let jump = search.jump(&view, Some(true), 2).unwrap();
        assert_eq!((jump.index, jump.wrapped), (0, false));
        // Older than the oldest: round to the newest.
        let jump = search.jump(&view, Some(true), 2).unwrap();
        assert_eq!((jump.index, jump.wrapped), (2, true));
        assert!(search.wrapped);
        // Newer than the newest: round to the oldest.
        let jump = search.jump(&view, Some(false), 2).unwrap();
        assert_eq!((jump.index, jump.wrapped), (0, true));
        assert_eq!(search.current, Some(ItemKey::Logged(Seq(1))));

        typed(&mut search, &view, "x");
        assert_eq!(search.count(&view), "no matches");
        assert_eq!(search.jump(&view, Some(true), 2), None);
    }

    #[test]
    fn a_match_only_in_hidden_content_expands_its_item() {
        let blob = BlobView {
            id: BlobId::new(),
            state: BlobState::Available {
                path: "/nowhere".into(),
            },
        };
        let view = session(vec![
            message(1, "run it"),
            logged(
                2,
                Entry::ToolCall {
                    name: "bash".into(),
                    summary: "ls".into(),
                    input: Some(skywave_core::FullView::Blob(blob.clone())),
                },
            ),
        ]);
        let mut reads = 0;
        let mut search = TimelineSearch::new();
        let read = |b: &BlobView| {
            reads += 1;
            assert_eq!(b.id, blob.id);
            Some(BlobText {
                text: Arc::from("first\nthe needle\n"),
                cut: true,
            })
        };
        search.edit(&view, |q| q.push_str("needle"), read);
        let jump = search.jump(&view, None, 1).unwrap();
        assert_eq!(jump.expand, Some(Seq(2)));
        assert!(jump.in_blob);
        assert_eq!(search.current_line(&view).map(|(_, l)| l.line), Some(1));
        assert_eq!(
            search.partial().as_deref(),
            Some("1 output searched only in its first 1.0 MiB")
        );

        // The blob is read once; a match the title shows expands nothing.
        search.edit(
            &view,
            |q| *q = "ls".into(),
            |_: &BlobView| -> Option<BlobText> { panic!("read again") },
        );
        let jump = search.jump(&view, None, 1).unwrap();
        assert_eq!((jump.index, jump.expand, jump.in_blob), (1, None, false));
        assert!(search.blob_text(blob.id).is_some());
        assert_eq!(reads, 1);
    }

    #[test]
    fn the_current_match_follows_its_item() {
        let mut view = session(vec![agent(1, "needle"), agent(3, "needle")]);
        let mut search = TimelineSearch::new();
        typed(&mut search, &view, "needle");
        search.jump(&view, None, 1);
        assert_eq!(search.current, Some(ItemKey::Logged(Seq(3))));
        // Gone: the match nearest its old index.
        view.timeline.remove(1);
        search.refresh(&view, Some(1), no_blobs);
        assert_eq!(search.current, Some(ItemKey::Logged(Seq(1))));
    }

    #[test]
    fn ranges_count_utf16_code_units() {
        let mut search = TimelineSearch::new();
        typed(&mut search, &session(Vec::new()), "x");
        assert_eq!(search.ranges_utf16("héllo 👋 x"), vec![9..10]);
        let mut search = TimelineSearch::new();
        typed(&mut search, &session(Vec::new()), "👋");
        assert_eq!(search.ranges_utf16("héllo 👋 x"), vec![6..8]);
    }
}
