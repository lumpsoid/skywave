//! How Claude Code's tools are shown (docs/agents/claude-code.md §6b, D3).
//!
//! [`call_display`] maps a `tool_use` onto an agent-agnostic [`ToolDisplay`]
//! from its input alone, so an approval for the call shows the same thing
//! (an edit's diff included) before the tool runs. [`result_display`] reads
//! the outcome from the structured `tool_use_result` the CLI sends next to
//! each `tool_result`, not from the model-facing text: that text carries
//! notes for the model ("file state is current in your context …") which
//! stay in the whole output but not in the preview.
//!
//! Content is handed over uncut; the mapper applies the protocol's caps.

use serde_json::{Map, Value};

use super::protocol::DENY_MESSAGE;
use crate::agent::{BodyFormat, DisplayBody, ResultDisplay, ToolDisplay, ToolKind};

/// Status of a call whose approval was denied.
pub const DENIED: &str = "denied";
/// Status of a call whose approval was rejected by an interrupt or aborted.
pub const CANCELLED: &str = "cancelled";

/// What the CLI's structured result says when an interrupt rejects a call.
const REJECTED: &str = "User rejected tool use";
/// The marker of an approval that failed because its stream closed.
const ABORTED: &str = "AbortError";
/// How a string-form `tool_use_result` (a failure) starts.
const ERROR_PREFIX: &str = "Error: ";
/// How a failed command's error starts.
const EXIT_CODE: &str = "Exit code ";
/// Largest line product the diff aligns; bigger edits show as replaced.
const DIFF_MAX_CELLS: usize = 1 << 20;

/// How to show a call to tool `name` with `input`; `None` for a tool shown
/// generically.
pub fn call_display(name: &str, input: &Value) -> Option<ToolDisplay> {
    let input = input.as_object()?;
    match name {
        "Write" => {
            let content = str_field(input, "content").unwrap_or("");
            display(
                input,
                ToolKind::FileWrite,
                str_field(input, "file_path")?,
                None,
                text_body(content),
                &["file_path", "content"],
            )
        }
        "Edit" => {
            let diff = line_diff(
                str_field(input, "old_string").unwrap_or(""),
                str_field(input, "new_string").unwrap_or(""),
            );
            let all = input.get("replace_all").and_then(Value::as_bool) == Some(true);
            display(
                input,
                ToolKind::FileEdit,
                str_field(input, "file_path")?,
                all.then(|| "all occurrences".to_owned()),
                diff_body(diff),
                &["file_path", "old_string", "new_string", "replace_all"],
            )
        }
        "MultiEdit" => {
            let edits = input.get("edits")?.as_array()?;
            let mut diff = String::new();
            for edit in edits {
                if edits.len() > 1 {
                    diff.push_str("@@\n");
                }
                diff.push_str(&line_diff(
                    edit.get("old_string").and_then(Value::as_str).unwrap_or(""),
                    edit.get("new_string").and_then(Value::as_str).unwrap_or(""),
                ));
            }
            display(
                input,
                ToolKind::FileEdit,
                str_field(input, "file_path")?,
                Some(count(edits.len(), "edit", "edits")),
                diff_body(diff),
                // Each edit's replace_all is not shown.
                &["file_path"],
            )
        }
        "Bash" => display(
            input,
            ToolKind::Shell,
            &format!("$ {}", str_field(input, "command")?),
            str_field(input, "description").map(str::to_owned),
            None,
            &["command", "description"],
        ),
        "Read" => {
            let offset = input.get("offset").and_then(Value::as_u64);
            let limit = input.get("limit").and_then(Value::as_u64);
            let detail = match (offset, limit) {
                (Some(from), Some(n)) => Some(format!("lines {from}–{}", from + n.max(1) - 1)),
                (Some(from), None) => Some(format!("from line {from}")),
                (None, Some(n)) => Some(format!("first {}", count(n as usize, "line", "lines"))),
                (None, None) => None,
            };
            display(
                input,
                ToolKind::FileRead,
                str_field(input, "file_path")?,
                detail,
                None,
                &["file_path", "offset", "limit"],
            )
        }
        "Grep" => {
            let scope: Vec<&str> = ["path", "glob", "type"]
                .iter()
                .filter_map(|key| str_field(input, key))
                .collect();
            display(
                input,
                ToolKind::Search,
                str_field(input, "pattern")?,
                (!scope.is_empty()).then(|| scope.join(" · ")),
                None,
                &["pattern", "path", "glob", "type"],
            )
        }
        "Glob" => display(
            input,
            ToolKind::Search,
            str_field(input, "pattern")?,
            str_field(input, "path").map(str::to_owned),
            None,
            &["pattern", "path"],
        ),
        "WebFetch" => display(
            input,
            ToolKind::Fetch,
            str_field(input, "url")?,
            str_field(input, "prompt").map(str::to_owned),
            None,
            &["url", "prompt"],
        ),
        "WebSearch" => display(
            input,
            ToolKind::Fetch,
            str_field(input, "query")?,
            None,
            None,
            &["query"],
        ),
        "Task" | "Agent" => display(
            input,
            ToolKind::Subagent,
            str_field(input, "description")?,
            str_field(input, "subagent_type").map(str::to_owned),
            text_body(str_field(input, "prompt").unwrap_or("")),
            &["description", "subagent_type", "prompt"],
        ),
        _ => None,
    }
}

