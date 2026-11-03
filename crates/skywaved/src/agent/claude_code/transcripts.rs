//! The CLI's stored transcripts as an [`OutsideConversations`] source
//! (docs/agents/claude-code.md §6c).
//!
//! The CLI keeps every conversation, run from a terminal or by Skywave, as
//! `<projects>/<dir>/<id>.jsonl`, `<projects>` being
//! `$CLAUDE_CONFIG_DIR/projects` or `~/.claude/projects` ([`projects_dir`])
//! and `<dir>` the conversation's cwd made a file name ([`project_dir`]).
//!
//! **Locating.** One rule for listing and history: a transcript is a
//! regular file named `<uuid>.jsonl` (lowercase, hyphenated, as the CLI
//! writes ids) directly in a project dir, neither of them a symlink; `subagents/` and `tool-results/`
//! beside it are never read. Its workdir is the `cwd` of its first `user`
//! line that is not a subagent's (`isSidechain`), and it is kept only when
//! `project_dir(cwd)` is the dir it sits in. The dir name alone never
//! decides: it is lossy (`/a/b-c` and `/a/b/c` share one), so a
//! conversation of `/a/b-c` sitting in `/a/b/c`'s dir is skipped. A listed
//! workdir must also be canonical (a cwd through a symlink cannot be
//! resumed: "No conversation found") and under a root. Lines are read
//! leniently: one that does not parse (the last line of a conversation
//! the terminal is still writing) is skipped.
//!
//! **Listing** stats every candidate under the project dirs whose name
//! starts like a root's, newest mtime first, and reads files in that order
//! until it kept `max`, so it reads about `max` files, not all of them. A
//! file's summary is cached by `(path, len, mtime)`.
//!
//! **History** walks the active branch: from the last `user` or `assistant`
//! line back through `parentUuid` (across a `/compact` boundary by its
//! `logicalParentUuid`), then reversed, which is what `--resume` sees. Where
//! a parent is missing it falls back to file order. Subagent, meta and
//! compact-summary lines are dropped; the rest is translated with the same
//! helpers as the live stream ([`protocol::assistant_events`],
//! [`protocol::tool_results`]), so a call looks the same either way.
//!
//! Blocking: file system calls only.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Instant, SystemTime};

use chrono::{DateTime, Utc};
use serde_json::Value;
use skywave_proto::SessionId;
use uuid::Uuid;

use super::protocol::{self, str_field};
use crate::agent::{
    HistoryItem, HistoryRead, Listed, OutsideConversation, OutsideConversations, SourceError,
};
use crate::dirs;

/// A transcript larger than this is not read: it would be held in memory
/// whole. Far above any seen (10 MB).
pub const MAX_TRANSCRIPT_BYTES: u64 = 256 << 20;

/// The longest project dir name the CLI writes as is; a longer one is cut
/// and given a hash ([`project_dir`]).
const DIR_NAME_MAX: usize = 200;

/// Lines parsed between two deadline checks.
const DEADLINE_EVERY: usize = 64;

/// The projects dir the CLI uses with this process's environment:
/// `$CLAUDE_CONFIG_DIR/projects`, else `$HOME/.claude/projects`. The CLI
/// the adapter spawns inherits the same environment.
pub fn projects_dir() -> Option<PathBuf> {
    let non_empty = |key| std::env::var_os(key).filter(|v| !v.is_empty());
    if let Some(config) = non_empty("CLAUDE_CONFIG_DIR") {
        return Some(PathBuf::from(config).join("projects"));
    }
    non_empty("HOME").map(|home| PathBuf::from(home).join(".claude").join("projects"))
}

/// The project dir name the CLI keeps `cwd`'s conversations under: each
/// UTF-16 unit that is not an ASCII letter or digit becomes `-`; a name
/// longer than 200 is cut to 200 and given `-` and a hash of `cwd` (a
/// JavaScript string hash, base 36), as probed with 2.1.310.
pub fn project_dir(cwd: &str) -> String {
    let mut name = sanitise(cwd);
    if name.len() > DIR_NAME_MAX {
        name.truncate(DIR_NAME_MAX);
        name.push('-');
        name.push_str(&base36(js_hash(cwd).unsigned_abs()));
    }
    name
}

fn sanitise(cwd: &str) -> String {
    let mut name = String::with_capacity(cwd.len());
    for ch in cwd.chars() {
        if ch.is_ascii_alphanumeric() {
            name.push(ch);
        } else {
            name.extend(std::iter::repeat_n('-', ch.len_utf16()));
        }
    }
    name
}

/// `hash = (hash << 5) - hash + unit | 0` over the UTF-16 units.
fn js_hash(text: &str) -> i32 {
    text.encode_utf16().fold(0i32, |hash, unit| {
        hash.wrapping_shl(5)
            .wrapping_sub(hash)
            .wrapping_add(i32::from(unit))
    })
}

