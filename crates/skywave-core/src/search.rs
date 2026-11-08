//! Finding text in an open session's timeline: which items a search matches
//! and where in a text. Heads keep the query and which match is current;
//! the rules are here so every head matches alike.
//!
//! What the view holds is searched: messages (pending and imported ones too), errors,
//! tool names, titles, details, statuses, the bounded previews and the
//! whole outputs that came inline in their events. So is
//! the content of the blobs behind the timeline that are already
//! downloaded, as far as the head reads them ([`BlobText`]); a search
//! never downloads one, and says how many it could not search. A timeline
//! narrowed to one context is searched as shown.

use std::ops::Range;
use std::sync::Arc;

use crate::blobs::read_head;
use crate::view::{
    AgentSetting, BlobId, BlobState, BlobView, Effort, Entry, FullView, ImportedEntry,
    TimelineItem, ToolOutcome, ToolView,
};

/// Most of a downloaded blob a search reads: the head of a longer one.
pub const HEAD_BYTES: u64 = 1024 * 1024;

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

    /// The first line of `text` (0 the first, lines split at `\n`) the
    /// query matches in. Line by line, so a long text is not folded whole.
    pub fn first_line(&self, text: &str) -> Option<usize> {
        if self.text.is_empty() {
            return None;
        }
        if self.folded.is_none() {
            let at = text.find(self.text.as_str())?;
            return Some(text[..at].bytes().filter(|&b| b == b'\n').count());
        }
        text.split('\n').position(|line| self.matches(line))
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

/// A downloaded blob's text as a search reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobText {
    /// The head of the content; how much of it a head reads is its own.
    pub text: Arc<str>,
    /// The content goes on past `text`: only its head was searched.
    pub cut: bool,
}

impl BlobText {
    /// A downloaded blob's head as a search reads it, at most
    /// [`HEAD_BYTES`]; `None` if it is not downloaded or its file cannot be
    /// read.
    pub fn read(blob: &BlobView) -> Option<BlobText> {
        let BlobState::Available { path } = &blob.state else {
            return None;
        };
        let (text, size) = read_head(path, HEAD_BYTES).ok()?;
        Some(BlobText {
            text: text.into(),
            cut: size > HEAD_BYTES,
        })
    }
}

/// A line of a blob's content where a query matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobLine {
    pub id: BlobId,
    /// 0 the first line, lines split at `\n`.
    pub line: usize,
}

/// A timeline item a query matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hit {
    /// The item's timeline index.
    pub index: usize,
    /// The first match in the content of the item's downloaded blobs, in
    /// [`blobs`] order; `None` if it matches only in what the view holds.
    pub blob: Option<BlobLine>,
}

/// What a search found, and how much content it could not search.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Found {
    /// The matching items, oldest first.
    pub hits: Vec<Hit>,
    /// Blobs behind the timeline that are not downloaded (any state short
    /// of [`BlobState::Available`]), so their content was not searched.
    pub not_downloaded: usize,
    /// Downloaded blobs searched only in their head ([`BlobText::cut`]).
    pub cut: usize,
}

/// The timeline items `query` matches, oldest first. `blob_text` gives a
/// downloaded blob's text: it is called only for blobs that are
/// [`BlobState::Available`], and a blob it gives nothing for (its file
/// cannot be read) is skipped.
pub fn find(
    timeline: &[TimelineItem],
    query: &Query,
    blob_text: &mut dyn FnMut(&BlobView) -> Option<BlobText>,
) -> Found {
    let mut found = Found::default();
    if query.is_empty() {
        return found;
    }
    for (index, item) in timeline.iter().enumerate() {
        let mut blob = None;
        if let TimelineItem::Logged { entry, .. } = item {
            for view in blobs(entry) {
                if !matches!(view.state, BlobState::Available { .. }) {
                    found.not_downloaded += 1;
                    continue;
                }
                let Some(text) = blob_text(view) else {
                    continue;
                };
                found.cut += usize::from(text.cut);
                if blob.is_none() {
                    blob = query
                        .first_line(&text.text)
                        .map(|line| BlobLine { id: view.id, line });
                }
            }
        }
        if blob.is_some() || texts(item).into_iter().any(|text| query.matches(text)) {
            found.hits.push(Hit { index, blob });
        }
    }
    found
}