/// How a call to `tool` (if known) ended: `structured` is the message's
/// `tool_use_result`, `text` the model-facing `tool_result` content, kept
/// whole as the output.
pub fn result_display(
    tool: Option<&str>,
    structured: Option<&Value>,
    text: String,
    is_error: bool,
) -> ResultDisplay {
    let (status, preview) = match structured {
        Some(Value::String(message)) => {
            failure(message.strip_prefix(ERROR_PREFIX).unwrap_or(message))
        }
        Some(Value::Object(fields)) => success(tool, fields),
        // Without a structured result (older CLIs), only a failure's text is
        // worth reading.
        _ if is_error => failure(&text),
        _ => (None, None),
    };
    ResultDisplay {
        is_error,
        status,
        preview: preview.filter(|p| *p != text),
        output: text,
    }
}

/// Status and preview of a failure reported as `message`.
fn failure(message: &str) -> (Option<String>, Option<String>) {
    let message = message.trim();
    if message == DENY_MESSAGE {
        return (Some(DENIED.to_owned()), Some(message.to_owned()));
    }
    if message == REJECTED || message.contains(ABORTED) {
        return (Some(CANCELLED.to_owned()), Some(message.to_owned()));
    }
    if let Some(rest) = message.strip_prefix(EXIT_CODE) {
        let (code, output) = rest.split_once('\n').unwrap_or((rest, ""));
        if let Ok(code) = code.trim().parse::<i64>() {
            return (Some(format!("exit {code}")), Some(output.to_owned()));
        }
    }
    (None, Some(message.to_owned()))
}

/// Status and preview of a structured result of `tool`.
fn success(tool: Option<&str>, fields: &Map<String, Value>) -> (Option<String>, Option<String>) {
    match tool {
        Some("Bash") => {
            let stdout = str_field(fields, "stdout").unwrap_or("");
            let stderr = str_field(fields, "stderr").unwrap_or("");
            let preview = match (stdout.is_empty(), stderr.is_empty()) {
                (_, true) => stdout.to_owned(),
                (true, false) => stderr.to_owned(),
                (false, false) => format!("{}\n{stderr}", stdout.trim_end_matches('\n')),
            };
            let interrupted = fields.get("interrupted").and_then(Value::as_bool) == Some(true);
            (interrupted.then(|| "interrupted".to_owned()), Some(preview))
        }
        Some("Write" | "Edit" | "MultiEdit") => {
            let status = if str_field(fields, "type") == Some("create") {
                Some("created".to_owned())
            } else {
                fields
                    .get("structuredPatch")
                    .and_then(Value::as_array)
                    .map(|hunks| {
                        let (added, removed) = patch_counts(hunks);
                        format!("updated +{added} -{removed}")
                    })
            };
            // The call's body already shows the content or the diff.
            (status, Some(String::new()))
        }
        Some("Read") => {
            let Some(file) = fields.get("file").and_then(Value::as_object) else {
                return (None, None);
            };
            let lines = file
                .get("numLines")
                .and_then(Value::as_u64)
                .map(|n| count(n as usize, "line", "lines"));
            (lines, str_field(file, "content").map(str::to_owned))
        }
        Some("Grep") => {
            let number = |key| fields.get(key).and_then(Value::as_u64).map(|n| n as usize);
            let status = match str_field(fields, "mode") {
                Some("content") => number("numLines").map(|n| count(n, "line", "lines")),
                Some("count") => number("numMatches").map(|n| count(n, "match", "matches")),
                _ => number("numFiles").map(|n| count(n, "file", "files")),
            };
            let preview = str_field(fields, "content")
                .map(str::to_owned)
                .or_else(|| filenames(fields));
            (status, preview)
        }
        Some("Glob") => {
            let files = fields
                .get("numFiles")
                .and_then(Value::as_u64)
                .map(|n| count(n as usize, "file", "files"));
            (files, filenames(fields))
        }
        Some("WebFetch") => {
            let code = fields.get("code").and_then(Value::as_u64);
            let status = code.map(|code| match str_field(fields, "codeText") {
                Some(text) if !text.is_empty() => format!("{code} {text}"),
                _ => code.to_string(),
            });
            (status, str_field(fields, "result").map(str::to_owned))
        }
        _ => (None, None),
    }
}