fn base36(mut n: u32) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    loop {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
        if n == 0 {
            break;
        }
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// `id` as the CLI writes a conversation id, or `None`. Checked before
/// any file system call: the id becomes a path component.
fn conversation_id(id: &str) -> Option<Uuid> {
    Uuid::parse_str(id)
        .ok()
        .filter(|uuid| uuid.hyphenated().to_string() == id)
}

/// The CLI's transcripts under one projects dir.
#[derive(Debug)]
pub struct ClaudeTranscripts {
    projects: PathBuf,
    cache: Mutex<HashMap<PathBuf, Cached>>,
    files_read: AtomicUsize,
}

#[derive(Debug)]
struct Cached {
    len: u64,
    mtime: SystemTime,
    summary: Option<Summary>,
}

/// What a listing needs of one transcript.
#[derive(Debug, Clone)]
struct Summary {
    cwd: String,
    title: Option<String>,
    first_prompt: Option<String>,
    last_active: Option<DateTime<Utc>>,
    messages: u32,
}

/// A transcript file found by a listing.
struct Candidate {
    path: PathBuf,
    dir: String,
    id: String,
    len: u64,
    mtime: SystemTime,
}

impl ClaudeTranscripts {
    /// The transcripts under `projects` (see [`projects_dir`]).
    pub fn new(projects: PathBuf) -> ClaudeTranscripts {
        ClaudeTranscripts {
            projects,
            cache: Mutex::new(HashMap::new()),
            files_read: AtomicUsize::new(0),
        }
    }

    /// Transcript files opened and read so far, cache misses only.
    pub fn files_read(&self) -> usize {
        self.files_read.load(Ordering::Relaxed)
    }

    /// Every `<uuid>.jsonl` in a project dir whose name starts like one of
    /// `prefixes`, newest first.
    fn candidates(&self, prefixes: &[String]) -> Result<Vec<Candidate>, SourceError> {
        let dirs = match fs::read_dir(&self.projects) {
            Ok(dirs) => dirs,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(SourceError::Failed(format!(
                    "{}: {e}",
                    self.projects.display()
                )));
            }
        };
        let mut found = Vec::new();
        for dir in dirs.flatten() {
            if !dir.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let Ok(name) = dir.file_name().into_string() else {
                continue;
            };
            if !prefixes.iter().any(|p| name.starts_with(p.as_str())) {
                continue;
            }
            let Ok(files) = fs::read_dir(dir.path()) else {
                continue;
            };
            for file in files.flatten() {
                if !file.file_type().is_ok_and(|t| t.is_file()) {
                    continue;
                }
                let Some(id) = file
                    .file_name()
                    .to_str()
                    .and_then(|f| f.strip_suffix(".jsonl"))
                    .filter(|stem| conversation_id(stem).is_some())
                    .map(str::to_owned)
                else {
                    continue;
                };
                let Ok(meta) = file.metadata() else { continue };
                found.push(Candidate {
                    path: file.path(),
                    dir: name.clone(),
                    id,
                    len: meta.len(),
                    mtime: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                });
            }
        }
        found.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| a.path.cmp(&b.path)));
        Ok(found)
    }

    /// `candidate`'s summary, from the cache while its length and mtime are
    /// unchanged; `None` for a file with no workdir or too large, `Err` past
    /// the deadline.
    fn summary(
        &self,
        candidate: &Candidate,
        deadline: Instant,
    ) -> Result<Option<Summary>, TimedOut> {
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(cached) = cache.get(&candidate.path)
            && cached.len == candidate.len
            && cached.mtime == candidate.mtime
        {
            return Ok(cached.summary.clone());
        }
        drop(cache);
        let read = if candidate.len > MAX_TRANSCRIPT_BYTES {
            Ok(None)
        } else {
            self.read(&candidate.path)
        };
        let summary = match read {
            Ok(Some(bytes)) => Transcript::parse(&bytes, deadline)?.summary(),
            Ok(None) => None,
            // Not cached: the next listing tries again.
            Err(_) => return Ok(None),
        };
        cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        cache.insert(
            candidate.path.clone(),
            Cached {
                len: candidate.len,
                mtime: candidate.mtime,
                summary: summary.clone(),
            },
        );
        Ok(summary)
    }

    /// The bytes of the file at `path`, `None` when larger than
    /// [`MAX_TRANSCRIPT_BYTES`].
    fn read(&self, path: &Path) -> std::io::Result<Option<Vec<u8>>> {
        self.files_read.fetch_add(1, Ordering::Relaxed);
        let mut bytes = Vec::new();
        File::open(path)?
            .take(MAX_TRANSCRIPT_BYTES + 1)
            .read_to_end(&mut bytes)?;
        Ok((bytes.len() as u64 <= MAX_TRANSCRIPT_BYTES).then_some(bytes))
    }
}

impl OutsideConversations for ClaudeTranscripts {
    fn own(&self, db: &crate::db::Db) -> crate::error::Result<HashMap<String, SessionId>> {
        super::own_conversations(db.conn())
    }

    fn list(
        &self,
        roots: &[PathBuf],
        own: &HashMap<String, SessionId>,
        max: usize,
        deadline: Instant,
    ) -> Result<Listed, SourceError> {
        let mut prefixes: Vec<String> = dirs::canonical_roots(roots)
            .iter()
            .filter_map(|root| root.to_str())
            .map(|root| {
                let mut prefix = sanitise(root);
                prefix.truncate(DIR_NAME_MAX);
                prefix
            })
            .collect();
        prefixes.dedup();
        let mut listed = Listed::default();
        if prefixes.is_empty() {
            return Ok(listed);
        }
        for candidate in self.candidates(&prefixes)? {
            if listed.conversations.len() >= max || Instant::now() >= deadline {
                listed.truncated = true;
                break;
            }
            let summary = match self.summary(&candidate, deadline) {
                Ok(Some(summary)) => summary,
                Ok(None) => continue,
                Err(TimedOut) => {
                    listed.truncated = true;
                    break;
                }
            };
            if project_dir(&summary.cwd) != candidate.dir
                || dirs::canonical_under_roots(roots, &summary.cwd).as_ref() != Ok(&summary.cwd)
            {
                continue;
            }
            listed.conversations.push(OutsideConversation {
                session: own.get(&candidate.id).copied(),
                id: candidate.id,
                workdir: summary.cwd,
                title: summary.title,
                first_prompt: summary.first_prompt,
                last_active: summary
                    .last_active
                    .unwrap_or_else(|| DateTime::<Utc>::from(candidate.mtime)),
                messages: summary.messages,
            });
        }
        Ok(listed)
    }

    fn history(&self, workdir: &Path, id: &str, deadline: Instant) -> HistoryRead {
        let Some(id) = conversation_id(id) else {
            return HistoryRead::NotFound;
        };
        let Some(workdir) = workdir.to_str() else {
            return HistoryRead::NotFound;
        };
        // The listing's rule: a project dir and a transcript are real
        // entries, never symlinks.
        let dir = self.projects.join(project_dir(workdir));
        if !fs::symlink_metadata(&dir).is_ok_and(|meta| meta.is_dir()) {
            return HistoryRead::NotFound;
        }
        let path = dir.join(format!("{id}.jsonl"));
        match fs::symlink_metadata(&path) {
            Ok(meta) if !meta.is_file() => return HistoryRead::NotFound,
            Ok(meta) if meta.len() > MAX_TRANSCRIPT_BYTES => {
                return HistoryRead::Failed("too large".to_owned());
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return HistoryRead::NotFound,
            Err(e) => return HistoryRead::Failed(e.to_string()),
        }
        let bytes = match self.read(&path) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return HistoryRead::Failed("too large".to_owned()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return HistoryRead::NotFound,
            Err(e) => return HistoryRead::Failed(e.to_string()),
        };
        let Ok(transcript) = Transcript::parse(&bytes, deadline) else {
            return HistoryRead::Failed("timed out".to_owned());
        };
        if transcript.cwd() != Some(workdir) {
            return HistoryRead::NotFound;
        }
        HistoryRead::Read {
            items: transcript.items(),
        }
    }
}

/// The deadline passed while reading.
#[derive(Debug)]
struct TimedOut;

/// A transcript's lines that parse as JSON objects, in file order.
struct Transcript {
    lines: Vec<Value>,
}

impl Transcript {
    fn parse(bytes: &[u8], deadline: Instant) -> Result<Transcript, TimedOut> {
        let mut lines = Vec::new();
        for (n, line) in bytes.split(|b| *b == b'\n').enumerate() {
            if n % DEADLINE_EVERY == 0 && Instant::now() >= deadline {
                return Err(TimedOut);
            }
            if let Ok(value @ Value::Object(_)) = serde_json::from_slice::<Value>(line) {
                lines.push(value);
            }
        }
        Ok(Transcript { lines })
    }

