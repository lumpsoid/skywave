//! Local replica behaviour: idempotent apply, gap rejection, and state that
//! survives a reopen (cold start == normal start).

use std::path::{Path, PathBuf};

use chrono::{TimeZone, Utc};
use skywave_core::{
    BlobPolicy, BlobStatus, Draft, OutboxStatus, ReadMark, Settings, Store, StoreError,
};
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
        assert!(store.claim_send(commands[0].id, Utc::now()).unwrap());
        assert!(store.claim_send(commands[1].id, Utc::now()).unwrap());
        store
            .record_result(
                commands[1].id,
                &CommandResult::Accepted { seq: Seq(42) },
                Utc::now(),
            )
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
    store
        .record_result(commands[2].id, &rejected, Utc::now())
        .unwrap();
    store
        .record_result(
            commands[2].id,
            &CommandResult::Accepted { seq: Seq(43) },
            Utc::now(),
        )
        .unwrap();
    assert_eq!(
        store.outbox().unwrap()[2].status,
        OutboxStatus::Done(rejected)
    );

    let unknown = CommandId::new();
    assert!(matches!(
        store.record_result(unknown, &CommandResult::Accepted { seq: Seq(1) }, Utc::now()),
        Err(StoreError::UnknownCommand(id)) if id == unknown
    ));
}

fn at(second: u32) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 30, 9, 0, second).unwrap()
}

#[test]
fn claim_send_counts_tries_and_keeps_first_time() {
    let (_tmp, dir) = data_dir();
    let store = Store::open(&dir).unwrap();
    let command = send(SessionId::new(), "hi");
    store.enqueue(&command).unwrap();
    let entry = &store.outbox().unwrap()[0];
    assert_eq!(
        (entry.tries, entry.first_sent_at, entry.answered_at),
        (0, None, None)
    );

    assert!(store.claim_send(command.id, at(1)).unwrap());
    assert!(store.claim_send(command.id, at(2)).unwrap());
    let entry = &store.outbox().unwrap()[0];
    assert_eq!(entry.status, OutboxStatus::Sending);
    assert_eq!(
        (entry.tries, entry.first_sent_at, entry.answered_at),
        (2, Some(at(1)), None)
    );

    let accepted = CommandResult::Accepted { seq: Seq(7) };
    store.record_result(command.id, &accepted, at(3)).unwrap();
    // A later answer changes neither the result nor its time.
    store
        .record_result(command.id, &CommandResult::Accepted { seq: Seq(8) }, at(4))
        .unwrap();
    let entry = &store.outbox().unwrap()[0];
    assert_eq!(entry.status, OutboxStatus::Done(accepted));
    assert_eq!(
        (entry.tries, entry.first_sent_at, entry.answered_at),
        (2, Some(at(1)), Some(at(3)))
    );
}

#[test]
fn claim_send_refuses_answered_or_missing_rows() {
    let (_tmp, dir) = data_dir();
    let store = Store::open(&dir).unwrap();
    let command = send(SessionId::new(), "hi");
    store.enqueue(&command).unwrap();
    assert!(store.claim_send(command.id, at(1)).unwrap());
    let rejected = CommandResult::Rejected {
        reason: RejectReason::UnknownSession,
    };
    store.record_result(command.id, &rejected, at(2)).unwrap();

    assert!(!store.claim_send(command.id, at(3)).unwrap());
    assert!(!store.claim_send(CommandId::new(), at(3)).unwrap());
    let entry = &store.outbox().unwrap()[0];
    assert_eq!(entry.status, OutboxStatus::Done(rejected));
    assert_eq!(entry.tries, 1, "a refused claim counts nothing");
}

#[test]
fn tries_survive_restart() {
    let (_tmp, dir) = data_dir();
    let command = send(SessionId::new(), "hi");
    {
        let store = Store::open(&dir).unwrap();
        store.enqueue(&command).unwrap();
        assert!(store.claim_send(command.id, at(1)).unwrap());
    }
    // `Sending` goes back to `Queued`, but the attempt is remembered.
    let store = Store::open(&dir).unwrap();
    let entry = &store.outbox().unwrap()[0];
    assert_eq!(entry.status, OutboxStatus::Queued);
    assert_eq!((entry.tries, entry.first_sent_at), (1, Some(at(1))));

    assert!(store.claim_send(command.id, at(2)).unwrap());
    let entry = &store.outbox().unwrap()[0];
    assert_eq!((entry.tries, entry.first_sent_at), (2, Some(at(1))));
}

