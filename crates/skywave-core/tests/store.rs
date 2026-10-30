//! Local replica behaviour: idempotent apply, gap rejection, and state that
//! survives a reopen (cold start == normal start).

use std::path::{Path, PathBuf};

use chrono::{TimeZone, Utc};
use skywave_core::{BlobPolicy, BlobStatus, OutboxStatus, Settings, Store, StoreError};
use skywave_proto::{
    ApprovalId, BlobId, BlobRef, Command, CommandId, CommandKind, CommandResult, Decision,
    DeviceId, Event, EventBatch, EventKind, RejectReason, Seq, SessionId, SessionState, ToolResult,
};

fn event(session_id: SessionId, minute: u32, kind: EventKind) -> Event {
    Event {
        session_id,
        at: Utc.with_ymd_and_hms(2026, 10, 29, 9, minute, 0).unwrap(),
        kind,
    }
}

/// A seven-event log over two sessions; `blob` is a tool output in session A.
struct Log {
    a: SessionId,
    b: SessionId,
    approval: ApprovalId,
    blob: BlobRef,
    events: Vec<Event>,
}

impl Log {
    fn new() -> Self {
        let (a, b) = (SessionId::new(), SessionId::new());
        let approval = ApprovalId::new();
        let blob = BlobRef {
            id: BlobId::new(),
            size: 10_000,
        };
        let events = vec![
            event(
                a,
                0,
                EventKind::SessionCreated {
                    workdir: "/srv/a".into(),
                },
            ),
            event(
                a,
                1,
                EventKind::SessionStateChanged(SessionState::AgentWorking),
            ),
            event(
                b,
                2,
                EventKind::SessionCreated {
                    workdir: "/srv/b".into(),
                },
            ),
            event(
                a,
                3,
                EventKind::ToolResult(ToolResult {
                    call: None,
                    is_error: false,
                    status: None,
                    preview: "ran tests".into(),
                    lines: 1,
                    output: Some(blob.clone()),
                }),
            ),
            event(
                a,
                4,
                EventKind::ApprovalRequested {
                    id: approval,
                    call: None,
                    tool: "bash".into(),
                    summary: "rm -rf target".into(),
                    input: None,
                },
            ),
            event(
                b,
                5,
                EventKind::UserMessage {
                    command_id: CommandId::new(),
                    device_id: DeviceId::new(),
                    text: "hi".into(),
                },
            ),
            event(
                a,
                6,
                EventKind::ApprovalResolved {
                    id: approval,
                    decision: Decision::Approve,
                    by: DeviceId::new(),
                },
            ),
        ];
        Log {
            a,
            b,
            approval,
            blob,
            events,
        }
    }

    /// Events with seqs `from..=to` (1-based, inclusive).
    fn batch(&self, from: u64, to: u64) -> EventBatch {
        let range = usize::try_from(from - 1).unwrap()..usize::try_from(to).unwrap();
        EventBatch {
            from: Seq(from),
            events: self.events[range].to_vec(),
        }
    }
}

fn data_dir() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("core");
    (tmp, dir)
}

fn event_count(store: &Store, log: &Log) -> usize {
    store.session_events(log.a).unwrap().len() + store.session_events(log.b).unwrap().len()
}

#[test]
fn fresh_store_is_empty_with_defaults() {
    let (_tmp, dir) = data_dir();
    let snapshot = Store::open(&dir).unwrap().load().unwrap();
    assert_eq!(snapshot.cursor, Seq::ZERO);
    assert!(
        snapshot.sessions.is_empty() && snapshot.outbox.is_empty() && snapshot.blobs.is_empty()
    );
    assert_eq!(
        snapshot.settings,
        Settings {
            blob_policy: BlobPolicy::OnDemand,
            server_address: None
        }
    );
}

#[test]
fn apply_and_replay_are_idempotent() {
    let log = Log::new();
    let mut store = Store::open_in_memory().unwrap();

    let first = store.apply_batch(&log.batch(1, 4)).unwrap();
    assert_eq!((first.cursor, first.new_events), (Seq(4), 4));
    let after_first = store.load().unwrap();

    // Replaying the same batch changes nothing.
    let replay = store.apply_batch(&log.batch(1, 4)).unwrap();
    assert_eq!((replay.cursor, replay.new_events), (Seq(4), 0));
    assert!(replay.sessions.is_empty());
    assert_eq!(store.load().unwrap(), after_first);
    assert_eq!(event_count(&store, &log), 4);

    // An overlapping range stores only the new tail.
    let overlap = store.apply_batch(&log.batch(3, 7)).unwrap();
    assert_eq!((overlap.cursor, overlap.new_events), (Seq(7), 3));
    assert_eq!(event_count(&store, &log), 7);

    // An empty long-poll batch (`from = cursor + 1`) and a stale one are no-ops.
    let empty = store
        .apply_batch(&EventBatch {
            from: Seq(8),
            events: vec![],
        })
        .unwrap();
    assert_eq!((empty.cursor, empty.new_events), (Seq(7), 0));
    let stale = store
        .apply_batch(&EventBatch {
            from: Seq(2),
            events: vec![],
        })
        .unwrap();
    assert_eq!(stale.cursor, Seq(7));

    // Applying in one go yields the same replica as applying piecewise.
    let mut whole = Store::open_in_memory().unwrap();
    whole.apply_batch(&log.batch(1, 7)).unwrap();
    assert_eq!(whole.load().unwrap(), store.load().unwrap());
    assert_eq!(
        whole.session_events(log.a).unwrap(),
        store.session_events(log.a).unwrap()
    );
}