    /// The cwd of the first main-conversation `user` line.
    fn cwd(&self) -> Option<&str> {
        self.lines
            .iter()
            .find(|line| str_field(line, "type") == "user" && !flag(line, "isSidechain"))
            .and_then(|line| line.get("cwd"))
            .and_then(Value::as_str)
    }

    fn summary(&self) -> Option<Summary> {
        let cwd = self.cwd()?.to_owned();
        let title = self
            .lines
            .iter()
            .rev()
            .filter(|line| str_field(line, "type") == "ai-title")
            .map(|line| str_field(line, "aiTitle"))
            .find(|title| !title.is_empty())
            .map(str::to_owned);
        let last_active = self.lines.iter().rev().find_map(timestamp);
        let items = self.items();
        let first_prompt = items.iter().find_map(|item| match item {
            HistoryItem::User { text, .. } => text
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .map(str::to_owned),
            HistoryItem::Agent { .. } => None,
        });
        let messages = items
            .iter()
            .filter(|item| {
                matches!(
                    item,
                    HistoryItem::User { .. }
                        | HistoryItem::Agent {
                            event: crate::agent::AdapterEvent::Text { .. },
                            ..
                        }
                )
            })
            .count();
        Some(Summary {
            cwd,
            title,
            first_prompt,
            last_active,
            messages: u32::try_from(messages).unwrap_or(u32::MAX),
        })
    }

    /// The active branch's messages translated, oldest first.
    fn items(&self) -> Vec<HistoryItem> {
        let mut tools = HashMap::new();
        let mut items = Vec::new();
        for line in self.branch() {
            if flag(line, "isMeta") || flag(line, "isCompactSummary") {
                continue;
            }
            let at = timestamp(line);
            let agent = |event| HistoryItem::Agent { at, event };
            match str_field(line, "type") {
                "assistant" => {
                    let mut events = Vec::new();
                    protocol::assistant_events(line, None, &mut tools, &mut events);
                    items.extend(events.into_iter().map(agent));
                }
                _ => {
                    let results = protocol::tool_results(line);
                    let structured =
                        protocol::single_structured(&results, line.get("toolUseResult"));
                    for result in results {
                        let tool = result.tool_use_id.and_then(|id| tools.remove(id));
                        items.push(agent(result.into_event(tool.as_deref(), structured)));
                    }
                    if let Some(text) = user_text(line) {
                        items.push(HistoryItem::User { at, text });
                    }
                }
            }
        }
        items
    }