#[test]
fn migration_marks_existing_rows_attempted() {
    let (_tmp, dir) = data_dir();
    let session = SessionId::new();
    let (queued, answered) = (send(session, "one"), send(session, "two"));
    {
        let store = Store::open(&dir).unwrap();
        store.enqueue(&queued).unwrap();
        store.enqueue(&answered).unwrap();
        store
            .record_result(answered.id, &CommandResult::Accepted { seq: Seq(1) }, at(1))
            .unwrap();
    }
    // A replica from before the columns.
    drop_outbox_delivery_columns(&dir);
    drop_unread(&dir);
    drop_drafts(&dir);
    set_user_version(&dir, 2);

    let store = Store::open(&dir).unwrap();
    let outbox = store.outbox().unwrap();
    // Either may have reached the server, so neither counts as never sent.
    assert!(outbox.iter().all(|e| e.tries == 1));
    assert!(
        outbox
            .iter()
            .all(|e| e.first_sent_at.is_none() && e.answered_at.is_none())
    );
}

fn drop_outbox_delivery_columns(dir: &Path) {
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    conn.execute_batch(
        "ALTER TABLE outbox DROP COLUMN tries;
         ALTER TABLE outbox DROP COLUMN first_sent_at;
         ALTER TABLE outbox DROP COLUMN answered_at;",
    )
    .unwrap();
}

fn drop_drafts(dir: &Path) {
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    conn.execute_batch("DROP TABLE drafts").unwrap();
}

fn drop_unread(dir: &Path) {
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    conn.execute_batch(
        "DROP TABLE read_marks;
         ALTER TABLE sessions DROP COLUMN notable;",
    )
    .unwrap();
}

#[test]
fn notable_count_is_projected_and_backfilled() {
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
    let errored = SessionState::Errored {
        message: "boom".into(),
    };
    // `a`: an agent message, an approval request and an error are news;
    // a message, a tool result and other state changes are not.
    let batch = EventBatch {
        from: Seq(1),
        events: vec![
            created(a, 0),
            created(b, 1),
            event(
                a,
                2,
                EventKind::UserMessage {
                    command_id: CommandId::new(),
                    device_id: DeviceId::new(),
                    text: "hi".into(),
                },
            ),
            event(
                a,
                3,
                EventKind::SessionStateChanged(SessionState::AgentWorking),
            ),
            event(a, 4, EventKind::AgentMessage { text: "ok".into() }),
            event(
                a,
                5,
                EventKind::ApprovalRequested {
                    id: ApprovalId::new(),
                    call: None,
                    tool: "bash".into(),
                    summary: "ls".into(),
                    input: None,
                },
            ),
            event(
                a,
                6,
                EventKind::ToolResult(ToolResult {
                    call: None,
                    is_error: true,
                    status: None,
                    preview: "no".into(),
                    lines: 1,
                    output: None,
                }),
            ),
            event(a, 7, EventKind::SessionStateChanged(errored)),
            event(b, 8, EventKind::SessionStateChanged(SessionState::Idle)),
        ],
    };
    let mut store = Store::open(&dir).unwrap();
    store.apply_batch(&batch).unwrap();
    let notable = |store: &Store, id| store.session(id).unwrap().unwrap().notable;
    assert_eq!((notable(&store, a), notable(&store, b)), (3, 0));
    assert!(store.read_marks().unwrap().is_empty(), "nothing read yet");
    drop(store);

    // A replica from before the column: the migration derives it from the
    // log and marks everything stored as read, so nothing shows up as new.
    drop_unread(&dir);
    set_user_version(&dir, 4);
    let store = Store::open(&dir).unwrap();
    assert_eq!((notable(&store, a), notable(&store, b)), (3, 0));
    let marks = store.load().unwrap().read_marks;
    assert_eq!(
        marks[&a],
        ReadMark {
            seq: Seq(8),
            notable: 3
        }
    );
    assert_eq!(
        marks[&b],
        ReadMark {
            seq: Seq(9),
            notable: 0
        }
    );
}

