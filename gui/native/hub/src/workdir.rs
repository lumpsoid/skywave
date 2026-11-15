//! The workdir screen as [`WorkdirScreen`]: core's [`WorkdirView`] worded
//! with [`skywave_present::workdir`], as the TUI words it without its key
//! hints, and the text of the diff or file shown.
//!
//! Everything shown came from the server, so every name and line goes
//! through [`clean_line`].

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use skywave_core::workdir::PushOutcome;
use skywave_core::{
    BlobId, BlobState, BlobView, ChangedFile, ChangesResult, EntryKind, FileBody, FileChange,
    FileResult, LinkTarget, ListResult, PushState, WorkdirBody, WorkdirDownload, WorkdirFetch,
    WorkdirPane, WorkdirPush, WorkdirView, read_head,
};
use skywave_present::format::bytes;
use skywave_present::workdir::{
    DiffLine, NOT_FETCHED, change_label, change_mark, changes_capped, changes_wording,
    commit_time_label, counts_label, diff_lines, diff_wording, entry_detail, entry_name,
    fetch_label, file_wording, head_label, list_truncated, list_wording, log_capped, log_label,
    log_wording, sorted_entries, taken_label,
};
use skywave_proto::{LogResult, UploadResult, WriteFileResult};

use crate::signals::{
    ScreenView, WorkdirChangeKind, WorkdirChangeRow, WorkdirCommitRow, WorkdirContent,
    WorkdirDownloadRow, WorkdirEntryRow, WorkdirLine, WorkdirLineKind, WorkdirPaneKind,
    WorkdirPushRow, WorkdirScreen,
};
use skywave_present::text::clean_line;

use crate::timeline::{blob_facts, content_lines};

/// Most of a diff or file the screen carries. Lower than the TUI's
/// 4 MiB: every line crosses to Dart in the screen signal, which is sent
/// whole whenever it changes. Past it, "Open" shows the whole file.
pub const WORKDIR_SHOW_BYTES: u64 = 1024 * 1024;
/// Most lines of a diff or file the screen carries, for the same reason.
pub const WORKDIR_SHOW_LINES: usize = 20_000;

/// The text of the blob the shown diff or file pane holds, read once per
/// blob rather than on every snapshot: it can be a megabyte. One slot:
/// only one pane shows at a time, and a downloaded blob's file does not
/// change.
#[derive(Debug, Default)]
pub struct WorkdirText {
    read: Option<Read>,
}

#[derive(Debug)]
struct Read {
    id: BlobId,
    path: PathBuf,
    text: Result<Text, String>,
}

/// The head of a diff or file, as the screen shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Text {
    lines: Vec<WorkdirLine>,
    /// More lines, or more bytes, than shown.
    cut: Option<String>,
}

impl WorkdirText {
    /// The text of the blob `view` shows, once it is downloaded, or why
    /// its file could not be read; `None` on a list pane, and before the
    /// blob is here.
    pub fn of(&mut self, view: &WorkdirView) -> Option<&Result<Text, String>> {
        let blob = shown_blob(view)?;
        let BlobState::Available { path } = &blob.state else {
            return None;
        };
        let fresh = self
            .read
            .as_ref()
            .is_some_and(|read| read.id == blob.id && read.path == *path);
        if !fresh {
            let diff = matches!(view.body, WorkdirBody::Diff { .. });
            self.read = Some(Read {
                id: blob.id,
                path: path.clone(),
                text: read(path, diff),
            });
        }
        self.read.as_ref().map(|read| &read.text)
    }
}

/// The blob a diff or file pane shows, if it has one.
pub fn shown_blob(view: &WorkdirView) -> Option<&BlobView> {
    match &view.body {
        WorkdirBody::Diff { blob, .. } | WorkdirBody::File { blob, .. } => blob.as_ref(),
        _ => None,
    }
}

