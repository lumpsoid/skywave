//! The storage screen: the server's report on what its data takes, as
//! totals and one list at a time (sessions, the largest blobs, the shell
//! runs), worded once for every head.
//!
//! Session titles are text from the server: heads sanitise them (and a
//! shell run's command line) before writing them anywhere raw.

use skywave_core::{
    BlobId, BlobPending, BlobUse, SessionId, StorageReport, StorageStatus, StorageUnavailable,
    StorageView,
};

use crate::blob_actions::{BlobAction, BlobTarget, offers_for};
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
    /// What a head may do from this row; `None` for a session row and a
    /// shell run whose output is kept as no blob.
    pub actions: Option<RowActions>,
}

/// What may be done to the blob a storage row stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowActions {
    /// The blob to act on: the row's own, or a shell run's `output_blob`.
    pub blob: BlobId,
    /// In the order a head offers them; empty while `pending`.
    pub offers: Vec<BlobAction>,
    /// This device's own delete or crop of the blob, as a timeline item
    /// shows it ([`pending_note`](crate::blob_actions::pending_note)).
    pub pending: Option<BlobPending>,
}

impl RowActions {
    /// Delete any blob; crop one of a croppable kind
    /// ([`croppable`](crate::blob_actions::croppable)) only
    /// when its session is in this device's replica, whose timeline holds
    /// the lines a crop is chosen from. Nothing while this device's own
    /// delete or crop of it is pending, as
    /// [`item_offers`](crate::blob_actions::item_offers), nor once the
    /// replica's log holds a delete or crop of it ([`StorageView::gone`]),
    /// which the report may predate.
    fn of(view: &StorageView, blob: BlobId, holds: BlobUse, session: SessionId) -> RowActions {
        let pending = view.pending.get(&blob).copied();
        let offers = if pending.is_some() || view.gone.contains(&blob) {
            Vec::new()
        } else if view.titles.contains_key(&session) {
            offers_for(holds)
        } else {
            vec![BlobAction::Delete]
        };
        RowActions {
            blob,
            offers,
            pending,
        }
    }
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
        StorageStatus::Reclaiming => Some("Reclaiming space on the server…".to_owned()),
        StorageStatus::Unavailable(StorageUnavailable::NoRoomToReclaim) => Some(if have {
            "Not enough free disk on the server to reclaim space; this report may be out of date."
                .to_owned()
        } else {
            "Not enough free disk on the server to reclaim space".to_owned()
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
                actions: None,
            })
            .collect(),
        Grouping::Blobs => report
            .blobs
            .iter()
            .map(|b| Row {
                label: title(b.session),
                detail: format!("{} · event {}", holds_label(b.holds), b.seq.0),
                size: format::bytes(b.size),
                actions: Some(RowActions::of(view, b.id, b.holds, b.session)),
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
                actions: r
                    .output_blob
                    .map(|blob| RowActions::of(view, blob, BlobUse::ShellOutput, r.session)),
            })
            .collect(),
    }
}

/// The blob the `at`th row of `grouping` ([`rows`]) acts on, with what the
/// crop page needs: a blob row's own, titled by its session and what it
/// holds; a shell run's output, titled by its command. `None` for a
/// session row and a run kept as no blob.
pub fn row_target(view: &StorageView, grouping: Grouping, at: usize) -> Option<BlobTarget> {
    let report = view.report.as_ref()?;
    let row = rows(view, grouping).into_iter().nth(at)?;
    let actions = row.actions?;
    let (session, holds, title) = match grouping {
        Grouping::Sessions => return None,
        Grouping::Blobs => {
            let blob = report.blobs.get(at)?;
            let title = format!("{} · {}", row.label, holds_label(blob.holds));
            (blob.session, blob.holds, title)
        }
        Grouping::ShellRuns => {
            let run = report.shell_runs.get(at)?;
            (run.session, BlobUse::ShellOutput, run.command_line.clone())
        }
    };
    Some(BlobTarget {
        blob: actions.blob,
        offers: actions.offers,
        pending: actions.pending,
        session,
        holds,
        title,
    })
}

