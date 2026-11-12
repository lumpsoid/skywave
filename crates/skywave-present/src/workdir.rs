//! How a session's workdir reads: what kind each diff line is, a changed
//! file's mark and counts, a directory's entries in order, when a snapshot
//! was taken, and the wording for every empty, refused or failed read.
//! Plain strings, no styling; a head colours and lays them out.

use chrono::{DateTime, Utc};
use skywave_core::{
    ChangeSet, ChangedFile, ChangesResult, DiffBody, DirEntry, EntryKind, FileBody, FileChange,
    FileResult, LinkTarget, ListResult, WorkdirFetch,
};
use skywave_proto::{Commit, CommitLog, LogResult, limits};

use crate::format;

/// What one line of a unified diff is, for colouring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLine {
    /// A file's header: `diff --git`, `index`, the mode, rename and
    /// similarity lines, `Binary files … differ`, and `---`/`+++`.
    FileHeader,
    /// A hunk header: `@@ -1,3 +1,4 @@`.
    Hunk,
    Added,
    Removed,
    /// An unchanged line: a leading space, or an empty line.
    Context,
    /// Anything else: `\ No newline at end of file`, and lines no rule
    /// above names.
    Meta,
}

/// Header lines that start neither with a sign nor with a space.
const HEADER_PREFIXES: &[&str] = &[
    "diff ",
    "index ",
    "new file mode",
    "deleted file mode",
    "old mode",
    "new mode",
    "similarity index",
    "dissimilarity index",
    "rename from",
    "rename to",
    "copy from",
    "copy to",
    "Binary files",
];

/// Each line of a whole unified diff with its kind. Heads use this for a
/// diff they hold whole; [`diff_line`] guesses from one line.
///
/// It reads in two modes. It starts in header mode, and a `diff --git `
/// line goes back to it anywhere: no content line starts with `d`. In
/// header mode, `@@` starts a hunk and every other line is a header line
/// (`Meta` if no header rule names it). So a diff with no `diff --git`
/// line, such as the server's diff of an untracked file (`--- /dev/null`,
/// `+++ b/<path>`, `@@`), reads right. In hunk mode a line's first
/// character decides: `@@` is a hunk header, `+` added, `-` removed, a
/// space or an empty line context, and anything else (`\ No newline at
/// end of file`) `Meta`. So a removed `---x` or an added `+++y` is content.
pub fn diff_lines(text: &str) -> impl Iterator<Item = (DiffLine, &str)> {
    let mut in_hunk = false;
    text.lines().map(move |line| {
        if line.starts_with("diff --git ") {
            in_hunk = false;
        }
        let kind = if line.starts_with("@@") {
            in_hunk = true;
            DiffLine::Hunk
        } else if !in_hunk {
            header_line(line)
        } else if line.starts_with('+') {
            DiffLine::Added
        } else if line.starts_with('-') {
            DiffLine::Removed
        } else if line.is_empty() || line.starts_with(' ') {
            DiffLine::Context
        } else {
            DiffLine::Meta
        };
        (kind, line)
    })
}

/// A line outside any hunk: a header line, or `Meta`.
fn header_line(line: &str) -> DiffLine {
    if line.starts_with("+++")
        || line.starts_with("---")
        || HEADER_PREFIXES.iter().any(|p| line.starts_with(p))
    {
        DiffLine::FileHeader
    } else {
        DiffLine::Meta
    }
}

/// What `line` is, from the line alone; for a whole diff use
/// [`diff_lines`]. `---`/`+++` are headers before they are signs, as the
/// timeline reads a diff, so a removed line whose text starts with `--`
/// (or an added one with `++`) reads as a header: a line-by-line rule
/// cannot tell them apart. A submodule's `-Subproject commit …`/
/// `+Subproject commit …` lines are ordinary removed and added lines.
pub fn diff_line(line: &str) -> DiffLine {
    if line.starts_with("+++") || line.starts_with("---") {
        DiffLine::FileHeader
    } else if line.starts_with('+') {
        DiffLine::Added
    } else if line.starts_with('-') {
        DiffLine::Removed
    } else if line.is_empty() || line.starts_with(' ') {
        DiffLine::Context
    } else if line.starts_with("@@") {
        DiffLine::Hunk
    } else {
        header_line(line)
    }
}

