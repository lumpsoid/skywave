//! The storage screen: what the server's data takes on its disk, as
//! [`skywave_present::storage`] words it, one list at a time.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use skywave_core::StorageView;
use skywave_present::storage::{Grouping, Row, rows, status_line, totals};

use crate::app::App;
use crate::notify;

/// Longest label shown before an ellipsis.
const LABEL_MAX: usize = 48;

pub(super) fn render(frame: &mut Frame, area: Rect, app: &App, view: &StorageView) {
    let ui = &app.ui().storage;
    let totals = view.report.as_ref().map(totals).unwrap_or_default();

    let mut head_lines = Vec::new();
    if let Some(status) = status_line(view) {
        head_lines.push(Line::from(status).dark_gray());
    }
    let width = totals.iter().map(|t| t.label.len()).max().unwrap_or(0);
    for total in &totals {
        head_lines.push(Line::from(format!(
            "{:<width$}  {}",
            total.label, total.value
        )));
    }
    let at = Grouping::EVERY
        .iter()
        .position(|g| *g == ui.grouping)
        .unwrap_or(0);
    let mut choices = vec![Span::raw("List (Tab):").dark_gray()];
    for (i, grouping) in Grouping::EVERY.iter().enumerate() {
        choices.push(Span::raw(" "));
        choices.push(if i == at {
            Span::raw(format!("[{}]", grouping.label())).bold()
        } else {
            Span::raw(format!(" {} ", grouping.label()))
        });
    }
    head_lines.push(Line::from(""));
    head_lines.push(Line::from(choices));

    let [head, body] = Layout::vertical([
        Constraint::Length(u16::try_from(head_lines.len()).unwrap_or(u16::MAX)),
        Constraint::Min(1),
    ])
    .areas(area);
    frame.render_widget(Paragraph::new(head_lines), head);

    let rows = rows(view, ui.grouping);
    let lines = if view.report.is_none() {
        Vec::new()
    } else if rows.is_empty() {
        vec![Line::from(ui.grouping.empty_label()).dark_gray()]
    } else {
        table(&rows)
    };
    let end = lines.len().saturating_sub(usize::from(body.height));
    ui.end.set(end);
    let top = ui.top.min(end);
    frame.render_widget(
        Paragraph::new(lines).scroll((u16::try_from(top).unwrap_or(u16::MAX), 0)),
        body,
    );
}

/// Each row as its size and label, with what makes it up beneath.
fn table(rows: &[Row]) -> Vec<Line<'static>> {
    let size = rows
        .iter()
        .map(|r| r.size.chars().count())
        .max()
        .unwrap_or(0);
    rows.iter()
        .flat_map(|row| {
            [
                Line::from(vec![
                    Span::raw(format!("{:>size$}  ", row.size)).bold(),
                    Span::raw(cut(&notify::sanitise(&row.label))),
                ]),
                Line::from(format!("{:>size$}  {}", "", notify::sanitise(&row.detail))).dark_gray(),
            ]
        })
        .collect()
}

fn cut(text: &str) -> String {
    match text.char_indices().nth(LABEL_MAX) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_owned(),
    }
}
