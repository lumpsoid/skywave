//! The workdir screen: a session's git changes and a changed file's diff,
//! or its files and one file's content, as last read from the server.
//! The wording is [`skywave_present::workdir`]'s; this lays it out.
//!
//! Everything shown came from the server, so every name and line goes
//! through [`clean`] before it reaches the terminal.

use std::path::Path;
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{HighlightSpacing, List, ListItem, ListState, Paragraph, Wrap};
use skywave_core::{
    BlobState, BlobView, ChangesResult, EntryKind, FileChange, FileResult, LinkTarget, ListResult,
    WorkdirBody, WorkdirFetch, WorkdirPane, WorkdirView, read_head,
};
use skywave_present::format::bytes;
use skywave_present::workdir::{
    DiffLine, NOT_FETCHED, change_mark, changes_capped, changes_wording, counts_label, diff_lines,
    diff_wording, entry_detail, entry_name, fetch_label, file_wording, head_label, list_truncated,
    list_wording, taken_label,
};

use skywave_present::text::clean;

use super::timeline::{blob_note, content_lines, wrap};
use crate::app::{App, BlobLines, ReadLines, WorkdirRow, WorkdirUi, workdir_rows};

/// Most of a diff or file read to show: the server sends no file past
/// 4 MiB, so this cuts only what it never sends; past it the pane says
/// how much shows.
pub const WORKDIR_SHOW_BYTES: u64 = 4 * 1024 * 1024;

pub(super) fn render(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    view: &WorkdirView,
    wall: SystemTime,
) {
    let ui = &app.ui().workdir;
    let now = DateTime::<Utc>::from(wall);
    let [head, body] = Layout::vertical([Constraint::Length(3), Constraint::Min(1)]).areas(area);
    frame.render_widget(Paragraph::new(head_lines(view, now)), head);

    match &view.body {
        WorkdirBody::Nothing => {
            // The header says it is loading.
            if view.fetch != WorkdirFetch::Loading {
                note(frame, body, NOT_FETCHED.to_owned());
            }
        }
        WorkdirBody::Changes(result) => match changes_wording(result) {
            Some(wording) => note(frame, body, wording),
            None => {
                let capped = match result {
                    ChangesResult::Changes(set) => changes_capped(set),
                    _ => None,
                };
                rows(frame, body, ui, view, capped.into_iter().collect());
            }
        },
        WorkdirBody::Log(_) => note(frame, body, "Commits are shown in the GUI.".to_owned()),
        WorkdirBody::Dir(result) => {
            let notes = list_wording(result)
                .into_iter()
                .chain(list_truncated(result))
                .collect();
            rows(frame, body, ui, view, notes);
        }
        WorkdirBody::Diff { file, blob } => match diff_wording(file.as_ref()) {
            Some(wording) => note(frame, body, wording),
            None => text(frame, body, ui, blob.as_ref(), true),
        },
        WorkdirBody::File { result, blob } => match file_wording(result) {
            Some(wording) => note(frame, body, wording),
            None => text(frame, body, ui, blob.as_ref(), false),
        },
    }
}