#[test]
fn batch_with_gap_is_rejected_and_changes_nothing() {
    let log = Log::new();
    let mut store = Store::open_in_memory().unwrap();
    store.apply_batch(&log.batch(1, 2)).unwrap();
    let before = store.load().unwrap();

    let err = store.apply_batch(&log.batch(4, 5)).unwrap_err();
    assert!(
        matches!(
            err,
            StoreError::Gap {
                cursor: Seq(2),
                from: Seq(4)
            }
        ),
        "{err}"
    );
    assert_eq!(store.load().unwrap(), before);
    assert_eq!(event_count(&store, &log), 2);

    let zero = EventBatch {
        from: Seq::ZERO,
        events: log.events[..1].to_vec(),
    };
    assert!(matches!(
        store.apply_batch(&zero),
        Err(StoreError::BatchFromZero)
    ));
}

#[test]
fn sessions_are_projected_from_events() {
    let log = Log::new();
    let mut store = Store::open_in_memory().unwrap();
    store.apply_batch(&log.batch(1, 5)).unwrap();

    let a = store.session(log.a).unwrap().unwrap();
    assert_eq!(a.workdir.as_deref(), Some("/srv/a"));
    assert_eq!(a.state, SessionState::AgentWorking);
    assert_eq!(a.pending_approval(), Some(log.approval));
    assert_eq!(a.last_seq, Seq(5));

    let applied = store.apply_batch(&log.batch(6, 7)).unwrap();
    assert_eq!(applied.sessions.len(), 2);
    let sessions = store.sessions().unwrap();
    // Most recently active first.
    assert_eq!(
        sessions.iter().map(|s| s.id).collect::<Vec<_>>(),
        vec![log.a, log.b]
    );
    assert_eq!(sessions[0].pending_approval(), None);
    assert_eq!(sessions[1].last_seq, Seq(6));

    let b_events = store.session_events(log.b).unwrap();
    assert_eq!(
        b_events,
        vec![
            (Seq(3), log.events[2].clone()),
            (Seq(6), log.events[5].clone())
        ]
    );
}

#[test]
fn referenced_blobs_are_registered_and_track_state() {
    let log = Log::new();
    let mut store = Store::open_in_memory().unwrap();
    store.apply_batch(&log.batch(1, 7)).unwrap();

    let blob = store.blob(log.blob.id).unwrap().unwrap();
    assert_eq!(
        (blob.size, &blob.status),
        (10_000, &BlobStatus::NotDownloaded)
    );
    assert_eq!(store.blobs().unwrap(), vec![blob]);

    let partial = BlobStatus::Downloading {
        got: 4096,
        path: "blobs/x.part".into(),
    };
    store.set_blob_status(log.blob.id, &partial).unwrap();
    assert_eq!(store.blob(log.blob.id).unwrap().unwrap().status, partial);

    // Replaying the referencing event does not reset the download.
    store.apply_batch(&log.batch(4, 4)).unwrap();
    assert_eq!(store.blob(log.blob.id).unwrap().unwrap().status, partial);

    let unknown = BlobId::new();
    assert!(matches!(
        store.set_blob_status(unknown, &BlobStatus::NotDownloaded),
        Err(StoreError::UnknownBlob(id)) if id == unknown
    ));
}

#[test]
fn reopen_restores_state() {
    let log = Log::new();
    let (_tmp, dir) = data_dir();
    let (snapshot, a_events) = {
        let mut store = Store::open(&dir).unwrap();
        store.apply_batch(&log.batch(1, 7)).unwrap();
        store.set_blob_policy(BlobPolicy::Eager).unwrap();
        store
            .set_server_address(Some("skywave.example:7443"))
            .unwrap();
        store
            .set_blob_status(
                log.blob.id,
                &BlobStatus::Available {
                    path: "blobs/x".into(),
                },
            )
            .unwrap();
        store.enqueue(&send(log.b, "queued while offline")).unwrap();
        (store.load().unwrap(), store.session_events(log.a).unwrap())
    };

    let store = Store::open(&dir).unwrap();
    assert_eq!(store.load().unwrap(), snapshot);
    assert_eq!(store.session_events(log.a).unwrap(), a_events);
    assert_eq!(snapshot.cursor, Seq(7));
    assert_eq!(snapshot.sessions.len(), 2);
    assert_eq!(snapshot.settings.blob_policy, BlobPolicy::Eager);
    assert_eq!(
        snapshot.settings.server_address.as_deref(),
        Some("skywave.example:7443")
    );

    store.set_server_address(None).unwrap();
    assert_eq!(store.settings().unwrap().server_address, None);
}

