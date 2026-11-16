//! The storage screen: the server's report on what its data takes, as
//! totals and one list at a time (sessions, the largest blobs, the shell
//! runs), worded once for every head.
//!
//! Session titles are text from the server: heads sanitise them (and a
//! shell run's command line) before writing them anywhere raw.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};

use skywave_core::{
    BlobId, BlobPending, BlobUse, DeviceSessionStorage, DeviceStorage, SessionId, SessionStorage,
    StorageReport, StorageStatus, StorageUnavailable, StorageView,
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

/// One part of a stacked bar of what takes the disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Kind {
    pub label: &'static str,
    pub bytes: u64,
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
    /// The session a Sessions row stands for; `None` for other rows.
    pub session: Option<SessionFacts>,
}

/// What a Sessions row stands for, so a head can select rows and act on them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionFacts {
    pub session: SessionId,
    /// The bytes the server's report gives the session (its events and
    /// blobs); `None` when the report does not list it.
    pub server_bytes: Option<u64>,
    /// The bytes this device keeps for the session; `None` when this
    /// device's report does not list it.
    pub device_bytes: Option<u64>,
    /// The session is in this replica. The flags below are `false` when not.
    pub here: bool,
    /// On the archived list.
    pub archived: bool,
    /// As [`SessionRow::deletable`](skywave_core::SessionRow::deletable).
    pub deletable: bool,
    /// As
    /// [`SessionRow::deletable_everywhere`](skywave_core::SessionRow::deletable_everywhere).
    pub deletable_everywhere: bool,
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
        } else if view.sessions.contains_key(&session) {
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

/// What the screen says above this device's storage while it has no fresh
/// answer; `None` once it has one.
pub fn device_status_line(view: &StorageView) -> Option<String> {
    if view.device_reclaiming {
        Some("Reclaiming this device's space…".to_owned())
    } else if view.device.is_none() {
        Some("Reading this device's storage…".to_owned())
    } else {
        None
    }
}

fn sized(label: &'static str, n: u64) -> Total {
    Total {
        label,
        value: format::bytes(n),
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
    vec![
        Total {
            label: "Free",
            value: free,
        },
        sized("Used", used),
        sized("Database", report.database),
        sized("Reclaimable", report.reclaimable),
        sized("Event blobs", report.event_blobs),
        sized("Workdir blobs", report.workdir_blobs),
        sized("Running shell output", report.shell_running),
        Total {
            label: "Log file",
            value: report
                .log_file
                .map_or_else(|| "none (logs to stderr)".to_owned(), format::bytes),
        },
    ]
}

/// This device's totals: what it uses, then what of that is which part.
pub fn device_totals(device: &DeviceStorage) -> Vec<Total> {
    let used = device.database + device.blobs + device.attachments;
    vec![
        sized("Used", used),
        sized("Database", device.database),
        sized("Reclaimable", device.reclaimable),
        sized("Blobs", device.blobs),
        sized("Attachments", device.attachments),
    ]
}

/// The server's report as the parts of what takes its disk, in the order
/// [`totals`] lists them.
pub fn server_kinds(report: &StorageReport) -> Vec<Kind> {
    vec![
        Kind {
            label: "Database",
            bytes: report.database,
        },
        Kind {
            label: "Event blobs",
            bytes: report.event_blobs,
        },
        Kind {
            label: "Workdir blobs",
            bytes: report.workdir_blobs,
        },
        Kind {
            label: "Running shell output",
            bytes: report.shell_running,
        },
        Kind {
            label: "Log file",
            bytes: report.log_file.unwrap_or(0),
        },
    ]
}

/// This device's storage as the parts of what takes its disk, in the order
/// [`device_totals`] lists them.
pub fn device_kinds(device: &DeviceStorage) -> Vec<Kind> {
    vec![
        Kind {
            label: "Database",
            bytes: device.database,
        },
        Kind {
            label: "Blobs",
            bytes: device.blobs,
        },
        Kind {
            label: "Attachments",
            bytes: device.attachments,
        },
    ]
}

/// What a session's row calls it: its title in this replica, else that it
/// is not here.
fn title_of(view: &StorageView, id: SessionId) -> String {
    view.sessions
        .get(&id)
        .map(|s| s.title.clone())
        .unwrap_or_else(|| format!("session {} (not on this device)", short(id)))
}

/// One session of the Sessions list, with what each report says of it.
struct Listed<'a> {
    id: SessionId,
    server: Option<&'a SessionStorage>,
    device: Option<&'a DeviceSessionStorage>,
}

impl Listed<'_> {
    fn server_bytes(&self) -> Option<u64> {
        self.server.map(|s| s.event_bytes + s.blob_bytes)
    }

    fn device_bytes(&self) -> Option<u64> {
        self.device.map(|d| d.event_bytes + d.blob_bytes)
    }

    fn total(&self) -> u64 {
        self.server_bytes().unwrap_or(0) + self.device_bytes().unwrap_or(0)
    }
}

