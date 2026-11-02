//! The usage screen: cost and tokens over a span of days, one grouping at a
//! time, totalled by [`skywave_present::usage`].

use std::time::SystemTime;

use chrono::{DateTime, Local, Utc};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use skywave_core::UsageView;
use skywave_present::usage::{Grouping, NO_TURNS, Report, Row, Span as Days};

use crate::app::App;
use crate::notify;

/// Longest label shown before an ellipsis.
const LABEL_MAX: usize = 48;

pub(super) fn render(frame: &mut Frame, area: Rect, app: &App, view: &UsageView, wall: SystemTime) {
    let ui = &app.ui().usage;
    let now = DateTime::<Utc>::from(wall).with_timezone(&Local);
    let report = Report::new(view, ui.span, &now);
    let rows = report.rows(ui.grouping);

    let [head, body, foot] = Layout::vertical([
        Constraint::Length(4),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .areas(area);

    let choices = |label: &'static str, chosen: usize, all: Vec<&'static str>| {
        let mut spans = vec![Span::raw(label).dark_gray()];
        for (i, choice) in all.into_iter().enumerate() {
            spans.push(Span::raw(" "));
            spans.push(if i == chosen {
                Span::raw(format!("[{choice}]")).bold()
            } else {
                Span::raw(format!(" {choice} "))
            });
        }
        Line::from(spans)
    };
    let span_at = Days::EVERY.iter().position(|s| *s == ui.span).unwrap_or(0);
    let grouping_at = Grouping::EVERY
        .iter()
        .position(|g| *g == ui.grouping)
        .unwrap_or(0);
    let head_lines = vec![
        choices("Range (r):", span_at, Days::EVERY.map(Days::label).to_vec()),
        choices(
            "Group (Tab):",
            grouping_at,
            Grouping::EVERY.map(Grouping::label).to_vec(),
        ),
        Line::from(""),
        Line::from(format!("Total  {}", report.total.line())).bold(),
    ];
    frame.render_widget(Paragraph::new(head_lines), head);

    let lines = if rows.is_empty() {
        vec![Line::from(NO_TURNS).dark_gray()]
    } else {
        table(rows)
    };
    let end = lines.len().saturating_sub(usize::from(body.height));
    ui.end.set(end);
    let top = ui.top.min(end);
    frame.render_widget(
        Paragraph::new(lines).scroll((u16::try_from(top).unwrap_or(u16::MAX), 0)),
        body,
    );

    frame.render_widget(
        Paragraph::new(Line::from(report.note).dark_gray()).wrap(Wrap { trim: true }),
        foot,
    );
}

/// The rows in aligned columns: label, cost, tokens, turns.
fn table(rows: &[Row]) -> Vec<Line<'static>> {
    let cells: Vec<[String; 4]> = rows
        .iter()
        .map(|row| {
            let mut label = cut(&notify::sanitise(&row.label));
            if row.archived {
                label.push_str(" (archived)");
            }
            [
                label,
                row.totals.cost_label(),
                row.totals.tokens_label(),
                row.totals.turns_label(),
            ]
        })
        .collect();
    let width = |i: usize| {
        cells
            .iter()
            .map(|c| c[i].chars().count())
            .max()
            .unwrap_or(0)
    };
    let (label, cost, tokens) = (width(0), width(1), width(2));
    cells
        .into_iter()
        .map(|[l, c, t, n]| Line::from(format!("{l:<label$}  {c:>cost$}  {t:<tokens$}  {n}")))
        .collect()
}

fn cut(text: &str) -> String {
    match text.char_indices().nth(LABEL_MAX) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_owned(),
    }
}
