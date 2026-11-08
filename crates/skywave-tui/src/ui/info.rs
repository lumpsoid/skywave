//! A session's info page: its sections as
//! [`skywave_present::session_info`] words them, the usage limits as the
//! usage limits page shows them, then its subagents.

use chrono::{DateTime, Utc};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use skywave_core::SessionInfoView;
use skywave_present::session_info::{NO_SUBAGENTS, Section, sections, subagents};

use super::limits;
use crate::app::App;
use crate::notify;

pub(super) fn render(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    view: &SessionInfoView,
    now: DateTime<Utc>,
) {
    let mut sections = sections(view).into_iter();
    let mut lines = Vec::new();
    // The session first, then the account's limits, then the rest.
    if let Some(session) = sections.next() {
        lines.extend(section(&session));
    }
    lines.push(Line::from(""));
    lines.push(Line::from("Usage limits").bold());
    lines.extend(limits::lines(&view.limits, now));
    for rest in sections {
        lines.push(Line::from(""));
        lines.extend(section(&rest));
    }
    lines.push(Line::from(""));
    let rows = subagents(view);
    lines.push(Line::from(format!("Subagents ({})", rows.len())).bold());
    if rows.is_empty() {
        lines.push(Line::from(NO_SUBAGENTS).dark_gray());
    }
    for row in rows {
        let mut head = Vec::new();
        if !row.trail.is_empty() {
            head.push(Span::raw(format!("{} › ", notify::sanitise(&row.trail))).dark_gray());
        }
        head.push(Span::raw(notify::sanitise(&row.title)).bold());
        if let Some(detail) = &row.detail {
            head.push(Span::raw(format!(" · {}", notify::sanitise(detail))).dark_gray());
        }
        lines.push(Line::from(head));
        let status = Line::from(format!("  {}", notify::sanitise(&row.status)));
        lines.push(if row.live {
            status.cyan()
        } else {
            status.dark_gray()
        });
    }

    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let ui = &app.ui().info;
    let end = paragraph
        .line_count(area.width)
        .saturating_sub(usize::from(area.height));
    ui.end.set(end);
    let top = ui.top.min(end);
    frame.render_widget(
        paragraph.scroll((u16::try_from(top).unwrap_or(u16::MAX), 0)),
        area,
    );
}

/// A section's title, then its fields with their values aligned.
fn section(section: &Section) -> Vec<Line<'static>> {
    let width = section
        .fields
        .iter()
        .map(|f| notify::sanitise(&f.label).chars().count())
        .max()
        .unwrap_or(0);
    let mut lines = vec![Line::from(section.title).bold()];
    lines.extend(section.fields.iter().map(|field| {
        Line::from(vec![
            Span::raw(format!("  {:<width$}  ", notify::sanitise(&field.label))).dark_gray(),
            Span::raw(notify::sanitise(&field.value)),
        ])
    }));
    lines
}
