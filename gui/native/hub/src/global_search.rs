//! The search page's query and hits: debounced, run off the hub task by
//! `Core::search`, the newest answer kept.

use std::time::{Duration, Instant};

use chrono::{DateTime, Local, Utc};
use skywave_core::search::{Query, SessionHit, SessionsFound};
use skywave_core::{Screen, SearchView, Seq, SessionId, SessionView};
use skywave_present::format::activity_label;
use skywave_present::search_hits;
use skywave_present::text::clean_line;
use skywave_present::utf16::byte_ranges_to_utf16;

use crate::search::utf16_range;
use crate::signals::{ScreenView, SearchRowView};

/// How long typing must pause before the query is searched.
pub const SEARCH_AFTER: Duration = Duration::from_millis(250);

/// An item a search hit opened: the session's head jumps to it, with the
/// query's matches marked, once the session's events are loaded.
#[derive(Debug)]
pub struct Reveal {
    pub session: SessionId,
    pub seq: Seq,
    pub query: String,
}

/// The search page's query, scope and hits, as the head holds them.
#[derive(Debug, Default)]
pub struct GlobalSearch {
    query: String,
    scope: Option<SessionId>,
    /// The search screen showed at the last view.
    shown: bool,
    /// When to search the query next.
    due: Option<Instant>,
    /// The number of the newest search asked.
    asked: u64,
    /// The newest answer for the current query and scope.
    found: Option<SessionsFound>,
    /// The item of the hit opened last, until its session shows it.
    reveal: Option<Reveal>,
}

impl GlobalSearch {
    /// The query is now `query`, typed at `now`. Empty: there is nothing to
    /// find, so the hits go and an answer still on its way is dropped.
    /// Otherwise the search is due once typing pauses, and the hits held stay
    /// until the answer comes.
    pub fn typed(&mut self, query: String, now: Instant) {
        self.query = query;
        if self.query.is_empty() {
            self.found = None;
            self.due = None;
            self.asked += 1;
        } else {
            self.due = Some(now + SEARCH_AFTER);
        }
    }

    /// The screen the head showed at `now`. Entering the search page, or
    /// changing its scope, searches the held query at once: the data may have
    /// changed, and a scope change clears the old hits. Any other screen ends
    /// the search page.
    pub fn follow(&mut self, screen: &Screen, now: Instant) {
        // A reveal lives while its session, or the search page, is the screen.
        let kept = match screen {
            Screen::Search(_) => true,
            Screen::Session(session) => self
                .reveal
                .as_ref()
                .is_some_and(|reveal| reveal.session == session.id),
            _ => false,
        };
        if !kept {
            self.reveal = None;
        }
        let Screen::Search(view) = screen else {
            self.shown = false;
            self.due = None;
            return;
        };
        let scope = view.scope.as_ref().map(|s| s.id);
        let changed = scope != self.scope;
        if changed {
            self.scope = scope;
            self.found = None;
        }
        if (!self.shown || changed) && !self.query.is_empty() {
            self.due = Some(now);
        }
        self.shown = true;
    }

    /// When the search is due, `None` when none is.
    pub fn next_wake(&self) -> Option<Instant> {
        self.due
    }

    /// The search to run at `now` if it is due: its number, query and scope.
    /// Taking it makes its answer the only one kept.
    pub fn take_due(&mut self, now: Instant) -> Option<(u64, String, Option<SessionId>)> {
        if self.due.is_none_or(|due| due > now) {
            return None;
        }
        self.due = None;
        self.asked += 1;
        Some((self.asked, self.query.clone(), self.scope))
    }

    /// The answer to search number `asked`. Kept only when it is the newest
    /// search asked: an older one's answer arriving late is dropped.
    pub fn answered(&mut self, asked: u64, found: SessionsFound) {
        if asked == self.asked {
            self.found = Some(found);
        }
    }

    /// The hit `key` names, from the answer held.
    fn hit(&self, key: &str) -> Option<&SessionHit> {
        self.found
            .as_ref()?
            .hits
            .iter()
            .find(|hit| row_key(hit) == key)
    }

    /// Opens the hit `key` names: its session is returned. An item hit keeps
    /// a reveal of the item with the query; a session hit clears any reveal.
    /// `None` when no hit has `key`, the results having changed since the row
    /// was shown.
    pub fn open(&mut self, key: &str) -> Option<SessionId> {
        let hit = self.hit(key)?;
        let session = hit.session;
        self.reveal = hit.item.as_ref().map(|item| Reveal {
            session,
            seq: item.seq,
            query: self.query.clone(),
        });
        Some(session)
    }

    /// The reveal, taken once `view`, the open session, has its events loaded
    /// (its timeline is not empty). `None` while the reveal is for another
    /// session, or there is none.
    pub fn take_reveal(&mut self, view: &SessionView) -> Option<Reveal> {
        let reveal = self.reveal.as_ref()?;
        if reveal.session != view.id || view.timeline.is_empty() {
            return None;
        }
        self.reveal.take()
    }
}

/// The search page as the screen shows it: the held query and hits, scoped
/// as `view` says, with the times labelled at `wall_ms`.
pub fn screen(view: &SearchView, search: &GlobalSearch, wall_ms: i64) -> ScreenView {
    let now = DateTime::<Utc>::from_timestamp_millis(wall_ms).unwrap_or_default();
    let query = Query::new(&search.query);
    let rows = search
        .found
        .iter()
        .flat_map(|found| &found.hits)
        .map(|hit| row(hit, &query, now))
        .collect();
    ScreenView::Search {
        scope_id: view.scope.as_ref().map(|s| s.id.0.to_string()),
        scope_title: view.scope.as_ref().map(|s| clean_line(&s.title)),
        query: search.query.clone(),
        count_label: (!search.query.is_empty()).then(|| match &search.found {
            Some(found) => search_hits::count_label(found),
            None => "searching…".to_owned(),
        }),
        partial_label: search.found.as_ref().and_then(search_hits::partial),
        rows,
    }
}

/// `hit` as a row: its snippet, and where the query matches in the snippet.
fn row(hit: &SessionHit, query: &Query, now: DateTime<Utc>) -> SearchRowView {
    let (snippet, ranges) = search_hits::snippet(hit, query);
    let matched = byte_ranges_to_utf16(&snippet, &ranges)
        .into_iter()
        .map(utf16_range)
        .collect();
    let at = hit.item.as_ref().map_or(hit.last_activity, |item| item.at);
    SearchRowView {
        key: row_key(hit),
        session_title: clean_line(&hit.title),
        kind_label: search_hits::kind_label(hit),
        time_label: activity_label(at, now, &Local),
        snippet,
        matched,
    }
}

/// The key of `hit`'s row: its session, and its item when it is one.
fn row_key(hit: &SessionHit) -> String {
    match &hit.item {
        Some(item) => format!("{}:{}", hit.session.0, item.seq.0),
        None => hit.session.0.to_string(),
    }
}