/// The head of the blob at `path`: a diff's lines with their kinds from
/// the whole diff ([`diff_lines`]), a file's plain.
fn read(path: &Path, diff: bool) -> Result<Text, String> {
    let (text, size) = read_head(path, WORKDIR_SHOW_BYTES)
        .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
    let line = |kind, text: &str| WorkdirLine {
        kind,
        text: clean_line(text),
    };
    let mut lines: Vec<WorkdirLine> = if diff {
        diff_lines(&text)
            .take(WORKDIR_SHOW_LINES + 1)
            .map(|(kind, text)| line(line_kind(kind), text))
            .collect()
    } else {
        content_lines(&text)
            .into_iter()
            .take(WORKDIR_SHOW_LINES + 1)
            .map(|text| line(WorkdirLineKind::Plain, text))
            .collect()
    };
    let more_lines = lines.len() > WORKDIR_SHOW_LINES;
    lines.truncate(WORKDIR_SHOW_LINES);
    let cut = (more_lines || size > WORKDIR_SHOW_BYTES).then(|| {
        let shown = if more_lines {
            format!("{WORKDIR_SHOW_LINES} lines")
        } else {
            bytes(WORKDIR_SHOW_BYTES)
        };
        format!("cut short: {shown} of {} shown", bytes(size))
    });
    Ok(Text { lines, cut })
}

fn line_kind(kind: DiffLine) -> WorkdirLineKind {
    match kind {
        DiffLine::FileHeader => WorkdirLineKind::FileHeader,
        DiffLine::Hunk => WorkdirLineKind::Hunk,
        DiffLine::Added => WorkdirLineKind::Added,
        DiffLine::Removed => WorkdirLineKind::Removed,
        DiffLine::Context => WorkdirLineKind::Context,
        DiffLine::Meta => WorkdirLineKind::Meta,
    }
}

/// The screen for `view`, with `text` the shown blob's ([`WorkdirText::of`]).
pub fn screen(view: &WorkdirView, text: Option<&Result<Text, String>>, wall_ms: i64) -> ScreenView {
    let now = DateTime::from_timestamp_millis(wall_ms).unwrap_or_default();
    let (pane, path) = match &view.pane {
        WorkdirPane::Changes => (WorkdirPaneKind::Changes, String::new()),
        WorkdirPane::Commits => (WorkdirPaneKind::Commits, String::new()),
        WorkdirPane::Diff { path, .. } => (WorkdirPaneKind::Diff, path.clone()),
        WorkdirPane::Dir { path } => (WorkdirPaneKind::Dir, path.clone()),
        WorkdirPane::File { path } => (WorkdirPaneKind::File, path.clone()),
    };
    let wording = match &view.body {
        WorkdirBody::Nothing => {
            // The fetch label says it is loading.
            (view.fetch != WorkdirFetch::Loading).then(|| NOT_FETCHED.to_owned())
        }
        WorkdirBody::Changes(result) => changes_wording(result),
        WorkdirBody::Log(result) => log_wording(result),
        WorkdirBody::Diff { file, .. } => diff_wording(file.as_ref()),
        WorkdirBody::Dir(result) => list_wording(result),
        WorkdirBody::File { result, .. } => file_wording(result),
    };
    let notes = match &view.body {
        WorkdirBody::Changes(ChangesResult::Changes(set)) => changes_capped(set),
        WorkdirBody::Log(LogResult::Log(log)) => log_capped(log),
        WorkdirBody::Dir(result) => list_truncated(result),
        _ => None,
    };
    let changes = match &view.body {
        WorkdirBody::Changes(ChangesResult::Changes(set)) => {
            set.files.iter().map(change_row).collect()
        }
        _ => Vec::new(),
    };
    let commits = match &view.body {
        WorkdirBody::Log(LogResult::Log(log)) => log
            .commits
            .iter()
            .map(|commit| WorkdirCommitRow {
                short: clean_line(&commit.short),
                subject: clean_line(&commit.subject),
                author: clean_line(&commit.author),
                time_label: commit_time_label(commit, now),
            })
            .collect(),
        _ => Vec::new(),
    };
    let entries = match &view.body {
        WorkdirBody::Dir(ListResult::Listed {
            path: dir, entries, ..
        }) => sorted_entries(entries)
            .into_iter()
            .map(|entry| WorkdirEntryRow {
                name: clean_line(&entry_name(entry)),
                detail: entry_detail(entry),
                opens: match entry.kind {
                    EntryKind::Dir
                    | EntryKind::Link {
                        to: LinkTarget::Dir,
                    } => WorkdirPaneKind::Dir,
                    // The server says why one is not shown.
                    _ => WorkdirPaneKind::File,
                },
                path: join(dir, &entry.name),
            })
            .collect(),
        _ => Vec::new(),
    };
    let content = shown_blob(view).filter(|_| wording.is_none()).map(|blob| {
        let (lines, cut, error) = match text {
            Some(Ok(text)) => (text.lines.clone(), text.cut.clone(), None),
            Some(Err(error)) => (Vec::new(), None, Some(clean_line(error))),
            None => (Vec::new(), None, None),
        };
        WorkdirContent {
            blob: blob_facts(blob),
            lines,
            cut,
            error,
        }
    });
    ScreenView::Workdir {
        workdir: WorkdirScreen {
            session_id: view.session.0.to_string(),
            title: clean_line(&view.title),
            workdir: view.workdir.as_deref().map(clean_line),
            pane,
            can_go_up: pane == WorkdirPaneKind::Dir && !path.is_empty(),
            path,
            last_dir: view.last_dir.clone(),
            header: header(view, now),
            fetch_label: fetch_label(&view.fetch).map(|label| clean_line(&label)),
            fetch_failed: matches!(view.fetch, WorkdirFetch::Failed(_)),
            wording,
            notes: notes.into_iter().collect(),
            changes,
            commits,
            entries,
            content,
            downloads: view.downloads.iter().map(download_row).collect(),
            pushes: view.pushes.iter().map(push_row).collect(),
        },
    }
}