/// How a file changed, in words: "modified", "untracked", ….
pub fn change_label(change: FileChange) -> &'static str {
    match change {
        FileChange::Added => "added",
        FileChange::Modified => "modified",
        FileChange::Deleted => "deleted",
        FileChange::Renamed => "renamed",
        FileChange::TypeChanged => "type changed",
        FileChange::Untracked => "untracked",
        FileChange::Conflicted => "conflicted",
    }
}

/// How a file changed, in one letter, as `git status --short` marks it:
/// M A D R T ? U.
pub fn change_mark(change: FileChange) -> char {
    match change {
        FileChange::Added => 'A',
        FileChange::Modified => 'M',
        FileChange::Deleted => 'D',
        FileChange::Renamed => 'R',
        FileChange::TypeChanged => 'T',
        FileChange::Untracked => '?',
        FileChange::Conflicted => 'U',
    }
}

/// A changed file's line counts: "+12 −3", or "binary" for a file git
/// found binary. Binary is read off the diff, not off missing counts: an
/// untracked file too large to diff, or left out, has no counts either,
/// and gets `None` (its diff wording says why, [`diff_wording`]).
pub fn counts_label(file: &ChangedFile) -> Option<String> {
    if file.diff == DiffBody::Binary {
        return Some("binary".to_owned());
    }
    match (file.added, file.removed) {
        (Some(added), Some(removed)) => Some(format!("+{added} \u{2212}{removed}")),
        _ => None,
    }
}

/// A directory's entries as shown: directories (and links to one) first,
/// then by name, byte by byte.
pub fn sorted_entries(entries: &[DirEntry]) -> Vec<&DirEntry> {
    let mut sorted: Vec<&DirEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| {
        is_dir(b.kind)
            .cmp(&is_dir(a.kind))
            .then_with(|| a.name.cmp(&b.name))
    });
    sorted
}

/// Whether an entry opens as a directory.
fn is_dir(kind: EntryKind) -> bool {
    matches!(
        kind,
        EntryKind::Dir
            | EntryKind::Link {
                to: LinkTarget::Dir
            }
    )
}

/// An entry's name, with a `/` after a directory's.
pub fn entry_name(entry: &DirEntry) -> String {
    match entry.kind {
        EntryKind::Dir => format!("{}/", entry.name),
        _ => entry.name.clone(),
    }
}

/// What an entry's line shows after its name: a file's size ("812 B"),
/// where a link leads ("→ directory", "→ file", "→ outside the workdir",
/// "→ missing"), or that it is a special file. `None` for a directory.
pub fn entry_detail(entry: &DirEntry) -> Option<String> {
    match entry.kind {
        EntryKind::Dir => None,
        EntryKind::File => entry.size.map(format::bytes),
        EntryKind::Other => Some("special file".to_owned()),
        EntryKind::Link { to } => Some(
            match to {
                LinkTarget::Dir => "\u{2192} directory",
                LinkTarget::File => "\u{2192} file",
                LinkTarget::Outside => "\u{2192} outside the workdir",
                LinkTarget::Dangling => "\u{2192} missing",
            }
            .to_owned(),
        ),
    }
}

/// An entry's line: its [name](entry_name) and, two spaces on, its
/// [detail](entry_detail): "main.rs  812 B", "src/".
pub fn entry_label(entry: &DirEntry) -> String {
    match entry_detail(entry) {
        Some(detail) => format!("{}  {detail}", entry_name(entry)),
        None => entry_name(entry),
    }
}

/// When a snapshot was taken, by the server's clock: `taken 14:02:11`, with
/// the date in front (`taken 2026-11-02 14:02:11`) when `at` is not on
/// `now`'s day. Fixed text: no running age, as
/// [`delivery_label`](format::delivery_label).
pub fn taken_label(at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    if at.date_naive() == now.date_naive() {
        format!("taken {}", at.format("%H:%M:%S"))
    } else {
        format!("taken {}", at.format("%Y-%m-%d %H:%M:%S"))
    }
}