/// What a blob holds, in words.
pub fn holds_label(holds: BlobUse) -> &'static str {
    match holds {
        BlobUse::ToolInput => "tool input",
        BlobUse::ToolBody => "tool diff or body",
        BlobUse::ToolOutput => "tool output",
        BlobUse::ApprovalInput => "approval input",
        BlobUse::ShellOutput => "shell output",
        BlobUse::Message => "message",
        BlobUse::Attachment => "image",
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
    use std::collections::{HashMap, HashSet};

    use skywave_core::{
        Action, BlobId, BlobStorage, CommandId, Seq, SessionStorage, ShellRunStorage,
    };

    use super::*;

    fn report() -> StorageReport {
        StorageReport {
            database: 2 * 1024 * 1024,
            reclaimable: 0,
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
            pending: HashMap::new(),
            gone: HashSet::new(),
            device: None,
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
                actions: None,
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

    fn blob(session: SessionId, holds: BlobUse) -> BlobStorage {
        BlobStorage {
            id: BlobId::new(),
            size: 1024,
            session,
            seq: Seq(3),
            holds,
        }
    }

    fn run(session: SessionId, output_blob: Option<BlobId>) -> ShellRunStorage {
        ShellRunStorage {
            run: CommandId::new(),
            session,
            command_line: "make".into(),
            event_bytes: 10,
            output: 2048,
            output_blob,
        }
    }

    fn offers(row: &Row) -> Option<(BlobId, Vec<BlobAction>)> {
        row.actions.as_ref().map(|a| (a.blob, a.offers.clone()))
    }

    #[test]
    fn blob_rows_offer_a_delete_and_a_crop_of_output_held_here() {
        use BlobAction::{Crop, Delete};
        let (here, gone) = (SessionId::new(), SessionId::new());
        let mut report = report();
        report.blobs = vec![
            blob(here, BlobUse::ToolOutput),
            blob(here, BlobUse::ShellOutput),
            blob(here, BlobUse::ToolInput),
            blob(here, BlobUse::ToolBody),
            blob(here, BlobUse::ApprovalInput),
            blob(gone, BlobUse::ToolOutput),
        ];
        let ids: Vec<BlobId> = report.blobs.iter().map(|b| b.id).collect();
        let mut view = view(Some(report), StorageStatus::Ready);
        view.titles.insert(here, "Fix the log".into());

        let rows = rows(&view, Grouping::Blobs);
        let got: Vec<_> = rows.iter().map(offers).collect();
        assert_eq!(
            got,
            vec![
                Some((ids[0], vec![Crop, Delete])),
                Some((ids[1], vec![Crop, Delete])),
                Some((ids[2], vec![Delete])),
                Some((ids[3], vec![Delete])),
                Some((ids[4], vec![Delete])),
                // Not in this replica: no lines to choose a crop from.
                Some((ids[5], vec![Delete])),
            ]
        );
    }

    #[test]
    fn run_rows_act_on_their_output_blob_and_offer_nothing_without_one() {
        use BlobAction::{Crop, Delete};
        let (here, gone) = (SessionId::new(), SessionId::new());
        let (kept, elsewhere) = (BlobId::new(), BlobId::new());
        let mut report = report();
        report.shell_runs = vec![
            run(here, Some(kept)),
            run(here, None),
            run(gone, Some(elsewhere)),
        ];
        let mut view = view(Some(report), StorageStatus::Ready);
        view.titles.insert(here, "Fix the log".into());

        let rows = rows(&view, Grouping::ShellRuns);
        let got: Vec<_> = rows.iter().map(offers).collect();
        assert_eq!(
            got,
            vec![
                Some((kept, vec![Crop, Delete])),
                None,
                Some((elsewhere, vec![Delete])),
            ]
        );
    }

    #[test]
    fn row_targets_name_the_rows_blob_and_what_the_crop_page_opens_on() {
        use BlobAction::{Crop, Delete};
        let (here, gone) = (SessionId::new(), SessionId::new());
        let ran = BlobId::new();
        let mut report = report();
        report.blobs = vec![
            blob(here, BlobUse::ToolOutput),
            blob(gone, BlobUse::ToolOutput),
        ];
        report.shell_runs = vec![run(here, Some(ran)), run(here, None)];
        report.sessions = vec![SessionStorage {
            session: here,
            events: 1,
            event_bytes: 10,
            blobs: 1,
            blob_bytes: 1024,
        }];
        let own = report.blobs[0].id;
        let mut view = view(Some(report), StorageStatus::Ready);
        view.titles.insert(here, "Fix the log".into());

        let target = row_target(&view, Grouping::Blobs, 0).unwrap();
        assert_eq!(
            target.open_crop(),
            Action::OpenCrop {
                blob: own,
                session: here,
                holds: BlobUse::ToolOutput,
                title: "Fix the log · tool output".into(),
            }
        );
        assert_eq!(target.offers, [Crop, Delete]);
        let elsewhere = row_target(&view, Grouping::Blobs, 1).unwrap();
        assert_eq!((elsewhere.session, elsewhere.offers), (gone, vec![Delete]));
        // A run acts on its output blob, titled by its command.
        let target = row_target(&view, Grouping::ShellRuns, 0).unwrap();
        assert_eq!(
            (
                target.blob,
                target.holds,
                target.title.as_str(),
                target.session
            ),
            (ran, BlobUse::ShellOutput, "make", here)
        );
        assert_eq!(row_target(&view, Grouping::ShellRuns, 1), None, "no blob");
        assert_eq!(row_target(&view, Grouping::Sessions, 0), None);
        assert_eq!(row_target(&view, Grouping::Blobs, 2), None, "past the end");
    }

    #[test]
    fn a_row_whose_blob_has_an_own_delete_or_crop_pending_offers_nothing() {
        use BlobAction::{Crop, Delete};
        use skywave_core::{BlobCommand, FailReason, Outgoing, RejectReason};
        let here = SessionId::new();
        let (ran, cropped) = (BlobId::new(), BlobId::new());
        let mut report = report();
        report.blobs = vec![
            blob(here, BlobUse::ToolOutput),
            blob(here, BlobUse::ToolOutput),
        ];
        report.shell_runs = vec![run(here, Some(ran))];
        let (refused_blob, free_blob) = (report.blobs[0].id, report.blobs[1].id);
        let mut view = view(Some(report), StorageStatus::Ready);
        view.titles.insert(here, "Fix the log".into());
        let refused = BlobPending {
            command: CommandId::new(),
            kind: BlobCommand::Crop,
            outgoing: Outgoing::Failed(FailReason::Rejected(RejectReason::BadCrop)),
            discardable: true,
        };
        let queued = BlobPending {
            command: CommandId::new(),
            kind: BlobCommand::Delete,
            outgoing: Outgoing::Queued,
            discardable: true,
        };
        view.pending.insert(refused_blob, refused);
        view.pending.insert(ran, queued);
        // A pending command of a blob no row shows touches no row.
        view.pending.insert(cropped, queued);

        let blobs = rows(&view, Grouping::Blobs);
        let actions: Vec<_> = blobs.iter().map(|r| r.actions.clone().unwrap()).collect();
        assert_eq!(
            (actions[0].offers.clone(), actions[0].pending),
            (Vec::new(), Some(refused))
        );
        assert_eq!(
            (
                actions[1].blob,
                actions[1].offers.clone(),
                actions[1].pending
            ),
            (free_blob, vec![Crop, Delete], None)
        );
        let runs = rows(&view, Grouping::ShellRuns);
        let run = runs[0].actions.clone().unwrap();
        assert_eq!(
            (run.blob, run.offers, run.pending),
            (ran, Vec::new(), Some(queued))
        );
    }

    #[test]
    fn a_row_whose_blob_the_log_already_deleted_or_cropped_offers_nothing() {
        use BlobAction::{Crop, Delete};
        let here = SessionId::new();
        let (gone_run, kept_run) = (BlobId::new(), BlobId::new());
        let mut report = report();
        report.blobs = vec![
            blob(here, BlobUse::ToolOutput),
            blob(here, BlobUse::ToolOutput),
        ];
        report.shell_runs = vec![run(here, Some(gone_run)), run(here, Some(kept_run))];
        let (gone_blob, kept_blob) = (report.blobs[0].id, report.blobs[1].id);
        let mut view = view(Some(report), StorageStatus::Ready);
        view.titles.insert(here, "Fix the log".into());
        view.gone.extend([gone_blob, gone_run]);

        let blobs = rows(&view, Grouping::Blobs);
        assert_eq!(offers(&blobs[0]), Some((gone_blob, Vec::new())));
        assert_eq!(offers(&blobs[1]), Some((kept_blob, vec![Crop, Delete])));
        assert_eq!(blobs[0].actions.as_ref().unwrap().pending, None);
        let runs = rows(&view, Grouping::ShellRuns);
        assert_eq!(offers(&runs[0]), Some((gone_run, Vec::new())));
        assert_eq!(offers(&runs[1]), Some((kept_run, vec![Crop, Delete])));
        assert_eq!(
            row_target(&view, Grouping::Blobs, 0).map(|t| t.offers),
            Some(Vec::new())
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
        assert_eq!(
            status_line(&view(Some(report()), StorageStatus::Reclaiming)).unwrap(),
            "Reclaiming space on the server…"
        );
        let no_room = StorageStatus::Unavailable(StorageUnavailable::NoRoomToReclaim);
        assert_eq!(
            status_line(&view(None, no_room.clone())).unwrap(),
            "Not enough free disk on the server to reclaim space"
        );
        assert_eq!(
            status_line(&view(Some(report()), no_room)).unwrap(),
            "Not enough free disk on the server to reclaim space; this report may be out of date."
        );
        assert!(rows(&view(None, StorageStatus::Loading), Grouping::Blobs).is_empty());
    }

    #[test]
    fn groupings_cycle() {
        assert_eq!(Grouping::default().next(), Grouping::Blobs);
        assert_eq!(Grouping::ShellRuns.next(), Grouping::Sessions);
    }
}