/// The blobs whose content an entry shows, in the order it shows them: a
/// call's body and output, its whole input only when it has neither nor
/// an inline output (the input is then all there is to see), and its
/// approval's own input when that differs from the call's; a long
/// message's whole text.
pub fn blobs(entry: &Entry) -> Vec<&BlobView> {
    match entry {
        Entry::AgentMessage { full, .. }
        | Entry::Imported {
            entry:
                ImportedEntry::UserMessage { full, .. } | ImportedEntry::AgentMessage { full, .. },
            ..
        } => full.as_ref().and_then(FullView::blob).into_iter().collect(),
        Entry::Tool(view)
        | Entry::Imported {
            entry: ImportedEntry::Tool(view),
            ..
        } => tool_blobs(view),
        Entry::ToolCall { input, .. } | Entry::Approval { input, .. } => input
            .as_ref()
            .and_then(FullView::blob)
            .into_iter()
            .collect(),
        Entry::ToolResult { output, .. }
        | Entry::Imported {
            entry: ImportedEntry::ToolResult { output, .. },
            ..
        } => output
            .as_ref()
            .and_then(FullView::blob)
            .into_iter()
            .collect(),
        _ => Vec::new(),
    }
}

fn tool_blobs(view: &ToolView) -> Vec<&BlobView> {
    let mut out: Vec<&BlobView> = Vec::new();
    out.extend(
        view.body
            .as_ref()
            .and_then(|body| body.full.as_ref()?.blob()),
    );
    let mut inline = false;
    if let ToolOutcome::Done {
        output: Some(output),
        ..
    } = &view.outcome
    {
        out.extend(output.blob());
        inline = output.inline().is_some();
    }
    // As `skywave_present::sections` offers it: not beside an inline output.
    if out.is_empty() && !inline {
        out.extend(view.input.as_ref().and_then(FullView::blob));
    }
    // What the approval asked, when it differs from the call's input.
    out.extend(view.approval_input().and_then(FullView::blob));
    out
}

fn tool_texts(view: &ToolView) -> Vec<&str> {
    let mut out = vec![view.name.as_str(), view.title.as_str()];
    out.extend(view.detail.as_deref());
    out.extend(view.body.as_ref().map(|body| body.preview.as_str()));
    if let ToolOutcome::Done {
        status,
        preview,
        output,
        ..
    } = &view.outcome
    {
        out.extend(status.as_deref());
        out.push(preview);
        out.extend(output.as_ref().and_then(FullView::inline));
    }
    out
}

/// A lone result's summary, and its whole output when that is inline.
fn result_texts<'a>(summary: &'a str, output: &'a Option<FullView>) -> Vec<&'a str> {
    let mut out = vec![summary];
    out.extend(output.as_ref().and_then(FullView::inline));
    out
}

