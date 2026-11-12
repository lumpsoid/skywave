use rinf::SignalPiece;
use serde::{Deserialize, Serialize};

use super::timeline::BlobFacts;

/// Which pane of the workdir screen shows, or `ShowWorkdir` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SignalPiece)]
pub enum WorkdirPaneKind {
    /// What changed against `HEAD`.
    Changes,
    /// The newest commits on `HEAD`.
    Commits,
    /// One changed file's diff.
    Diff,
    /// One directory.
    Dir,
    /// One file.
    File,
}

/// A session's workdir as last read from the server: its git changes and
/// a changed file's diff, or its files and one file's content. Every
/// label is worded by the hub; the page lays them out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct WorkdirScreen {
    pub session_id: String,
    /// The session's title.
    pub title: String,
    pub workdir: Option<String>,
    pub pane: WorkdirPaneKind,
    /// The pane's path, relative to the workdir: a diff's or a file's, a
    /// directory's (empty for the workdir itself); empty for the changes.
    pub path: String,
    /// The directory the Files tab opens: `ShowWorkdir(Dir, last_dir)`.
    pub last_dir: String,
    /// What the pane shows and when it was read: `main @ 1a2b3c4 · taken
    /// 14:02:11`, `./src · taken …`, `M src/a.rs  +1 −2`, `a.txt  812 B`.
    pub header: String,
    /// `Loading…`, `Refresh failed: <reason>`; `None` when idle.
    pub fetch_label: Option<String>,
    /// `fetch_label` is a failure.
    pub fetch_failed: bool,
    /// What the pane says instead of its rows or content: never read, not
    /// a git repository, empty, binary, too large, …; `None` when they show.
    pub wording: Option<String>,
    /// Said under the rows: a capped change set, a truncated listing.
    pub notes: Vec<String>,
    /// The changes pane's files, as the server sorted them.
    pub changes: Vec<WorkdirChangeRow>,
    /// The commits pane's commits, newest first.
    pub commits: Vec<WorkdirCommitRow>,
    /// A directory below the workdir: a `..` row, which sends `Back`.
    pub can_go_up: bool,
    /// The directory pane's entries, directories first.
    pub entries: Vec<WorkdirEntryRow>,
    /// The diff or file shown, on those panes only, when it has a blob.
    pub content: Option<WorkdirContent>,
}

/// How a changed file changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, SignalPiece)]
pub enum WorkdirChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    TypeChanged,
    Untracked,
    Conflicted,
}

/// One changed file. It opens with `ShowWorkdir(Diff, path, untracked)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct WorkdirChangeRow {
    pub change: WorkdirChangeKind,
    /// `M`, `A`, `D`, `R`, `T`, `?`, `U`, as `git status --short` marks it.
    pub mark: String,
    /// `modified`, `untracked`, ….
    pub change_label: String,
    /// The path as shown: control characters dropped.
    pub path_label: String,
    /// The path as the server named it, for `ShowWorkdir`.
    pub path: String,
    /// A rename's old path.
    pub from: Option<String>,
    /// `+12 −3`, `binary`.
    pub counts_label: Option<String>,
    pub untracked: bool,
}

/// One commit of the commits pane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct WorkdirCommitRow {
    /// The abbreviated sha.
    pub short: String,
    /// The first line of its message.
    pub subject: String,
    pub author: String,
    /// `3 h ago`; `None` when the commit has no usable date.
    pub time_label: Option<String>,
}

/// One directory entry. It opens with `ShowWorkdir(opens, path)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct WorkdirEntryRow {
    /// The name as shown, with a `/` after a directory's.
    pub name: String,
    /// `812 B`, `→ directory`, `special file`; `None` for a directory.
    pub detail: Option<String>,
    /// `Dir` for a directory or a link to one, else `File`: the server's
    /// answer words what is not a file.
    pub opens: WorkdirPaneKind,
    /// Relative to the workdir, as the server named it, for `ShowWorkdir`.
    pub path: String,
}

/// The diff or file a pane shows: its blob, and its text once the blob is
/// here. Only the pane shown carries it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct WorkdirContent {
    /// Its download; `OpenFull` with its id opens the whole file once
    /// `Available`.
    pub blob: BlobFacts,
    /// The head of the text, as the hub caps it; empty until downloaded.
    pub lines: Vec<WorkdirLine>,
    /// `cut short: 1.0 MiB of 3.2 MiB shown`, when the text is longer.
    pub cut: Option<String>,
    /// The downloaded file could not be read.
    pub error: Option<String>,
}

/// One line of a diff or a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct WorkdirLine {
    pub kind: WorkdirLineKind,
    pub text: String,
}

/// A diff line's kind, from the whole diff; `Plain` for a file's lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, SignalPiece)]
pub enum WorkdirLineKind {
    FileHeader,
    Hunk,
    Added,
    Removed,
    Context,
    Meta,
    Plain,
}
