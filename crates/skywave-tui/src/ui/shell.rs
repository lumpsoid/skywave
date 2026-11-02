//! The session's shell runs, which stand in for the timeline while the
//! composer writes shell commands: per run its `$ command` line with its
//! condition, how it stands, and the output's tail. An expanded run shows
//! the whole output ([`whole_output`]), read once from its downloaded blob,
//! the only I/O here; a run taller than the list pages with PgUp/PgDn.

use std::collections::HashMap;
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
use skywave_core::{BlobId, BlobState, SessionView, ShellEnd, ShellRun, ShellRunState};
use skywave_present::format::{
    blob_size, bytes, outgoing_label, queued_age, shell_end_label, shell_took, when_marker,
};
use skywave_present::output::{
    OUTPUT_SHOW_BYTES, WholeOutput, lines_before, output_text, whole_output,
};

use super::outgoing_badge;
use super::timeline::{clean, device_label, wrap};
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
    let mut outputs = ui.shell_outputs.borrow_mut();
    let mut lines = Vec::new();
    let mut shown = None;
    for (i, run) in runs.iter().enumerate() {
        if i > 0 {
            lines.push(Line::from(""));
        }
        let selected = focused && ui.shell_selected == Some(run.run);
        let start = lines.len();
        let output = if ui.shell_expanded.contains(&run.run) {
            Shown::Whole(whole_text(run, &mut outputs))
        } else {
            Shown::Tail
        };
        lines.extend(run_lines(run, selected, output, now, width));
        if selected {
            shown = Some((start, lines.len()));
        }
    }
    // The selected run in sight, whole when it fits, else from where it
    // is scrolled to (PgUp/PgDn); with none selected, the newest.
    let height = area.height as usize;
    let top = match shown {
        Some((start, end)) if end - start <= height => end.saturating_sub(height),
        Some((start, end)) => start + ui.shell_scroll.min(end - start - height),
        None => lines.len(),
    }
    .min(lines.len().saturating_sub(height));
    ui.shell_room
        .set((height, shown.map_or(0, |(start, end)| end - start)));
    let lines: Vec<Line> = lines.into_iter().skip(top).take(height).collect();
    frame.render_widget(Paragraph::new(lines), area);
}

/// One run: the command, how it stands, then its output.
fn run_lines(
    run: &ShellRun,
    selected: bool,
    output: Shown,
    now: DateTime<Utc>,
    width: usize,
) -> Vec<Line<'static>> {
    let expanded = matches!(output, Shown::Whole(_));
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

    for line in output_lines(run, output) {
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

/// What of a run's output shows.
#[derive(Clone, Copy)]
enum Shown<'a> {
    /// The newest [`TAIL_LINES`] lines.
    Tail,
    /// All of it ([`whole_output`]): the downloaded output when it was read
    /// (this text), else the run's own.
    Whole(Option<&'a str>),
}

/// `run`'s downloaded output, read once into `outputs`: `None` while there
/// is none here, or it cannot be read.
fn whole_text<'a>(
    run: &ShellRun,
    outputs: &'a mut HashMap<BlobId, Option<String>>,
) -> Option<&'a str> {
    let WholeOutput::Blob(blob) = whole_output(run) else {
        return None;
    };
    let BlobState::Available { path } = &blob.state else {
        return None;
    };
    outputs
        .entry(blob.id)
        .or_insert_with(|| read_output(path).ok())
        .as_deref()
}

/// The output shown: the newest [`TAIL_LINES`] lines, or expanded all of
/// it, from the downloaded output once it is here; with a note of what is
/// left out.
fn output_lines(run: &ShellRun, shown: Shown) -> Vec<Span<'static>> {
    let (text, all_shown, hint) = match (shown, whole_output(run)) {
        (Shown::Tail, _) => (
            run.tail_or_preview.as_str(),
            false,
            " · Enter shows more".to_owned(),
        ),
        (Shown::Whole(_), WholeOutput::Here(text)) => (text, true, String::new()),
        (Shown::Whole(_), WholeOutput::Partial { text, note }) => {
            (text, true, format!(" · {note}"))
        }
        (Shown::Whole(Some(read)), WholeOutput::Blob(_)) => {
            (read, true, " · y copies them all".to_owned())
        }
        (Shown::Whole(None), WholeOutput::Blob(blob)) => {
            let hint = match &blob.state {
                BlobState::Available { .. } => " · y copies them all".to_owned(),
                state @ (BlobState::Requested { .. } | BlobState::Downloading { .. }) => {
                    let size = blob_size(state).map(bytes).unwrap_or_default();
                    format!(" · downloading {size}")
                }
                _ => " · d downloads them".to_owned(),
            };
            (run.tail_or_preview.as_str(), true, hint)
        }
    };
    let all: Vec<&str> = text.lines().collect();
    let keep = if all_shown { all.len() } else { TAIL_LINES };
    let shown = &all[all.len().saturating_sub(keep)..];
    let mut spans = Vec::new();
    let hidden = lines_before(run, text) + (all.len() - shown.len());
    if hidden > 0 {
        spans.push(Span::raw(format!("… {hidden} earlier lines{hint}")).dark_gray());
    }
    spans.extend(shown.iter().map(|line| Span::raw(clean(line))));
    spans
}

/// The output at `path` as it shows whole: its last [`OUTPUT_SHOW_BYTES`],
/// through [`output_text`].
fn read_output(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    let cut = size > OUTPUT_SHOW_BYTES;
    if cut {
        file.seek(SeekFrom::Start(size - OUTPUT_SHOW_BYTES))?;
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(output_text(&bytes, cut))
}