/// The Sessions rows: the union of the server's sessions and this device's,
/// largest first by both sizes. A tie keeps the server report's order, then
/// this device's (the sort is stable).
fn session_rows(view: &StorageView) -> Vec<Row> {
    let server = view
        .report
        .as_ref()
        .map(|r| r.sessions.as_slice())
        .unwrap_or_default();
    let device = view
        .device
        .as_ref()
        .map(|d| d.sessions.as_slice())
        .unwrap_or_default();
    let on_device: HashMap<SessionId, &DeviceSessionStorage> =
        device.iter().map(|d| (d.session, d)).collect();
    let on_server: HashSet<SessionId> = server.iter().map(|s| s.session).collect();
    let mut listed: Vec<Listed<'_>> = server
        .iter()
        .map(|s| Listed {
            id: s.session,
            server: Some(s),
            device: on_device.get(&s.session).copied(),
        })
        .collect();
    listed.extend(
        device
            .iter()
            .filter(|d| !on_server.contains(&d.session))
            .map(|d| Listed {
                id: d.session,
                server: None,
                device: Some(d),
            }),
    );
    listed.sort_by_key(|l| Reverse(l.total()));
    let has_report = view.report.is_some();
    listed
        .iter()
        .map(|l| session_row(view, l, has_report))
        .collect()
}

/// A Sessions row. The server part reads as the server's report always has
/// (`"{n} events · {bytes} in the log · {blobs}"`), or says the session is
/// not on the server when that report is here without it.
fn session_row(view: &StorageView, listed: &Listed<'_>, has_report: bool) -> Row {
    let mut parts = Vec::new();
    if let Some(s) = listed.server {
        parts.push(format!(
            "{} · {} in the log · {}",
            count(s.events, "event"),
            format::bytes(s.event_bytes),
            match s.blobs {
                0 => "no blobs".to_owned(),
                n => format!("{} {}", count(n, "blob"), format::bytes(s.blob_bytes)),
            }
        ));
    } else if has_report {
        parts.push("not on the server".to_owned());
    }
    if let Some(bytes) = listed.device_bytes() {
        parts.push(format!("{} on this device", format::bytes(bytes)));
    }
    let known = view.sessions.get(&listed.id);
    Row {
        label: title_of(view, listed.id),
        detail: parts.join(" · "),
        size: format::bytes(listed.total()),
        actions: None,
        session: Some(SessionFacts {
            session: listed.id,
            server_bytes: listed.server_bytes(),
            device_bytes: listed.device_bytes(),
            here: known.is_some(),
            archived: known.is_some_and(|s| s.archived),
            deletable: known.is_some_and(|s| s.deletable),
            deletable_everywhere: known.is_some_and(|s| s.deletable_everywhere),
        }),
    }
}