#[test]
fn read_marks_round_trip() {
    let (_tmp, dir) = data_dir();
    let (a, b) = (SessionId::new(), SessionId::new());
    let mark = |seq, notable| ReadMark {
        seq: Seq(seq),
        notable,
    };
    {
        let store = Store::open(&dir).unwrap();
        store.set_read_mark(a, mark(3, 1)).unwrap();
        store.set_read_mark(b, mark(4, 0)).unwrap();
        store.set_read_mark(a, mark(7, 2)).unwrap();
    }
    let store = Store::open(&dir).unwrap();
    let marks = store.load().unwrap().read_marks;
    assert_eq!(marks.len(), 2);
    assert_eq!(marks[&a], mark(7, 2), "a mark replaces the last one");
    assert_eq!(marks[&b], mark(4, 0));
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
    drop_outbox_delivery_columns(&dir);
    drop_unread(&dir);
    drop_drafts(&dir);
    set_user_version(&dir, 1);
    let store = Store::open(&dir).unwrap();
    assert!(stopped(&store, a));
    assert!(!stopped(&store, b));
}

fn set_user_version(dir: &Path, version: i64) {
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    conn.pragma_update(None, "user_version", version).unwrap();
}

#[test]
fn drafts_round_trip_and_blank_deletes() {
    let (_tmp, dir) = data_dir();
    let (a, b) = (SessionId::new(), SessionId::new());
    let draft = |text: &str, cursor| Draft {
        text: text.into(),
        cursor,
    };
    {
        let store = Store::open(&dir).unwrap();
        store.set_draft(a, Some(&draft("héllo\nwörld", 7))).unwrap();
        store.set_draft(b, Some(&draft("first", 0))).unwrap();
        store.set_draft(b, Some(&draft("second", 6))).unwrap();
    }
    let store = Store::open(&dir).unwrap();
    let drafts = store.load().unwrap().drafts;
    assert_eq!(drafts.len(), 2);
    assert_eq!(drafts[&a], draft("héllo\nwörld", 7));
    assert_eq!(drafts[&b], draft("second", 6), "a save replaces the draft");

    store.set_draft(a, None).unwrap();
    store.set_draft(SessionId::new(), None).unwrap();
    assert_eq!(store.drafts().unwrap().keys().collect::<Vec<_>>(), [&b]);
}

#[test]
fn enqueue_message_clears_its_draft_atomically() {
    let (_tmp, dir) = data_dir();
    let (a, b) = (SessionId::new(), SessionId::new());
    let draft = |text: &str| Draft {
        text: text.into(),
        cursor: 0,
    };
    let store = Store::open(&dir).unwrap();
    store.set_draft(a, Some(&draft("to a"))).unwrap();
    store.set_draft(b, Some(&draft("to b"))).unwrap();

    // Other commands leave drafts alone.
    store
        .enqueue(&Command {
            id: CommandId::new(),
            kind: CommandKind::Interrupt { session_id: a },
        })
        .unwrap();
    assert_eq!(store.drafts().unwrap().len(), 2);

    // An enqueue that fails after deleting the draft keeps it: the outbox
    // insert comes second and aborts the whole transaction.
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER full BEFORE INSERT ON outbox BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
    )
    .unwrap();
    assert!(store.enqueue(&send(a, "to a")).is_err());
    assert_eq!(store.drafts().unwrap().len(), 2);
    assert_eq!(store.pending_outbox().unwrap().len(), 1);

    conn.execute_batch("DROP TRIGGER full").unwrap();
    store.enqueue(&send(a, "to a")).unwrap();
    assert_eq!(store.drafts().unwrap().keys().collect::<Vec<_>>(), [&b]);
    assert_eq!(store.pending_outbox().unwrap().len(), 2);
}