/// The session and the panes, then what this pane shows and when it was
/// read, and the state of reading it again.
fn head_lines(view: &WorkdirView, now: DateTime<Utc>) -> Vec<Line<'static>> {
    let changes = matches!(view.pane, WorkdirPane::Changes | WorkdirPane::Diff { .. });
    let tab = |label: &'static str, on: bool| {
        if on {
            Span::raw(format!(" {label} ")).reversed()
        } else {
            Span::raw(format!(" {label} ")).dark_gray()
        }
    };
    let mut first = vec![
        tab("Changes", changes),
        tab("Files", !changes),
        Span::raw("  "),
        Span::raw(clean(&view.title)).bold(),
    ];
    if let Some(workdir) = &view.workdir {
        first.push(Span::raw(format!(" · {}", clean(workdir))).dark_gray());
    }

    let (what, at) = match (&view.pane, &view.body) {
        (WorkdirPane::Changes, WorkdirBody::Changes(ChangesResult::Changes(set))) => {
            (vec![Span::raw(clean(&head_label(set)))], Some(set.at))
        }
        (WorkdirPane::Changes, _) => (vec![Span::raw("Changes")], None),
        (WorkdirPane::Commits, _) => (vec![Span::raw("Changes")], None),
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
            let mut spans = vec![mark(change), Span::raw(" "), Span::raw(clean(path)).bold()];
            if let Some(from) = file.and_then(|file| file.from.as_ref()) {
                spans.push(Span::raw(format!(" (from {})", clean(from))).dark_gray());
            }
            if let Some(counts) = file.and_then(counts_label) {
                spans.push(Span::raw(format!("  {counts}")).dark_gray());
            }
            (spans, None)
        }
        (WorkdirPane::Dir { path }, body) => {
            let at = match body {
                WorkdirBody::Dir(ListResult::Listed { at, .. }) => Some(*at),
                _ => None,
            };
            (vec![Span::raw(format!("./{}", clean(path))).bold()], at)
        }
        (WorkdirPane::File { path }, body) => {
            let mut spans = vec![Span::raw(clean(path)).bold()];
            let mut at = None;
            if let WorkdirBody::File {
                result: FileResult::File {
                    at: taken, size, ..
                },
                ..
            } = body
            {
                spans.push(Span::raw(format!("  {}", bytes(*size))).dark_gray());
                at = Some(*taken);
            }
            (spans, at)
        }
    };
    let mut second = what;
    if let Some(at) = at {
        second.push(Span::raw(format!(" · {}", taken_label(at, now))).dark_gray());
    }
    match (&view.fetch, fetch_label(&view.fetch)) {
        (WorkdirFetch::Failed(_), Some(label)) => {
            second.push(Span::raw(format!(" · {}", clean(&label))).red());
        }
        (_, Some(label)) => second.push(Span::raw(format!(" · {label}")).yellow()),
        (_, None) => {}
    }
    vec![Line::from(first), Line::from(second), Line::from("")]
}

/// A changed file's mark, coloured by how it changed.
fn mark(change: FileChange) -> Span<'static> {
    let span = Span::raw(change_mark(change).to_string());
    match change {
        FileChange::Added | FileChange::Untracked => span.green(),
        FileChange::Deleted => span.red(),
        FileChange::Conflicted => span.red().bold(),
        FileChange::Modified | FileChange::TypeChanged => span.yellow(),
        FileChange::Renamed => span.cyan(),
    }
}

/// Text in place of a pane's content: why there is none.
fn note(frame: &mut Frame, area: Rect, text: String) {
    frame.render_widget(
        Paragraph::new(Line::from(clean(&text)).dark_gray()).wrap(Wrap { trim: true }),
        area,
    );
}

/// A list pane's rows, the selected one marked, with `notes` under them.
fn rows(frame: &mut Frame, area: Rect, ui: &WorkdirUi, view: &WorkdirView, notes: Vec<String>) {
    let rows = workdir_rows(view);
    let [list, below] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(notes.len() as u16)]).areas(area);
    ui.page.set(usize::from(list.height));
    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| ListItem::new(row_line(row)))
        .collect();
    let at = ui.at(&rows);
    let mut state = ListState::default().with_selected((!rows.is_empty()).then_some(at));
    frame.render_stateful_widget(
        List::new(items)
            .highlight_symbol("> ")
            .highlight_spacing(HighlightSpacing::Always)
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
        list,
        &mut state,
    );
    let notes: Vec<Line> = notes
        .into_iter()
        .map(|text| Line::from(clean(&text)).dark_gray())
        .collect();
    frame.render_widget(Paragraph::new(notes), below);
}

fn row_line(row: &WorkdirRow) -> Line<'static> {
    match row {
        WorkdirRow::Change(file) => {
            let mut spans = vec![mark(file.change), Span::raw(" ")];
            if let Some(from) = &file.from {
                spans.push(Span::raw(format!("{} → ", clean(from))).dark_gray());
            }
            spans.push(Span::raw(clean(&file.path)));
            if let Some(counts) = counts_label(file) {
                spans.push(Span::raw(format!("  {counts}")).dark_gray());
            }
            Line::from(spans)
        }
        WorkdirRow::Up => Line::from("../").blue(),
        WorkdirRow::Entry(entry) => {
            let name = Span::raw(clean(&entry_name(entry)));
            let dir = matches!(
                entry.kind,
                EntryKind::Dir
                    | EntryKind::Link {
                        to: LinkTarget::Dir
                    }
            );
            let mut spans = vec![if dir { name.blue().bold() } else { name }];
            if let Some(detail) = entry_detail(entry) {
                spans.push(Span::raw(format!("  {detail}")).dark_gray());
            }
            Line::from(spans)
        }
    }
}