    /// The main conversation's `user` and `assistant` lines on the active
    /// branch, with the results of its parallel calls, oldest first; in
    /// file order where the branch cannot be walked (a parent missing, a
    /// cycle).
    fn branch(&self) -> Vec<&Value> {
        let message = |line: &Value| {
            matches!(str_field(line, "type"), "user" | "assistant") && !flag(line, "isSidechain")
        };
        let Some(leaf) = self.lines.iter().rposition(message) else {
            return Vec::new();
        };
        let by_uuid: HashMap<&str, usize> = self
            .lines
            .iter()
            .enumerate()
            .filter_map(|(i, line)| line.get("uuid").and_then(Value::as_str).map(|u| (u, i)))
            .collect();
        let mut seen = HashSet::new();
        let mut at = Some(leaf);
        while let Some(i) = at {
            if !seen.insert(i) {
                return self.lines.iter().filter(|l| message(l)).collect();
            }
            let line = &self.lines[i];
            let parent = line
                .get("parentUuid")
                .and_then(Value::as_str)
                .or_else(|| line.get("logicalParentUuid").and_then(Value::as_str));
            at = match parent {
                None => None,
                Some(parent) => match by_uuid.get(parent) {
                    Some(&j) => Some(j),
                    None => return self.lines.iter().filter(|l| message(l)).collect(),
                },
            };
        }
        // Parallel calls: each `tool_result` line is parented to the
        // assistant line holding its own `tool_use`, so all but the last
        // result are siblings off the chain. Keep a result line whose
        // parent is on the chain; lines are appended, so file order is the
        // chain's order with those results in place.
        let on_chain = |parent: &str| by_uuid.get(parent).is_some_and(|j| seen.contains(j));
        (0..self.lines.len())
            .filter(|i| {
                let line = &self.lines[*i];
                seen.contains(i)
                    || (message(line)
                        && results_only(line)
                        && line
                            .get("parentUuid")
                            .and_then(Value::as_str)
                            .is_some_and(on_chain))
            })
            .map(|i| &self.lines[i])
            .filter(|l| message(l))
            .collect()
    }
}

/// Whether `line` is a `user` line holding tool results and nothing else.
fn results_only(line: &Value) -> bool {
    str_field(line, "type") == "user"
        && protocol::content_blocks(line).is_some_and(|blocks| {
            !blocks.is_empty() && blocks.iter().all(|b| str_field(b, "type") == "tool_result")
        })
}

fn flag(line: &Value, key: &str) -> bool {
    line.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn timestamp(line: &Value) -> Option<DateTime<Utc>> {
    let text = line.get("timestamp")?.as_str()?;
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// A `user` line's text: its string content, or its text blocks joined.
/// Kept verbatim, the CLI's own markers ("[Request interrupted by user]",
/// `<command-name>` wrappers) included.
fn user_text(line: &Value) -> Option<String> {
    let text = match line.get("message").and_then(|m| m.get("content"))? {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| str_field(b, "type") == "text")
            .map(|b| str_field(b, "text"))
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;
    use std::time::Duration;

    use serde_json::json;

    use super::*;
    use crate::agent::AdapterEvent;
    use crate::agent::claude_code::display;
    use crate::agent::claude_code::protocol::Protocol;

    const FIXTURES: &str = "tests/fixtures/claude-code";

    fn fixture(name: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(FIXTURES)
            .join(name);
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    /// The conversation id a fixture was recorded under.
    fn recorded_id(text: &str) -> String {
        text.lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .find_map(|l| {
                l.get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .expect("a sessionId")
    }

    fn later() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    /// A temp dir holding `root/` (the only root), `projects/` and
    /// whatever a test adds, all canonical.
    struct Env {
        _tmp: tempfile::TempDir,
        base: PathBuf,
        root: PathBuf,
        source: ClaudeTranscripts,
    }

    impl Env {
        fn new() -> Env {
            let tmp = tempfile::tempdir().unwrap();
            let base = fs::canonicalize(tmp.path()).unwrap();
            let root = base.join("root");
            fs::create_dir_all(&root).unwrap();
            fs::create_dir_all(base.join("projects")).unwrap();
            Env {
                source: ClaudeTranscripts::new(base.join("projects")),
                _tmp: tmp,
                base,
                root,
            }
        }

        fn roots(&self) -> Vec<PathBuf> {
            vec![self.root.clone()]
        }

        /// `rel` under the base, created.
        fn dir(&self, rel: &str) -> PathBuf {
            let dir = self.base.join(rel);
            fs::create_dir_all(&dir).unwrap();
            dir
        }

        /// `text` with the scrubbed cwd made `cwd`, saved as `<dir>/<id>.jsonl`.
        fn write(&self, text: &str, cwd: &Path, dir: &str, id: &str) -> PathBuf {
            let cwd = cwd.to_str().unwrap();
            let text = text
                .replace(&recorded_id(text), id)
                .replace("/WORKDIR", cwd)
                .replace("-WORKDIR", &project_dir(cwd));
            let path = self
                .base
                .join("projects")
                .join(dir)
                .join(format!("{id}.jsonl"));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, text).unwrap();
            path
        }

        /// Fixture `name` as a conversation of `workdir`, under its own id.
        fn place(&self, name: &str, workdir: &Path) -> (String, PathBuf) {
            let text = fixture(name);
            let id = recorded_id(&text);
            let path = self.write(&text, workdir, &project_dir(workdir.to_str().unwrap()), &id);
            (id, path)
        }

        fn list(&self, max: usize) -> Listed {
            self.source
                .list(&self.roots(), &HashMap::new(), max, later())
                .unwrap()
        }

        fn ids(&self, max: usize) -> Vec<String> {
            self.list(max)
                .conversations
                .into_iter()
                .map(|c| c.id)
                .collect()
        }

        fn history(&self, workdir: &Path, id: &str) -> HistoryRead {
            self.source.history(workdir, id, later())
        }

        fn items(&self, workdir: &Path, id: &str) -> Vec<HistoryItem> {
            match self.history(workdir, id) {
                HistoryRead::Read { items } => items,
                other => panic!("expected a history, got {other:?}"),
            }
        }
    }

    /// An item as a short tag, for order checks.
    fn tag(item: &HistoryItem) -> String {
        match item {
            HistoryItem::User { text, .. } => format!("user {text}"),
            HistoryItem::Agent { event, .. } => match event {
                AdapterEvent::Text { text, .. } => format!("text {text}"),
                AdapterEvent::ToolCall { name, .. } => format!("call {name}"),
                AdapterEvent::ToolResult { result, .. } => {
                    format!("result error={}", result.is_error)
                }
                other => panic!("not a history event: {other:?}"),
            },
        }
    }

    fn tags(items: &[HistoryItem]) -> Vec<String> {
        items.iter().map(tag).collect()
    }

    fn uuid() -> String {
        Uuid::new_v4().to_string()
    }

    fn set_mtime(path: &Path, secs: u64) {
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + secs))
            .unwrap();
    }

    #[test]
    fn project_dir_matches_the_names_the_cli_wrote() {
        // Probed with 2.1.310 (docs/agents/claude-code.md §6c).
        assert_eq!(
            project_dir("/tmp/skywave-sanitise-probe/a.b_c d+é"),
            "-tmp-skywave-sanitise-probe-a-b-c-d--"
        );
        let long = format!(
            "/tmp/skywave-sanitise-probe/{}/{}",
            "x".repeat(120),
            "y".repeat(120)
        );
        assert_eq!(
            project_dir(&long),
            format!(
                "-tmp-skywave-sanitise-probe-{}-{}-nusbbh",
                "x".repeat(120),
                "y".repeat(51)
            )
        );
        assert_eq!(project_dir("/a/b-c"), project_dir("/a/b/c"));
    }

    #[test]
    fn a_two_turn_talk_reads_in_order_with_times() {
        let env = Env::new();
        let workdir = env.dir("root/project");
        let (id, _) = env.place("transcripts/two_turn.jsonl", &workdir);
        let items = env.items(&workdir, &id);
        assert_eq!(
            tags(&items),
            [
                "user say hi".to_owned(),
                format!(
                    "text Hi! I'm Claude, running as Haiku 5.5 in `{}`. What can I help you with today?",
                    workdir.display()
                ),
                "user what did I just ask?".to_owned(),
                "text You asked me to \"say hi.\"".to_owned(),
            ]
        );
        let at = |item: &HistoryItem| match item {
            HistoryItem::User { at, .. } | HistoryItem::Agent { at, .. } => at.unwrap(),
        };
        assert!(items.windows(2).all(|w| at(&w[0]) <= at(&w[1])));
        assert_eq!(at(&items[0]).to_rfc3339(), "2026-11-03T09:32:09.183+00:00");
    }

    /// The transcript's main lines as the stream would have carried them,
    /// through the live adapter.
    fn live_events(text: &str) -> Vec<AdapterEvent> {
        let mut protocol = Protocol::new(Uuid::new_v4(), false);
        let mut events = Vec::new();
        for line in text.lines() {
            let Ok(line) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let kind = str_field(&line, "type");
            if !matches!(kind, "user" | "assistant") || flag(&line, "isSidechain") {
                continue;
            }
            let mut stream = json!({
                "type": kind,
                "message": line["message"],
                "parent_tool_use_id": null,
                "session_id": line["sessionId"],
            });
            if let Some(structured) = line.get("toolUseResult") {
                stream["tool_use_result"] = structured.clone();
            }
            events.extend(protocol.on_message(&stream).events.into_iter().filter(|e| {
                matches!(
                    e,
                    AdapterEvent::Text { .. }
                        | AdapterEvent::ToolCall { .. }
                        | AdapterEvent::ToolResult { .. }
                )
            }));
        }
        events
    }

    #[test]
    fn tool_calls_and_results_translate_as_the_live_stream_does() {
        let env = Env::new();
        let workdir = env.dir("root/project");
        let (id, path) = env.place("transcripts/tools.jsonl", &workdir);
        let items = env.items(&workdir, &id);
        assert_eq!(
            tags(&items)[1..11],
            [
                "call Bash",
                "result error=false",
                "call Write",
                "result error=false",
                "call Read",
                "result error=false",
                "call Edit",
                "result error=false",
                "call Bash",
                "result error=false",
            ]
            .map(str::to_owned)[..],
            "{:#?}",
            tags(&items)
        );
        assert!(tags(&items)[0].starts_with("user Do these one at a time"));
        assert!(tags(&items)[11].starts_with("text Done."));
        // Thinking blocks are in the file, and nowhere in the items.
        assert!(fixture("transcripts/tools.jsonl").contains(r#""type":"thinking""#));
        let agent: Vec<AdapterEvent> = items
            .iter()
            .filter_map(|item| match item {
                HistoryItem::Agent { event, .. } => Some(event.clone()),
                HistoryItem::User { .. } => None,
            })
            .collect();
        assert_eq!(agent, live_events(&fs::read_to_string(path).unwrap()));
        // Each result is linked to the call before it, read by its tool.
        let mut calls = 0;
        for pair in agent.windows(2) {
            if let [
                AdapterEvent::ToolCall { call, display, .. },
                AdapterEvent::ToolResult { call: answered, .. },
            ] = pair
            {
                assert!(call.is_some());
                assert_eq!(call, answered);
                assert!(display.is_some());
                calls += 1;
            }
        }
        assert_eq!(calls, 5);
        let Some(AdapterEvent::ToolResult { result, .. }) = agent.get(9) else {
            panic!("{agent:#?}");
        };
        // The big output stays the stub the CLI wrote.
        assert!(
            result
                .output
                .starts_with("<persisted-output>\nOutput too large"),
            "{result:?}"
        );
    }

    #[test]
    fn displays_match_the_stream_fixtures_for_the_same_tool() {
        // The stream-json fixtures' calls of the tools the transcript has.
        let mut live: HashMap<String, ToolDisplayShape> = HashMap::new();
        for name in ["permissions.ndjson", "subagent_foreground_stop.ndjson"] {
            for line in fixture(name).lines() {
                let record: Value = serde_json::from_str(line).unwrap();
                if record["dir"] != "out" || record["msg"]["type"] != "assistant" {
                    continue;
                }
                for block in protocol::content_blocks(&record["msg"])
                    .into_iter()
                    .flatten()
                {
                    if block["type"] == "tool_use" {
                        let name = block["name"].as_str().unwrap();
                        live.insert(name.to_owned(), shape(name, &block["input"]));
                    }
                }
            }
        }
        let env = Env::new();
        let workdir = env.dir("root/project");
        let (id, _) = env.place("transcripts/tools.jsonl", &workdir);
        let mut compared = Vec::new();
        for item in env.items(&workdir, &id) {
            if let HistoryItem::Agent {
                event:
                    AdapterEvent::ToolCall {
                        name,
                        input,
                        display,
                        ..
                    },
                ..
            } = item
            {
                assert_eq!(display, display::call_display(&name, &input));
                assert!(display.is_some(), "{name}");
                if let Some(expected) = live.get(&name) {
                    assert_eq!(&shape(&name, &input), expected, "{name}");
                    compared.push(name);
                }
            }
        }
        compared.sort();
        compared.dedup();
        // Edit has no stream fixture to compare with.
        assert_eq!(compared, ["Bash", "Read", "Write"]);
    }

    /// What a call's display looks like, apart from its content.
    #[derive(Debug, PartialEq)]
    struct ToolDisplayShape {
        kind: crate::agent::ToolKind,
        has_body: bool,
        shows_all_input: bool,
        input_keys: Vec<String>,
    }

    fn shape(name: &str, input: &Value) -> ToolDisplayShape {
        let display = display::call_display(name, input).unwrap();
        let mut input_keys: Vec<String> = input
            .as_object()
            .unwrap()
            .keys()
            // Optional keys the model adds as it likes.
            .filter(|k| !matches!(k.as_str(), "description" | "timeout" | "replace_all"))
            .cloned()
            .collect();
        input_keys.sort();
        ToolDisplayShape {
            kind: display.kind,
            has_body: display.body.is_some(),
            shows_all_input: display.shows_all_input,
            input_keys,
        }
    }

    #[test]
    fn an_interrupted_turn_keeps_the_marker_and_the_rejected_result() {
        let env = Env::new();
        let workdir = env.dir("root/project");
        let (id, _) = env.place("transcripts/interrupt.jsonl", &workdir);
        let items = env.items(&workdir, &id);
        assert_eq!(
            tags(&items)[1..],
            [
                "call Bash",
                "result error=true",
                "user [Request interrupted by user for tool use]",
                "user say ok",
                "text ok",
            ]
            .map(str::to_owned)[..]
        );
    }

    #[test]
    fn a_subagent_is_its_call_and_result_only() {
        let env = Env::new();
        let workdir = env.dir("root/project");
        let (id, path) = env.place("transcripts/subagent.jsonl", &workdir);
        let subagent = fixture("transcripts/subagent.subagent-1.jsonl");
        let subagents = path.with_extension("").join("subagents");
        fs::create_dir_all(&subagents).unwrap();
        fs::write(subagents.join("agent-a1.jsonl"), &subagent).unwrap();
        let expected = vec![
            "user Use the Agent tool (subagent_type general-purpose, not in the background) to ask a helper to reply with only the word PONG. Wait for it, then tell me what it said.".to_owned(),
            "call Agent".to_owned(),
            "result error=false".to_owned(),
            "text The helper replied with: **PONG**".to_owned(),
        ];
        assert_eq!(tags(&env.items(&workdir, &id)), expected);
        // The subagents/ file is not a conversation.
        assert_eq!(env.ids(10), std::slice::from_ref(&id));

        // Built here, not recorded: the CLI wrote the subagent's lines
        // (`isSidechain`) apart, but a transcript holding them first, with a
        // cwd of their own, still reads the same.
        let mut mixed = subagent.replace("/WORKDIR", "/elsewhere");
        mixed.push_str(&fixture("transcripts/subagent.jsonl"));
        env.write(
            &mixed,
            &workdir,
            &project_dir(workdir.to_str().unwrap()),
            &id,
        );
        assert_eq!(tags(&env.items(&workdir, &id)), expected);
    }

    #[test]
    fn parallel_calls_keep_both_results_in_stream_order() {
        let env = Env::new();
        let workdir = env.dir("root/project");
        let (id, path) = env.place("transcripts/parallel.jsonl", &workdir);
        let items = env.items(&workdir, &id);
        let tags = tags(&items);
        assert_eq!(
            tags[1..5],
            [
                "call Read",
                "call Read",
                "result error=false",
                "result error=false"
            ]
            .map(str::to_owned)[..],
            "{tags:#?}"
        );
        assert!(tags[5].starts_with("text "), "{tags:#?}");
        assert_eq!(tags.len(), 6);
        let agent: Vec<AdapterEvent> = items
            .into_iter()
            .filter_map(|item| match item {
                HistoryItem::Agent { event, .. } => Some(event),
                HistoryItem::User { .. } => None,
            })
            .collect();
        // Each result names one of the calls, a different one each.
        let id_of = |event: &AdapterEvent| match event {
            AdapterEvent::ToolCall { call, .. } | AdapterEvent::ToolResult { call, .. } => {
                call.clone().unwrap()
            }
            other => panic!("{other:?}"),
        };
        assert_eq!(
            [id_of(&agent[0]), id_of(&agent[1])],
            [id_of(&agent[2]), id_of(&agent[3])]
        );
        assert_eq!(agent, live_events(&fs::read_to_string(path).unwrap()));
        // The recorded shape: the first result is parented to its own call's
        // line, off the chain the leaf walks.
        let text = fixture("transcripts/parallel.jsonl");
        let lines: Vec<Value> = text
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        let results: Vec<&Value> = lines.iter().filter(|l| results_only(l)).collect();
        assert_eq!(results.len(), 2);
        assert_ne!(results[0]["parentUuid"], results[1]["parentUuid"]);
    }

    #[test]
    fn a_symlinked_project_dir_or_transcript_is_not_read() {
        let env = Env::new();
        let workdir = env.dir("root/project");
        let dir = project_dir(workdir.to_str().unwrap());
        let projects = env.base.join("projects");
        let id = uuid();
        env.write(
            &fixture("transcripts/two_turn.jsonl"),
            &workdir,
            "elsewhere",
            &id,
        );
        symlink(projects.join("elsewhere"), projects.join(&dir)).unwrap();
        assert!(env.ids(10).is_empty());
        assert_eq!(env.history(&workdir, &id), HistoryRead::NotFound);
        // A real dir holding a symlinked transcript.
        fs::remove_file(projects.join(&dir)).unwrap();
        fs::create_dir(projects.join(&dir)).unwrap();
        let file = projects.join(&dir).join(format!("{id}.jsonl"));
        symlink(
            projects.join("elsewhere").join(format!("{id}.jsonl")),
            &file,
        )
        .unwrap();
        assert!(env.ids(10).is_empty());
        assert_eq!(env.history(&workdir, &id), HistoryRead::NotFound);
        // Positive control: the same file, really there.
        fs::remove_file(&file).unwrap();
        fs::rename(
            projects.join("elsewhere").join(format!("{id}.jsonl")),
            &file,
        )
        .unwrap();
        assert_eq!(env.ids(10), std::slice::from_ref(&id));
        assert!(matches!(
            env.history(&workdir, &id),
            HistoryRead::Read { .. }
        ));
    }

    #[test]
    fn compaction_is_walked_across_its_boundary() {
        let env = Env::new();
        let workdir = env.dir("root/project");
        let (id, _) = env.place("transcripts/compact.jsonl", &workdir);
        let tags = tags(&env.items(&workdir, &id));
        // The summary (`isCompactSummary`) and the caveat (`isMeta`) are left
        // out; the command's own lines stay verbatim.
        assert_eq!(tags.len(), 6, "{tags:#?}");
        assert_eq!(tags[0], "user remember the word PELICAN and say ok");
        assert_eq!(tags[1], "text ok");
        assert!(tags[2].starts_with("user <command-name>/compact</command-name>"));
        assert_eq!(
            tags[3],
            "user <local-command-stdout>Compacted (ctrl+o to see full summary)</local-command-stdout>"
        );
        assert_eq!(tags[4], "user which word did I ask you to remember?");
        assert_eq!(tags[5], "text PELICAN");
        // The boundary is what joins the halves: its parent is null.
        assert!(
            fixture("transcripts/compact.jsonl")
                .contains(r#""parentUuid":null,"logicalParentUuid":"#)
        );
    }

    /// two_turn with an abandoned prompt (a rewind) before its second
    /// prompt, built here: a sibling of the second prompt, so off the
    /// branch that ends the file.
    fn with_abandoned_branch(missing_parent: bool) -> String {
        let text = fixture("transcripts/two_turn.jsonl");
        let mut out = String::new();
        for line in text.lines() {
            let value: Value = serde_json::from_str(line).unwrap();
            if value["message"]["content"] == "what did I just ask?" {
                let mut abandoned = value.clone();
                abandoned["uuid"] = json!(uuid());
                abandoned["message"]["content"] = json!("abandoned");
                out.push_str(&abandoned.to_string());
                out.push('\n');
            }
            let mut value = value;
            if missing_parent && value["type"] == "assistant" {
                value["parentUuid"] = json!(uuid());
            }
            out.push_str(&value.to_string());
            out.push('\n');
        }
        out
    }

    #[test]
    fn the_active_branch_is_read_and_file_order_where_a_parent_is_missing() {
        let env = Env::new();
        let workdir = env.dir("root/project");
        let dir = project_dir(workdir.to_str().unwrap());
        let id = uuid();
        env.write(&with_abandoned_branch(false), &workdir, &dir, &id);
        let tags_read = tags(&env.items(&workdir, &id));
        assert_eq!(tags_read.len(), 4);
        assert!(!tags_read.contains(&"user abandoned".to_owned()));

        env.write(&with_abandoned_branch(true), &workdir, &dir, &id);
        let tags_read = tags(&env.items(&workdir, &id));
        assert_eq!(tags_read.len(), 5);
        assert_eq!(tags_read[2], "user abandoned");
    }

    #[test]
    fn a_listing_has_title_first_prompt_count_and_last_activity() {
        let env = Env::new();
        let workdir = env.dir("root/project");
        let (id, path) = env.place("transcripts/two_turn.jsonl", &workdir);
        let listed = env.list(10);
        assert!(!listed.truncated);
        let [c] = &listed.conversations[..] else {
            panic!("{listed:?}");
        };
        assert_eq!(c.id, id);
        assert_eq!(c.workdir, workdir.to_str().unwrap());
        assert_eq!(c.title, None);
        assert_eq!(c.first_prompt.as_deref(), Some("say hi"));
        assert_eq!(c.messages, 4);
        assert_eq!(c.session, None);
        // The last line with a time: the second answer.
        let last = fixture("transcripts/two_turn.jsonl")
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter_map(|l| timestamp(&l))
            .next_back()
            .unwrap();
        assert_eq!(c.last_active, last);

        // A title line, as the interactive CLI writes them (`-p` writes
        // none): the last one wins.
        let mut text = fs::read_to_string(&path).unwrap();
        for title in ["First title", "Say hi"] {
            text.push_str(
                &json!({ "type": "ai-title", "aiTitle": title, "sessionId": id }).to_string(),
            );
            text.push('\n');
        }
        fs::write(&path, text).unwrap();
        assert_eq!(
            env.list(10).conversations[0].title.as_deref(),
            Some("Say hi")
        );
    }

    #[test]
    fn a_truncated_last_line_is_skipped() {
        let env = Env::new();
        let workdir = env.dir("root/project");
        let (id, path) = env.place("transcripts/two_turn.jsonl", &workdir);
        let whole = env.items(&workdir, &id);
        let mut text = fs::read_to_string(&path).unwrap();
        text.push_str(
            r#"{"parentUuid":"b8d4f918","isSidechain":false,"type":"user","message":{"role":"us"#,
        );
        fs::write(&path, text).unwrap();
        assert_eq!(env.items(&workdir, &id), whole);
        assert_eq!(env.list(10).conversations[0].messages, 4);
    }

    #[test]
    fn only_canonical_workdirs_under_a_root_are_listed() {
        let env = Env::new();
        let inside = env.dir("root/inside");
        let outside = env.dir("outside");
        let (kept, _) = env.place("transcripts/two_turn.jsonl", &inside);
        // A cwd outside every root (one filed under an allowed workdir's
        // name: a_colliding_dir_name_from_outside_the_roots_is_skipped).
        let id = uuid();
        env.write(
            &fixture("transcripts/two_turn.jsonl"),
            &outside,
            &project_dir(outside.to_str().unwrap()),
            &id,
        );
        // A cwd through a symlink inside the root.
        symlink(&inside, env.root.join("link")).unwrap();
        let link = env.root.join("link");
        let linked = uuid();
        env.write(
            &fixture("transcripts/two_turn.jsonl"),
            &link,
            &project_dir(link.to_str().unwrap()),
            &linked,
        );
        assert_eq!(env.ids(10), std::slice::from_ref(&kept));

        // A symlinked root: conversations are listed by their canonical
        // cwd; one whose cwd runs through the root's link is not.
        symlink(&env.root, env.base.join("root-link")).unwrap();
        let via_root_link = env.base.join("root-link/inside");
        let through = uuid();
        env.write(
            &fixture("transcripts/two_turn.jsonl"),
            &via_root_link,
            &project_dir(via_root_link.to_str().unwrap()),
            &through,
        );
        let listed = env
            .source
            .list(&[env.base.join("root-link")], &HashMap::new(), 10, later())
            .unwrap();
        let ids: Vec<String> = listed.conversations.into_iter().map(|c| c.id).collect();
        assert_eq!(ids, [kept]);
    }

    #[test]
    fn a_dir_name_that_is_not_the_cwds_is_skipped() {
        let env = Env::new();
        let a = env.dir("root/a");
        let b = env.dir("root/b");
        let id = uuid();
        env.write(
            &fixture("transcripts/two_turn.jsonl"),
            &a,
            &project_dir(b.to_str().unwrap()),
            &id,
        );
        assert!(env.ids(10).is_empty());
        assert_eq!(env.history(&b, &id), HistoryRead::NotFound);
        assert_eq!(env.history(&a, &id), HistoryRead::NotFound);
        // Positive control: filed under its own cwd's name.
        env.write(
            &fixture("transcripts/two_turn.jsonl"),
            &a,
            &project_dir(a.to_str().unwrap()),
            &id,
        );
        assert_eq!(env.ids(10), std::slice::from_ref(&id));
        assert!(matches!(env.history(&a, &id), HistoryRead::Read { .. }));
    }

    #[test]
    fn a_colliding_dir_name_from_outside_the_roots_is_skipped() {
        let env = Env::new();
        // `<base>/root/r/x` (allowed) and `<base>/root-r-x` (outside the
        // root) share one project dir name.
        let allowed = env.dir("root/r/x");
        let outside = env.dir("root-r-x");
        let dir = project_dir(allowed.to_str().unwrap());
        assert_eq!(dir, project_dir(outside.to_str().unwrap()));
        let foreign = uuid();
        env.write(
            &fixture("transcripts/two_turn.jsonl"),
            &outside,
            &dir,
            &foreign,
        );
        assert!(env.ids(10).is_empty());
        assert_eq!(env.history(&allowed, &foreign), HistoryRead::NotFound);
        // Positive control: the allowed workdir's own conversation in the
        // same dir.
        let own = uuid();
        env.write(&fixture("transcripts/two_turn.jsonl"), &allowed, &dir, &own);
        assert_eq!(env.ids(10), std::slice::from_ref(&own));
        assert!(matches!(
            env.history(&allowed, &own),
            HistoryRead::Read { .. }
        ));
    }

    #[test]
    fn an_id_that_is_not_a_uuid_is_not_found_without_opening_a_file() {
        let env = Env::new();
        let workdir = env.dir("root/project");
        let (id, _) = env.place("transcripts/two_turn.jsonl", &workdir);
        // A transcript where `<dir>/../x.jsonl` would land.
        env.write(&fixture("transcripts/two_turn.jsonl"), &workdir, "", "x");
        assert!(env.base.join("projects/x.jsonl").is_file());
        let upper = id.to_uppercase();
        let simple = id.replace('-', "");
        let braced = format!("{{{id}}}");
        let urn = format!("urn:uuid:{id}");
        for bad in [
            "../x",
            "x",
            "",
            upper.as_str(),
            simple.as_str(),
            braced.as_str(),
            urn.as_str(),
        ] {
            assert_eq!(env.history(&workdir, bad), HistoryRead::NotFound, "{bad}");
        }
        assert_eq!(env.source.files_read(), 0);
        // Positive control.
        assert!(matches!(
            env.history(&workdir, &id),
            HistoryRead::Read { .. }
        ));
        assert_eq!(env.source.files_read(), 1);
    }

    #[test]
    fn history_of_a_missing_large_or_slow_transcript() {
        let env = Env::new();
        let workdir = env.dir("root/project");
        let (id, path) = env.place("transcripts/two_turn.jsonl", &workdir);
        assert_eq!(env.history(&workdir, &uuid()), HistoryRead::NotFound);
        assert_eq!(
            env.source.history(&workdir, &id, Instant::now()),
            HistoryRead::Failed("timed out".to_owned())
        );
        // Sparse: nothing is written, and nothing is read.
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_TRANSCRIPT_BYTES + 1)
            .unwrap();
        let read = env.source.files_read();
        assert_eq!(
            env.history(&workdir, &id),
            HistoryRead::Failed("too large".to_owned())
        );
        assert!(env.ids(10).is_empty());
        assert_eq!(env.source.files_read(), read);
    }

    #[test]
    fn own_conversations_carry_their_session() {
        let env = Env::new();
        let workdir = env.dir("root/project");
        let (id, _) = env.place("transcripts/two_turn.jsonl", &workdir);
        let (other, _) = env.place("transcripts/tools.jsonl", &workdir);
        let session = SessionId::new();
        let own = HashMap::from([(id.clone(), session)]);
        let listed = env.source.list(&env.roots(), &own, 10, later()).unwrap();
        let sessions: HashMap<String, Option<SessionId>> = listed
            .conversations
            .into_iter()
            .map(|c| (c.id, c.session))
            .collect();
        assert_eq!(
            sessions,
            HashMap::from([(id, Some(session)), (other, None)])
        );
    }

    #[test]
    fn own_conversations_are_read_from_the_contexts_handles() {
        let tmp = tempfile::tempdir().unwrap();
        let db = crate::db::Db::open(&tmp.path().join("db.sqlite")).unwrap();
        let session = SessionId::new();
        let moved_to = SessionId::new();
        let conn = db.conn();
        for id in [session, moved_to] {
            conn.execute(
                "INSERT INTO sessions (id, workdir, current_context) VALUES (?1, '/w', ?1)",
                [id.0.to_string()],
            )
            .unwrap();
        }
        let cli = Uuid::new_v4();
        let contexts = [
            (Some(protocol::resume_handle(cli).0), false),
            (Some(json!({ "other": "adapter" })), false),
            (None, false),
            // A context moved to another session is never started again.
            (Some(protocol::resume_handle(Uuid::new_v4()).0), true),
        ];
        for (handle, moved) in contexts {
            let context = skywave_proto::ContextId::new();
            conn.execute(
                "INSERT INTO contexts (id, session_id) VALUES (?1, ?2)",
                [context.0.to_string(), session.0.to_string()],
            )
            .unwrap();
            if moved {
                conn.execute(
                    "UPDATE contexts SET moved_to = ?2 WHERE id = ?1",
                    [context.0.to_string(), moved_to.0.to_string()],
                )
                .unwrap();
            }
            if let Some(handle) = handle {
                crate::agent::store_resume_handle(
                    &db,
                    context,
                    &crate::agent::ResumeHandle(handle),
                )
                .unwrap();
            }
        }
        assert_eq!(
            super::super::own_conversations(db.conn()).unwrap(),
            HashMap::from([(cli.to_string(), session)])
        );
    }

    /// `n` copies of two_turn in `workdir`, mtimes 0..n seconds apart;
    /// their ids, oldest first.
    fn copies(env: &Env, workdir: &Path, n: u64) -> Vec<String> {
        let dir = project_dir(workdir.to_str().unwrap());
        (0..n)
            .map(|i| {
                let id = uuid();
                let path = env.write(&fixture("transcripts/two_turn.jsonl"), workdir, &dir, &id);
                set_mtime(&path, i);
                id
            })
            .collect()
    }

    #[test]
    fn newest_first_up_to_the_cap_reading_about_that_many_files() {
        let env = Env::new();
        let workdir = env.dir("root/project");
        let mut ids = copies(&env, &workdir, 5);
        ids.reverse();
        let listed = env.list(3);
        assert!(listed.truncated);
        assert_eq!(
            listed
                .conversations
                .iter()
                .map(|c| c.id.clone())
                .collect::<Vec<_>>(),
            ids[..3]
        );
        assert_eq!(env.source.files_read(), 3);

        let listed = env.list(5);
        assert!(!listed.truncated, "nothing is left");
        assert_eq!(listed.conversations.len(), 5);
        assert_eq!(
            env.source.files_read(),
            5,
            "the first three came from the cache"
        );

        // Past the deadline: what it has, truncated.
        let listed = env
            .source
            .list(&env.roots(), &HashMap::new(), 5, Instant::now())
            .unwrap();
        assert!(listed.truncated);
        assert!(listed.conversations.is_empty());
    }

    #[test]
    fn the_cache_is_refreshed_when_a_file_grows() {
        let env = Env::new();
        let workdir = env.dir("root/project");
        let (id, path) = env.place("transcripts/two_turn.jsonl", &workdir);
        set_mtime(&path, 0);
        assert_eq!(env.list(10).conversations[0].messages, 4);
        assert_eq!(env.list(10).conversations[0].messages, 4);
        assert_eq!(env.source.files_read(), 1);

        // The conversation goes on in the terminal.
        let mut text = fs::read_to_string(&path).unwrap();
        let leaf = text
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .rfind(|l| l["type"] == "assistant")
            .unwrap()["uuid"]
            .clone();
        text.push_str(
            &json!({
                "type": "user", "isSidechain": false, "uuid": uuid(),
                "parentUuid": leaf, "cwd": workdir, "sessionId": id,
                "message": { "role": "user", "content": "one more" },
            })
            .to_string(),
        );
        text.push('\n');
        fs::write(&path, text).unwrap();
        set_mtime(&path, 1);
        assert_eq!(env.list(10).conversations[0].messages, 5);
        assert_eq!(env.source.files_read(), 2);
    }

    #[test]
    fn missing_projects_dir_lists_nothing() {
        let source = ClaudeTranscripts::new(PathBuf::from("/nonexistent/skywave/projects"));
        let listed = source
            .list(&[PathBuf::from("/")], &HashMap::new(), 10, later())
            .unwrap();
        assert_eq!(listed, Listed::default());
    }
}
