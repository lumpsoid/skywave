//! The session's shell runs, which stand in for the timeline while the
//! composer writes shell commands: per run its `$ command` line with its
//! condition, how it stands, and the output's tail. An expanded run shows
//! the whole output, read from its downloaded blob, the only I/O here.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use skywave_core::{BlobState, SessionView, ShellEnd, ShellRun, ShellRunState};
use skywave_present::format::{
    blob_size, bytes, outgoing_label, queued_age, shell_end_label, shell_took, when_marker,
};
use skywave_proto::strip_ansi;

use super::outgoing_badge;
use super::timeline::{SHOW_BYTES, SHOW_LINES, clean, device_label, wrap};
use crate::app::{Focus, SessionUi};

/// Output lines a run not expanded shows: the newest.
const TAIL_LINES: usize = 3;
/// Where a run's lines start, under the selection marker.
const INDENT: &str = "    ";

pub(super) fn render(
    frame: &mut Frame,
    area: Rect,
    session: &SessionView,
    ui: &SessionUi,
    wall: SystemTime,
) {
    let runs = &session.shell.runs;
    if runs.is_empty() {
        frame.render_widget(
            Paragraph::new(
                Line::from("No shell commands yet. Type one below; Ctrl-T writes to the agent.")
                    .dark_gray(),
            ),
            area,
        );
        return;
    }
    let now = DateTime::<Utc>::from(wall);
    let width = area.width as usize;
    let focused = ui.focus == Focus::Timeline;
    let mut lines = Vec::new();
    let mut shown = None;
    for (i, run) in runs.iter().enumerate() {
        if i > 0 {
            lines.push(Line::from(""));
        }
        let selected = focused && ui.shell_selected == Some(run.run);
        let start = lines.len();
        let expanded = ui.shell_expanded.contains(&run.run);
        lines.extend(run_lines(run, selected, expanded, now, width));
        if selected {
            shown = Some((start, lines.len()));
        }
    }
    // The selected run in sight, whole when it fits; else the newest.
    let height = area.height as usize;
    let top = match shown {
        Some((start, end)) if end - start <= height => end.saturating_sub(height),
        Some((start, _)) => start,
        None => lines.len(),
    }
    .min(lines.len().saturating_sub(height));
    let lines: Vec<Line> = lines.into_iter().skip(top).take(height).collect();
    frame.render_widget(Paragraph::new(lines), area);
}

/// One run: the command, how it stands, then its output.
fn run_lines(
    run: &ShellRun,
    selected: bool,
    expanded: bool,
    now: DateTime<Utc>,
    width: usize,
) -> Vec<Line<'static>> {
    let lead = if selected {
        Span::raw("▶ ").cyan().bold()
    } else {
        Span::raw("  ")
    };
    let rest = || vec![Span::raw(INDENT)];
    let mut command = run.command.lines();
    let first = command.next().unwrap_or_default();
    let more: Vec<&str> = command.collect();
    let mut head = vec![
        Span::raw("$ ").magenta().bold(),
        Span::raw(clean(first)).bold(),
    ];
    if !more.is_empty() && !expanded {
        head.push(Span::raw(" …").dark_gray());
    }
    if let Some(marker) = when_marker(run.when) {
        head.push(Span::raw(format!("  [{marker}]")).yellow());
    }
    let mut lines = wrap(vec![lead], rest(), head, width);
    if expanded {
        for line in &more {
            lines.extend(wrap(
                rest(),
                rest(),
                vec![Span::raw(clean(line)).bold()],
                width,
            ));
        }
    }

    let mut state = state_spans(run, now);
    if !run.by.me {
        state.push(Span::raw(format!(" · by {}", device_label(&run.by))).dark_gray());
    }
    if run.stopping && !matches!(run.state, ShellRunState::Finished(_)) {
        state.push(Span::raw(" · stopping").yellow());
    }
    lines.extend(wrap(rest(), rest(), state, width));

    let output = output_lines(run, expanded);
    for line in output {
        lines.extend(wrap(rest(), rest(), vec![line], width));
    }
    lines
}

/// How the run stands: waiting (and since when), running (and for how
/// long), or how it ended (and what it took).
fn state_spans(run: &ShellRun, now: DateTime<Utc>) -> Vec<Span<'static>> {
    match &run.state {
        ShellRunState::Pending | ShellRunState::Queued => {
            if let Some(outgoing) = run.outgoing.as_ref().filter(|_| run.refused()) {
                return vec![Span::styled(
                    outgoing_label(outgoing),
                    outgoing_badge(outgoing).1,
                )];
            }
            let age = queued_age(run, now).unwrap_or_else(|| "queued".to_owned());
            vec![Span::raw(age).dark_gray()]
        }
        ShellRunState::Running => {
            let took = shell_took(run, now).unwrap_or_default();
            vec![Span::raw(format!("running {took}")).cyan()]
        }
        ShellRunState::Finished(end) => {
            let style = match end {
                ShellEnd::Exited { code: 0 } => Style::new().green(),
                ShellEnd::Skipped => Style::new().dark_gray(),
                _ => Style::new().red(),
            };
            let mut label = shell_end_label(end, run.when);
            if let Some(took) = shell_took(run, now) {
                label.push_str(&format!(" · {took}"));
            }
            vec![Span::styled(label, style)]
        }
    }
}

/// The output shown: the newest [`TAIL_LINES`] lines, or expanded all of
/// the preview, or of the downloaded output once it is here; with a note
/// of what is left out.
fn output_lines(run: &ShellRun, expanded: bool) -> Vec<Span<'static>> {
    let blob = run.output.as_ref().filter(|_| expanded);
    let downloaded = blob.and_then(|blob| match &blob.state {
        BlobState::Available { path } => read_tail(path).ok(),
        _ => None,
    });
    let text = match &downloaded {
        Some(text) => text.as_str(),
        None => run.tail_or_preview.as_str(),
    };
    let all: Vec<&str> = text.lines().collect();
    let keep = if expanded { SHOW_LINES } else { TAIL_LINES };
    let shown = &all[all.len().saturating_sub(keep)..];
    let mut spans = Vec::new();
    let hidden = (run.lines as usize).max(all.len()) - shown.len();
    if hidden > 0 {
        let hint = match blob.map(|blob| &blob.state) {
            _ if !expanded => " · Enter shows more".to_owned(),
            Some(BlobState::Available { .. }) => " · y copies them all".to_owned(),
            Some(state @ (BlobState::Requested { .. } | BlobState::Downloading { .. })) => {
                let size = blob_size(state).map(bytes).unwrap_or_default();
                format!(" · downloading {size}")
            }
            Some(_) => " · d downloads them".to_owned(),
            None => String::new(),
        };
        spans.push(Span::raw(format!("… {hidden} earlier lines{hint}")).dark_gray());
    }
    spans.extend(shown.iter().map(|line| Span::raw(clean(line))));
    spans
}

/// The last [`SHOW_BYTES`] of the output at `path`, ANSI stripped.
fn read_tail(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    let cut = size > SHOW_BYTES;
    if cut {
        file.seek(SeekFrom::Start(size - SHOW_BYTES))?;
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let text = strip_ansi(&String::from_utf8_lossy(&bytes));
    // A cut lands mid-line: that line goes.
    Ok(match text.split_once('\n') {
        Some((_, rest)) if cut => rest.to_owned(),
        _ => text,
    })
}
