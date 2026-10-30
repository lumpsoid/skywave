//! Finding text in an open session's timeline: which items a search matches
//! and where in a text. Heads keep the query and which match is current;
//! the rules are here so every head matches alike.
//!
//! Only what the view already holds is searched: messages (pending ones
//! too), errors, tool names, titles, details, statuses and the bounded
//! previews. Blob content is not.

use std::ops::Range;

use crate::view::{Entry, TimelineItem, ToolOutcome};

/// What is searched for. Smart case: case is ignored unless the query has
/// an upper-case letter. An empty query matches nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    text: String,
    /// The query lower-cased, when case is ignored.
    folded: Option<Vec<char>>,
}

impl Query {
    pub fn new(text: &str) -> Query {
        let folded = (!text.chars().any(char::is_uppercase))
            .then(|| text.chars().flat_map(char::to_lowercase).collect());
        Query {
            text: text.to_owned(),
            folded,
        }
    }

    /// The query as typed.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Case is ignored: the query has no upper-case letter.
    pub fn ignores_case(&self) -> bool {
        self.folded.is_some()
    }

    /// Where the query occurs in `text`, as byte ranges on char boundaries,
    /// left to right and not overlapping.
    pub fn ranges(&self, text: &str) -> Vec<Range<usize>> {
        if self.text.is_empty() {
            return Vec::new();
        }
        match &self.folded {
            None => text
                .match_indices(self.text.as_str())
                .map(|(at, found)| at..at + found.len())
                .collect(),
            Some(needle) => folded_ranges(text, needle),
        }
    }

    pub fn matches(&self, text: &str) -> bool {
        !self.ranges(text).is_empty()
    }
}

/// Where `needle` (lower case) occurs in `text` lower-cased. A char may
/// lower-case to several, so each one keeps the byte range of the char it
/// came from and a match covers whole chars of `text`.
fn folded_ranges(text: &str, needle: &[char]) -> Vec<Range<usize>> {
    let folded: Vec<(char, Range<usize>)> = text
        .char_indices()
        .flat_map(|(at, c)| {
            let end = at + c.len_utf8();
            c.to_lowercase().map(move |lower| (lower, at..end))
        })
        .collect();
    let mut out: Vec<Range<usize>> = Vec::new();
    let mut i = 0;
    while i + needle.len() <= folded.len() {
        let window = &folded[i..i + needle.len()];
        if !window.iter().map(|(c, _)| *c).eq(needle.iter().copied()) {
            i += 1;
            continue;
        }
        let end = window[window.len() - 1].1.end;
        out.push(window[0].1.start..end);
        i += needle.len();
        // The rest of a char the match ended inside of.
        while folded.get(i).is_some_and(|(_, from)| from.start < end) {
            i += 1;
        }
    }
    out
}

/// The indices of the timeline items `query` matches, oldest first.
pub fn find(timeline: &[TimelineItem], query: &Query) -> Vec<usize> {
    if query.is_empty() {
        return Vec::new();
    }
    timeline
        .iter()
        .enumerate()
        .filter(|(_, item)| texts(item).into_iter().any(|text| query.matches(text)))
        .map(|(index, _)| index)
        .collect()
}