/// The rows of `grouping`: the sessions largest first (see [`session_rows`]),
/// the other lists as the server ordered them. The Sessions rows also come
/// from this device's report, so they show while offline; the other lists
/// need the server's report.
pub fn rows(view: &StorageView, grouping: Grouping) -> Vec<Row> {
    match (grouping, &view.report) {
        (Grouping::Sessions, _) => session_rows(view),
        (_, None) => Vec::new(),
        (Grouping::Blobs, Some(report)) => report
            .blobs
            .iter()
            .map(|b| Row {
                label: title_of(view, b.session),
                detail: format!("{} · event {}", holds_label(b.holds), b.seq.0),
                size: format::bytes(b.size),
                actions: Some(RowActions::of(view, b.id, b.holds, b.session)),
                session: None,
            })
            .collect(),
        (Grouping::ShellRuns, Some(report)) => report
            .shell_runs
            .iter()
            .map(|r| Row {
                label: format!("$ {}", r.command_line),
                detail: format!(
                    "{} · {} progress · {} output",
                    title_of(view, r.session),
                    format::bytes(r.event_bytes),
                    format::bytes(r.output)
                ),
                size: format::bytes(r.event_bytes + r.output),
                actions: r
                    .output_blob
                    .map(|blob| RowActions::of(view, blob, BlobUse::ShellOutput, r.session)),
                session: None,
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
        StorageSession,
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

    /// A session in this replica, with no flags set.
    fn known(title: &str) -> StorageSession {
        StorageSession {
            title: title.into(),
            archived: false,
            deletable: false,
            deletable_everywhere: false,
        }
    }

    fn device(sessions: Vec<DeviceSessionStorage>) -> DeviceStorage {
        DeviceStorage {
            database: 0,
            reclaimable: 0,
            blobs: 0,
            attachments: 0,
            sessions,
        }
    }

    fn view(report: Option<StorageReport>, status: StorageStatus) -> StorageView {
        StorageView {
            status,
            report,
            sessions: HashMap::new(),
            pending: HashMap::new(),
            gone: HashSet::new(),
            device: None,
            device_reclaiming: false,
        }
    }

    #[test]
    fn totals_lead_with_the_free_space_and_its_floor() {
        let mut report = report();
        report.reclaimable = 1024 * 1024;
        let totals = totals(&report);
        assert_eq!(
            totals[0],
            Total {
                label: "Free",
                value: "20.0 GiB · agents pause below 1.0 GiB".into()
            }
        );
        assert_eq!(totals[1].value, "5.0 MiB");
        assert_eq!(
            totals[2],
            Total {
                label: "Database",
                value: "2.0 MiB".into()
            }
        );
        assert_eq!(
            totals[3],
            Total {
                label: "Reclaimable",
                value: "1.0 MiB".into()
            }
        );
        assert_eq!(totals.last().unwrap().value, "none (logs to stderr)");
    }

    #[test]
    fn device_totals_and_kinds_add_up() {
        let device = DeviceStorage {
            database: 3 * 1024 * 1024,
            reclaimable: 1024 * 1024,
            blobs: 2 * 1024 * 1024,
            attachments: 512 * 1024,
            sessions: Vec::new(),
        };
        assert_eq!(
            device_totals(&device),
            vec![
                Total {
                    label: "Used",
                    value: "5.5 MiB".into()
                },
                Total {
                    label: "Database",
                    value: "3.0 MiB".into()
                },
                Total {
                    label: "Reclaimable",
                    value: "1.0 MiB".into()
                },
                Total {
                    label: "Blobs",
                    value: "2.0 MiB".into()
                },
                Total {
                    label: "Attachments",
                    value: "512.0 KiB".into()
                },
            ]
        );
        let kinds = device_kinds(&device);
        assert_eq!(
            kinds,
            vec![
                Kind {
                    label: "Database",
                    bytes: 3 * 1024 * 1024
                },
                Kind {
                    label: "Blobs",
                    bytes: 2 * 1024 * 1024
                },
                Kind {
                    label: "Attachments",
                    bytes: 512 * 1024
                },
            ]
        );
        // The parts add up to what the screen calls used.
        let used: u64 = kinds.iter().map(|k| k.bytes).sum();
        assert_eq!(format::bytes(used), device_totals(&device)[0].value);

        // The server's parts add up to its used total the same way.
        let report = report();
        let parts = server_kinds(&report);
        assert_eq!(
            parts.iter().map(|k| k.label).collect::<Vec<_>>(),
            [
                "Database",
                "Event blobs",
                "Workdir blobs",
                "Running shell output",
                "Log file"
            ]
        );
        let used: u64 = parts.iter().map(|k| k.bytes).sum();
        assert_eq!(format::bytes(used), totals(&report)[1].value);
    }

    #[test]
    fn device_status_line_says_while_this_device_is_read_or_reclaimed() {
        let mut view = view(None, StorageStatus::Ready);
        assert_eq!(
            device_status_line(&view).unwrap(),
            "Reading this device's storage…"
        );
        view.device = Some(device(Vec::new()));
        assert_eq!(device_status_line(&view), None);
        view.device_reclaiming = true;
        assert_eq!(
            device_status_line(&view).unwrap(),
            "Reclaiming this device's space…"
        );
    }

    #[test]
    fn session_rows_join_the_server_and_this_device() {
        let (x, y, z) = (SessionId::new(), SessionId::new(), SessionId::new());
        let mut report = report();
        report.sessions = vec![
            SessionStorage {
                session: x,
                events: 2,
                event_bytes: 2048,
                blobs: 0,
                blob_bytes: 0,
            },
            SessionStorage {
                session: y,
                events: 1,
                event_bytes: 1024,
                blobs: 1,
                blob_bytes: 4096,
            },
        ];
        let mut view = view(Some(report), StorageStatus::Ready);
        view.device = Some(device(vec![
            DeviceSessionStorage {
                session: x,
                event_bytes: 512,
                blob_bytes: 0,
            },
            DeviceSessionStorage {
                session: z,
                event_bytes: 100,
                blob_bytes: 0,
            },
        ]));
        view.sessions.insert(
            x,
            StorageSession {
                title: "x".into(),
                archived: true,
                deletable: true,
                deletable_everywhere: false,
            },
        );
        view.sessions.insert(z, known("z"));

        let rows = rows(&view, Grouping::Sessions);
        // Largest first: y 5120, x 2560, z 100.
        assert_eq!(
            rows,
            vec![
                Row {
                    label: format!("session {} (not on this device)", short(y)),
                    detail: "1 event · 1.0 KiB in the log · 1 blob 4.0 KiB".into(),
                    size: "5.0 KiB".into(),
                    actions: None,
                    session: Some(SessionFacts {
                        session: y,
                        server_bytes: Some(5120),
                        device_bytes: None,
                        here: false,
                        archived: false,
                        deletable: false,
                        deletable_everywhere: false,
                    }),
                },
                Row {
                    label: "x".into(),
                    detail: "2 events · 2.0 KiB in the log · no blobs · 512 B on this device"
                        .into(),
                    size: "2.5 KiB".into(),
                    actions: None,
                    session: Some(SessionFacts {
                        session: x,
                        server_bytes: Some(2048),
                        device_bytes: Some(512),
                        here: true,
                        archived: true,
                        deletable: true,
                        deletable_everywhere: false,
                    }),
                },
                Row {
                    label: "z".into(),
                    detail: "not on the server · 100 B on this device".into(),
                    size: "100 B".into(),
                    actions: None,
                    session: Some(SessionFacts {
                        session: z,
                        server_bytes: None,
                        device_bytes: Some(100),
                        here: true,
                        archived: false,
                        deletable: false,
                        deletable_everywhere: false,
                    }),
                },
            ]
        );
    }

    #[test]
    fn offline_the_sessions_list_shows_this_device_only() {
        let z = SessionId::new();
        let mut view = view(None, StorageStatus::Loading);
        view.device = Some(device(vec![DeviceSessionStorage {
            session: z,
            event_bytes: 100,
            blob_bytes: 0,
        }]));
        view.sessions.insert(z, known("z"));

        assert_eq!(
            rows(&view, Grouping::Sessions),
            vec![Row {
                label: "z".into(),
                detail: "100 B on this device".into(),
                size: "100 B".into(),
                actions: None,
                session: Some(SessionFacts {
                    session: z,
                    server_bytes: None,
                    device_bytes: Some(100),
                    here: true,
                    archived: false,
                    deletable: false,
                    deletable_everywhere: false,
                }),
            }]
        );
        assert!(rows(&view, Grouping::Blobs).is_empty());
        assert!(rows(&view, Grouping::ShellRuns).is_empty());
    }

    #[test]
    fn a_session_only_on_the_server_reads_as_before() {
        let x = SessionId::new();
        let mut report = report();
        report.sessions = vec![SessionStorage {
            session: x,
            events: 2,
            event_bytes: 2048,
            blobs: 0,
            blob_bytes: 0,
        }];
        let mut view = view(Some(report), StorageStatus::Ready);
        view.sessions.insert(x, known("x"));

        let rows = rows(&view, Grouping::Sessions);
        assert_eq!(rows[0].detail, "2 events · 2.0 KiB in the log · no blobs");
        assert_eq!(rows[0].size, "2.0 KiB");
        assert_eq!(
            rows[0].session,
            Some(SessionFacts {
                session: x,
                server_bytes: Some(2048),
                device_bytes: None,
                here: true,
                archived: false,
                deletable: false,
                deletable_everywhere: false,
            })
        );
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
        view.sessions.insert(here, known("Fix the log"));

        let sessions = rows(&view, Grouping::Sessions);
        assert_eq!(
            sessions,
            vec![Row {
                label: "Fix the log".into(),
                detail: "1 event · 2.0 KiB in the log · 2 blobs 1.0 MiB".into(),
                size: "1.0 MiB".into(),
                actions: None,
                session: Some(SessionFacts {
                    session: here,
                    server_bytes: Some(2048 + 1024 * 1024),
                    device_bytes: None,
                    here: true,
                    archived: false,
                    deletable: false,
                    deletable_everywhere: false,
                }),
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
        view.sessions.insert(here, known("Fix the log"));

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
        view.sessions.insert(here, known("Fix the log"));

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
        view.sessions.insert(here, known("Fix the log"));

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
        view.sessions.insert(here, known("Fix the log"));
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
        view.sessions.insert(here, known("Fix the log"));
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
