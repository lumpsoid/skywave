//! The timeline search as [`SearchUpdate`]: present's [`TimelineSearch`]
//! with its matches placed in the rows' own strings, in UTF-16.

use std::ops::Range;

use skywave_core::SessionView;
use skywave_present::policy::TimelineSearch;
use skywave_present::sections::ItemKey;

use crate::link::count;
use crate::signals::{LineHit, SearchHit, SearchUpdate, TimelineRow, Utf16Range};
use crate::timeline::key_string;

/// No search open.
pub fn closed() -> SearchUpdate {
    SearchUpdate {
        open: false,
        query: String::new(),
        count_label: String::new(),
        wrapped: false,
        partial_label: None,
        current_key: None,
        current_line: None,
        hits: Vec::new(),
    }
}

/// `search` over `view`, whose rows are `rows` as sent.
pub fn update(search: &TimelineSearch, view: &SessionView, rows: &[TimelineRow]) -> SearchUpdate {
    let ranges = |text: &str| -> Vec<Utf16Range> {
        search
            .ranges_utf16(text)
            .into_iter()
            .map(utf16_range)
            .collect()
    };
    let hits = search
        .hits()
        .iter()
        .filter_map(|hit| rows.get(hit.index))
        .map(|row| SearchHit {
            key: row.key.clone(),
            text: ranges(&row.text),
            title: row
                .tool
                .as_ref()
                .map(|t| ranges(&t.title))
                .unwrap_or_default(),
            detail: row
                .tool
                .as_ref()
                .and_then(|t| t.detail.as_deref())
                .map(ranges)
                .unwrap_or_default(),
            lines: row
                .sections
                .iter()
                .enumerate()
                .flat_map(|(s, section)| {
                    section
                        .lines
                        .iter()
                        .enumerate()
                        .map(move |(l, line)| (s, l, line))
                })
                .filter_map(|(section, line, text)| {
                    let found = ranges(&text.text);
                    (!found.is_empty()).then(|| LineHit {
                        section: count(section),
                        line: count(line),
                        ranges: found,
                    })
                })
                .collect(),
        })
        .collect();
    SearchUpdate {
        open: true,
        query: search.query.as_str().to_owned(),
        count_label: search.count(view),
        wrapped: search.wrapped,
        partial_label: search.partial(),
        current_key: search
            .current
            .and_then(|key| skywave_present::sections::find(view, key))
            .and_then(|i| view.timeline.get(i))
            .map(|item| key_string(ItemKey::of(item))),
        current_line: search.current_line(view).map(|(_, line)| count(line.line)),
        hits,
    }
}

pub(crate) fn utf16_range(range: Range<u64>) -> Utf16Range {
    let unit = |n: u64| u32::try_from(n).unwrap_or(u32::MAX);
    Utf16Range {
        start: unit(range.start),
        end: unit(range.end),
    }
}
