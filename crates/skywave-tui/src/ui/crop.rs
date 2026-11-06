//! The crop page: the output's lines, numbered as a crop numbers them, with
//! the kept ones marked; or why there are none to choose from here. The
//! rules are [`skywave_present::crop`]'s; this lays them out.
//!
//! Lines are read a window at a time through a [`LineIndex`] opened once
//! per page, and each goes through [`strip_ansi`] and [`clean`] by itself:
//! the content is the server's, and an escape string may span a line
//! break.

use std::sync::Arc;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use skywave_core::{BlobState, CropContent, CropView, LineIndex, LineSpan};
use skywave_present::crop::{keeps_label, kept, presets};
use skywave_present::format::{blob_size, bytes};
use skywave_present::text::clean;
use skywave_proto::strip_ansi;

use super::timeline::blob_note;
use crate::app::{App, CropIndex, CropUi};

pub(super) fn render(frame: &mut Frame, area: Rect, app: &App, view: &CropView) {
    let ui = &app.ui().crop;
    let mut head = vec![Line::from(vec![
        Span::raw("Crop ").dark_gray(),
        Span::raw(clean(&view.title)).bold(),
    ])];
    let lines = match &view.content {
        CropContent::Ready { lines, .. } => *lines,
        other => {
            head.push(Line::from(""));
            head.extend(not_ready(other));
            frame.render_widget(Paragraph::new(head).wrap(Wrap { trim: false }), area);
            return;
        }
    };
    let mut state = vec![Span::raw(keeps_label(kept(&view.keep), lines))];
    if let Some((first, last)) = ui.range() {
        state.push(
            Span::raw(format!(
                " · range {}–{}: v keeps or drops it, Esc cancels",
                first + 1,
                last + 1
            ))
            .cyan(),
        );
    }
    head.push(Line::from(state));
    let offered = presets(lines);
    if !offered.is_empty() {
        let labels: Vec<String> = offered
            .iter()
            .enumerate()
            .map(|(i, preset)| format!("{} {}", i + 1, preset.label()))
            .collect();
        head.push(Line::from(labels.join(" · ")).dark_gray());
    }

    let [head_area, body] = Layout::vertical([
        Constraint::Length(u16::try_from(head.len()).unwrap_or(u16::MAX)),
        Constraint::Min(1),
    ])
    .areas(area);
    frame.render_widget(Paragraph::new(head), head_area);
    list(frame, body, ui, view, lines);
}

/// What the page says while it has no lines to choose from.
fn not_ready(content: &CropContent) -> Vec<Line<'static>> {
    match content {
        CropContent::NotHere(state) => {
            let size =
                blob_size(state).map_or_else(String::new, |size| format!(" {}", bytes(size)));
            let mut out = vec![Line::from(format!(
                "This output isn't on this device. Download{size} to choose lines."
            ))];
            if let Some(note) = blob_note(state) {
                out.push(Line::from(note));
            }
            if matches!(
                state,
                BlobState::Downloading { .. } | BlobState::Requested { .. }
            ) {
                out.push(Line::from("The lines show once it is here.").dark_gray());
            }
            out
        }
        CropContent::Counting => vec![Line::from("Counting its lines…").dark_gray()],
        CropContent::Unreadable { reason } => {
            vec![Line::from(format!("Cannot read it here: {}", clean(reason))).red()]
        }
        CropContent::Gone => vec![Line::from(
            "This output was deleted or cropped since: there is nothing to crop here.",
        )],
        CropContent::Ready { .. } => Vec::new(),
    }
}

/// The window of lines from [`CropUi::top`], each numbered, `+` where kept,
/// the cursor's reversed and an open range's underlined.
fn list(frame: &mut Frame, area: Rect, ui: &CropUi, view: &CropView, lines: u32) {
    let CropContent::Ready { path, .. } = &view.content else {
        return;
    };
    let height = usize::from(area.height);
    ui.page.set(height);
    let mut cache = ui.index.borrow_mut();
    if cache.as_ref().is_none_or(|index| index.path != *path) {
        *cache = Some(CropIndex {
            path: path.clone(),
            read: LineIndex::open(path)
                .map(Arc::new)
                .map_err(|error| error.to_string()),
        });
    }
    let Some(CropIndex { read, .. }) = cache.as_ref() else {
        return;
    };
    let window = read.as_ref().map_err(String::clone).and_then(|index| {
        let height = u32::try_from(height).unwrap_or(u32::MAX);
        let top = ui.top.min(lines.saturating_sub(height));
        let end = top.saturating_add(height).min(lines);
        index
            .lines(u64::from(top)..u64::from(end))
            .map(|read| (top, read))
            .map_err(|error| error.to_string())
    });
    let (top, read) = match window {
        Ok(window) => window,
        Err(error) => {
            let line = format!("cannot read {}: {error}", path.display());
            frame.render_widget(
                Paragraph::new(Line::from(clean(&line)).red()).wrap(Wrap { trim: true }),
                area,
            );
            return;
        }
    };
    let digits = lines.max(1).to_string().len();
    let range = ui.range();
    let rows: Vec<Line> = read
        .iter()
        .zip(top..)
        .map(|(bytes, n)| {
            let is_kept = in_spans(&view.keep, n);
            let mark = if is_kept {
                Span::raw("+").green()
            } else {
                Span::raw(" ")
            };
            let cursor = if n == ui.cursor { ">" } else { " " };
            let text = clean(&strip_ansi(&String::from_utf8_lossy(bytes)));
            let mut style = Style::new();
            if n == ui.cursor {
                style = style.add_modifier(Modifier::REVERSED);
            }
            if range.is_some_and(|(first, last)| (first..=last).contains(&n)) {
                style = style.add_modifier(Modifier::UNDERLINED);
            }
            Line::from(vec![
                mark,
                Span::raw(cursor),
                Span::raw(format!("{:>digits$} │ ", n + 1)).dark_gray(),
                Span::raw(text),
            ])
            .style(style)
        })
        .collect();
    frame.render_widget(Paragraph::new(rows), area);
}

/// Whether `spans` keep line `n`.
fn in_spans(spans: &[LineSpan], n: u32) -> bool {
    spans
        .iter()
        .any(|s| n >= s.first && u64::from(n) < u64::from(s.first) + u64::from(s.count))
}