#[test]
fn withdraw_only_never_attempted_or_refused() {
    let (_tmp, dir) = data_dir();
    let store = Store::open(&dir).unwrap();
    let session = SessionId::new();
    let [fresh, claimed, requeued, refused, accepted] =
        ["fresh", "claimed", "requeued", "refused", "accepted"].map(|text| send(session, text));
    for command in [&fresh, &claimed, &requeued, &refused, &accepted] {
        store.enqueue(command).unwrap();
    }
    for command in [&claimed, &requeued, &refused, &accepted] {
        assert!(store.claim_send(command.id, at(1)).unwrap());
    }
    let rejected = CommandResult::Rejected {
        reason: RejectReason::UnknownSession,
    };
    store.record_result(refused.id, &rejected, at(2)).unwrap();
    store
        .record_result(accepted.id, &CommandResult::Accepted { seq: Seq(1) }, at(2))
        .unwrap();
    // A restart puts the claimed rows back to `Queued`; their tries count.
    drop(store);
    let store = Store::open(&dir).unwrap();
    assert!(store.claim_send(claimed.id, at(3)).unwrap());

    let discardable: Vec<(String, bool)> = store
        .outbox()
        .unwrap()
        .iter()
        .map(|e| match &e.command.kind {
            CommandKind::SendMessage { text, .. } => (text.clone(), e.discardable()),
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    assert_eq!(
        discardable,
        [
            ("fresh", true),
            ("claimed", false),
            ("requeued", false),
            ("refused", true),
            ("accepted", false),
        ]
        .map(|(text, discardable)| (text.to_owned(), discardable))
    );

    // What may have reached the server, or was accepted, stays.
    for command in [&claimed, &requeued, &accepted] {
        assert!(matches!(
            store.withdraw(command.id, None),
            Err(StoreError::NotWithdrawable(id)) if id == command.id
        ));
    }
    assert!(matches!(
        store.withdraw(CommandId::new(), None),
        Err(StoreError::NotWithdrawable(_))
    ));
    store.withdraw(fresh.id, None).unwrap();
    store.withdraw(refused.id, None).unwrap();
    let left: Vec<CommandId> = store
        .outbox()
        .unwrap()
        .iter()
        .map(|e| e.command.id)
        .collect();
    assert_eq!(left, [claimed.id, requeued.id, accepted.id]);

    // A session creation is withdrawn the same way; other kinds are not
    // discardable even if never sent.
    let create = Command {
        id: CommandId::new(),
        kind: CommandKind::CreateSession {
            workdir: "/etc".into(),
        },
    };
    let interrupt = Command {
        id: CommandId::new(),
        kind: CommandKind::Interrupt {
            session_id: session,
        },
    };
    store.enqueue(&create).unwrap();
    store.enqueue(&interrupt).unwrap();
    let outbox = store.outbox().unwrap();
    assert!(outbox[3].discardable());
    assert!(!outbox[4].discardable());
    store.withdraw(create.id, None).unwrap();
}

#[test]
fn withdraw_into_draft_is_one_transaction() {
    let (_tmp, dir) = data_dir();
    let session = SessionId::new();
    let command = send(session, "edit me");
    let older = Draft {
        text: "older".into(),
        cursor: 0,
    };
    let draft = Draft {
        text: "edit me\n\nolder".into(),
        cursor: 7,
    };
    let store = Store::open(&dir).unwrap();
    store.enqueue(&command).unwrap();
    store.set_draft(session, Some(&older)).unwrap();

    // A draft write that fails after the delete keeps the message queued:
    // the text is never lost.
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER full BEFORE UPDATE ON drafts BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
    )
    .unwrap();
    assert!(store.withdraw(command.id, Some((session, &draft))).is_err());
    assert_eq!(store.pending_outbox().unwrap().len(), 1);
    assert_eq!(store.drafts().unwrap()[&session], older);

    // Once it succeeds the text is in the draft only, never in both.
    conn.execute_batch("DROP TRIGGER full").unwrap();
    store.withdraw(command.id, Some((session, &draft))).unwrap();
    drop(store);
    let store = Store::open(&dir).unwrap();
    assert!(store.outbox().unwrap().is_empty());
    assert_eq!(store.drafts().unwrap()[&session], draft);

    // A refused withdraw writes no draft either.
    let other = SessionId::new();
    assert!(store.withdraw(command.id, Some((other, &draft))).is_err());
    assert!(!store.drafts().unwrap().contains_key(&other));
}
