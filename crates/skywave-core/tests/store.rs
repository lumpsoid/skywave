//! Local replica behaviour: idempotent apply, gap rejection, and state that
//! survives a reopen (cold start == normal start).

use std::path::{Path, PathBuf};

use chrono::{TimeZone, Utc};
use skywave_core::{
    BlobPolicy, BlobStatus, Core, Draft, OutboxStatus, PairedCredentials, ReadMark, Settings,
    Store, StoreError,
};
use skywave_proto::{
    ApprovalId, BlobId, BlobRef, Command, CommandId, CommandKind, CommandResult, Decision,
    DeviceId, DeviceInfo, Event, EventBatch, EventKind, RejectReason, Seq, SessionId, SessionState,
    ToolResult, TurnUsage,
};

fn event(session_id: SessionId, minute: u32, kind: EventKind) -> Event {
    Event {
        session_id,
        at: Utc.with_ymd_and_hms(2026, 10, 29, 9, minute, 0).unwrap(),
        command_id: kind.logged_by_command().then(CommandId::new),
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
                    suggestions: Vec::new(),
                },
            ),
            event(
                b,
                5,
                EventKind::UserMessage {
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
                    rule: None,
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

    let first = store.apply_batch(&log.batch(1, 4), Utc::now()).unwrap();
    assert_eq!((first.cursor, first.new_events), (Seq(4), 4));
    let after_first = store.load().unwrap();

    // Replaying the same batch changes nothing.
    let replay = store.apply_batch(&log.batch(1, 4), Utc::now()).unwrap();
    assert_eq!((replay.cursor, replay.new_events), (Seq(4), 0));
    assert!(replay.sessions.is_empty());
    assert_eq!(store.load().unwrap(), after_first);
    assert_eq!(event_count(&store, &log), 4);

    // An overlapping range stores only the new tail.
    let overlap = store.apply_batch(&log.batch(3, 7), Utc::now()).unwrap();
    assert_eq!((overlap.cursor, overlap.new_events), (Seq(7), 3));
    assert_eq!(event_count(&store, &log), 7);

    // An empty long-poll batch (`from = cursor + 1`) and a stale one are no-ops.
    let empty = store
        .apply_batch(
            &EventBatch {
                from: Seq(8),
                events: vec![],
            },
            Utc::now(),
        )
        .unwrap();
    assert_eq!((empty.cursor, empty.new_events), (Seq(7), 0));
    let stale = store
        .apply_batch(
            &EventBatch {
                from: Seq(2),
                events: vec![],
            },
            Utc::now(),
        )
        .unwrap();
    assert_eq!(stale.cursor, Seq(7));

    // Applying in one go yields the same replica as applying piecewise.
    let mut whole = Store::open_in_memory().unwrap();
    whole.apply_batch(&log.batch(1, 7), Utc::now()).unwrap();
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
    store.apply_batch(&log.batch(1, 2), Utc::now()).unwrap();
    let before = store.load().unwrap();

    let err = store.apply_batch(&log.batch(4, 5), Utc::now()).unwrap_err();
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
        store.apply_batch(&zero, Utc::now()),
        Err(StoreError::BatchFromZero)
    ));
}