/// What the pane shows, and when it was read.
fn header(view: &WorkdirView, now: DateTime<Utc>) -> String {
    let (what, at) = match (&view.pane, &view.body) {
        (WorkdirPane::Changes, WorkdirBody::Changes(ChangesResult::Changes(set))) => {
            (head_label(set), Some(set.at))
        }
        (WorkdirPane::Changes, _) => ("Changes".to_owned(), None),
        (WorkdirPane::Commits, WorkdirBody::Log(LogResult::Log(log))) => {
            (log_label(log), Some(log.at))
        }
        (WorkdirPane::Commits, _) => ("Commits".to_owned(), None),
        (WorkdirPane::Diff { path, untracked }, body) => {
            let file = match body {
                WorkdirBody::Diff { file, .. } => file.as_ref(),
                _ => None,
            };
            let change = file.map_or(
                if *untracked {
                    FileChange::Untracked
                } else {
                    FileChange::Modified
                },
                |file| file.change,
            );
            let mut what = format!("{} {path}", change_mark(change));
            if let Some(from) = file.and_then(|file| file.from.as_ref()) {
                what.push_str(&format!(" (from {from})"));
            }
            if let Some(counts) = file.and_then(counts_label) {
                what.push_str(&format!("  {counts}"));
            }
            (what, None)
        }
        (WorkdirPane::Dir { path }, body) => {
            let at = match body {
                WorkdirBody::Dir(ListResult::Listed { at, .. }) => Some(*at),
                _ => None,
            };
            (format!("./{path}"), at)
        }
        (WorkdirPane::File { path }, body) => match body {
            WorkdirBody::File {
                result: FileResult::File { at, size, .. },
                ..
            } => (format!("{path}  {}", bytes(*size)), Some(*at)),
            _ => (path.clone(), None),
        },
    };
    let header = match at {
        Some(at) => format!("{what} · {}", taken_label(at, now)),
        None => what,
    };
    clean_line(&header)
}

fn change_row(file: &ChangedFile) -> WorkdirChangeRow {
    WorkdirChangeRow {
        change: match file.change {
            FileChange::Added => WorkdirChangeKind::Added,
            FileChange::Modified => WorkdirChangeKind::Modified,
            FileChange::Deleted => WorkdirChangeKind::Deleted,
            FileChange::Renamed => WorkdirChangeKind::Renamed,
            FileChange::TypeChanged => WorkdirChangeKind::TypeChanged,
            FileChange::Untracked => WorkdirChangeKind::Untracked,
            FileChange::Conflicted => WorkdirChangeKind::Conflicted,
        },
        mark: change_mark(file.change).to_string(),
        change_label: change_label(file.change).to_owned(),
        path_label: clean_line(&file.path),
        path: file.path.clone(),
        from: file.from.as_deref().map(clean_line),
        counts_label: counts_label(file),
        untracked: file.change == FileChange::Untracked,
    }
}

