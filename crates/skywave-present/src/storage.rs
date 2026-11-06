//! The storage screen: the server's report on what its data takes, as
//! totals and one list at a time (sessions, the largest blobs, the shell
//! runs), worded once for every head.
//!
//! Session titles are text from the server: heads sanitise them (and a
//! shell run's command line) before writing them anywhere raw.

use skywave_core::{
    BlobUse, SessionId, StorageReport, StorageStatus, StorageUnavailable, StorageView,
};

use crate::format;
use crate::usage::short;

/// Which list the screen shows under the totals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Grouping {
    #[default]
    Sessions,
    Blobs,
    ShellRuns,
}

impl Grouping {
    /// In the order a head offers them.
    pub const EVERY: [Grouping; 3] = [Grouping::Sessions, Grouping::Blobs, Grouping::ShellRuns];

    pub fn label(self) -> &'static str {
        match self {
            Grouping::Sessions => "by session",
            Grouping::Blobs => "largest blobs",
            Grouping::ShellRuns => "shell runs",
        }
    }

    /// The grouping after this one, wrapping.
    pub fn next(self) -> Grouping {
        let at = Grouping::EVERY.iter().position(|g| *g == self).unwrap_or(0);
        Grouping::EVERY[(at + 1) % Grouping::EVERY.len()]
    }

    /// What the list says when it has no rows.
    pub fn empty_label(self) -> &'static str {
        match self {
            Grouping::Sessions => "No sessions on the server.",
            Grouping::Blobs => "No event keeps a blob.",
            Grouping::ShellRuns => "No shell runs.",
        }
    }
}

/// One line of the totals: what, and how much.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Total {
    pub label: &'static str,
    pub value: String,
}

/// One row of a list, largest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// The session's title, or a shell run's command; server text.
    pub label: String,
    /// What makes up the size, on one line; may hold a session title.
    pub detail: String,
    pub size: String,
}

/// What the screen says above the report while it has no fresh one;
/// `None` once the report is the server's latest answer.
pub fn status_line(view: &StorageView) -> Option<String> {
    let have = view.report.is_some();
    match &view.status {
        StorageStatus::Ready => None,
        StorageStatus::Loading if have => Some("Refreshing…".to_owned()),
        StorageStatus::Loading => Some("Asking the server…".to_owned()),
        StorageStatus::Unavailable(StorageUnavailable::Offline) => Some(if have {
            "Offline: this report may be out of date.".to_owned()
        } else {
            "Offline: the server's storage is read only when connected.".to_owned()
        }),
        StorageStatus::Unavailable(StorageUnavailable::Failed(reason)) => Some(if have {
            format!("Refresh failed ({reason}); this report may be out of date.")
        } else {
            format!("Could not read the server's storage: {reason}")
        }),
    }
}

/// The report's totals, the free space first.
pub fn totals(report: &StorageReport) -> Vec<Total> {
    let free = match (report.free, report.min_free) {
        (None, _) => "unknown".to_owned(),
        (Some(free), 0) => format::bytes(free),
        (Some(free), floor) => format!(
            "{} · agents pause below {}",
            format::bytes(free),
            format::bytes(floor)
        ),
    };
    let used = report.database
        + report.event_blobs
        + report.workdir_blobs
        + report.shell_running
        + report.log_file.unwrap_or(0);
    let total = |label, n| Total {
        label,
        value: format::bytes(n),
    };
    vec![
        Total {
            label: "Free",
            value: free,
        },
        total("Used", used),
        total("Database", report.database),
        total("Event blobs", report.event_blobs),
        total("Workdir blobs", report.workdir_blobs),
        total("Running shell output", report.shell_running),
        Total {
            label: "Log file",
            value: report
                .log_file
                .map_or_else(|| "none (logs to stderr)".to_owned(), format::bytes),
        },
    ]
}

/// The rows of `grouping`, largest first, as the server ordered them.
pub fn rows(view: &StorageView, grouping: Grouping) -> Vec<Row> {
    let Some(report) = &view.report else {
        return Vec::new();
    };
    let title = |id: SessionId| {
        view.titles
            .get(&id)
            .cloned()
            .unwrap_or_else(|| format!("session {} (not on this device)", short(id)))
    };
    match grouping {
        Grouping::Sessions => report
            .sessions
            .iter()
            .map(|s| Row {
                label: title(s.session),
                detail: format!(
                    "{} · {} in the log · {}",
                    count(s.events, "event"),
                    format::bytes(s.event_bytes),
                    match s.blobs {
                        0 => "no blobs".to_owned(),
                        n => format!("{} {}", count(n, "blob"), format::bytes(s.blob_bytes)),
                    }
                ),
                size: format::bytes(s.event_bytes + s.blob_bytes),
            })
            .collect(),
        Grouping::Blobs => report
            .blobs
            .iter()
            .map(|b| Row {
                label: title(b.session),
                detail: format!("{} · event {}", holds_label(b.holds), b.seq.0),
                size: format::bytes(b.size),
            })
            .collect(),
        Grouping::ShellRuns => report
            .shell_runs
            .iter()
            .map(|r| Row {
                label: format!("$ {}", r.command_line),
                detail: format!(
                    "{} · {} progress · {} output",
                    title(r.session),
                    format::bytes(r.event_bytes),
                    format::bytes(r.output)
                ),
                size: format::bytes(r.event_bytes + r.output),
            })
            .collect(),
    }
}