/// What a pane shows before its first answer.
pub const NOT_FETCHED: &str = "Not fetched yet; refresh when online.";

const NOT_GIT: &str = "Not a git repository. Browse the files under Files.";
const UNKNOWN_SESSION: &str = "The server no longer knows this session.";
const WORKDIR_GONE: &str = "The session's working directory no longer exists.";
const NOT_ALLOWED: &str = "Outside the session's working directory: not shown.";
const NOT_FOUND: &str = "Not found: it may have been moved or deleted.";

/// The state of a pane's read, shown beside its stored answer: "Loading…",
/// "Refresh failed: <reason>". `None` when idle.
pub fn fetch_label(fetch: &WorkdirFetch) -> Option<String> {
    match fetch {
        WorkdirFetch::Idle => None,
        WorkdirFetch::Loading => Some("Loading\u{2026}".to_owned()),
        WorkdirFetch::Failed(reason) => Some(format!("Refresh failed: {reason}")),
    }
}

/// Where a snapshot stands: "main @ 1a2b3c4", "detached @ 1a2b3c4",
/// "main (no commits yet)".
pub fn head_label(set: &ChangeSet) -> String {
    let branch = set.branch.as_deref().unwrap_or("detached");
    match &set.head {
        Some(head) => format!("{branch} @ {head}"),
        None => format!("{branch} (no commits yet)"),
    }
}

/// What the changes pane shows instead of a file list: why there is none,
/// or that the tree is clean. `None` when there are files to list.
pub fn changes_wording(result: &ChangesResult) -> Option<String> {
    let text = match result {
        ChangesResult::Changes(set) if set.files.is_empty() && set.total == 0 => match &set.head {
            Some(head) => format!("No changes since {head}."),
            None => "No commits and no files yet.".to_owned(),
        },
        ChangesResult::Changes(_) => return None,
        ChangesResult::NotGit => NOT_GIT.to_owned(),
        ChangesResult::Failed { reason } => format!("git failed: {reason}"),
        ChangesResult::UnknownSession => UNKNOWN_SESSION.to_owned(),
        ChangesResult::WorkdirGone => WORKDIR_GONE.to_owned(),
    };
    Some(text)
}

/// That a snapshot lists only some of its files: "Showing 500 of 1,234
/// changed files." `None` when it lists them all.
pub fn changes_capped(set: &ChangeSet) -> Option<String> {
    let shown = set.files.len();
    let total = usize::try_from(set.total).unwrap_or(usize::MAX);
    (total > shown).then(|| {
        format!(
            "Showing {} of {} changed files.",
            thousands(shown as u64),
            thousands(u64::from(set.total))
        )
    })
}

/// The commits pane's header: the branch, or `detached` when `HEAD` is.
pub fn log_label(log: &CommitLog) -> String {
    log.branch.as_deref().unwrap_or("detached").to_owned()
}

/// What the commits pane shows instead of rows: why there are none, or that
/// the branch has no commits yet. `None` when there are rows to list.
pub fn log_wording(result: &LogResult) -> Option<String> {
    let text = match result {
        LogResult::Log(log) if log.commits.is_empty() => "No commits yet.".to_owned(),
        LogResult::Log(_) => return None,
        LogResult::NotGit => NOT_GIT.to_owned(),
        LogResult::Failed { reason } => format!("git failed: {reason}"),
        LogResult::UnknownSession => UNKNOWN_SESSION.to_owned(),
        LogResult::WorkdirGone => WORKDIR_GONE.to_owned(),
    };
    Some(text)
}

/// That a log stops short of the older commits: "Showing the newest 50
/// commits." `None` when `more` is off, so the log is the whole history.
pub fn log_capped(log: &CommitLog) -> Option<String> {
    log.more.then(|| {
        format!(
            "Showing the newest {} commits.",
            thousands(log.commits.len() as u64)
        )
    })
}

/// A commit's author date as an age: "3 h ago". `None` when git gave no
/// usable date.
pub fn commit_time_label(commit: &Commit, now: DateTime<Utc>) -> Option<String> {
    commit.time.map(|t| format::ago(t, now))
}