fn send(session_id: SessionId, text: &str) -> Command {
    Command {
        id: CommandId::new(),
        kind: CommandKind::SendMessage {
            session_id,
            text: text.into(),
        },
    }
}

#[test]
fn outbox_survives_restart() {
    let (_tmp, dir) = data_dir();
    let session = SessionId::new();
    let commands = [
        send(session, "one"),
        send(session, "two"),
        send(session, "three"),
    ];
    {
        let store = Store::open(&dir).unwrap();
        for command in &commands {
            store.enqueue(command).unwrap();
        }
        // Re-enqueuing the same id is a no-op.
        store.enqueue(&commands[0]).unwrap();
        store.mark_sending(commands[0].id).unwrap();
        store.mark_sending(commands[1].id).unwrap();
        store
            .record_result(commands[1].id, &CommandResult::Accepted { seq: Seq(42) })
            .unwrap();

        let statuses: Vec<_> = store
            .outbox()
            .unwrap()
            .into_iter()
            .map(|e| e.status)
            .collect();
        let accepted = OutboxStatus::Done(CommandResult::Accepted { seq: Seq(42) });
        assert_eq!(
            statuses,
            vec![OutboxStatus::Sending, accepted, OutboxStatus::Queued]
        );
    }

    let store = Store::open(&dir).unwrap();
    let outbox = store.outbox().unwrap();
    // Oldest first, with the full command (same CommandId) kept for resend.
    assert_eq!(
        outbox.iter().map(|e| e.command.clone()).collect::<Vec<_>>(),
        commands.to_vec()
    );
    // Nothing is in flight after a restart, so `Sending` is back to `Queued`.
    assert_eq!(outbox[0].status, OutboxStatus::Queued);
    assert_eq!(
        outbox[1].status,
        OutboxStatus::Done(CommandResult::Accepted { seq: Seq(42) })
    );

    let pending: Vec<_> = store
        .pending_outbox()
        .unwrap()
        .into_iter()
        .map(|e| e.command.id)
        .collect();
    assert_eq!(pending, vec![commands[0].id, commands[2].id]);

    // The first result sticks; a done command stays done.
    let rejected = CommandResult::Rejected {
        reason: RejectReason::UnknownSession,
    };
    store.record_result(commands[2].id, &rejected).unwrap();
    store
        .record_result(commands[2].id, &CommandResult::Accepted { seq: Seq(43) })
        .unwrap();
    store.mark_sending(commands[2].id).unwrap();
    assert_eq!(
        store.outbox().unwrap()[2].status,
        OutboxStatus::Done(rejected)
    );

    let unknown = CommandId::new();
    assert!(
        matches!(store.mark_sending(unknown), Err(StoreError::UnknownCommand(id)) if id == unknown)
    );
}

#[test]
fn newer_schema_is_refused() {
    let (_tmp, dir) = data_dir();
    drop(Store::open(&dir).unwrap());
    set_user_version(&dir, 99);
    assert!(matches!(
        Store::open(&dir),
        Err(StoreError::NewerSchema { found: 99 })
    ));
}

#[test]
fn stopped_flag_folds_and_is_backfilled_by_the_migration() {
    let (_tmp, dir) = data_dir();
    let (a, b) = (SessionId::new(), SessionId::new());
    let created = |id, minute| {
        event(
            id,
            minute,
            EventKind::SessionCreated {
                workdir: "/srv/x".into(),
            },
        )
    };
    let message = |id, minute| {
        event(
            id,
            minute,
            EventKind::UserMessage {
                command_id: CommandId::new(),
                device_id: DeviceId::new(),
                text: "hi".into(),
            },
        )
    };
    // `a` stays stopped; `b` got a message after its stop.
    let batch = EventBatch {
        from: Seq(1),
        events: vec![
            created(a, 0),
            created(b, 1),
            message(a, 2),
            event(a, 3, EventKind::SessionStopped),
            event(b, 4, EventKind::SessionStopped),
            message(b, 5),
        ],
    };
    let mut store = Store::open(&dir).unwrap();
    store.apply_batch(&batch).unwrap();
    let stopped = |store: &Store, id| store.session(id).unwrap().unwrap().stopped;
    assert!(stopped(&store, a));
    assert!(!stopped(&store, b));
    drop(store);

    // A replica from before the column: the migration derives it from the log.
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    conn.execute_batch("ALTER TABLE sessions DROP COLUMN stopped")
        .unwrap();
    drop(conn);
    set_user_version(&dir, 1);
    let store = Store::open(&dir).unwrap();
    assert!(stopped(&store, a));
    assert!(!stopped(&store, b));
}

fn set_user_version(dir: &Path, version: i64) {
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    conn.pragma_update(None, "user_version", version).unwrap();
}
