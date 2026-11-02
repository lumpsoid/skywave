//! The workdir screen's keys and presentation state: which row a list
//! pane has selected, how far a text pane is scrolled, and the text read
//! of the blob it shows. What to read, and when, is the core's
//! ([`Action::ShowWorkdir`]).

use std::cell::{Cell, RefCell};
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent};
use skywave_core::{
    Action, BlobId, BlobState, BlobView, ChangedFile, ChangesResult, DirEntry, EntryKind,
    FileChange, LinkTarget, ListResult, Screen, SessionId, WorkdirBody, WorkdirPane, WorkdirView,
};
use skywave_present::workdir::{DiffLine, change_mark, entry_name, sorted_entries};

use super::App;
use crate::clipboard::Clip;

/// A list pane's row by what it shows, so the selection stays on it when
/// the listing is read again or the user comes back up to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkdirPick {
    /// A changed file: one path can be listed twice, once untracked.
    Change {
        path: String,
        untracked: bool,
    },
    /// `..`: up to the parent directory.
    Up,
    Entry {
        name: String,
    },
}

/// One row of a list pane.
#[derive(Debug, Clone, Copy)]
pub enum WorkdirRow<'a> {
    Change(&'a ChangedFile),
    Up,
    Entry(&'a DirEntry),
}

impl WorkdirRow<'_> {
    pub fn pick(&self) -> WorkdirPick {
        match self {
            WorkdirRow::Change(file) => WorkdirPick::Change {
                path: file.path.clone(),
                untracked: file.change == FileChange::Untracked,
            },
            WorkdirRow::Up => WorkdirPick::Up,
            WorkdirRow::Entry(entry) => WorkdirPick::Entry {
                name: entry.name.clone(),
            },
        }
    }
}

/// The rows the pane on screen lists: the changed files, or, for a
/// directory, `..` below the workdir and then its entries, directories
/// first. Empty for a text pane and for changes with nothing to list.
pub fn workdir_rows(view: &WorkdirView) -> Vec<WorkdirRow<'_>> {
    match &view.body {
        WorkdirBody::Changes(ChangesResult::Changes(set)) => {
            set.files.iter().map(WorkdirRow::Change).collect()
        }
        WorkdirBody::Dir(result) => {
            let mut rows = Vec::new();
            if matches!(&view.pane, WorkdirPane::Dir { path } if !path.is_empty()) {
                rows.push(WorkdirRow::Up);
            }
            if let ListResult::Listed { entries, .. } = result {
                rows.extend(sorted_entries(entries).into_iter().map(WorkdirRow::Entry));
            }
            rows
        }
        _ => Vec::new(),
    }
}

/// The text of the blob a diff or file pane shows, read once per blob
/// rather than every frame: it can be megabytes.
#[derive(Debug, Clone)]
pub(crate) struct BlobLines {
    pub(crate) id: BlobId,
    pub(crate) path: PathBuf,
    /// The lines, made safe for the terminal, or why the file could not
    /// be read.
    pub(crate) read: Result<ReadLines, String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ReadLines {
    /// Each line with its diff kind (`None` for a file's).
    pub(crate) lines: Vec<(Option<DiffLine>, String)>,
    /// The whole blob's size; more than was read when it was cut short.
    pub(crate) size: u64,
}

/// The workdir screen's presentation state.
#[derive(Debug, Clone, Default)]
pub struct WorkdirUi {
    /// The session and pane the state below belongs to; another resets it.
    shown: Option<(SessionId, WorkdirPane)>,
    /// A list pane's selected row; the first row when `None` or gone.
    pub(crate) selected: Option<WorkdirPick>,
    /// A text pane's first line shown.
    pub(crate) top: usize,
    /// The highest [`top`](Self::top) the last frame could use, as
    /// [`UiState::help_end`](super::UiState::help_end) is for the help.
    pub(crate) end: Cell<usize>,
    /// Rows the last frame's body had, for PgUp/PgDn.
    pub(crate) page: Cell<usize>,
    pub(crate) text: RefCell<Option<BlobLines>>,
}

impl WorkdirUi {
    /// Which of `rows` is selected: the first when none is, or the
    /// selected one is gone.
    pub fn at(&self, rows: &[WorkdirRow]) -> usize {
        self.selected
            .as_ref()
            .and_then(|picked| rows.iter().position(|row| row.pick() == *picked))
            .unwrap_or(0)
    }