/// The texts of an item a search looks through.
fn texts(item: &TimelineItem) -> Vec<&str> {
    let entry = match item {
        TimelineItem::Pending { text, .. } => return vec![text],
        TimelineItem::Logged { entry, .. } => entry,
    };
    match entry {
        Entry::UserMessage { text, .. } | Entry::AgentMessage { text } => vec![text],
        Entry::AgentErrored { message } => vec![message],
        Entry::Tool(view) => {
            let mut out = vec![view.name.as_str(), view.title.as_str()];
            out.extend(view.detail.as_deref());
            out.extend(view.body.as_ref().map(|body| body.preview.as_str()));
            if let ToolOutcome::Done {
                status, preview, ..
            } = &view.outcome
            {
                out.extend(status.as_deref());
                out.push(preview);
            }
            out
        }
        Entry::ToolCall { name, summary, .. } => vec![name, summary],
        Entry::ToolResult { summary, .. } => vec![summary],
        Entry::Approval { tool, summary, .. } => vec![tool, summary],
        Entry::Renamed { name, .. } => name.as_deref().into_iter().collect(),
        Entry::Interrupted | Entry::Stopped => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::DateTime;

    use super::*;
    use crate::view::{
        BodyFormat, BodyView, CallId, CommandId, DeviceId, Outgoing, Seq, ToolKind, ToolView,
    };

    fn logged(entry: Entry) -> TimelineItem {
        TimelineItem::Logged {
            seq: Seq(1),
            at: DateTime::UNIX_EPOCH,
            entry,
        }
    }

    fn tool(outcome: ToolOutcome) -> ToolView {
        ToolView {
            call: CallId::new(),
            name: "Bash".into(),
            kind: ToolKind::Shell,
            title: "$ cargo test".into(),
            detail: Some("in crates/core".into()),
            body: Some(BodyView {
                format: BodyFormat::Text,
                preview: "cargo test --workspace".into(),
                lines: 1,
                full: None,
            }),
            input: None,
            approval: None,
            outcome,
        }
    }

    fn done(status: &str, preview: &str) -> ToolOutcome {
        ToolOutcome::Done {
            is_error: true,
            status: Some(status.into()),
            preview: preview.into(),
            lines: 1,
            output: None,
            took: Duration::from_secs(1),
        }
    }

    #[test]
    fn case_is_ignored_unless_the_query_has_an_upper_case_letter() {
        let lower = Query::new("error");
        assert!(lower.ignores_case());
        assert_eq!(lower.ranges("Error, ERROR, error"), [0..5, 7..12, 14..19]);
        let upper = Query::new("Error");
        assert!(!upper.ignores_case());
        let ranges = upper.ranges("Error, ERROR, error");
        assert_eq!((ranges.len(), &ranges[0]), (1, &(0..5)));
    }

    #[test]
    fn an_empty_query_matches_nothing() {
        let query = Query::new("");
        assert!(query.ranges("anything").is_empty());
        assert!(find(&[logged(Entry::AgentMessage { text: "x".into() })], &query).is_empty());
    }

    #[test]
    fn ranges_are_byte_ranges_on_char_boundaries() {
        let text = "naïve Ünïcode, ÜNÏCODE";
        let ranges = Query::new("ünï").ranges(text);
        assert_eq!(ranges.len(), 2);
        for range in &ranges {
            assert_eq!(text[range.clone()].to_lowercase(), "ünï");
        }
        // A char that lower-cases to two is matched whole.
        let text = "aİb";
        let ranges = Query::new("i\u{307}b").ranges(text);
        assert_eq!((ranges.len(), &ranges[0]), (1, &(1..text.len())));
        assert_eq!(Query::new("i").ranges(text).len(), 1);
        // Matches do not overlap.
        assert_eq!(Query::new("aa").ranges("aaaa"), [0..2, 2..4]);
    }

    #[test]
    fn every_field_is_searched() {
        let commands = CommandId::new();
        let timeline = vec![
            logged(Entry::UserMessage {
                command_id: commands,
                text: "find the user".into(),
                from: DeviceId::new(),
                own: false,
                outgoing: None,
                delivery: None,
            }),
            logged(Entry::AgentMessage {
                text: "an agent reply".into(),
            }),
            logged(Entry::AgentErrored {
                message: "rate limited".into(),
            }),
            logged(Entry::Tool(Box::new(tool(done("exit 101", "test failed"))))),
            logged(Entry::ToolCall {
                name: "Grep".into(),
                summary: "pattern TODO".into(),
                input: None,
            }),
            logged(Entry::ToolResult {
                summary: "3 files".into(),
                output: None,
            }),
            logged(Entry::Approval {
                id: crate::view::ApprovalId::new(),
                tool: "Write".into(),
                summary: "notes.md".into(),
                input: None,
                status: crate::view::ApprovalStatus::Pending,
            }),
            logged(Entry::Interrupted),
            TimelineItem::Pending {
                command_id: CommandId::new(),
                text: "queued question".into(),
                outgoing: Outgoing::Queued,
                discardable: true,
                delivery: None,
            },
        ];
        let hit = |q: &str| find(&timeline, &Query::new(q));
        assert_eq!(hit("user"), [0]);
        assert_eq!(hit("reply"), [1]);
        assert_eq!(hit("rate limited"), [2]);
        // Tool name, title, detail, body, status and output preview.
        for q in [
            "bash",
            "$ cargo",
            "crates/core",
            "--workspace",
            "exit 101",
            "failed",
        ] {
            assert_eq!(hit(q), [3], "{q}");
        }
        assert_eq!(hit("grep"), [4]);
        assert_eq!(hit("todo"), [4]);
        assert_eq!(hit("3 files"), [5]);
        assert_eq!(hit("notes.md"), [6]);
        assert_eq!(hit("interrupted"), Vec::<usize>::new());
        assert_eq!(hit("queued"), [8]);
        assert_eq!(hit("e"), [0, 1, 2, 3, 4, 5, 6, 8]);
    }

    #[test]
    fn a_running_call_is_found_by_its_title() {
        let timeline = vec![logged(Entry::Tool(Box::new(tool(ToolOutcome::Running {
            since: DateTime::UNIX_EPOCH,
        }))))];
        assert_eq!(find(&timeline, &Query::new("cargo test")), [0]);
        assert!(find(&timeline, &Query::new("exit")).is_empty());
    }
}