/// Lines added and removed by a `structuredPatch`.
fn patch_counts(hunks: &[Value]) -> (usize, usize) {
    let lines = hunks
        .iter()
        .filter_map(|hunk| hunk.get("lines").and_then(Value::as_array))
        .flatten()
        .filter_map(Value::as_str);
    let mut added = 0;
    let mut removed = 0;
    for line in lines {
        if line.starts_with('+') {
            added += 1;
        } else if line.starts_with('-') {
            removed += 1;
        }
    }
    (added, removed)
}

/// A result's `filenames`, one per line.
fn filenames(fields: &Map<String, Value>) -> Option<String> {
    let names: Vec<&str> = fields
        .get("filenames")?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .collect();
    Some(names.join("\n"))
}

/// A display of `kind`; it shows all of `input` when every non-null field
/// is one of `shown`.
fn display(
    input: &Map<String, Value>,
    kind: ToolKind,
    title: &str,
    detail: Option<String>,
    body: Option<DisplayBody>,
    shown: &[&str],
) -> Option<ToolDisplay> {
    let shows_all_input = input
        .iter()
        .all(|(key, value)| value.is_null() || shown.contains(&key.as_str()));
    Some(ToolDisplay {
        kind,
        title: title.to_owned(),
        detail: detail.filter(|d| !d.trim().is_empty()),
        body,
        shows_all_input,
    })
}

fn text_body(text: &str) -> Option<DisplayBody> {
    (!text.is_empty()).then(|| DisplayBody {
        format: BodyFormat::Text,
        text: text.to_owned(),
    })
}

fn diff_body(diff: String) -> Option<DisplayBody> {
    (!diff.is_empty()).then_some(DisplayBody {
        format: BodyFormat::Diff,
        text: diff,
    })
}

