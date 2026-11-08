//! A shell run's whole output as every head shows it: where it is (the
//! run's own text, or its output blob), how much of a downloaded blob is
//! read, and how the raw bytes read become text. The heads read the file;
//! the rules are here.

use std::borrow::Cow;

use skywave_core::{Action, BlobState, BlobView, FullView, ShellRun, ShellRunState};
use skywave_proto::strip_ansi;

use crate::sections::{deleted_note, whole_lines};

/// Most of a downloaded output a head reads to show it whole: its newest
/// bytes, so a huge output shows its end, where the result is. Past it the
/// earlier lines are only counted; a copy still takes the file.
pub const OUTPUT_SHOW_BYTES: u64 = 512 * 1024;

/// Where a run's whole output is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WholeOutput<'a> {
    /// All of it is this text, ANSI stripped already: a finished run whose
    /// preview holds it, or a run with no output yet.
    Here(&'a str),
    /// Its output blob holds it, raw: read it once downloaded (its last
    /// [`OUTPUT_SHOW_BYTES`], through [`output_text`]), else download it.
    Blob(&'a BlobView),
    /// Only `text` is known, and `note` says why: the tail so far of a run
    /// still going, or the preview of a finished run whose output was not
    /// kept or was deleted on the server ([`deleted_note`]).
    Partial {
        text: &'a str,
        note: Cow<'static, str>,
    },
}

impl WholeOutput<'_> {
    /// What a head adds to its "copied" notice for a copy of this output's
    /// text: for [`WholeOutput::Partial`], that only the kept tail went
    /// and why (`only the kept tail · Deleted by laptop`), so a copy of
    /// part of an output never passes for all of it. `None` otherwise.
    pub fn copy_note(&self) -> Option<String> {
        match self {
            WholeOutput::Partial { note, .. } => Some(format!("only the kept tail · {note}")),
            WholeOutput::Here(_) | WholeOutput::Blob(_) => None,
        }
    }
}

/// The note on a running run's output: its whole output is a blob only
/// once it finished.
pub const OUTPUT_SO_FAR: &str = "the newest lines so far; the whole output once it finishes";

/// The note on a finished run whose preview is cut and has no blob.
pub const OUTPUT_LOST: &str = "the server could not keep the rest of the output";

/// Where `run`'s whole output is: its blob when it has one, else its own
/// text, which is all of it unless it is cut.
pub fn whole_output(run: &ShellRun) -> WholeOutput<'_> {
    let text = run.tail_or_preview.as_str();
    match &run.output {
        Some(FullView::Blob(blob)) => return WholeOutput::Blob(blob),
        // A run's output is never inline; were it, it would be all of it.
        Some(FullView::Inline(whole)) => return WholeOutput::Here(whole),
        Some(FullView::Deleted { by }) => {
            return WholeOutput::Partial {
                text,
                note: Cow::Owned(deleted_note(by)),
            };
        }
        Some(FullView::Lost) | None => {}
    }
    match run.state {
        ShellRunState::Finished(_) if run.truncated => WholeOutput::Partial {
            text,
            note: Cow::Borrowed(OUTPUT_LOST),
        },
        ShellRunState::Running if lines_before(run, text) > 0 => WholeOutput::Partial {
            text,
            note: Cow::Borrowed(OUTPUT_SO_FAR),
        },
        _ => WholeOutput::Here(text),
    }
}

/// The text of `bytes`, raw output read from a blob: ANSI stripped
/// ([`strip_ansi`]), invalid UTF-8 decoded lossily. `cut`: the bytes are
/// the file's tail, so their first line is likely partial and goes.
pub fn output_text(bytes: &[u8], cut: bool) -> String {
    let text = strip_ansi(&String::from_utf8_lossy(bytes));
    match text.split_once('\n') {
        Some((_, rest)) if cut => rest.to_owned(),
        _ => text,
    }
}

/// The download `run`'s output blob needs before it can be shown or copied
/// whole: none while it is here or on its way, or for a run without one.
/// A failed download is asked for again.
pub fn output_download(run: &ShellRun) -> Option<Action> {
    let blob = run.output.as_ref()?.blob()?;
    matches!(
        blob.state,
        BlobState::NotDownloaded { .. } | BlobState::Failed { .. }
    )
    .then_some(Action::DownloadBlob(blob.id))
}

/// The line count of `run`'s whole output so far: what a crop kept once
/// the server cropped it, else the run's own count ([`whole_lines`]).
pub fn output_lines(run: &ShellRun) -> u32 {
    whole_lines(run.lines, run.output.as_ref())
}

/// How many of `run`'s output lines ([`output_lines`]) come before `text`,
/// the newest of them: none when `text` holds them all.
pub fn lines_before(run: &ShellRun, text: &str) -> usize {
    (output_lines(run) as usize).saturating_sub(text.lines().count())
}

#[cfg(test)]
mod tests {
    use skywave_core::{BlobId, CommandId, DeviceId, DeviceRef, RunWhen, ShellEnd};

    use super::*;

    fn run(state: ShellRunState, text: &str, lines: u32) -> ShellRun {
        ShellRun {
            run: CommandId::new(),
            command: "make".into(),
            when: RunWhen::Always,
            by: DeviceRef {
                id: DeviceId::new(),
                name: None,
                me: true,
            },
            state,
            requested_at: None,
            started_at: None,
            finished_at: None,
            tail_or_preview: text.into(),
            lines,
            output: None,
            truncated: false,
            discardable: false,
            outgoing: None,
            stopping: false,
            delivery: None,
        }
    }

