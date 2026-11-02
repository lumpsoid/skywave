//! The open session's shell runs as [`ShellRunRow`]s, worded as the TUI's
//! run list words them, and the keyed diff that keeps Dart's list in step.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use chrono::{DateTime, Utc};
use skywave_core::{
    Action, BlobId, BlobState, CommandId, RunWhen, SessionView, ShellEnd, ShellRun, ShellRunState,
};
use skywave_present::format::{
    outgoing_label, queued_label, shell_end_label, shell_prompt, shell_took, when_marker,
};
use skywave_present::output::{
    OUTPUT_SHOW_BYTES, WholeOutput, lines_before, output_text, whole_output,
};

use crate::signals::{ShellPromptView, ShellRunKind, ShellRunRow, ShellUpdate};
use crate::timeline::{blob_facts, clean_line, clean_text, outgoing_view};

/// The downloaded outputs expanded rows showed, by blob, as `whole` holds
/// them: a file is read once, not on every snapshot or tick. A downloaded
/// blob's file does not change.
#[derive(Debug, Default)]
pub struct Outputs {
    read: HashMap<BlobId, Result<String, String>>,
}

impl Outputs {
    fn get(&mut self, id: BlobId, path: &Path) -> &Result<String, String> {
        self.read.entry(id).or_insert_with(|| {
            read_output(path)
                .map(|text| clean_text(&text))
                .map_err(|err| format!("cannot read {}: {err}", path.display()))
        })
    }
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

/// Every run of `view`'s, oldest first, at `wall_ms`; the `expanded` ones
/// with their whole output.
pub fn rows(
    view: &SessionView,
    expanded: &HashSet<CommandId>,
    outputs: &mut Outputs,
    wall_ms: i64,
) -> Vec<ShellRunRow> {
    let now = DateTime::<Utc>::from_timestamp_millis(wall_ms).unwrap_or_default();
    view.shell
        .runs
        .iter()
        .map(|run| {
            let mut row = row(run, now);
            if expanded.contains(&run.run) {
                row.expanded = true;
                (row.whole, row.whole_note) = whole(run, outputs);
            }
            row
        })
        .collect()
}

/// An expanded run's `whole` and `whole_note`.
fn whole(run: &ShellRun, outputs: &mut Outputs) -> (Option<String>, Option<String>) {
    let earlier = |text: &str, why: Option<&str>| {
        let n = lines_before(run, text);
        let why = why.map(|why| format!(" · {why}")).unwrap_or_default();
        (n > 0).then(|| format!("… {n} earlier lines not shown{why}"))
    };
    match whole_output(run) {
        WholeOutput::Here(text) => (Some(clean_text(text)), None),
        WholeOutput::Partial { text, note } => (
            Some(clean_text(text)),
            Some(earlier(text, Some(note)).unwrap_or_else(|| note.to_owned())),
        ),
        WholeOutput::Blob(blob) => match &blob.state {
            BlobState::Available { path } => match outputs.get(blob.id, path) {
                Ok(text) => (Some(text.clone()), earlier(text, None)),
                Err(err) => (None, Some(err.clone())),
            },
            _ => (None, None),
        },
    }
}

/// The download an expanded run's output needs: none while it is here or
/// on its way, or for a run without one.
pub fn output_fetch(view: &SessionView, run: CommandId) -> Option<Action> {
    let blob = view
        .shell
        .runs
        .iter()
        .find(|r| r.run == run)?
        .output
        .as_ref()?;
    matches!(
        blob.state,
        BlobState::NotDownloaded { .. } | BlobState::Failed { .. }
    )
    .then_some(Action::DownloadBlob(blob.id))
}

fn row(run: &ShellRun, now: DateTime<Utc>) -> ShellRunRow {
    let waiting = matches!(run.state, ShellRunState::Pending | ShellRunState::Queued);
    let finished = matches!(run.state, ShellRunState::Finished(_));
    let (state, mut state_label) = state(run, now);
    if run.stopping && !finished {
        state_label.push_str(" · stopping");
    }
    ShellRunRow {
        key: run.run.0.to_string(),
        command: clean_text(&run.command),
        when_label: when_marker(run.when).map(str::to_owned),
        state,
        state_label,
        by_label: (!run.by.me).then(|| format!("by {}", clean_line(&run.by.label()))),
        tail: clean_text(&run.tail_or_preview),
        lines: run.lines,
        output: run.output.as_ref().map(blob_facts),
        truncated: run.truncated,
        expanded: false,
        whole: None,
        whole_note: None,
        outgoing: run.outgoing.as_ref().map(outgoing_view),
        stopping: run.stopping,
        discardable: run.discardable,
        can_stop: (waiting || run.state == ShellRunState::Running)
            && !run.stopping
            && !run.refused(),
    }
}

/// How the run stands: waiting (and since when), running (and for how
/// long), or how it ended (and what it took).
fn state(run: &ShellRun, now: DateTime<Utc>) -> (ShellRunKind, String) {
    match &run.state {
        ShellRunState::Pending | ShellRunState::Queued => {
            if let Some(outgoing) = run.outgoing.as_ref().filter(|_| run.refused()) {
                return (ShellRunKind::Refused, outgoing_label(outgoing));
            }
            let kind = if run.state == ShellRunState::Pending {
                ShellRunKind::Pending
            } else {
                ShellRunKind::Queued
            };
            (
                kind,
                queued_label(run).unwrap_or_else(|| "queued".to_owned()),
            )
        }
        ShellRunState::Running => {
            let took = shell_took(run, now).unwrap_or_default();
            (ShellRunKind::Running, format!("running {took}"))
        }
        ShellRunState::Finished(end) => {
            let kind = match end {
                ShellEnd::Exited { code: 0 } => ShellRunKind::Succeeded,
                ShellEnd::Stopped => ShellRunKind::Stopped,
                ShellEnd::Skipped => ShellRunKind::Skipped,
                _ => ShellRunKind::Failed,
            };
            let mut label = clean_line(&shell_end_label(end, run.when));
            if let Some(took) = shell_took(run, now) {
                label.push_str(&format!(" · {took}"));
            }
            (kind, label)
        }
    }
}

/// Whether the rows show a clock that runs: a run's time running.
pub fn live(view: &SessionView) -> bool {
    view.shell
        .runs
        .iter()
        .any(|run| run.state == ShellRunState::Running)
}

/// The shell composer's prompt for `view`, the next command running on
/// `when`.
pub fn prompt(view: &SessionView, when: RunWhen) -> ShellPromptView {
    let prompt = shell_prompt(view.shell.cwd.as_deref(), when);
    ShellPromptView {
        cwd: prompt.cwd.map(clean_line),
        when_label: prompt.marker.map(str::to_owned),
        sigil: prompt.sigil.to_owned(),
    }
}

/// The condition after `when` in the chip's cycle: always → if ok → if
/// failed → always.
pub fn next_when(when: RunWhen) -> RunWhen {
    match when {
        RunWhen::Always => RunWhen::OnSuccess,
        RunWhen::OnSuccess => RunWhen::OnFailure,
        RunWhen::OnFailure => RunWhen::Always,
    }
}

/// What one run's own stop does, as [`shell_stop`] would for it: this
/// device's run the server never saw is taken back, any other waiting or
/// running run stopped. `None` for a run that is over, already stopping,
/// or refused, or one `view` does not have.
///
/// [`shell_stop`]: skywave_present::policy::shell_stop
pub fn run_stop(view: &SessionView, run: skywave_core::CommandId) -> Option<Action> {
    let run = view.shell.runs.iter().find(|r| r.run == run)?;
    let live = matches!(
        run.state,
        ShellRunState::Pending | ShellRunState::Queued | ShellRunState::Running
    );
    if !live || run.stopping || run.refused() {
        return None;
    }
    let waiting = run.state != ShellRunState::Running;
    Some(if waiting && run.by.me && run.discardable {
        Action::Discard(run.run)
    } else {
        Action::StopShell {
            session_id: view.id,
            run: run.run,
        }
    })
}

/// The updates that turn Dart's `old` rows into `new`: removals, then
/// upserts by ascending index. `None` when the change is not a keyed diff
/// (rows that stayed changed order): send a snapshot.
pub fn diff(old: &[ShellRunRow], new: &[ShellRunRow]) -> Option<Vec<ShellUpdate>> {
    let new_keys: HashSet<&str> = new.iter().map(|r| r.key.as_str()).collect();
    let old_at: HashMap<&str, usize> = old
        .iter()
        .enumerate()
        .map(|(i, r)| (r.key.as_str(), i))
        .collect();
    let stayed_old = old
        .iter()
        .map(|r| r.key.as_str())
        .filter(|k| new_keys.contains(k));
    let stayed_new = new
        .iter()
        .map(|r| r.key.as_str())
        .filter(|k| old_at.contains_key(k));
    if !stayed_old.eq(stayed_new) {
        return None;
    }
    let mut out: Vec<ShellUpdate> = old
        .iter()
        .filter(|r| !new_keys.contains(r.key.as_str()))
        .map(|r| ShellUpdate::Removed { key: r.key.clone() })
        .collect();
    for (index, row) in new.iter().enumerate() {
        if old_at
            .get(row.key.as_str())
            .is_some_and(|&i| old[i] == *row)
        {
            continue;
        }
        out.push(ShellUpdate::Upserted {
            key: row.key.clone(),
            run: row.clone(),
            index: crate::link::count(index),
        });
    }
    Some(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn row(key: &str, label: &str) -> ShellRunRow {
        ShellRunRow {
            key: key.into(),
            command: "make".into(),
            when_label: None,
            state: ShellRunKind::Queued,
            state_label: label.into(),
            by_label: None,
            tail: String::new(),
            lines: 0,
            output: None,
            truncated: false,
            expanded: false,
            whole: None,
            whole_note: None,
            outgoing: None,
            stopping: false,
            discardable: false,
            can_stop: true,
        }
    }

    /// Applies `updates` to `rows` as Dart's holder does.
    fn apply(rows: &mut Vec<ShellRunRow>, updates: Vec<ShellUpdate>) {
        for update in updates {
            match update {
                ShellUpdate::Snapshot { runs, .. } => *rows = runs,
                ShellUpdate::Upserted { key, run, index } => {
                    rows.retain(|r| r.key != key);
                    rows.insert(index as usize, run);
                }
                ShellUpdate::Removed { key } => rows.retain(|r| r.key != key),
            }
        }
    }

    #[test]
    fn a_diff_rebuilds_the_new_rows_and_sends_only_changes() {
        let old = vec![row("a", "queued 1s"), row("b", "queued 1s"), row("c", "x")];
        let new = vec![row("a", "running 1s"), row("c", "x"), row("d", "queued 0s")];
        let updates = diff(&old, &new).unwrap();
        assert_eq!(
            updates
                .iter()
                .map(|u| match u {
                    ShellUpdate::Removed { key } => format!("-{key}"),
                    ShellUpdate::Upserted { key, index, .. } => format!("+{key}@{index}"),
                    ShellUpdate::Snapshot { .. } => "snapshot".into(),
                })
                .collect::<Vec<_>>(),
            ["-b", "+a@0", "+d@2"]
        );
        let mut rows = old;
        apply(&mut rows, updates);
        assert_eq!(rows, new);
    }

    #[test]
    fn a_reorder_is_a_snapshot() {
        let old = vec![row("a", "x"), row("b", "x")];
        let new = vec![row("b", "x"), row("a", "x")];
        assert_eq!(diff(&old, &new), None);
        assert_eq!(diff(&old, &old), Some(Vec::new()));
    }
}