/// A download as the save flow reads it: where its ask stands, else where
/// its content's download stands, with the file once it is on the device.
fn download_row(download: &WorkdirDownload) -> WorkdirDownloadRow {
    let (label, failed, blob, local_path) =
        match (&download.fetch, &download.result, &download.blob) {
            (WorkdirFetch::Loading, _, _) => {
                ("Asking the server\u{2026}".to_owned(), false, None, None)
            }
            (WorkdirFetch::Failed(reason), _, _) => {
                (format!("Download failed: {reason}"), true, None, None)
            }
            (
                WorkdirFetch::Idle,
                Some(FileResult::File {
                    content: FileBody::Blob(_),
                    ..
                }),
                Some(view),
            ) => {
                let local_path = match &view.state {
                    BlobState::Available { path } => Some(path.display().to_string()),
                    _ => None,
                };
                let facts = blob_facts(view);
                let failed = matches!(view.state, BlobState::Failed { .. });
                (facts.label.clone(), failed, Some(facts), local_path)
            }
            (
                WorkdirFetch::Idle,
                Some(FileResult::File {
                    content: FileBody::TooLarge,
                    size,
                    ..
                }),
                _,
            ) => (
                format!("Too large to download: {}", bytes(*size)),
                true,
                None,
                None,
            ),
            (WorkdirFetch::Idle, Some(other), _) => (
                file_wording(other).unwrap_or_else(|| "Cannot download.".to_owned()),
                true,
                None,
                None,
            ),
            (WorkdirFetch::Idle, None, _) => ("Not asked yet.".to_owned(), false, None, None),
        };
    WorkdirDownloadRow {
        path: download.path.clone(),
        label: clean_line(&label),
        failed,
        blob,
        local_path,
    }
}

/// A push into the workdir as the save flow reads it: where it stands. An
/// `Exists` answer carries the sha256 of the file there, to resend the push
/// with once the user agrees to replace it.
fn push_row(push: &WorkdirPush) -> WorkdirPushRow {
    let (label, pushing, done, existing_sha256, failed) = match &push.state {
        PushState::Pushing => ("Sending\u{2026}".to_owned(), true, false, None, false),
        PushState::Failed(reason) => (format!("Push failed: {reason}"), false, false, None, true),
        PushState::Done(PushOutcome::Unreadable { reason }) => (
            format!("Cannot read the file: {reason}"),
            false,
            false,
            None,
            true,
        ),
        PushState::Done(PushOutcome::UploadRefused(UploadResult::TooLarge)) => {
            ("Too large to send".to_owned(), false, false, None, true)
        }
        PushState::Done(
            PushOutcome::UploadRefused(UploadResult::DiskLow)
            | PushOutcome::Written(WriteFileResult::DiskLow),
        ) => (
            "The server's disk is low".to_owned(),
            false,
            false,
            None,
            true,
        ),
        PushState::Done(PushOutcome::UploadRefused(UploadResult::HashMismatch)) => (
            "The file changed while it was sent".to_owned(),
            false,
            false,
            None,
            true,
        ),
        PushState::Done(PushOutcome::UploadRefused(UploadResult::Stored { .. })) => {
            ("Push failed".to_owned(), false, false, None, true)
        }
        PushState::Done(PushOutcome::Written(WriteFileResult::Written { size, .. })) => (
            format!("Written \u{b7} {}", bytes(*size)),
            false,
            true,
            None,
            false,
        ),
        PushState::Done(PushOutcome::Written(WriteFileResult::Same { .. })) => {
            ("Already there".to_owned(), false, true, None, false)
        }
        PushState::Done(PushOutcome::Written(WriteFileResult::Exists { sha256, size })) => (
            format!("A file is already there ({})", bytes(*size)),
            false,
            false,
            Some(sha256.to_string()),
            false,
        ),
        PushState::Done(PushOutcome::Written(WriteFileResult::NotStaged)) => {
            ("The upload was lost".to_owned(), false, false, None, true)
        }
        PushState::Done(PushOutcome::Written(WriteFileResult::IsADirectory)) => {
            ("A directory is there".to_owned(), false, false, None, true)
        }
        PushState::Done(PushOutcome::Written(WriteFileResult::NotAFile)) => (
            "Something other than a file is there".to_owned(),
            false,
            false,
            None,
            true,
        ),
        PushState::Done(PushOutcome::Written(WriteFileResult::UnknownSession)) => {
            ("The session is gone".to_owned(), false, false, None, true)
        }
        PushState::Done(PushOutcome::Written(WriteFileResult::WorkdirGone)) => {
            ("The workdir is gone".to_owned(), false, false, None, true)
        }
        PushState::Done(PushOutcome::Written(WriteFileResult::NotAllowed)) => {
            ("Not allowed there".to_owned(), false, false, None, true)
        }
        PushState::Done(PushOutcome::Written(WriteFileResult::NotFound)) => (
            "Its folder does not exist".to_owned(),
            false,
            false,
            None,
            true,
        ),
    };
    WorkdirPushRow {
        path: push.path.clone(),
        source: push.source.display().to_string(),
        label: clean_line(&label),
        pushing,
        done,
        existing_sha256,
        failed,
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}
