//! How the search page shows a hit: its kind, its snippet, the count.

use std::ops::Range;

use skywave_core::search::{HitKind, Query, SessionHit, SessionsFound};

use crate::policy::partial_label;
use crate::text::clean;

/// Most characters of a hit's line a snippet shows.
pub const SNIPPET_CHARS: usize = 160;

/// What kind of hit `hit` is, as the page labels it.
pub fn kind_label(hit: &SessionHit) -> String {
    let Some(item) = &hit.item else {
        return "Session".to_owned();
    };
    match &item.kind {
        HitKind::Tool { name } if item.output => format!("{name} output"),
        HitKind::Tool { name } => name.clone(),
        _ if item.output => "Output".to_owned(),
        HitKind::UserMessage => "Prompt".to_owned(),
        HitKind::AgentMessage => "Agent".to_owned(),
        HitKind::Other => "Event".to_owned(),
    }
}

/// The text a hit shows for `query`, made safe to show, and where the query
/// matches in it. A long text is cut to [`SNIPPET_CHARS`] characters around
/// its first match, with `…` where it goes on either side.
pub fn snippet(hit: &SessionHit, query: &Query) -> (String, Vec<Range<usize>>) {
    let raw = match &hit.item {
        Some(item) => item.line.as_str(),
        None if query.matches(&hit.title) => hit.title.as_str(),
        None => hit.workdir.as_deref().unwrap_or_default(),
    };
    let text = clean(raw);
    let chars = text.chars().count();
    if chars <= SNIPPET_CHARS {
        let ranges = query.ranges(&text);
        return (text, ranges);
    }
    let first = query
        .ranges(&text)
        .first()
        .map_or(0, |range| text[..range.start].chars().count());
    let start = first
        .saturating_sub(SNIPPET_CHARS / 4)
        .min(chars - SNIPPET_CHARS);
    let body: String = text.chars().skip(start).take(SNIPPET_CHARS).collect();
    let before = if start > 0 { "…" } else { "" };
    let after = if start + SNIPPET_CHARS < chars {
        "…"
    } else {
        ""
    };
    let window = format!("{before}{body}{after}");
    let ranges = query.ranges(&window);
    (window, ranges)
}

/// How many hits the search found: `no matches`, `1 match`, `3 matches`, or
/// `first 200 matches` when the search stopped at its cap.
pub fn count_label(found: &SessionsFound) -> String {
    let n = found.hits.len();
    if found.more {
        return format!("first {n} matches");
    }
    match n {
        0 => "no matches".to_owned(),
        1 => "1 match".to_owned(),
        n => format!("{n} matches"),
    }
}

/// What the search could not look through, if anything: see
/// [`partial_label`](crate::policy::partial_label).
pub fn partial(found: &SessionsFound) -> Option<String> {
    partial_label(found.not_downloaded, found.cut)
}

#[cfg(test)]
mod tests {
    use chrono::DateTime;
    use skywave_core::search::{ItemHit, SEARCH_HITS_MAX};
    use skywave_core::{Seq, SessionId};

    use super::*;

    fn session(title: &str, workdir: Option<&str>, item: Option<ItemHit>) -> SessionHit {
        SessionHit {
            session: SessionId::new(),
            title: title.to_owned(),
            workdir: workdir.map(str::to_owned),
            last_activity: DateTime::UNIX_EPOCH,
            item,
        }
    }

    fn item(kind: HitKind, line: &str, output: bool) -> ItemHit {
        ItemHit {
            seq: Seq(1),
            at: DateTime::UNIX_EPOCH,
            kind,
            line: line.to_owned(),
            output,
        }
    }

    fn line_hit(kind: HitKind, line: &str, output: bool) -> SessionHit {
        session("title", None, Some(item(kind, line, output)))
    }

    fn sessions_found(n: usize, more: bool) -> SessionsFound {
        SessionsFound {
            hits: (0..n).map(|_| session("t", None, None)).collect(),
            more,
            not_downloaded: 0,
            cut: 0,
        }
    }

    #[test]
    fn each_kind_has_its_label() {
        let tool = |name: &str, output| line_hit(HitKind::Tool { name: name.into() }, "x", output);
        assert_eq!(kind_label(&session("t", None, None)), "Session");
        assert_eq!(
            kind_label(&line_hit(HitKind::UserMessage, "x", false)),
            "Prompt"
        );
        assert_eq!(
            kind_label(&line_hit(HitKind::AgentMessage, "x", false)),
            "Agent"
        );
        assert_eq!(kind_label(&tool("Bash", false)), "Bash");
        assert_eq!(kind_label(&tool("Bash", true)), "Bash output");
        assert_eq!(kind_label(&line_hit(HitKind::Other, "x", false)), "Event");
        assert_eq!(kind_label(&line_hit(HitKind::Other, "x", true)), "Output");
        assert_eq!(
            kind_label(&line_hit(HitKind::AgentMessage, "x", true)),
            "Output"
        );
    }

    #[test]
    fn a_short_line_comes_back_whole_with_its_ranges() {
        let h = line_hit(HitKind::AgentMessage, "the fox", false);
        let (text, ranges) = snippet(&h, &Query::new("fox"));
        assert_eq!(text, "the fox");
        assert_eq!(ranges, std::iter::once(4..7).collect::<Vec<_>>());
    }

    #[test]
    fn a_long_line_is_cut_around_its_first_match() {
        let line = format!("{}fox{}", "a".repeat(400), "b".repeat(400));
        let h = line_hit(HitKind::AgentMessage, &line, false);
        let (text, ranges) = snippet(&h, &Query::new("fox"));
        assert!(text.starts_with('…'), "{text}");
        assert!(text.ends_with('…'), "{text}");
        assert_eq!(text.chars().count(), SNIPPET_CHARS + 2);
        assert_eq!(ranges.len(), 1);
        assert_eq!(&text[ranges[0].clone()], "fox");
    }

    #[test]
    fn a_session_hit_snippets_its_title_when_it_matches_else_its_workdir() {
        let s = session("Fix the parser", Some("/src/app"), None);
        assert_eq!(
            snippet(&s, &Query::new("parser")),
            (
                "Fix the parser".to_owned(),
                std::iter::once(8..14).collect::<Vec<_>>(),
            )
        );
        assert_eq!(
            snippet(&s, &Query::new("app")),
            (
                "/src/app".to_owned(),
                std::iter::once(5..8).collect::<Vec<_>>(),
            )
        );
        let bare = session("Fix", None, None);
        assert_eq!(
            snippet(&bare, &Query::new("zzz")),
            (String::new(), Vec::new())
        );
    }

    #[test]
    fn the_count_says_how_many_matches() {
        assert_eq!(count_label(&sessions_found(0, false)), "no matches");
        assert_eq!(count_label(&sessions_found(1, false)), "1 match");
        assert_eq!(count_label(&sessions_found(3, false)), "3 matches");
        assert_eq!(count_label(&sessions_found(3, true)), "first 3 matches");
        assert_eq!(
            count_label(&sessions_found(SEARCH_HITS_MAX, true)),
            format!("first {SEARCH_HITS_MAX} matches")
        );
    }

    #[test]
    fn the_partial_note_is_the_partial_label() {
        let mut found = sessions_found(0, false);
        found.not_downloaded = 2;
        assert_eq!(partial(&found), Some("2 outputs not downloaded".to_owned()));
        found.not_downloaded = 0;
        assert_eq!(partial(&found), None);
    }
}