    const DONE: ShellRunState = ShellRunState::Finished(ShellEnd::Exited { code: 0 });

    #[test]
    fn the_whole_output_is_the_blob_else_the_runs_own_text() {
        let short = run(DONE, "a\nb", 2);
        assert_eq!(whole_output(&short), WholeOutput::Here("a\nb"));
        assert_eq!(
            whole_output(&run(ShellRunState::Queued, "", 0)),
            WholeOutput::Here("")
        );

        let mut long = run(DONE, "y\nz", 40);
        long.truncated = true;
        assert_eq!(
            whole_output(&long),
            WholeOutput::Partial {
                text: "y\nz",
                note: OUTPUT_LOST.into()
            }
        );
        let blob = BlobView {
            id: BlobId::new(),
            state: BlobState::NotDownloaded { size: 9 },
            change: None,
            pending: None,
        };
        long.output = Some(FullView::Blob(blob.clone()));
        assert_eq!(whole_output(&long), WholeOutput::Blob(&blob));
        // Deleted on the server: the preview is all there is, and says so.
        long.output = Some(FullView::Deleted {
            by: DeviceRef {
                id: DeviceId::new(),
                name: Some("laptop".into()),
                me: false,
            },
        });
        assert_eq!(
            whole_output(&long),
            WholeOutput::Partial {
                text: "y\nz",
                note: "Deleted by laptop".into()
            }
        );
        // A copy of it says it is only the tail, and why.
        assert_eq!(
            whole_output(&long).copy_note().as_deref(),
            Some("only the kept tail · Deleted by laptop")
        );
        assert_eq!(whole_output(&short).copy_note(), None);
        let blob_run = {
            let mut r = run(DONE, "y\nz", 40);
            r.output = Some(FullView::Blob(blob.clone()));
            r
        };
        assert_eq!(whole_output(&blob_run).copy_note(), None);

        // Running: the tail is all of it only while no line is left out.
        let going = run(ShellRunState::Running, "a\nb", 2);
        assert_eq!(whole_output(&going), WholeOutput::Here("a\nb"));
        let going = run(ShellRunState::Running, "y\nz", 3);
        assert_eq!(
            whole_output(&going),
            WholeOutput::Partial {
                text: "y\nz",
                note: OUTPUT_SO_FAR.into()
            }
        );
    }

    #[test]
    fn output_is_stripped_and_a_cut_drops_its_partial_first_line() {
        let raw = b"ial line\n\x1b[31mred\x1b[0m\n\xffok\n";
        assert_eq!(output_text(raw, true), "red\n\u{fffd}ok\n");
        assert_eq!(output_text(raw, false), "ial line\nred\n\u{fffd}ok\n");
        // A cut tail of one line keeps it: nothing else would show.
        assert_eq!(output_text(b"no newline", true), "no newline");
    }

    #[test]
    fn an_output_is_downloaded_unless_here_or_on_its_way() {
        let id = BlobId::new();
        let with = |state: BlobState| {
            let mut done = run(DONE, "y\nz", 40);
            done.output = Some(FullView::Blob(BlobView {
                id,
                state,
                change: None,
                pending: None,
            }));
            output_download(&done)
        };
        let fetch = Some(Action::DownloadBlob(id));
        assert_eq!(with(BlobState::NotDownloaded { size: 9 }), fetch);
        // A failed download is retried.
        assert_eq!(
            with(BlobState::Failed {
                size: 9,
                reason: "gone".into()
            }),
            fetch
        );
        assert_eq!(with(BlobState::Requested { size: 9 }), None);
        assert_eq!(
            with(BlobState::Downloading {
                got: 1,
                size: 9,
                paused: None
            }),
            None
        );
        assert_eq!(
            with(BlobState::Available {
                path: "/blobs/x".into()
            }),
            None
        );
        // No blob, or none kept: nothing to download.
        assert_eq!(output_download(&run(DONE, "y\nz", 2)), None);
        let mut lost = run(DONE, "y\nz", 40);
        lost.output = Some(FullView::Lost);
        assert_eq!(output_download(&lost), None);
    }

    #[test]
    fn lines_before_counts_what_the_text_leaves_out() {
        assert_eq!(lines_before(&run(DONE, "y\nz\n", 40), "y\nz\n"), 38);
        assert_eq!(lines_before(&run(DONE, "a", 1), "a"), 0);
        assert_eq!(lines_before(&run(DONE, "", 0), ""), 0);
    }

    #[test]
    fn a_cropped_output_counts_only_the_kept_lines() {
        let mut cut = run(DONE, "out 399\nout 400\n", 400);
        cut.truncated = true;
        cut.output = Some(FullView::Blob(BlobView {
            id: BlobId::new(),
            state: BlobState::Available {
                path: "/blobs/x".into(),
            },
            pending: None,
            change: Some(skywave_core::BlobChange::Cropped {
                kept: 5,
                of: 400,
                by: DeviceRef {
                    id: DeviceId::new(),
                    name: None,
                    me: true,
                },
            }),
        }));
        assert_eq!(output_lines(&cut), 5);
        // The downloaded text holds every kept line: none are earlier.
        let kept = "a\nb\nc\nd\ne\n";
        assert_eq!(lines_before(&cut, kept), 0);
        // The preview, from the original, leaves out at most what was kept.
        assert_eq!(lines_before(&cut, "out 399\nout 400\n"), 3);
        // Control: uncropped, the run's own count.
        cut.output = None;
        assert_eq!(lines_before(&cut, kept), 395);
    }
}