/// What the diff pane shows instead of a diff, for `file` as the stored
/// changes have it: why there is no text diff, or that the file left the
/// changes. `None` when the diff is a blob to show.
pub fn diff_wording(file: Option<&ChangedFile>) -> Option<String> {
    let Some(file) = file else {
        return Some(
            "This file is no longer in the changes. Refresh to see the current state.".to_owned(),
        );
    };
    match file.diff {
        DiffBody::Blob(_) => None,
        DiffBody::Binary => Some("Binary file: there is no text diff.".to_owned()),
        DiffBody::TooLarge { bytes } => Some(format!(
            "Diff too large to show: {}, past {}.",
            format::bytes(bytes),
            format::bytes(limits::WORKDIR_DIFF_MAX_BYTES)
        )),
        DiffBody::Omitted => Some(format!(
            "Diff not included: this snapshot's diffs reached {}.",
            format::bytes(limits::WORKDIR_DIFFS_MAX_BYTES)
        )),
    }
}

/// What the files pane shows instead of entries: why there are none, or
/// that the directory is empty. `None` when there are entries to list.
pub fn list_wording(result: &ListResult) -> Option<String> {
    let text = match result {
        ListResult::Listed { entries, .. } if entries.is_empty() => "Empty directory.",
        ListResult::Listed { .. } => return None,
        ListResult::NotADirectory => "Not a directory.",
        ListResult::UnknownSession => UNKNOWN_SESSION,
        ListResult::WorkdirGone => WORKDIR_GONE,
        ListResult::NotAllowed => NOT_ALLOWED,
        ListResult::NotFound => NOT_FOUND,
    };
    Some(text.to_owned())
}

/// That a listing stops short: "Showing the first 1,000 entries." `None`
/// for a whole one, or for no listing.
pub fn list_truncated(result: &ListResult) -> Option<String> {
    match result {
        ListResult::Listed {
            entries,
            truncated: true,
            ..
        } => Some(format!(
            "Showing the first {} entries.",
            thousands(entries.len() as u64)
        )),
        _ => None,
    }
}

/// What the file pane shows instead of content: why it is not shown.
/// `None` when the content is a blob to show.
pub fn file_wording(result: &FileResult) -> Option<String> {
    let text = match result {
        FileResult::File {
            content: FileBody::Blob(_),
            ..
        } => return None,
        FileResult::File {
            content: FileBody::Binary,
            size,
            ..
        } => format!("Binary file ({}): not shown.", format::bytes(*size)),
        FileResult::File {
            content: FileBody::TooLarge,
            size,
            ..
        } => format!(
            "Too large to show: {}, past {}.",
            format::bytes(*size),
            format::bytes(limits::WORKDIR_FILE_MAX_BYTES)
        ),
        FileResult::IsADirectory => "A directory: open it under Files.".to_owned(),
        FileResult::NotAFile => "A FIFO, socket or device: not opened.".to_owned(),
        FileResult::UnknownSession => UNKNOWN_SESSION.to_owned(),
        FileResult::WorkdirGone => WORKDIR_GONE.to_owned(),
        FileResult::NotAllowed => NOT_ALLOWED.to_owned(),
        FileResult::NotFound => NOT_FOUND.to_owned(),
    };
    Some(text)
}