/// A diff (coloured by line kind) or a file (with line numbers) from its
/// blob, scrolled to [`WorkdirUi::top`]; where the blob is instead, when
/// it is not here.
fn text(frame: &mut Frame, area: Rect, ui: &WorkdirUi, blob: Option<&BlobView>, diff: bool) {
    let Some(blob) = blob else {
        return;
    };
    let BlobState::Available { path } = &blob.state else {
        if let Some(note) = blob_note(&blob.state) {
            frame.render_widget(Paragraph::new(Line::from(note)), area);
        }
        return;
    };
    let mut cache = ui.text.borrow_mut();
    if !cache
        .as_ref()
        .is_some_and(|read| read.id == blob.id && read.path == *path)
    {
        *cache = Some(BlobLines {
            id: blob.id,
            path: path.clone(),
            read: read(path, diff),
        });
    }
    let Some(BlobLines { read, .. }) = cache.as_ref() else {
        return;
    };
    let read = match read {
        Ok(read) => read,
        Err(error) => {
            let line = format!("cannot read {}: {error}", path.display());
            frame.render_widget(
                Paragraph::new(Line::from(clean(&line)).red()).wrap(Wrap { trim: true }),
                area,
            );
            return;
        }
    };
    let cut = read.size > WORKDIR_SHOW_BYTES;
    let [content, below] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(u16::from(cut))]).areas(area);
    if cut {
        frame.render_widget(
            Paragraph::new(
                Line::from(format!(
                    "… showing the first {} of {}",
                    bytes(WORKDIR_SHOW_BYTES),
                    bytes(read.size)
                ))
                .dark_gray(),
            ),
            below,
        );
    }

    let width = usize::from(content.width);
    let height = usize::from(content.height);
    let digits = read.lines.len().max(1).to_string().len();
    let rows_of = |i: usize| -> Vec<Line<'static>> {
        let (kind, line) = &read.lines[i];
        let (first, rest) = if diff {
            (Vec::new(), vec![Span::raw("  ")])
        } else {
            (
                vec![Span::raw(format!("{:>digits$} │ ", i + 1)).dark_gray()],
                vec![Span::raw(format!("{:>digits$} │ ", "")).dark_gray()],
            )
        };
        let style = kind.map_or(Style::new(), line_style);
        wrap(first, rest, vec![Span::styled(line.clone(), style)], width)
    };

    // The highest top whose lines still fill the pane: from the end back.
    let mut end = read.lines.len();
    let mut used = 0;
    while end > 0 {
        let rows = rows_of(end - 1).len();
        if used + rows > height {
            break;
        }
        used += rows;
        end -= 1;
    }
    ui.end.set(end);
    ui.page.set(height);
    let mut lines = Vec::new();
    for i in ui.top.min(end)..read.lines.len() {
        if lines.len() >= height {
            break;
        }
        lines.extend(rows_of(i));
    }
    frame.render_widget(Paragraph::new(lines), content);
}

/// The head of the blob at `path`, as lines safe for the terminal: a
/// diff's with their kinds.
fn read(path: &Path, diff: bool) -> Result<ReadLines, String> {
    let (text, size) = read_head(path, WORKDIR_SHOW_BYTES).map_err(|error| error.to_string())?;
    let lines = if diff {
        diff_lines(&text)
            .map(|(kind, line)| (Some(kind), clean(line)))
            .collect()
    } else {
        content_lines(&text)
            .into_iter()
            .map(|line| (None, clean(line)))
            .collect()
    };
    Ok(ReadLines { lines, size })
}

/// A diff line's colour by its kind.
fn line_style(kind: DiffLine) -> Style {
    match kind {
        DiffLine::FileHeader => Style::new().bold(),
        DiffLine::Hunk => Style::new().cyan(),
        DiffLine::Added => Style::new().green(),
        DiffLine::Removed => Style::new().red(),
        DiffLine::Context => Style::new(),
        DiffLine::Meta => Style::new().dark_gray(),
    }
}