/// What a blob holds, in words.
pub fn holds_label(holds: BlobUse) -> &'static str {
    match holds {
        BlobUse::ToolInput => "tool input",
        BlobUse::ToolBody => "tool diff or body",
        BlobUse::ToolOutput => "tool output",
        BlobUse::ApprovalInput => "approval input",
        BlobUse::ShellOutput => "shell output",
    }
}

fn count(n: u64, what: &str) -> String {
    if n == 1 {
        format!("1 {what}")
    } else {
        format!("{n} {what}s")
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use skywave_core::{BlobId, BlobStorage, CommandId, Seq, SessionStorage, ShellRunStorage};

    use super::*;

    fn report() -> StorageReport {
        StorageReport {
            database: 2 * 1024 * 1024,
            event_blobs: 3 * 1024 * 1024,
            workdir_blobs: 0,
            shell_running: 512,
            log_file: None,
            free: Some(20 * 1024 * 1024 * 1024),
            min_free: 1024 * 1024 * 1024,
            sessions: Vec::new(),
            blobs: Vec::new(),
            shell_runs: Vec::new(),
        }
    }

    fn view(report: Option<StorageReport>, status: StorageStatus) -> StorageView {
        StorageView {
            status,
            report,
            titles: HashMap::new(),
        }
    }

    #[test]
    fn totals_lead_with_the_free_space_and_its_floor() {
        let totals = totals(&report());
        assert_eq!(
            totals[0],
            Total {
                label: "Free",
                value: "20.0 GiB · agents pause below 1.0 GiB".into()
            }
        );
        assert_eq!(totals[1].value, "5.0 MiB");
        assert_eq!(totals.last().unwrap().value, "none (logs to stderr)");
    }

    #[test]
    fn rows_name_sessions_by_title_or_say_they_are_not_here() {
        let (here, gone) = (SessionId::new(), SessionId::new());
        let mut report = report();
        report.sessions = vec![SessionStorage {
            session: here,
            events: 1,
            event_bytes: 2048,
            blobs: 2,
            blob_bytes: 1024 * 1024,
        }];
        report.blobs = vec![BlobStorage {
            id: BlobId(uuid::Uuid::nil()),
            size: 1024 * 1024,
            session: gone,
            seq: Seq(9),
            holds: BlobUse::ToolOutput,
        }];
        report.shell_runs = vec![ShellRunStorage {
            run: CommandId::new(),
            session: here,
            command_line: "make".into(),
            event_bytes: 1024,
            output: 0,
            output_blob: None,
        }];
        let mut view = view(Some(report), StorageStatus::Ready);
        view.titles.insert(here, "Fix the log".into());

        let sessions = rows(&view, Grouping::Sessions);
        assert_eq!(
            sessions,
            vec![Row {
                label: "Fix the log".into(),
                detail: "1 event · 2.0 KiB in the log · 2 blobs 1.0 MiB".into(),
                size: "1.0 MiB".into(),
            }]
        );
        let blobs = rows(&view, Grouping::Blobs);
        assert!(blobs[0].label.ends_with("(not on this device)"));
        assert_eq!(blobs[0].detail, "tool output · event 9");
        let runs = rows(&view, Grouping::ShellRuns);
        assert_eq!(runs[0].label, "$ make");
        assert_eq!(
            runs[0].detail,
            "Fix the log · 1.0 KiB progress · 0 B output"
        );
    }

    #[test]
    fn the_status_line_says_whether_the_report_is_fresh() {
        assert_eq!(
            status_line(&view(Some(report()), StorageStatus::Ready)),
            None
        );
        assert_eq!(
            status_line(&view(None, StorageStatus::Loading)).unwrap(),
            "Asking the server…"
        );
        let failed = StorageStatus::Unavailable(StorageUnavailable::Failed("timed out".into()));
        assert_eq!(
            status_line(&view(Some(report()), failed)).unwrap(),
            "Refresh failed (timed out); this report may be out of date."
        );
        assert!(rows(&view(None, StorageStatus::Loading), Grouping::Blobs).is_empty());
    }

    #[test]
    fn groupings_cycle() {
        assert_eq!(Grouping::default().next(), Grouping::Blobs);
        assert_eq!(Grouping::ShellRuns.next(), Grouping::Sessions);
    }
}