/// A line diff of `old` to `new`, one line each, prefixed `-` (removed),
/// `+` (added) or ` ` (kept), with no hunk headers.
pub fn line_diff(old: &str, new: &str) -> String {
    let old: Vec<&str> = old.lines().collect();
    let new: Vec<&str> = new.lines().collect();
    let head = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let tail = old[head..]
        .iter()
        .rev()
        .zip(new[head..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let (a, b) = (&old[head..old.len() - tail], &new[head..new.len() - tail]);

    let mut out = String::new();
    let mut push = |prefix: char, line: &str| {
        out.push(prefix);
        out.push_str(line);
        out.push('\n');
    };
    old[..head].iter().for_each(|line| push(' ', line));
    if a.len().saturating_mul(b.len()) > DIFF_MAX_CELLS {
        a.iter().for_each(|line| push('-', line));
        b.iter().for_each(|line| push('+', line));
    } else {
        // lcs[i][j]: longest common subsequence of a[i..] and b[j..].
        let mut lcs = vec![vec![0u32; b.len() + 1]; a.len() + 1];
        for i in (0..a.len()).rev() {
            for j in (0..b.len()).rev() {
                lcs[i][j] = if a[i] == b[j] {
                    lcs[i + 1][j + 1] + 1
                } else {
                    lcs[i + 1][j].max(lcs[i][j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < a.len() || j < b.len() {
            if i < a.len() && j < b.len() && a[i] == b[j] {
                push(' ', a[i]);
                i += 1;
                j += 1;
            } else if j == b.len() || (i < a.len() && lcs[i + 1][j] >= lcs[i][j + 1]) {
                push('-', a[i]);
                i += 1;
            } else {
                push('+', b[j]);
                j += 1;
            }
        }
    }
    old[old.len() - tail..]
        .iter()
        .for_each(|line| push(' ', line));
    out
}

fn str_field<'a>(fields: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    fields.get(key).and_then(Value::as_str)
}

fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn an_edit_shows_a_line_diff() {
        assert_eq!(line_diff("a\nb\nc", "a\nB\nc"), " a\n-b\n+B\n c\n");
        assert_eq!(line_diff("", "x\ny"), "+x\n+y\n");
        assert_eq!(line_diff("x", ""), "-x\n");
        assert_eq!(line_diff("a\nb", "b\nc"), "-a\n b\n+c\n");
        let edit = call_display(
            "Edit",
            &json!({ "file_path": "src/a.rs", "old_string": "let x = 1;", "new_string": "let x = 2;" }),
        )
        .unwrap();
        assert_eq!(edit.kind, ToolKind::FileEdit);
        assert_eq!(edit.title, "src/a.rs");
        assert!(edit.shows_all_input);
        assert_eq!(
            edit.body,
            Some(DisplayBody {
                format: BodyFormat::Diff,
                text: "-let x = 1;\n+let x = 2;\n".to_owned(),
            })
        );
        let multi = call_display(
            "MultiEdit",
            &json!({ "file_path": "a", "edits": [
                { "old_string": "1", "new_string": "2" },
                { "old_string": "3", "new_string": "4" },
            ] }),
        )
        .unwrap();
        assert_eq!(multi.detail.as_deref(), Some("2 edits"));
        assert_eq!(multi.body.unwrap().text, "@@\n-1\n+2\n@@\n-3\n+4\n");
        assert!(!multi.shows_all_input);
    }

    #[test]
    fn calls_map_onto_kinds() {
        let shell = call_display(
            "Bash",
            &json!({ "command": "ls -la", "description": "List files" }),
        )
        .unwrap();
        assert_eq!(
            (shell.kind, shell.title.as_str(), shell.detail.as_deref()),
            (ToolKind::Shell, "$ ls -la", Some("List files"))
        );
        assert!(shell.shows_all_input && shell.body.is_none());
        let timed = call_display("Bash", &json!({ "command": "make", "timeout": 600000 })).unwrap();
        assert!(!timed.shows_all_input);

        let read = call_display(
            "Read",
            &json!({ "file_path": "a", "offset": 10, "limit": 5 }),
        );
        assert_eq!(read.unwrap().detail.as_deref(), Some("lines 10–14"));
        let grep = call_display("Grep", &json!({ "pattern": "fn main", "path": "src" })).unwrap();
        assert_eq!(
            (grep.kind, grep.detail.as_deref()),
            (ToolKind::Search, Some("src"))
        );
        assert_eq!(
            call_display("Glob", &json!({ "pattern": "**/*.rs" }))
                .unwrap()
                .kind,
            ToolKind::Search
        );
        assert_eq!(
            call_display("WebFetch", &json!({ "url": "https://x", "prompt": "p" }))
                .unwrap()
                .kind,
            ToolKind::Fetch
        );
        assert_eq!(
            call_display("WebSearch", &json!({ "query": "q" }))
                .unwrap()
                .kind,
            ToolKind::Fetch
        );
        let task = call_display(
            "Task",
            &json!({ "description": "Find it", "subagent_type": "Explore", "prompt": "look" }),
        )
        .unwrap();
        assert_eq!(
            (task.kind, task.title.as_str(), task.detail.as_deref()),
            (ToolKind::Subagent, "Find it", Some("Explore"))
        );
        // Unknown tools, and known ones missing their main field, are generic.
        assert_eq!(call_display("TodoWrite", &json!({ "todos": [] })), None);
        assert_eq!(call_display("Bash", &json!({ "description": "x" })), None);
    }

    #[test]
    fn results_come_from_the_structured_result() {
        let bash = result_display(
            Some("Bash"),
            Some(&json!({ "stdout": "out", "stderr": "warn", "interrupted": false })),
            "out\nwarn".to_owned(),
            false,
        );
        assert_eq!((bash.status, bash.preview), (None, None));

        let failed = result_display(
            Some("Bash"),
            Some(&json!("Error: Exit code 2\nno such file")),
            "Exit code 2\nno such file".to_owned(),
            true,
        );
        assert_eq!(failed.status.as_deref(), Some("exit 2"));
        assert_eq!(failed.preview.as_deref(), Some("no such file"));

        let edit = result_display(
            Some("Edit"),
            Some(&json!({ "filePath": "a", "structuredPatch": [
                { "lines": [" a", "-b", "+B", "+C"] },
            ] })),
            "The file a has been updated. (file state is current in your context)".to_owned(),
            false,
        );
        assert_eq!(edit.status.as_deref(), Some("updated +2 -1"));
        assert_eq!(edit.preview.as_deref(), Some(""));

        let grep = result_display(
            Some("Grep"),
            Some(&json!({ "mode": "files_with_matches", "numFiles": 2, "filenames": ["a", "b"] })),
            "Found 2 files\na\nb".to_owned(),
            false,
        );
        assert_eq!(grep.status.as_deref(), Some("2 files"));
        assert_eq!(grep.preview.as_deref(), Some("a\nb"));
    }

    #[test]
    fn denial_and_abort_are_recognised() {
        let denied = result_display(None, None, DENY_MESSAGE.to_owned(), true);
        assert_eq!(denied.status.as_deref(), Some(DENIED));
        let aborted = result_display(
            Some("Write"),
            Some(&json!(
                "Error: Tool permission request failed: AbortError: Stream closed"
            )),
            "Tool permission request failed: AbortError: Stream closed".to_owned(),
            true,
        );
        assert_eq!(aborted.status.as_deref(), Some(CANCELLED));
        assert_eq!(aborted.preview, None);
        let other = result_display(None, Some(&json!("Error: boom")), "boom".to_owned(), true);
        assert_eq!((other.status, other.preview), (None, None));
    }
}
