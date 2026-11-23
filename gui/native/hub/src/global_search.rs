//! The search page's query and hits: debounced, run off the hub task by
//! `Core::search`, the newest answer kept.

use std::time::{Duration, Instant};

use chrono::{DateTime, Local, Utc};
use skywave_core::search::{Query, SessionHit, SessionsFound};
use skywave_core::{Screen, SearchView, SessionId};
use skywave_present::format::activity_label;
use skywave_present::search_hits;
use skywave_present::text::clean_line;
use skywave_present::utf16::byte_ranges_to_utf16;

use crate::search::utf16_range;
use crate::signals::{ScreenView, SearchRowView};

/// How long typing must pause before the query is searched.
pub const SEARCH_AFTER: Duration = Duration::from_millis(250);

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