/// The texts of an item a search looks through.
fn texts(item: &TimelineItem) -> Vec<&str> {
    let entry = match item {
        TimelineItem::Pending { text, .. } => return vec![text],
        TimelineItem::Logged { entry, .. } => entry,
    };
    match entry {
        Entry::UserMessage { text, .. } | Entry::AgentMessage { text, .. } => vec![text],
        Entry::AgentErrored { message } => vec![message],
        Entry::AgentVersionUnconfirmed { version, .. } => vec![version],
        Entry::Tool(view) => tool_texts(view),
        Entry::Imported { entry, .. } => match entry {
            ImportedEntry::UserMessage { text, .. } | ImportedEntry::AgentMessage { text, .. } => {
                vec![text]
            }
            ImportedEntry::Tool(view) => tool_texts(view),
            ImportedEntry::ToolResult { summary, output } => result_texts(summary, output),
        },
        Entry::ToolCall { name, summary, .. } => vec![name, summary],
        Entry::ToolResult { summary, output } => result_texts(summary, output),
        Entry::Approval { tool, summary, .. } => vec![tool, summary],
        Entry::Renamed { name, .. } => name.as_deref().into_iter().collect(),
        Entry::ContextStarted { name, .. }
        | Entry::ContextSwitched { name, .. }
        | Entry::ContextMoved { name, .. }
        | Entry::MovedFrom { name, .. } => vec![name],
        Entry::Adopted { conversation } => vec![conversation],
        Entry::AgentSetting {
            setting, refused, ..
        } => {
            let mut out: Vec<&str> = match setting {
                AgentSetting::Model(model) => vec![model],
                // The wire names, as the agent spells them.
                AgentSetting::Effort(effort) => match effort {
                    None => vec!["none"],
                    Some(Effort::Low) => vec!["low"],
                    Some(Effort::Medium) => vec!["medium"],
                    Some(Effort::High) => vec!["high"],
                    Some(Effort::XHigh) => vec!["xhigh"],
                    Some(Effort::Max) => vec!["max"],
                },
                AgentSetting::Mode(_) => Vec::new(),
            };
            out.extend(refused.as_deref());
            out
        }
        Entry::Interrupted
        | Entry::Stopped
        | Entry::Archived { .. }
        | Entry::TurnUsage(_)
        | Entry::ImportFinished { .. } => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::DateTime;

    use super::*;
    use crate::view::{
        BodyFormat, BodyView, CallId, CommandId, ContextId, DeviceId, DeviceRef, Outgoing, Seq,
        ToolApproval, ToolKind, ToolView,
    };

    /// The matching items' indices, no blob downloaded.
    fn indices(timeline: &[TimelineItem], query: &Query) -> Vec<usize> {
        find(timeline, query, &mut |_| panic!("nothing is downloaded"))
            .hits
            .iter()
            .map(|hit| hit.index)
            .collect()
    }

    fn logged(entry: Entry) -> TimelineItem {
        TimelineItem::Logged {
            parent: None,
            seq: Seq(1),
            at: DateTime::UNIX_EPOCH,
            context: ContextId::first_of(crate::view::SessionId::new()),
            entry,
        }
    }

    fn tool(outcome: ToolOutcome) -> ToolView {
        ToolView {
            subagent: None,
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
        assert!(
            indices(
                &[logged(Entry::AgentMessage {
                    text: "x".into(),
                    full: None,
                })],
                &query
            )
            .is_empty()
        );
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
                from: DeviceRef {
                    id: DeviceId::new(),
                    name: None,
                    me: false,
                },
                outgoing: None,
                delivery: None,
            }),
            logged(Entry::AgentMessage {
                text: "an agent reply".into(),
                full: None,
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
                answer: None,
            }),
            logged(Entry::Interrupted),
            TimelineItem::Pending {
                command_id: CommandId::new(),
                context: ContextId::new(),
                text: "queued question".into(),
                outgoing: Outgoing::Queued,
                discardable: true,
                delivery: None,
            },
        ];
        let hit = |q: &str| indices(&timeline, &Query::new(q));
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
    fn an_adopted_entry_is_found_by_its_conversation_id() {
        let timeline = vec![
            logged(Entry::Adopted {
                conversation: "3f2a-91bc".into(),
            }),
            logged(Entry::Adopted {
                conversation: "7c00-aaaa".into(),
            }),
        ];
        let hit = |q: &str| indices(&timeline, &Query::new(q));
        assert_eq!(hit("91bc"), [0]);
        assert_eq!(hit("aaaa"), [1]);
        assert!(hit("zzzz").is_empty());
    }

    #[test]
    fn an_effort_entry_is_found_by_its_level() {
        let timeline = vec![
            logged(Entry::AgentSetting {
                setting: AgentSetting::Effort(Some(Effort::XHigh)),
                by: None,
                refused: None,
            }),
            logged(Entry::AgentSetting {
                setting: AgentSetting::Effort(Some(Effort::Low)),
                by: None,
                refused: Some("kept medium".into()),
            }),
        ];
        let hit = |q: &str| indices(&timeline, &Query::new(q));
        assert_eq!(hit("xhigh"), [0]);
        assert_eq!(hit("low"), [1]);
        assert_eq!(hit("kept"), [1]);
        assert!(hit("max").is_empty());
    }

    #[test]
    fn a_running_call_is_found_by_its_title() {
        let timeline = vec![logged(Entry::Tool(Box::new(tool(ToolOutcome::Running {
            since: DateTime::UNIX_EPOCH,
        }))))];
        assert_eq!(indices(&timeline, &Query::new("cargo test")), [0]);
        assert!(indices(&timeline, &Query::new("exit")).is_empty());
    }

    fn blob(state: BlobState) -> BlobView {
        BlobView {
            id: BlobId::new(),
            state,
            change: None,
            pending: None,
        }
    }

    fn result(output: BlobView) -> TimelineItem {
        logged(Entry::ToolResult {
            summary: "output".into(),
            output: Some(FullView::Blob(output)),
        })
    }

    #[test]
    fn an_inline_output_is_searched_as_text_at_hand() {
        let inline = |text: &str| Some(FullView::Inline(text.into()));
        let mut call = done("exit 0", "ok");
        if let ToolOutcome::Done { output, .. } = &mut call {
            *output = inline("ok\nnote for the model");
        }
        let timeline = vec![
            logged(Entry::Tool(Box::new(tool(call)))),
            logged(Entry::ToolResult {
                summary: "output".into(),
                output: inline("a lone note for the model"),
            }),
            // Control: the same words absent.
            logged(Entry::ToolResult {
                summary: "output".into(),
                output: inline("nothing here"),
            }),
        ];
        // Never through the blob callback: there is no blob.
        assert_eq!(
            indices(&timeline, &Query::new("note for the model")),
            [0, 1]
        );
        let found = find(&timeline, &Query::new("note"), &mut |_| None);
        assert_eq!((found.not_downloaded, found.cut), (0, 0));
    }

    #[test]
    fn a_long_messages_whole_text_is_searched_once_here() {
        let here = blob(BlobState::Available {
            path: "/data/blobs/m".into(),
        });
        let away = blob(BlobState::NotDownloaded { size: 40_000 });
        let timeline = vec![
            logged(Entry::AgentMessage {
                text: "# Report".into(),
                full: Some(FullView::Blob(here.clone())),
            }),
            logged(Entry::Imported {
                at: DateTime::UNIX_EPOCH,
                entry: ImportedEntry::UserMessage {
                    text: "log:".into(),
                    full: Some(FullView::Blob(away)),
                },
            }),
            // Control: a whole message without the words.
            logged(Entry::AgentMessage {
                text: "nothing".into(),
                full: None,
            }),
        ];
        let mut asked = Vec::new();
        let found = find(&timeline, &Query::new("deep finding"), &mut |view| {
            asked.push(view.id);
            Some(BlobText {
                text: "# Report\n\nthe deep finding\n".into(),
                cut: false,
            })
        });
        assert_eq!(asked, [here.id]);
        assert_eq!(found.not_downloaded, 1);
        assert_eq!(
            found.hits,
            [Hit {
                index: 0,
                blob: Some(BlobLine {
                    id: here.id,
                    line: 2
                }),
            }]
        );
    }

    #[test]
    fn blob_callback_not_called_for_undownloaded_blobs() {
        let available = blob(BlobState::Available {
            path: "/data/blobs/x".into(),
        });
        let timeline = vec![
            result(blob(BlobState::NotDownloaded { size: 10 })),
            result(blob(BlobState::Requested { size: 10 })),
            result(blob(BlobState::Downloading {
                got: 1,
                size: 10,
                paused: Some(crate::view::Pause::Offline),
            })),
            result(blob(BlobState::Failed {
                size: 10,
                reason: "size mismatch".into(),
            })),
            result(available.clone()),
        ];
        let mut asked = Vec::new();
        let found = find(&timeline, &Query::new("needle"), &mut |view| {
            asked.push(view.id);
            Some(BlobText {
                text: "hay\nhay\nthe NEEDLE\n".into(),
                cut: false,
            })
        });
        assert_eq!(asked, [available.id]);
        assert_eq!(found.not_downloaded, 4);
        assert_eq!(found.cut, 0);
        assert_eq!(
            found.hits,
            [Hit {
                index: 4,
                blob: Some(BlobLine {
                    id: available.id,
                    line: 2
                }),
            }]
        );
        // An empty query reads nothing and counts nothing.
        let found = find(&timeline, &Query::new(""), &mut |_| panic!("read"));
        assert_eq!(found, Found::default());
    }

    #[test]
    fn blob_content_is_searched_in_the_order_it_shows() {
        let path = || BlobState::Available {
            path: "/data/blobs/x".into(),
        };
        let (body, output, input) = (blob(path()), blob(path()), blob(path()));
        let mut view = tool(ToolOutcome::Done {
            is_error: false,
            status: None,
            preview: String::new(),
            lines: 40,
            output: Some(FullView::Blob(output.clone())),
            took: Duration::from_secs(1),
        });
        view.body.as_mut().unwrap().full = Some(FullView::Blob(body.clone()));
        // The whole input is not shown beside a body or an output.
        view.input = Some(FullView::Blob(input.clone()));
        let timeline = vec![logged(Entry::Tool(Box::new(view)))];
        let texts = [
            (body.id, "fn main() {}\n"),
            (output.id, "ok\nok\nwarning: unused\n"),
            (input.id, "warning"),
        ];
        let mut cut = |view: &BlobView| {
            let (_, text) = texts.iter().find(|(id, _)| *id == view.id).unwrap();
            Some(BlobText {
                text: (*text).into(),
                cut: view.id == output.id,
            })
        };
        let found = find(&timeline, &Query::new("Warning"), &mut cut);
        assert!(found.hits.is_empty(), "case matches: {found:?}");
        let found = find(&timeline, &Query::new("warning"), &mut cut);
        assert_eq!(
            found.hits[0].blob,
            Some(BlobLine {
                id: output.id,
                line: 2
            })
        );
        assert_eq!((found.not_downloaded, found.cut), (0, 1));
        // A match in the view's own text has no blob line.
        let found = find(&timeline, &Query::new("cargo"), &mut cut);
        assert_eq!(found.hits[0].blob, None);
        // An unreadable blob is skipped.
        let found = find(&timeline, &Query::new("warning"), &mut |_| None);
        assert!(found.hits.is_empty());
    }

    #[test]
    fn an_approvals_own_input_is_searched_when_it_differs() {
        let path = || BlobState::Available {
            path: "/data/blobs/x".into(),
        };
        let (input, asked) = (blob(path()), blob(path()));
        let approval = |own: &BlobView| ToolApproval {
            id: crate::view::ApprovalId::new(),
            question: false,
            status: crate::view::ApprovalStatus::Pending,
            answer: None,
            input: Some(FullView::Blob(own.clone())),
        };
        let mut view = tool(ToolOutcome::Running {
            since: DateTime::UNIX_EPOCH,
        });
        view.body = None;
        view.input = Some(FullView::Blob(input.clone()));
        view.approval = Some(approval(&asked));
        let texts = [(input.id, "rm -rf build\n"), (asked.id, "rm -rf /\n")];
        let mut read = |view: &BlobView| {
            let (_, text) = texts.iter().find(|(id, _)| *id == view.id).unwrap();
            Some(BlobText {
                text: (*text).into(),
                cut: false,
            })
        };
        let timeline = vec![logged(Entry::Tool(Box::new(view.clone())))];
        let found = find(&timeline, &Query::new("rm -rf /"), &mut read);
        assert_eq!(
            found.hits[0].blob,
            Some(BlobLine {
                id: asked.id,
                line: 0
            })
        );
        // The call's own input blob over again is not searched twice.
        view.approval = Some(approval(&input));
        assert_eq!(blobs(&Entry::Tool(Box::new(view))), vec![&input]);
    }

    #[test]
    fn a_blob_is_read_up_to_its_head() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob");
        std::fs::write(&path, "x".repeat(HEAD_BYTES as usize + 1)).unwrap();
        let text = BlobText::read(&blob(BlobState::Available { path: path.clone() })).unwrap();
        assert_eq!((text.text.len() as u64, text.cut), (HEAD_BYTES, true));
        std::fs::write(&path, "short").unwrap();
        let text = BlobText::read(&blob(BlobState::Available { path })).unwrap();
        assert_eq!((&*text.text, text.cut), ("short", false));
        let gone = dir.path().join("gone");
        assert_eq!(
            BlobText::read(&blob(BlobState::Available { path: gone })),
            None
        );
        assert_eq!(
            BlobText::read(&blob(BlobState::NotDownloaded { size: 5 })),
            None
        );
    }

    #[test]
    fn first_line_counts_lines_either_way() {
        let text = "one\ntwo\nThree\nthree";
        assert_eq!(Query::new("three").first_line(text), Some(2));
        assert_eq!(Query::new("three").first_line("x\r\nthree"), Some(1));
        assert_eq!(Query::new("Three").first_line(text), Some(2));
        assert_eq!(Query::new("tHree").first_line(text), None);
        assert_eq!(Query::new("").first_line(text), None);
    }
}