/// A count with thousands separated: "500", "1,234".
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn at(d: u32, h: u32, m: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, d, h, m, s).unwrap()
    }

    fn changed(change: FileChange, counts: Option<(u32, u32)>, diff: DiffBody) -> ChangedFile {
        ChangedFile {
            path: "f".into(),
            from: None,
            change,
            added: counts.map(|c| c.0),
            removed: counts.map(|c| c.1),
            diff,
        }
    }

    fn set(head: Option<&str>, branch: Option<&str>, files: usize, total: u32) -> ChangeSet {
        ChangeSet {
            at: at(3, 14, 2, 11),
            branch: branch.map(str::to_owned),
            head: head.map(str::to_owned),
            files: (0..files)
                .map(|_| changed(FileChange::Modified, Some((1, 1)), DiffBody::Omitted))
                .collect(),
            total,
        }
    }

    fn entry(name: &str, kind: EntryKind, size: Option<u64>) -> DirEntry {
        DirEntry {
            name: name.into(),
            kind,
            size,
        }
    }

    #[test]
    fn diff_line_kinds() {
        let cases = [
            ("diff --git a/f b/f", DiffLine::FileHeader),
            ("index 1a2b3c4..5d6e7f8 100644", DiffLine::FileHeader),
            ("new file mode 100644", DiffLine::FileHeader),
            ("deleted file mode 100644", DiffLine::FileHeader),
            ("old mode 100644", DiffLine::FileHeader),
            ("new mode 100755", DiffLine::FileHeader),
            ("similarity index 90%", DiffLine::FileHeader),
            ("rename from a", DiffLine::FileHeader),
            ("rename to b", DiffLine::FileHeader),
            ("Binary files a/x and b/x differ", DiffLine::FileHeader),
            ("--- a/f", DiffLine::FileHeader),
            ("+++ b/f", DiffLine::FileHeader),
            ("--- /dev/null", DiffLine::FileHeader),
            ("-old", DiffLine::Removed),
            ("+new", DiffLine::Added),
            ("-", DiffLine::Removed),
            ("+", DiffLine::Added),
            ("-Subproject commit 1a2b", DiffLine::Removed),
            ("+Subproject commit 3c4d", DiffLine::Added),
            ("@@ -1,3 +1,4 @@ fn main()", DiffLine::Hunk),
            (" same", DiffLine::Context),
            ("", DiffLine::Context),
            // A context line whose text looks like a header stays context.
            (" index 1", DiffLine::Context),
            (" @@ x", DiffLine::Context),
            ("\\ No newline at end of file", DiffLine::Meta),
            ("something else", DiffLine::Meta),
        ];
        for (line, kind) in cases {
            assert_eq!(diff_line(line), kind, "{line:?}");
        }
    }

    fn kinds(text: &str) -> Vec<(DiffLine, &str)> {
        diff_lines(text).collect()
    }

    #[test]
    fn diff_lines_read_dashes_in_a_hunk_as_content() {
        use DiffLine::*;
        let text = "diff --git a/f b/f\nindex 1..2 100644\n--- a/f\n+++ b/f\n\
                    @@ -1,2 +1,2 @@\n---x\n+++y\n same\n\n\\ No newline at end of file\n";
        assert_eq!(
            kinds(text),
            [
                (FileHeader, "diff --git a/f b/f"),
                (FileHeader, "index 1..2 100644"),
                (FileHeader, "--- a/f"),
                (FileHeader, "+++ b/f"),
                (Hunk, "@@ -1,2 +1,2 @@"),
                (Removed, "---x"),
                (Added, "+++y"),
                (Context, " same"),
                (Context, ""),
                (Meta, "\\ No newline at end of file"),
            ]
        );
        // The single-line rule cannot tell: the control for the above.
        assert_eq!(diff_line("---x"), FileHeader);
        assert_eq!(diff_line("+++y"), FileHeader);
    }

    #[test]
    fn diff_lines_restart_headers_at_each_section() {
        use DiffLine::*;
        // A type change: the old file deleted, the new one added.
        let text = "diff --git a/l b/l\ndeleted file mode 120000\nindex 1..0\n--- a/l\n\
                    +++ /dev/null\n@@ -1 +0,0 @@\n-target\n\\ No newline at end of file\n\
                    diff --git a/l b/l\nnew file mode 100644\nindex 0..2\n--- /dev/null\n\
                    +++ b/l\n@@ -0,0 +1 @@\n+text\n";
        let got: Vec<DiffLine> = diff_lines(text).map(|(kind, _)| kind).collect();
        assert_eq!(
            got,
            [
                FileHeader, FileHeader, FileHeader, FileHeader, FileHeader, Hunk, Removed, Meta,
                FileHeader, FileHeader, FileHeader, FileHeader, FileHeader, Hunk, Added,
            ]
        );
    }

    #[test]
    fn diff_lines_read_an_untracked_diff_from_header_mode() {
        use DiffLine::*;
        // As the server writes an untracked file's diff: no `diff --git`.
        let text = "--- /dev/null\n+++ b/new\n@@ -0,0 +1,2 @@\n+a\n+++b\n";
        assert_eq!(
            kinds(text),
            [
                (FileHeader, "--- /dev/null"),
                (FileHeader, "+++ b/new"),
                (Hunk, "@@ -0,0 +1,2 @@"),
                (Added, "+a"),
                (Added, "+++b"),
            ]
        );
    }

    #[test]
    fn change_labels_and_marks() {
        let cases = [
            (FileChange::Added, "added", 'A'),
            (FileChange::Modified, "modified", 'M'),
            (FileChange::Deleted, "deleted", 'D'),
            (FileChange::Renamed, "renamed", 'R'),
            (FileChange::TypeChanged, "type changed", 'T'),
            (FileChange::Untracked, "untracked", '?'),
            (FileChange::Conflicted, "conflicted", 'U'),
        ];
        for (change, label, mark) in cases {
            assert_eq!(change_label(change), label);
            assert_eq!(change_mark(change), mark);
        }
    }

    #[test]
    fn counts_read_binary_off_the_diff() {
        let modified = changed(FileChange::Modified, Some((12, 3)), DiffBody::Omitted);
        assert_eq!(counts_label(&modified).as_deref(), Some("+12 \u{2212}3"));
        let binary = changed(FileChange::Modified, None, DiffBody::Binary);
        assert_eq!(counts_label(&binary).as_deref(), Some("binary"));
        // No counts but not binary: no label, and never "binary".
        let big = changed(
            FileChange::Untracked,
            None,
            DiffBody::TooLarge { bytes: 2 << 20 },
        );
        assert_eq!(counts_label(&big), None);
        let omitted = changed(FileChange::Untracked, None, DiffBody::Omitted);
        assert_eq!(counts_label(&omitted), None);
        // A conflict that matches HEAD.
        let even = changed(FileChange::Conflicted, Some((0, 0)), DiffBody::Omitted);
        assert_eq!(counts_label(&even).as_deref(), Some("+0 \u{2212}0"));
    }

    #[test]
    fn entries_sort_directories_first_then_by_name() {
        let entries = [
            entry("b.rs", EntryKind::File, Some(1)),
            entry("zeta", EntryKind::Dir, None),
            entry("A.md", EntryKind::File, Some(1)),
            entry(
                "link",
                EntryKind::Link {
                    to: LinkTarget::Dir,
                },
                None,
            ),
            entry("alpha", EntryKind::Dir, None),
            entry(
                "away",
                EntryKind::Link {
                    to: LinkTarget::Outside,
                },
                None,
            ),
            entry("fifo", EntryKind::Other, None),
        ];
        let names: Vec<&str> = sorted_entries(&entries)
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(
            names,
            ["alpha", "link", "zeta", "A.md", "away", "b.rs", "fifo"]
        );
    }

    #[test]
    fn entry_labels() {
        let link = |to| entry("l", EntryKind::Link { to }, None);
        let cases = [
            (entry("src", EntryKind::Dir, None), "src/"),
            (entry("a.rs", EntryKind::File, Some(812)), "a.rs  812 B"),
            (
                entry("b.bin", EntryKind::File, Some(12_595)),
                "b.bin  12.3 KiB",
            ),
            (entry("odd", EntryKind::File, None), "odd"),
            (entry("p", EntryKind::Other, None), "p  special file"),
            (link(LinkTarget::Dir), "l  \u{2192} directory"),
            (link(LinkTarget::File), "l  \u{2192} file"),
            (link(LinkTarget::Outside), "l  \u{2192} outside the workdir"),
            (link(LinkTarget::Dangling), "l  \u{2192} missing"),
        ];
        for (entry, label) in cases {
            assert_eq!(entry_label(&entry), label);
        }
    }

    #[test]
    fn taken_label_adds_the_date_off_today() {
        let taken = at(3, 14, 2, 11);
        assert_eq!(taken_label(taken, at(3, 23, 59, 59)), "taken 14:02:11");
        assert_eq!(
            taken_label(taken, at(4, 0, 0, 1)),
            "taken 2026-11-03 14:02:11"
        );
    }

    #[test]
    fn fetch_labels() {
        assert_eq!(fetch_label(&WorkdirFetch::Idle), None);
        assert_eq!(
            fetch_label(&WorkdirFetch::Loading).as_deref(),
            Some("Loading\u{2026}")
        );
        assert_eq!(
            fetch_label(&WorkdirFetch::Failed(
                "server unreachable: not connected".into()
            ))
            .as_deref(),
            Some("Refresh failed: server unreachable: not connected")
        );
        assert_eq!(NOT_FETCHED, "Not fetched yet; refresh when online.");
    }

    #[test]
    fn head_labels() {
        assert_eq!(
            head_label(&set(Some("1a2b3c4"), Some("main"), 0, 0)),
            "main @ 1a2b3c4"
        );
        assert_eq!(
            head_label(&set(Some("1a2b3c4"), None, 0, 0)),
            "detached @ 1a2b3c4"
        );
        assert_eq!(
            head_label(&set(None, Some("main"), 0, 0)),
            "main (no commits yet)"
        );
    }

    #[test]
    fn changes_wordings() {
        let wording = |r: ChangesResult| changes_wording(&r);
        assert_eq!(
            wording(ChangesResult::Changes(set(Some("1a2b3c4"), None, 0, 0))).as_deref(),
            Some("No changes since 1a2b3c4.")
        );
        assert_eq!(
            wording(ChangesResult::Changes(set(None, Some("main"), 0, 0))).as_deref(),
            Some("No commits and no files yet.")
        );
        assert_eq!(
            wording(ChangesResult::Changes(set(Some("1a2b3c4"), None, 1, 1))),
            None
        );
        assert_eq!(
            wording(ChangesResult::NotGit).as_deref(),
            Some("Not a git repository. Browse the files under Files.")
        );
        assert_eq!(
            wording(ChangesResult::Failed {
                reason: "git exited 128".into()
            })
            .as_deref(),
            Some("git failed: git exited 128")
        );
        assert_eq!(
            wording(ChangesResult::UnknownSession).as_deref(),
            Some(UNKNOWN_SESSION)
        );
        assert_eq!(
            wording(ChangesResult::WorkdirGone).as_deref(),
            Some(WORKDIR_GONE)
        );
    }

    #[test]
    fn capped_changes_say_how_many() {
        assert_eq!(
            changes_capped(&set(None, None, 500, 1234)).as_deref(),
            Some("Showing 500 of 1,234 changed files.")
        );
        assert_eq!(changes_capped(&set(None, None, 3, 3)), None);
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }

    fn commit(i: usize, time: Option<DateTime<Utc>>) -> Commit {
        Commit {
            sha: format!("{i:040x}"),
            short: format!("{i:07x}"),
            author: "Ada".to_owned(),
            time,
            subject: format!("commit {i}"),
        }
    }

    fn log(branch: Option<&str>, commits: usize, more: bool) -> CommitLog {
        CommitLog {
            at: at(3, 14, 2, 11),
            branch: branch.map(str::to_owned),
            commits: (0..commits)
                .map(|i| commit(i, Some(at(3, 14, 2, 11))))
                .collect(),
            more,
        }
    }

    #[test]
    fn log_labels() {
        assert_eq!(log_label(&log(Some("main"), 0, false)), "main");
        assert_eq!(log_label(&log(None, 0, false)), "detached");
    }

    #[test]
    fn log_wordings() {
        let wording = |r: LogResult| log_wording(&r);
        assert_eq!(
            wording(LogResult::Log(log(Some("main"), 0, false))).as_deref(),
            Some("No commits yet.")
        );
        assert_eq!(wording(LogResult::Log(log(Some("main"), 1, false))), None);
        assert_eq!(
            wording(LogResult::NotGit).as_deref(),
            Some("Not a git repository. Browse the files under Files.")
        );
        assert_eq!(
            wording(LogResult::Failed {
                reason: "boom".into()
            })
            .as_deref(),
            Some("git failed: boom")
        );
        assert_eq!(
            wording(LogResult::UnknownSession).as_deref(),
            Some("The server no longer knows this session.")
        );
        assert_eq!(
            wording(LogResult::WorkdirGone).as_deref(),
            Some("The session's working directory no longer exists.")
        );
    }

    #[test]
    fn a_log_with_more_says_how_many_it_shows() {
        assert_eq!(
            log_capped(&log(Some("main"), 50, true)).as_deref(),
            Some("Showing the newest 50 commits.")
        );
        assert_eq!(log_capped(&log(Some("main"), 50, false)), None);
    }

    #[test]
    fn commit_times_read_as_an_age() {
        let now = at(3, 17, 2, 11);
        assert_eq!(
            commit_time_label(&commit(0, Some(at(3, 14, 2, 11))), now).as_deref(),
            Some("3 h ago")
        );
        assert_eq!(commit_time_label(&commit(0, None), now), None);
    }

    fn blob() -> skywave_proto::BlobRef {
        skywave_proto::BlobRef {
            id: uuid::Uuid::from_u128(5).into(),
            size: 10,
            sha256: skywave_proto::ContentHash::of(b""),
        }
    }

    #[test]
    fn diff_wordings() {
        let with = |diff| diff_wording(Some(&changed(FileChange::Modified, None, diff)));
        assert_eq!(with(DiffBody::Blob(blob())), None);
        assert_eq!(
            diff_wording(None).as_deref(),
            Some("This file is no longer in the changes. Refresh to see the current state.")
        );
        assert_eq!(
            with(DiffBody::Binary).as_deref(),
            Some("Binary file: there is no text diff.")
        );
        assert_eq!(
            with(DiffBody::TooLarge { bytes: 3 << 20 }).as_deref(),
            Some("Diff too large to show: 3.0 MiB, past 1.0 MiB.")
        );
        assert_eq!(
            with(DiffBody::Omitted).as_deref(),
            Some("Diff not included: this snapshot's diffs reached 8.0 MiB.")
        );
    }

    #[test]
    fn list_wordings() {
        let listed = |entries: Vec<DirEntry>, truncated| ListResult::Listed {
            at: at(3, 14, 2, 11),
            path: String::new(),
            entries,
            truncated,
        };
        let one = vec![entry("a", EntryKind::File, Some(1))];
        assert_eq!(
            list_wording(&listed(vec![], false)).as_deref(),
            Some("Empty directory.")
        );
        assert_eq!(list_wording(&listed(one.clone(), false)), None);
        let cases = [
            (ListResult::NotADirectory, "Not a directory."),
            (ListResult::UnknownSession, UNKNOWN_SESSION),
            (ListResult::WorkdirGone, WORKDIR_GONE),
            (ListResult::NotAllowed, NOT_ALLOWED),
            (ListResult::NotFound, NOT_FOUND),
        ];
        for (result, text) in cases {
            assert_eq!(list_wording(&result).as_deref(), Some(text));
            assert_eq!(list_truncated(&result), None);
        }
        assert_eq!(
            list_truncated(&listed(one.clone(), true)).as_deref(),
            Some("Showing the first 1 entries.")
        );
        assert_eq!(list_truncated(&listed(one, false)), None);
    }

    #[test]
    fn file_wordings() {
        let file = |content, size| FileResult::File {
            at: at(3, 14, 2, 11),
            path: "f".into(),
            size,
            content,
        };
        assert_eq!(file_wording(&file(FileBody::Blob(blob()), 10)), None);
        assert_eq!(
            file_wording(&file(FileBody::Binary, 2048)).as_deref(),
            Some("Binary file (2.0 KiB): not shown.")
        );
        assert_eq!(
            file_wording(&file(FileBody::TooLarge, 5 << 20)).as_deref(),
            Some("Too large to show: 5.0 MiB, past 4.0 MiB.")
        );
        let cases = [
            (
                FileResult::IsADirectory,
                "A directory: open it under Files.",
            ),
            (
                FileResult::NotAFile,
                "A FIFO, socket or device: not opened.",
            ),
            (FileResult::UnknownSession, UNKNOWN_SESSION),
            (FileResult::WorkdirGone, WORKDIR_GONE),
            (FileResult::NotAllowed, NOT_ALLOWED),
            (FileResult::NotFound, NOT_FOUND),
        ];
        for (result, text) in cases {
            assert_eq!(file_wording(&result).as_deref(), Some(text));
        }
    }
}