#[test]
fn sessions_are_projected_from_events() {
    let log = Log::new();
    let mut store = Store::open_in_memory().unwrap();
    store.apply_batch(&log.batch(1, 5), Utc::now()).unwrap();

    let a = store.session(log.a).unwrap().unwrap();
    assert_eq!(a.workdir.as_deref(), Some("/srv/a"));
    assert_eq!(a.state, SessionState::AgentWorking);
    assert_eq!(a.pending_approval(), Some(log.approval));
    assert_eq!(a.last_seq, Seq(5));

    let applied = store.apply_batch(&log.batch(6, 7), Utc::now()).unwrap();
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
    store.apply_batch(&log.batch(1, 7), Utc::now()).unwrap();

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
    store.apply_batch(&log.batch(4, 4), Utc::now()).unwrap();
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
        store.apply_batch(&log.batch(1, 7), Utc::now()).unwrap();
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
    drop_alerted(&dir);
    drop_deleted(&dir);
    drop_usage(&dir);
    drop_archived(&dir);
    drop_titles(&dir);
    drop_unread(&dir);
    drop_drafts(&dir);
    drop_devices(&dir);
    drop_agent(&dir);
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

fn drop_titles(dir: &Path) {
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    conn.execute_batch(
        "ALTER TABLE sessions DROP COLUMN name;
         ALTER TABLE sessions DROP COLUMN first_prompt;",
    )
    .unwrap();
}

fn drop_archived(dir: &Path) {
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    conn.execute_batch("ALTER TABLE sessions DROP COLUMN archived;")
        .unwrap();
}

fn drop_alerted(dir: &Path) {
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    conn.execute_batch("DROP TABLE alerted_approvals;").unwrap();
}

fn drop_deleted(dir: &Path) {
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    conn.execute_batch("DROP TABLE deleted_sessions;").unwrap();
}

fn drop_usage(dir: &Path) {
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    conn.execute_batch("ALTER TABLE sessions DROP COLUMN usage;")
        .unwrap();
}

#[test]
fn turn_usage_is_summed_kept_and_starts_empty_after_the_migration() {
    let (_tmp, dir) = data_dir();
    let id = SessionId::new();
    let turn = |minute, usage| event(id, minute, EventKind::TurnUsage(usage));
    let batch = EventBatch {
        from: Seq(1),
        events: vec![
            event(
                id,
                0,
                EventKind::SessionCreated {
                    workdir: "/srv/x".into(),
                },
            ),
            turn(
                1,
                TurnUsage {
                    duration_ms: Some(1_000),
                    tokens_in: Some(3_000),
                    tokens_out: Some(800),
                    cost_usd: Some(0.25),
                    model: Some("model-a".into()),
                },
            ),
            turn(
                2,
                TurnUsage {
                    duration_ms: Some(500),
                    cost_usd: Some(0.5),
                    ..TurnUsage::default()
                },
            ),
        ],
    };
    let mut store = Store::open(&dir).unwrap();
    store.apply_batch(&batch, Utc::now()).unwrap();
    drop(store);

    // Kept over a reopen.
    let store = Store::open(&dir).unwrap();
    let usage = store.session(id).unwrap().unwrap().usage.unwrap();
    assert_eq!(usage.last_turn.duration_ms, Some(500));
    assert_eq!(usage.last_turn.tokens_in, None);
    assert_eq!(
        usage.total,
        TurnUsage {
            duration_ms: Some(1_500),
            tokens_in: Some(3_000),
            tokens_out: Some(800),
            cost_usd: Some(0.75),
            model: Some("model-a".into()),
        }
    );
    drop(store);

    // A replica from before the column holds no usage events.
    drop_alerted(&dir);
    drop_deleted(&dir);
    drop_usage(&dir);
    drop_devices(&dir);
    drop_agent(&dir);
    set_user_version(&dir, 7);
    let store = Store::open(&dir).unwrap();
    assert_eq!(store.session(id).unwrap().unwrap().usage, None);
}

#[test]
fn archived_flag_is_projected_and_starts_clear_after_the_migration() {
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
    let archived = |id, minute, archived| {
        event(
            id,
            minute,
            EventKind::SessionArchived {
                archived,
                by: DeviceId::new(),
            },
        )
    };
    // `a` stays archived; `b` is brought back by a message.
    let batch = EventBatch {
        from: Seq(1),
        events: vec![
            created(a, 0),
            created(b, 1),
            archived(a, 2, true),
            archived(b, 3, true),
            event(
                b,
                4,
                EventKind::UserMessage {
                    device_id: DeviceId::new(),
                    text: "back".into(),
                },
            ),
        ],
    };
    let mut store = Store::open(&dir).unwrap();
    store.apply_batch(&batch, Utc::now()).unwrap();
    let summary = |store: &Store, id| store.session(id).unwrap().unwrap();
    let a_summary = summary(&store, a);
    assert!(a_summary.archived);
    assert_eq!(a_summary.last_seq, Seq(1), "archiving is not activity");
    assert!(!summary(&store, b).archived);
    drop(store);

    // A replica from before the column (protocol v3: it holds no archive
    // events): everything starts active.
    drop_alerted(&dir);
    drop_deleted(&dir);
    drop_usage(&dir);
    drop_archived(&dir);
    drop_devices(&dir);
    drop_agent(&dir);
    set_user_version(&dir, 6);
    let store = Store::open(&dir).unwrap();
    assert!(!summary(&store, a).archived);
    assert_eq!(summary(&store, a).last_seq, Seq(1));
}

#[test]
fn names_and_first_prompts_are_projected_and_backfilled() {
    let (_tmp, dir) = data_dir();
    let (a, b, c) = (SessionId::new(), SessionId::new(), SessionId::new());
    let created = |id, minute| {
        event(
            id,
            minute,
            EventKind::SessionCreated {
                workdir: "/srv/x".into(),
            },
        )
    };
    let message = |id, minute, text: &str| {
        event(
            id,
            minute,
            EventKind::UserMessage {
                device_id: DeviceId::new(),
                text: text.into(),
            },
        )
    };
    let long = format!("{}\nsecond line", "é".repeat(250));
    // `a`: a multi-line first prompt after a blank one, then a rename;
    // `b`: a first line too long to keep; `c`: no message at all.
    let batch = EventBatch {
        from: Seq(1),
        events: vec![
            created(a, 0),
            created(b, 1),
            created(c, 2),
            message(a, 3, " \n "),
            message(a, 4, "\n  fix the flaky test \r\nin ci"),
            message(a, 5, "and then"),
            message(b, 6, &long),
            event(
                a,
                7,
                EventKind::SessionRenamed {
                    name: Some("flaky".into()),
                    by: DeviceId::new(),
                },
            ),
        ],
    };
    let mut store = Store::open(&dir).unwrap();
    store.apply_batch(&batch, Utc::now()).unwrap();
    let summary = |store: &Store, id| store.session(id).unwrap().unwrap();
    let a_summary = summary(&store, a);
    assert_eq!(a_summary.name.as_deref(), Some("flaky"));
    assert_eq!(
        a_summary.first_prompt.as_deref(),
        Some("fix the flaky test")
    );
    assert_eq!(a_summary.last_seq, Seq(6), "a rename is not activity");
    let expected_b = "é".repeat(200);
    assert_eq!(
        summary(&store, b).first_prompt.as_deref(),
        Some(&*expected_b)
    );
    assert_eq!(summary(&store, c).first_prompt, None);
    let before: Vec<_> = [a, b, c].map(|id| summary(&store, id)).into();
    drop(store);

    // A replica from before the columns (protocol v3: it holds no renames):
    // the migration derives the first prompts from the log like the fold.
    drop_alerted(&dir);
    drop_deleted(&dir);
    drop_usage(&dir);
    drop_archived(&dir);
    drop_titles(&dir);
    drop_devices(&dir);
    drop_agent(&dir);
    set_user_version(&dir, 5);
    let store = Store::open(&dir).unwrap();
    for expected in before {
        let actual = summary(&store, expected.id);
        assert_eq!(actual.first_prompt, expected.first_prompt);
        assert_eq!(actual.name, None, "no name is backfilled");
    }
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
                    suggestions: Vec::new(),
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
    store.apply_batch(&batch, Utc::now()).unwrap();
    let notable = |store: &Store, id| store.session(id).unwrap().unwrap().notable;
    assert_eq!((notable(&store, a), notable(&store, b)), (3, 0));
    assert!(store.read_marks().unwrap().is_empty(), "nothing read yet");
    drop(store);

    // A replica from before the column: the migration derives it from the
    // log and marks everything stored as read, so nothing shows up as new.
    drop_alerted(&dir);
    drop_deleted(&dir);
    drop_usage(&dir);
    drop_archived(&dir);
    drop_titles(&dir);
    drop_unread(&dir);
    drop_devices(&dir);
    drop_agent(&dir);
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

/// A pulled event settles the outbox command it names that is still
/// unanswered, as accepted at its seq, and leaves an answered one alone,
/// as it does another device's command.
#[test]
fn events_settle_the_commands_they_name() {
    let mut store = Store::open_in_memory().unwrap();
    let session_id = SessionId::new();
    let [lost, answered] = [send(session_id, "lost"), send(session_id, "answered")];
    for command in [&lost, &answered] {
        store.enqueue(command).unwrap();
        assert!(store.claim_send(command.id, at(1)).unwrap());
    }
    let rejected = CommandResult::Rejected {
        reason: RejectReason::UnknownSession,
    };
    store.record_result(answered.id, &rejected, at(2)).unwrap();
    let logged = |command: Option<&Command>| Event {
        command_id: command.map(|c| c.id).or(Some(CommandId::new())),
        ..event(
            session_id,
            1,
            EventKind::UserMessage {
                device_id: DeviceId::new(),
                text: "x".into(),
            },
        )
    };
    let batch = EventBatch {
        from: Seq(1),
        events: vec![logged(None), logged(Some(&lost)), logged(Some(&answered))],
    };

    let applied = store.apply_batch(&batch, at(3)).unwrap();
    assert_eq!(applied.settled, [(lost.id, Seq(2))]);
    let outbox = store.outbox().unwrap();
    let statuses: Vec<_> = outbox
        .iter()
        .map(|e| (e.status.clone(), e.answered_at))
        .collect();
    assert_eq!(
        statuses,
        [
            (
                OutboxStatus::Done(CommandResult::Accepted { seq: Seq(2) }),
                Some(at(3))
            ),
            (OutboxStatus::Done(rejected), Some(at(2))),
        ]
    );
    // The reply that comes after changes nothing.
    store
        .record_result(lost.id, &CommandResult::Accepted { seq: Seq(2) }, at(4))
        .unwrap();
    assert_eq!(store.outbox().unwrap(), outbox);
    assert!(store.pending_outbox().unwrap().is_empty());
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
    store.apply_batch(&batch, Utc::now()).unwrap();
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
    drop_alerted(&dir);
    drop_deleted(&dir);
    drop_usage(&dir);
    drop_archived(&dir);
    drop_titles(&dir);
    drop_unread(&dir);
    drop_drafts(&dir);
    drop_devices(&dir);
    drop_agent(&dir);
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
    let [fresh, claimed, requeued, refused, undeliverable, accepted] = [
        "fresh",
        "claimed",
        "requeued",
        "refused",
        "undeliverable",
        "accepted",
    ]
    .map(|text| send(session, text));
    for command in [
        &fresh,
        &claimed,
        &requeued,
        &refused,
        &undeliverable,
        &accepted,
    ] {
        store.enqueue(command).unwrap();
    }
    for command in [&claimed, &requeued, &refused, &undeliverable, &accepted] {
        assert!(store.claim_send(command.id, at(1)).unwrap());
    }
    let rejected = CommandResult::Rejected {
        reason: RejectReason::UnknownSession,
    };
    store.record_result(refused.id, &rejected, at(2)).unwrap();
    store.record_undeliverable(undeliverable.id, at(2)).unwrap();
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
            ("undeliverable", true),
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
    store.withdraw(undeliverable.id, None).unwrap();
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

// ---- local delete ---------------------------------------------------------------

/// The whole log applied, plus what this device keeps for both sessions:
/// drafts, read marks, an answered and an unanswered message each, and
/// session A's blob downloaded.
fn replica_with_both_sessions(dir: &Path, log: &Log) -> (Store, [Command; 4]) {
    let mut store = Store::open(dir).unwrap();
    store.apply_batch(&log.batch(1, 7), Utc::now()).unwrap();
    let draft = Draft {
        text: "later".into(),
        cursor: 0,
    };
    let commands = [
        send(log.a, "answered a"),
        send(log.a, "waiting a"),
        send(log.b, "answered b"),
        send(log.b, "waiting b"),
    ];
    for (i, command) in commands.iter().enumerate() {
        store.enqueue(command).unwrap();
        if i % 2 == 0 {
            let result = CommandResult::Accepted { seq: Seq(1) };
            store.record_result(command.id, &result, at(1)).unwrap();
        }
    }
    for id in [log.a, log.b] {
        store.set_draft(id, Some(&draft)).unwrap();
        let mark = ReadMark {
            seq: Seq(2),
            notable: 0,
        };
        store.set_read_mark(id, mark).unwrap();
    }
    let available = BlobStatus::Available {
        path: "/data/blobs/x".into(),
    };
    store.set_blob_status(log.blob.id, &available).unwrap();
    (store, commands)
}

#[test]
fn local_delete_is_one_transaction() {
    let log = Log::new();
    let (_tmp, dir) = data_dir();
    let (mut store, commands) = replica_with_both_sessions(&dir, &log);
    let before = store.load().unwrap();
    let a_events = store.session_events(log.a).unwrap();

    // A delete that fails at its last write (the tombstone) changes nothing.
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER full BEFORE INSERT ON deleted_sessions
         BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
    )
    .unwrap();
    assert!(store.delete_session_locally(log.a, at(2)).is_err());
    assert_eq!(store.load().unwrap(), before);
    assert_eq!(store.session_events(log.a).unwrap(), a_events);

    conn.execute_batch("DROP TRIGGER full").unwrap();
    assert_eq!(
        store.delete_session_locally(log.a, at(2)).unwrap(),
        [log.blob.id],
        "the blobs whose files go"
    );
    drop(store);

    // Everything of A is gone, over a reopen; B is untouched.
    let store = Store::open(&dir).unwrap();
    let after = store.load().unwrap();
    assert_eq!(after.cursor, Seq(7));
    assert_eq!(
        after.sessions.iter().map(|s| s.id).collect::<Vec<_>>(),
        [log.b]
    );
    assert!(store.session_events(log.a).unwrap().is_empty());
    assert_eq!(store.session_events(log.b).unwrap().len(), 2);
    assert!(after.blobs.is_empty());
    assert_eq!(after.drafts.keys().collect::<Vec<_>>(), [&log.b]);
    assert_eq!(after.read_marks.keys().collect::<Vec<_>>(), [&log.b]);
    // Only A's answered command goes; an unanswered one is the caller's to
    // refuse the delete over.
    let left: Vec<CommandId> = after.outbox.iter().map(|e| e.command.id).collect();
    assert_eq!(left, [commands[1].id, commands[2].id, commands[3].id]);
    assert_eq!(after.deleted, [log.a].into());
}

#[test]
fn events_for_deleted_session_advance_cursor_but_are_not_stored() {
    let log = Log::new();
    let mut store = Store::open_in_memory().unwrap();
    store.apply_batch(&log.batch(1, 3), Utc::now()).unwrap();
    store.delete_session_locally(log.a, at(2)).unwrap();

    // The rest of A (its blob and approval among it) arrives with B's message.
    let applied = store.apply_batch(&log.batch(4, 7), Utc::now()).unwrap();
    assert_eq!(applied.cursor, Seq(7));
    assert_eq!(applied.new_events, 1, "only B's message is stored");
    assert_eq!(applied.sessions, [log.b]);
    assert!(applied.blobs.is_empty());
    let snapshot = store.load().unwrap();
    assert_eq!(snapshot.cursor, Seq(7));
    assert_eq!(
        snapshot.sessions.iter().map(|s| s.id).collect::<Vec<_>>(),
        [log.b],
        "no summary brings A back"
    );
    assert!(store.session_events(log.a).unwrap().is_empty());
    assert!(store.blob(log.blob.id).unwrap().is_none());
}

#[test]
fn cursor_stays_contiguous_after_local_delete() {
    let log = Log::new();
    let mut store = Store::open_in_memory().unwrap();
    store.apply_batch(&log.batch(1, 2), Utc::now()).unwrap();
    store.delete_session_locally(log.a, at(2)).unwrap();

    // Batches of only A's events still move the cursor, so the next batch
    // follows on and a gap is still a gap.
    assert_eq!(
        store
            .apply_batch(&log.batch(3, 3), Utc::now())
            .unwrap()
            .cursor,
        Seq(3)
    );
    assert_eq!(
        store
            .apply_batch(&log.batch(4, 5), Utc::now())
            .unwrap()
            .cursor,
        Seq(5)
    );
    assert!(matches!(
        store.apply_batch(&log.batch(7, 7), Utc::now()),
        Err(StoreError::Gap {
            cursor: Seq(5),
            from: Seq(7)
        })
    ));
    // A replay of what was skipped stays skipped.
    assert_eq!(
        store
            .apply_batch(&log.batch(1, 7), Utc::now())
            .unwrap()
            .cursor,
        Seq(7)
    );
    assert!(store.session_events(log.a).unwrap().is_empty());

    // B's stored events have holes where A's were; read per session they
    // are whole, and the replica equals one built without A at all.
    let b_events = store.session_events(log.b).unwrap();
    assert_eq!(
        b_events,
        [
            (Seq(3), log.events[2].clone()),
            (Seq(6), log.events[5].clone())
        ]
    );
    let mut whole = Store::open_in_memory().unwrap();
    whole.apply_batch(&log.batch(1, 7), Utc::now()).unwrap();
    assert_eq!(whole.session_events(log.b).unwrap(), b_events);
    assert_eq!(whole.session(log.b).unwrap(), store.session(log.b).unwrap());
}

/// A CA certificate as pairing stores it: a fresh self-signed PEM, so
/// every call is another server.
fn ca_pem() -> String {
    let key = rcgen::KeyPair::generate().unwrap();
    rcgen::CertificateParams::new(vec!["skywave ca".to_owned()])
        .unwrap()
        .self_signed(&key)
        .unwrap()
        .pem()
}

fn paired_with(ca_cert_pem: &str) -> PairedCredentials {
    PairedCredentials {
        device_id: DeviceId::new(),
        device_key_pem: "KEY".into(),
        device_cert_pem: "CERT".into(),
        ca_cert_pem: ca_cert_pem.to_owned(),
    }
}

/// Rows in each table that pairing with another server must empty.
fn server_rows(dir: &Path) -> Vec<(&'static str, i64)> {
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    [
        "events",
        "sessions",
        "outbox",
        "blobs",
        "drafts",
        "read_marks",
        "deleted_sessions",
    ]
    .into_iter()
    .map(|table| {
        let rows = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        (table, rows)
    })
    .collect()
}

#[test]
fn pairing_with_another_server_resets_the_replica_and_the_same_server_keeps_it() {
    let log = Log::new();
    let (_tmp, dir) = data_dir();
    let ca = ca_pem();
    let core = Core::open_detached(&dir).unwrap();
    let seam = core.seam();
    seam.pairing_succeeded("old.example:7443", paired_with(&ca))
        .unwrap();
    seam.apply_events(&log.batch(1, 7)).unwrap();

    // Something in every table, and a downloaded blob.
    let blob_file = skywave_core::blob_dir(&dir).join(log.blob.id.0.to_string());
    std::fs::create_dir_all(blob_file.parent().unwrap()).unwrap();
    std::fs::write(&blob_file, "content").unwrap();
    let mut store = Store::open(&dir).unwrap();
    store
        .set_blob_status(
            log.blob.id,
            &BlobStatus::Available {
                path: blob_file.clone(),
            },
        )
        .unwrap();
    store.enqueue(&send(log.b, "queued")).unwrap();
    let draft = Draft {
        text: "unsent".into(),
        cursor: 0,
    };
    store.set_draft(log.a, Some(&draft)).unwrap();
    let mark = ReadMark {
        seq: Seq(3),
        notable: 1,
    };
    store.set_read_mark(log.a, mark).unwrap();
    store
        .delete_session_locally(SessionId::new(), at(0))
        .unwrap();
    store.set_blob_policy(BlobPolicy::Eager).unwrap();
    let before = store.load().unwrap();
    let rows = server_rows(&dir);
    assert!(rows.iter().all(|(_, n)| *n > 0), "{rows:?}");

    // Again with the same server, e.g. after a revocation: all kept.
    seam.pairing_succeeded("old.example:7443", paired_with(&ca))
        .unwrap();
    assert_eq!(store.load().unwrap(), before);
    assert_eq!(server_rows(&dir), rows);
    assert!(blob_file.exists());

    // Another server: nothing of the old one is left.
    seam.pairing_succeeded("new.example:7443", paired_with(&ca_pem()))
        .unwrap();
    let rows = server_rows(&dir);
    assert!(rows.iter().all(|(_, n)| *n == 0), "{rows:?}");
    let after = store.load().unwrap();
    assert_eq!(after.cursor, Seq::ZERO);
    assert_eq!(
        after.settings,
        Settings {
            blob_policy: BlobPolicy::Eager,
            server_address: Some("new.example:7443".into()),
        },
        "the blob policy is this device's"
    );
    assert!(!blob_file.exists(), "blob files go with their rows");

    // The new server's log starts from the beginning.
    let applied = seam.apply_events(&log.batch(1, 2)).unwrap();
    assert_eq!((applied.cursor, applied.new_events), (Seq(2), 2));
}

fn requested(session_id: SessionId, minute: u32, id: ApprovalId) -> Event {
    event(
        session_id,
        minute,
        EventKind::ApprovalRequested {
            id,
            call: None,
            tool: "Bash".into(),
            summary: "ls".into(),
            input: None,
            suggestions: Vec::new(),
        },
    )
}

fn created(session_id: SessionId, minute: u32) -> Event {
    event(
        session_id,
        minute,
        EventKind::SessionCreated {
            workdir: "/srv/x".into(),
        },
    )
}

fn sorted(ids: impl IntoIterator<Item = ApprovalId>) -> Vec<ApprovalId> {
    let mut ids: Vec<_> = ids.into_iter().collect();
    ids.sort();
    ids
}

#[test]
fn alerted_approvals_are_kept_until_they_stop_pending() {
    let (_tmp, dir) = data_dir();
    let (a, b) = (SessionId::new(), SessionId::new());
    let (x, y, z) = (ApprovalId::new(), ApprovalId::new(), ApprovalId::new());
    let mut store = Store::open(&dir).unwrap();
    store
        .apply_batch(
            &EventBatch {
                from: Seq(1),
                events: vec![
                    created(a, 0),
                    created(b, 1),
                    requested(a, 2, x),
                    requested(a, 3, y),
                    requested(b, 4, z),
                ],
            },
            Utc::now(),
        )
        .unwrap();
    assert!(store.load().unwrap().alerted.is_empty(), "none alerted yet");
    store.mark_alerted(&[(a, x), (b, z)]).unwrap();
    store.mark_alerted(&[(a, x)]).unwrap();
    drop(store);

    // Kept over a reopen.
    let mut store = Store::open(&dir).unwrap();
    assert_eq!(sorted(store.load().unwrap().alerted), sorted([x, z]));

    // Resolved or cancelled, it goes with the batch that says so; another
    // session's events leave it alone.
    store
        .apply_batch(
            &EventBatch {
                from: Seq(6),
                events: vec![
                    event(
                        a,
                        5,
                        EventKind::ApprovalResolved {
                            id: x,
                            decision: Decision::Approve,
                            by: DeviceId::new(),
                            rule: None,
                        },
                    ),
                    event(a, 6, EventKind::AgentMessage { text: "ok".into() }),
                ],
            },
            Utc::now(),
        )
        .unwrap();
    assert_eq!(sorted(store.alerted().unwrap()), vec![z]);
    store.mark_alerted(&[(a, y)]).unwrap();
    store
        .apply_batch(
            &EventBatch {
                from: Seq(8),
                events: vec![event(
                    a,
                    7,
                    EventKind::ApprovalCancelled {
                        id: y,
                        reason: "restart".into(),
                    },
                )],
            },
            Utc::now(),
        )
        .unwrap();
    assert_eq!(sorted(store.alerted().unwrap()), vec![z]);

    // Its session deleted here, it goes too.
    store.delete_session_locally(b, Utc::now()).unwrap();
    assert!(store.alerted().unwrap().is_empty());

    // As it does with a new server.
    let w = ApprovalId::new();
    store
        .apply_batch(
            &EventBatch {
                from: Seq(9),
                events: vec![requested(a, 8, w)],
            },
            Utc::now(),
        )
        .unwrap();
    store.mark_alerted(&[(a, w)]).unwrap();
    store.reset_for_server("other:7443").unwrap();
    assert!(store.alerted().unwrap().is_empty());
}

#[test]
fn the_migration_counts_every_pending_approval_as_alerted() {
    let (_tmp, dir) = data_dir();
    let (a, b) = (SessionId::new(), SessionId::new());
    let (x, y, z) = (ApprovalId::new(), ApprovalId::new(), ApprovalId::new());
    let mut store = Store::open(&dir).unwrap();
    store
        .apply_batch(
            &EventBatch {
                from: Seq(1),
                events: vec![
                    created(a, 0),
                    created(b, 1),
                    requested(a, 2, x),
                    requested(a, 3, y),
                    requested(b, 4, z),
                    event(
                        b,
                        5,
                        EventKind::ApprovalCancelled {
                            id: z,
                            reason: "restart".into(),
                        },
                    ),
                ],
            },
            Utc::now(),
        )
        .unwrap();
    drop(store);

    // A replica from before the table: what is pending is old news.
    drop_alerted(&dir);
    drop_devices(&dir);
    drop_agent(&dir);
    set_user_version(&dir, 9);
    let store = Store::open(&dir).unwrap();
    assert_eq!(sorted(store.load().unwrap().alerted), sorted([x, y]));
}

fn drop_devices(dir: &Path) {
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    conn.execute_batch("DROP TABLE devices;").unwrap();
}

/// Undoes the agent settings migration.
fn drop_agent(dir: &Path) {
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    conn.execute_batch("ALTER TABLE sessions DROP COLUMN agent; DROP TABLE offered_models;")
        .unwrap();
}

#[test]
fn device_names_survive_a_reopen_and_go_with_the_server() {
    let (_tmp, dir) = data_dir();
    let (laptop, phone) = (DeviceId::new(), DeviceId::new());
    let named = |id, name: &str| DeviceInfo {
        id,
        name: name.into(),
    };
    let mut store = Store::open(&dir).unwrap();
    store
        .set_devices(&[named(laptop, "laptop"), named(phone, "phone")])
        .unwrap();
    // A later list renames; one it leaves out keeps its name.
    store.set_devices(&[named(phone, "Pixel 8")]).unwrap();
    drop(store);

    let mut store = Store::open(&dir).unwrap();
    let devices = store.load().unwrap().devices;
    assert_eq!(devices.len(), 2);
    assert_eq!(devices[&laptop], "laptop");
    assert_eq!(devices[&phone], "Pixel 8");

    store.reset_for_server("other:7443").unwrap();
    assert!(store.devices().unwrap().is_empty());
}

#[test]
fn agent_settings_and_offered_models_are_kept() {
    use skywave_proto::{ModelChoice, PermissionMode};
    let (_tmp, dir) = data_dir();
    let id = SessionId::new();
    let other = SessionId::new();
    let models = vec![ModelChoice {
        id: "sonnet".into(),
        label: "Sonnet".into(),
        detail: None,
        resolved: None,
    }];
    let mut store = Store::open(&dir).unwrap();
    let event = |session_id, kind| Event {
        session_id,
        at: chrono::Utc::now(),
        command_id: None,
        kind,
    };
    let batch = EventBatch {
        from: Seq(1),
        events: vec![
            event(
                id,
                EventKind::SessionCreated {
                    workdir: "/srv/a".into(),
                },
            ),
            event(
                id,
                EventKind::PermissionModeChanged {
                    mode: PermissionMode::Plan,
                    answers: None,
                },
            ),
            event(
                other,
                EventKind::SessionCreated {
                    workdir: "/srv/b".into(),
                },
            ),
            event(
                other,
                EventKind::ModelsOffered {
                    models: models.clone(),
                },
            ),
        ],
    };
    store.apply_batch(&batch, chrono::Utc::now()).unwrap();
    drop(store);
    let store = Store::open(&dir).unwrap();
    let summary = store.session(id).unwrap().unwrap();
    assert_eq!(summary.agent.mode.current, Some(PermissionMode::Plan));
    // A mode is not activity.
    assert_eq!(summary.last_seq, Seq(1));
    assert_eq!(store.load().unwrap().models, models);
}