    /// Follows the screen to `view`: a new pane starts at its top, except
    /// that going up selects the directory or file come from, and leaving
    /// a diff selects its file.
    fn follow(&mut self, view: Option<&WorkdirView>) {
        let Some(view) = view else {
            self.shown = None;
            return;
        };
        let now = (view.session, view.pane.clone());
        if self.shown.as_ref() == Some(&now) {
            return;
        }
        let was = self
            .shown
            .replace(now)
            .filter(|(id, _)| *id == view.session);
        self.top = 0;
        self.selected = match (was.map(|(_, pane)| pane), &view.pane) {
            (
                Some(WorkdirPane::Dir { path: from } | WorkdirPane::File { path: from }),
                WorkdirPane::Dir { path },
            ) if !from.is_empty() && parent(&from) == path.as_str() => Some(WorkdirPick::Entry {
                name: base(&from).to_owned(),
            }),
            (Some(WorkdirPane::Diff { path, untracked }), WorkdirPane::Changes) => {
                Some(WorkdirPick::Change { path, untracked })
            }
            _ => None,
        };
    }
}

/// `path`'s directory, relative to the workdir: empty for the workdir.
fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

fn base(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

/// The directory a files pane lists: as the server named it, else as asked.
fn dir_path(view: &WorkdirView) -> &str {
    match (&view.body, &view.pane) {
        (WorkdirBody::Dir(ListResult::Listed { path, .. }), _) => path,
        (_, WorkdirPane::Dir { path }) => path,
        _ => "",
    }
}

/// The blob a diff or file pane shows, if it has one.
pub fn shown_blob(view: &WorkdirView) -> Option<&BlobView> {
    match &view.body {
        WorkdirBody::Diff { blob, .. } | WorkdirBody::File { blob, .. } => blob.as_ref(),
        _ => None,
    }
}

/// The download a blob not here asks for: one never asked for, or one
/// that failed (the core does not ask again for those by itself).
fn download(blob: &BlobView) -> Option<Action> {
    match blob.state {
        BlobState::NotDownloaded { .. } | BlobState::Failed { .. } => {
            Some(Action::DownloadBlob(blob.id))
        }
        _ => None,
    }
}

impl App {
    /// Keeps the workdir screen's state with the screen shown.
    pub(super) fn follow_workdir(&mut self) {
        let view = match &self.view.screen {
            Screen::Workdir(view) => Some(view),
            _ => None,
        };
        self.ui.workdir.follow(view);
    }

    pub(super) fn workdir_key(&mut self, key: KeyEvent) -> Option<Action> {
        match key.code {
            KeyCode::Char('q') => return self.quit(),
            KeyCode::Esc => return Some(Action::Back),
            KeyCode::Char('y') => return self.workdir_copy(),
            _ => {}
        }
        let Screen::Workdir(view) = &self.view.screen else {
            return None;
        };
        let ui = &mut self.ui.workdir;
        let page = ui.page.get().max(1);
        if let WorkdirPane::Diff { .. } | WorkdirPane::File { .. } = view.pane {
            let end = ui.end.get();
            let top = ui.top.min(end);
            match key.code {
                KeyCode::Down | KeyCode::Char('j') => ui.top = (top + 1).min(end),
                KeyCode::Up | KeyCode::Char('k') => ui.top = top.saturating_sub(1),
                KeyCode::PageDown => ui.top = (top + page).min(end),
                KeyCode::PageUp => ui.top = top.saturating_sub(page),
                KeyCode::Home | KeyCode::Char('g') => ui.top = 0,
                KeyCode::End | KeyCode::Char('G') => ui.top = end,
                KeyCode::Enter | KeyCode::Char('d') => return shown_blob(view).and_then(download),
                _ => return workdir_common(view, key),
            }
            return None;
        }
        let rows = workdir_rows(view);
        let at = ui.at(&rows);
        let last = rows.len().saturating_sub(1);
        let to = match key.code {
            KeyCode::Down | KeyCode::Char('j') => (at + 1).min(last),
            KeyCode::Up | KeyCode::Char('k') => at.saturating_sub(1),
            KeyCode::PageDown => (at + page).min(last),
            KeyCode::PageUp => at.saturating_sub(page),
            KeyCode::Home | KeyCode::Char('g') => 0,
            KeyCode::End | KeyCode::Char('G') => last,
            KeyCode::Enter => {
                return match rows.get(at)? {
                    WorkdirRow::Change(file) => Some(Action::ShowWorkdir(WorkdirPane::Diff {
                        path: file.path.clone(),
                        untracked: file.change == FileChange::Untracked,
                    })),
                    // The core goes up from a directory below the workdir.
                    WorkdirRow::Up => Some(Action::Back),
                    WorkdirRow::Entry(entry) => {
                        let path = join(dir_path(view), &entry.name);
                        Some(Action::ShowWorkdir(match entry.kind {
                            EntryKind::Dir
                            | EntryKind::Link {
                                to: LinkTarget::Dir,
                            } => WorkdirPane::Dir { path },
                            // The server says why one is not shown.
                            _ => WorkdirPane::File { path },
                        }))
                    }
                };
            }
            _ => return workdir_common(view, key),
        };
        ui.selected = rows.get(to).map(WorkdirRow::pick);
        None
    }

    /// `y`: a diff or file's whole content once it is here (else it
    /// downloads, and the notice says to press `y` again), or a list
    /// pane's rows as text.
    fn workdir_copy(&mut self) -> Option<Action> {
        let Screen::Workdir(view) = &self.view.screen else {
            return None;
        };
        if let Some(blob) = shown_blob(view) {
            let blob = blob.clone();
            return self.copy_blob('y', blob, |path| Clip::File { path, strip: None });
        }
        let lines: Vec<String> = workdir_rows(view)
            .into_iter()
            .filter_map(|row| match row {
                WorkdirRow::Change(file) => {
                    Some(format!("{} {}", change_mark(file.change), file.path))
                }
                WorkdirRow::Up => None,
                WorkdirRow::Entry(entry) => Some(entry_name(entry)),
            })
            .collect();
        if lines.is_empty() {
            self.ui.notice = Some("nothing to copy".into());
        } else {
            self.ui.copy = Some(Clip::Text(lines.join("\n") + "\n"));
        }
        None
    }
}

/// The keys every pane shares: `Tab` to the other pane, `r` to read again.
fn workdir_common(view: &WorkdirView, key: KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Tab | KeyCode::BackTab => Some(Action::ShowWorkdir(match view.pane {
            WorkdirPane::Changes | WorkdirPane::Diff { .. } => WorkdirPane::Dir {
                path: view.last_dir.clone(),
            },
            WorkdirPane::Dir { .. } | WorkdirPane::File { .. } => WorkdirPane::Changes,
        })),
        KeyCode::Char('r') => Some(Action::RefreshWorkdir),
        _ => None,
    }
}
