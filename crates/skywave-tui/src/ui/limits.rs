//! The usage limits page: the account's limits as
//! [`skywave_present::limits`] words them, each with a bar.

use chrono::{DateTime, Utc};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use skywave_core::UsageLimitsView;
use skywave_present::limits::{Body, LimitRow, body, status_line};

use crate::notify;

/// Cells of a full bar.
const BAR: usize = 30;

pub(super) fn render(frame: &mut Frame, area: Rect, view: &UsageLimitsView, now: DateTime<Utc>) {
    let mut lines = Vec::new();
    if let Some(status) = status_line(view) {
        lines.push(Line::from(status).dark_gray());
        lines.push(Line::from(""));
    }
    match body(view.answer.as_ref(), now) {
        Body::Empty(text) => lines.push(Line::from(text).dark_gray()),
        Body::Limits { read, rows } => {
            lines.push(Line::from(read).dark_gray());
            let width = rows
                .iter()
                .map(|r| notify::sanitise(&r.label).chars().count())
                .max()
                .unwrap_or(0);
            for row in &rows {
                lines.push(Line::from(""));
                lines.extend(limit(row, width));
            }
        }
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

/// A limit as its label, use and reset, then its bar.
fn limit(row: &LimitRow, width: usize) -> [Line<'static>; 2] {
    let label = notify::sanitise(&row.label);
    let percent = row.percent.as_deref().unwrap_or("");
    let mut head = vec![
        Span::raw(format!("{label:<width$}  ")).bold(),
        Span::raw(format!("{percent:>4}")),
    ];
    if let Some(resets) = &row.resets {
        head.push(Span::raw(format!("  {resets}")).dark_gray());
    }
    let style = if row.warning {
        Style::new().yellow()
    } else {
        Style::new().cyan()
    };
    let bar = match row.fill {
        Some(fill) => {
            let full = ((fill * BAR as f64).round() as usize).min(BAR);
            Line::from(vec![
                Span::styled("█".repeat(full), style),
                Span::raw("░".repeat(BAR - full)).dark_gray(),
            ])
        }
        None => Line::from("use not reported").dark_gray(),
    };
    [Line::from(head), bar]
}
