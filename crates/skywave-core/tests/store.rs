//! Local replica behaviour: idempotent apply, gap rejection, and state that
//! survives a reopen (cold start == normal start).

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{TimeZone, Utc};
use skywave_core::attachment::AttachedImage;
use skywave_core::workdir::{WorkdirAnswer, WorkdirAsk};
use skywave_core::{
    Action, BlobFate, BlobPolicy, BlobStatus, ComposerMode, Core, CoreError, Draft, LockError,
    OutboxStatus, PairedCredentials, PendingCrop, ReadMark, RunWhen, Screen, ServerAddress,
    ServerFacts, Settings, Snippet, SnippetId, Store, StoreError,
};
use skywave_proto::{
    AgentWork, ApprovalId, BackgroundTask, BlobId, BlobRef, BlobUse, ChangeSet, ChangedFile,
    ChangesResult, Command, CommandId, CommandKind, CommandResult, ContentHash, ContextId,
    Decision, DeviceId, DeviceInfo, DiffBody, Event, EventBatch, EventKind, FileBody, FileChange,
    FileResult, Full, ImageType, ImportOutcome, ImportedItem, LimitSeverity, LimitWindow, LineSpan,
    RejectReason, Schedule, ScheduleId, Seq, SessionId, SessionOrigin, SessionState, TaskId,
    TaskKind, ToolResult, TurnUsage, UsageLimit, UsageLimitsResult,
};

fn event(session_id: SessionId, minute: u32, kind: EventKind) -> Event {
    Event {
        session_id,
        at: Utc.with_ymd_and_hms(2026, 10, 29, 9, minute, 0).unwrap(),
        command_id: kind.logged_by_command().then(CommandId::new),
        context: ContextId::first_of(session_id),
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
            sha256: ContentHash::of(b"ran tests"),
        };
        let events = vec![
            event(
                a,
                0,
                EventKind::SessionCreated {
                    workdir: "/srv/a".into(),
                    context_name: "start".into(),
                    from: None,
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
                    context_name: "start".into(),
                    from: None,
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
                    output: Some(Full::Blob(blob.clone())),
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
                    attachments: Vec::new(),
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

fn addr(text: &str) -> ServerAddress {
    ServerAddress::parse(text).unwrap()
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
            server: ServerFacts::default(),
        }
    );
}

#[test]
fn a_second_core_on_one_data_dir_is_refused_until_the_first_closes() {
    let (_tmp, dir) = data_dir();
    let first = Core::open_detached(&dir).unwrap();
    let err = Core::open_detached(&dir).err().unwrap();
    match &err {
        CoreError::Lock(LockError::InUse { path }) => assert_eq!(path, &dir),
        other => panic!("expected InUse, got {other:?}"),
    }
    assert_eq!(
        err.to_string(),
        format!("another skywave client is using {}", dir.display())
    );
    // An engine core is refused the same way.
    assert!(matches!(
        Core::open(&dir).err().unwrap(),
        CoreError::Lock(LockError::InUse { .. })
    ));
    drop(first);
    let again = Core::open(&dir).unwrap();
    again.shutdown(Duration::from_secs(5)).unwrap();
    Core::open_detached(&dir).unwrap();
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
    // Most recently written to first: `b`'s message outranks `a`'s later
    // approval.
    assert_eq!(
        sessions.iter().map(|s| s.id).collect::<Vec<_>>(),
        vec![log.b, log.a]
    );
    assert_eq!(sessions[1].pending_approval(), None);
    assert_eq!(sessions[0].last_seq, Seq(6));

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

/// Every blob keeps the hash its reference promised (event and workdir
/// blobs alike, over a reopen), and loses it with its row; a row without
/// one, as a replica from before hashes has, leaves the replica unreadable.
#[test]
fn a_blob_keeps_its_hash_until_its_row_goes() {
    let log = Log::new();
    let (_tmp, dir) = data_dir();
    let mut store = Store::open(&dir).unwrap();
    store.apply_batch(&log.batch(1, 7), Utc::now()).unwrap();
    let diff = diff_blob(7);
    put(&mut store, &changes_ask(log.b), &changes(&[&diff]));
    drop(store);

    let store = Store::open(&dir).unwrap();
    let hashes = |store: &Store| -> Vec<(BlobId, ContentHash)> {
        store
            .blobs()
            .unwrap()
            .into_iter()
            .map(|b| (b.id, b.sha256))
            .collect()
    };
    let mut want = vec![(log.blob.id, log.blob.sha256), (diff.id, diff.sha256)];
    want.sort_by_key(|(id, _)| *id);
    assert_ne!(log.blob.sha256, diff.sha256);
    assert_eq!(hashes(&store), want);
    assert_eq!(store.blob(diff.id).unwrap().unwrap().sha256, diff.sha256);

    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    let rows = |conn: &rusqlite::Connection| -> i64 {
        conn.query_row("SELECT count(*) FROM blob_hashes", [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(rows(&conn), 2);
    let mut store = store;
    store.delete_session_locally(log.a, at(2)).unwrap();
    assert_eq!(hashes(&store), [(diff.id, diff.sha256)]);
    assert_eq!(rows(&conn), 1, "the event blob's hash went with its row");

    conn.execute_batch("DELETE FROM blob_hashes").unwrap();
    let err = store.blobs().unwrap_err();
    assert!(err.unreadable(), "{err}");
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
}

fn send(session_id: SessionId, text: &str) -> Command {
    Command {
        id: CommandId::new(),
        kind: CommandKind::SendMessage {
            session_id,
            context: ContextId::first_of(session_id),
            text: text.into(),
            attachments: Vec::new(),
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
    drop_shell(&dir);
    drop_outbox_delivery_columns(&dir);
    drop_alerted(&dir);
    drop_deleted(&dir);
    drop_usage(&dir);
    drop_archived(&dir);
    drop_titles(&dir);
    drop_unread(&dir);
    drop_drafts(&dir);
    drop_devices(&dir);
    drop_snippets(&dir);
    drop_work(&dir);
    drop_contexts(&dir);
    drop_agent(&dir);
    drop_turns(&dir);
    drop_workdir(&dir);
    drop_import_columns(&dir);
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

fn drop_attachments(dir: &Path) {
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    conn.execute_batch(
        "DROP TABLE draft_attachments;
         DROP TABLE attachments;",
    )
    .unwrap();
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
                    context_name: "start".into(),
                    from: None,
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
                    ..TurnUsage::default()
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
            ..TurnUsage::default()
        }
    );
    drop(store);

    // A replica from before the column holds no usage events.
    drop_shell(&dir);
    drop_alerted(&dir);
    drop_deleted(&dir);
    drop_usage(&dir);
    drop_devices(&dir);
    drop_snippets(&dir);
    drop_work(&dir);
    drop_contexts(&dir);
    drop_agent(&dir);
    drop_turns(&dir);
    drop_workdir(&dir);
    drop_import_columns(&dir);
    set_user_version(&dir, 7);
    let store = Store::open(&dir).unwrap();
    assert_eq!(store.session(id).unwrap().unwrap().usage, None);
}

#[test]
fn turns_are_kept_by_seq_dropped_with_their_session_and_backfilled() {
    let (_tmp, dir) = data_dir();
    let (a, b) = (SessionId::new(), SessionId::new());
    let created = |id| {
        event(
            id,
            0,
            EventKind::SessionCreated {
                workdir: "/srv/x".into(),
                context_name: "start".into(),
                from: None,
            },
        )
    };
    let priced = TurnUsage {
        tokens_in: Some(3_000),
        cost_usd: Some(0.25),
        model: Some("model-a".into()),
        ..TurnUsage::default()
    };
    let batch = EventBatch {
        from: Seq(1),
        events: vec![
            created(a),
            created(b),
            event(a, 1, EventKind::TurnUsage(priced.clone())),
            event(b, 2, EventKind::TurnUsage(TurnUsage::default())),
        ],
    };
    let mut store = Store::open(&dir).unwrap();
    store.apply_batch(&batch, Utc::now()).unwrap();
    drop(store);

    let turns = |store: &Store| -> Vec<(Seq, SessionId, TurnUsage)> {
        store
            .load()
            .unwrap()
            .turns
            .into_iter()
            .map(|t| (t.seq, t.session_id, t.usage))
            .collect()
    };
    let store = Store::open(&dir).unwrap();
    let both = vec![
        (Seq(3), a, priced.clone()),
        (Seq(4), b, TurnUsage::default()),
    ];
    assert_eq!(turns(&store), both, "kept over a reopen");
    assert_eq!(
        store.turns().unwrap()[0].at,
        Utc.with_ymd_and_hms(2026, 10, 29, 9, 1, 0).unwrap()
    );
    drop(store);

    // A replica from before the table backfills it from the stored log.
    drop_turns(&dir);
    drop_workdir(&dir);
    drop_import_columns(&dir);
    set_user_version(&dir, skywave_core::store::SCHEMA_VERSION - 14);
    let mut store = Store::open(&dir).unwrap();
    assert_eq!(turns(&store), both);
    assert_eq!(
        store.turns().unwrap()[1].at,
        Utc.with_ymd_and_hms(2026, 10, 29, 9, 2, 0).unwrap()
    );

    store.delete_session_locally(b, Utc::now()).unwrap();
    assert_eq!(turns(&store), vec![(Seq(3), a, priced)]);
    // A deleted session's later turns are not stored.
    let later = EventBatch {
        from: Seq(5),
        events: vec![event(b, 3, EventKind::TurnUsage(TurnUsage::default()))],
    };
    store.apply_batch(&later, Utc::now()).unwrap();
    assert_eq!(turns(&store).len(), 1);

    store.reset_for_server().unwrap();
    assert!(turns(&store).is_empty());
}

#[test]
fn background_work_folds_quietly_survives_a_reopen_and_starts_empty_after_the_migration() {
    let (_tmp, dir) = data_dir();
    let id = SessionId::new();
    let task = BackgroundTask {
        call: None,
        id: TaskId("b7".into()),
        kind: TaskKind::Shell,
        title: "npm run dev".into(),
        parent: None,
        ambient: false,
    };
    let schedule = Schedule {
        id: ScheduleId("s1".into()),
        when: "0 9 * * *".into(),
        recurring: true,
        prompt: "summarise the night's CI".into(),
        outlives_agent: None,
    };
    let created = event(
        id,
        0,
        EventKind::SessionCreated {
            workdir: "/srv/x".into(),
            context_name: "start".into(),
            from: None,
        },
    );
    let mut store = Store::open(&dir).unwrap();
    store
        .apply_batch(
            &EventBatch {
                from: Seq(1),
                events: vec![
                    created,
                    event(
                        id,
                        1,
                        EventKind::BackgroundTasksChanged {
                            tasks: vec![task.clone()],
                        },
                    ),
                    event(
                        id,
                        2,
                        EventKind::SchedulesChanged {
                            schedules: vec![schedule.clone()],
                        },
                    ),
                ],
            },
            Utc::now(),
        )
        .unwrap();
    let summary = store.session(id).unwrap().unwrap();
    assert_eq!(
        summary.work,
        AgentWork {
            tasks: vec![task],
            schedules: vec![schedule.clone()],
        }
    );
    // Neither is news nor activity.
    assert_eq!((summary.notable, summary.last_seq), (0, Seq(1)));

    // The last set wins and leaves the other set alone; a notable event in
    // the same session does count, so the check above can see one.
    store
        .apply_batch(
            &EventBatch {
                from: Seq(4),
                events: vec![
                    event(id, 3, EventKind::BackgroundTasksChanged { tasks: vec![] }),
                    event(
                        id,
                        4,
                        EventKind::AgentMessage {
                            parent: None,
                            text: "hi".into(),
                            full: None,
                        },
                    ),
                ],
            },
            Utc::now(),
        )
        .unwrap();
    drop(store);

    // Kept over a reopen.
    let store = Store::open(&dir).unwrap();
    let summary = store.session(id).unwrap().unwrap();
    assert_eq!(
        summary.work,
        AgentWork {
            tasks: vec![],
            schedules: vec![schedule],
        }
    );
    assert_eq!((summary.notable, summary.last_seq), (1, Seq(5)));
    drop(store);

    // A replica from before the column holds no background work events.
    drop_shell(&dir);
    drop_work(&dir);
    drop_turns(&dir);
    drop_workdir(&dir);
    drop_import_columns(&dir);
    set_user_version(&dir, skywave_core::store::SCHEMA_VERSION - 16);
    let store = Store::open(&dir).unwrap();
    let summary = store.session(id).unwrap().unwrap();
    assert_eq!(summary.work, AgentWork::default());
    assert_eq!(summary.notable, 1, "the rest of the row is kept");
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
                context_name: "start".into(),
                from: None,
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
                    attachments: Vec::new(),
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
    drop_shell(&dir);
    drop_alerted(&dir);
    drop_deleted(&dir);
    drop_usage(&dir);
    drop_archived(&dir);
    drop_devices(&dir);
    drop_snippets(&dir);
    drop_work(&dir);
    drop_contexts(&dir);
    drop_agent(&dir);
    drop_turns(&dir);
    drop_workdir(&dir);
    drop_import_columns(&dir);
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
                context_name: "start".into(),
                from: None,
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
                attachments: Vec::new(),
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
    drop_shell(&dir);
    drop_alerted(&dir);
    drop_deleted(&dir);
    drop_usage(&dir);
    drop_archived(&dir);
    drop_titles(&dir);
    drop_devices(&dir);
    drop_snippets(&dir);
    drop_work(&dir);
    drop_contexts(&dir);
    drop_agent(&dir);
    drop_turns(&dir);
    drop_workdir(&dir);
    drop_import_columns(&dir);
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
                context_name: "start".into(),
                from: None,
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
                    attachments: Vec::new(),
                },
            ),
            event(
                a,
                3,
                EventKind::SessionStateChanged(SessionState::AgentWorking),
            ),
            event(
                a,
                4,
                EventKind::AgentMessage {
                    parent: None,
                    text: "ok".into(),
                    full: None,
                },
            ),
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
    drop_shell(&dir);
    drop_alerted(&dir);
    drop_deleted(&dir);
    drop_usage(&dir);
    drop_archived(&dir);
    drop_titles(&dir);
    drop_unread(&dir);
    drop_devices(&dir);
    drop_snippets(&dir);
    drop_work(&dir);
    drop_contexts(&dir);
    drop_agent(&dir);
    drop_turns(&dir);
    drop_workdir(&dir);
    drop_import_columns(&dir);
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
                attachments: Vec::new(),
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

fn stamp(dir: &Path) -> Vec<u32> {
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    let mut stmt = conn.prepare("SELECT version FROM protocol_stamp").unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn a_replica_of_every_migrated_protocol_version_opens_restamped() {
    let (_tmp, dir) = data_dir();
    drop(Store::open(&dir).unwrap());
    for older in skywave_proto::MIGRATED_SINCE..skywave_core::PROTOCOL_VERSION {
        let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
        conn.execute("UPDATE protocol_stamp SET version = ?1", [older])
            .unwrap();
        drop(conn);
        drop(Store::open(&dir).unwrap());
        assert_eq!(
            stamp(&dir),
            vec![skywave_core::PROTOCOL_VERSION],
            "from {older}"
        );
    }
}

/// The tool results a v23 replica holds (as its wire snapshot wrote them)
/// read unchanged once v24's step ran: a whole output stays the blob or
/// loss it was.
#[test]
fn v24_reads_the_tool_results_a_v23_replica_holds() {
    let (_tmp, dir) = data_dir();
    drop(Store::open(&dir).unwrap());
    let session = "01920000-0000-7000-8000-000000000001";
    let event = |kind: &str| {
        format!(
            r#"{{"session_id":"{session}","at":"2026-10-29T12:00:00Z","command_id":null,"context":"{session}","kind":{kind}}}"#
        )
    };
    let v23 = [
        event(
            r#"{"type":"tool_result","call":"01920000-0000-7000-8000-000000000007","is_error":false,"status":"exit 0","preview":"running","lines":40,"output":{"blob":{"id":"01920000-0000-7000-8000-000000000005","size":48213,"sha256":"7b24cf3d897fd680e0258c1c7c23db50a5428581ed1785c08de505c381b4c4b5"}}}"#,
        ),
        event(
            r#"{"type":"tool_result","call":null,"is_error":false,"status":null,"preview":"running","lines":40,"output":"lost"}"#,
        ),
        event(
            r#"{"type":"tool_result","call":null,"is_error":true,"status":"exit 1","preview":"no such file","lines":1,"output":null}"#,
        ),
    ];
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    for (seq, json) in (1i64..).zip(&v23) {
        conn.execute(
            "INSERT INTO events (seq, session_id, event) VALUES (?1, ?2, ?3)",
            rusqlite::params![seq, session, json],
        )
        .unwrap();
    }
    conn.execute("UPDATE protocol_stamp SET version = 23", [])
        .unwrap();
    drop(conn);

    let store = Store::open(&dir).unwrap();
    assert_eq!(stamp(&dir), vec![skywave_core::PROTOCOL_VERSION]);
    let id = SessionId(session.parse().unwrap());
    let outputs: Vec<Option<Full>> = store
        .session_events(id)
        .unwrap()
        .into_iter()
        .map(|(_, event)| match event.kind {
            EventKind::ToolResult(ToolResult { output, .. }) => output,
            other => panic!("not a result: {other:?}"),
        })
        .collect();
    assert!(matches!(&outputs[0], Some(Full::Blob(b)) if b.size == 48213));
    assert_eq!(outputs[1..], [Some(Full::Lost), None]);
}

/// The messages a v23 replica holds (its wire snapshot's, without `full`)
/// read as whole messages once v24's step ran.
#[test]
fn v24_reads_the_messages_a_v23_replica_holds() {
    let (_tmp, dir) = data_dir();
    drop(Store::open(&dir).unwrap());
    let session = "01920000-0000-7000-8000-000000000001";
    let event = |kind: &str| {
        format!(
            r#"{{"session_id":"{session}","at":"2026-10-29T12:00:00Z","command_id":null,"context":"{session}","kind":{kind}}}"#
        )
    };
    let v23 = [
        event(r#"{"type":"agent_message","text":"Running cargo test.","parent":null}"#),
        event(
            r#"{"type":"imported","at":null,"item":{"type":"user_message","text":"Where is the event log?"}}"#,
        ),
        event(
            r#"{"type":"imported","at":null,"item":{"type":"agent_message","text":"In crates/skywaved/src/event_log.rs."}}"#,
        ),
    ];
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    for (seq, json) in (1i64..).zip(&v23) {
        conn.execute(
            "INSERT INTO events (seq, session_id, event) VALUES (?1, ?2, ?3)",
            rusqlite::params![seq, session, json],
        )
        .unwrap();
    }
    conn.execute("UPDATE protocol_stamp SET version = 23", [])
        .unwrap();
    drop(conn);

    let store = Store::open(&dir).unwrap();
    assert_eq!(stamp(&dir), vec![skywave_core::PROTOCOL_VERSION]);
    let id = SessionId(session.parse().unwrap());
    let texts: Vec<(String, bool)> =
        store
            .session_events(id)
            .unwrap()
            .into_iter()
            .map(|(_, event)| match event.kind {
                EventKind::AgentMessage { text, full, .. }
                | EventKind::Imported {
                    item:
                        ImportedItem::UserMessage { text, full }
                        | ImportedItem::AgentMessage { text, full },
                    ..
                } => (text, full.is_none()),
                other => panic!("not a message: {other:?}"),
            })
            .collect();
    assert_eq!(
        texts,
        [
            ("Running cargo test.".to_owned(), true),
            ("Where is the event log?".to_owned(), true),
            ("In crates/skywaved/src/event_log.rs.".to_owned(), true),
        ]
    );
}

/// The turn usage a v24 replica holds (its events, `turns` rows and the
/// session's `SessionUsage`, all without the context fields) reads once
/// v25's step ran: no fill, an unknown one in the session's usage.
#[test]
fn v25_reads_the_turn_usage_a_v24_replica_holds() {
    let (_tmp, dir) = data_dir();
    let id = SessionId::new();
    let usage = TurnUsage {
        duration_ms: Some(12_400),
        tokens_in: Some(3_100),
        tokens_out: Some(812),
        cost_usd: Some(0.04),
        model: Some("model-a".into()),
        ..TurnUsage::default()
    };
    let batch = EventBatch {
        from: Seq(1),
        events: vec![
            event(
                id,
                0,
                EventKind::SessionCreated {
                    workdir: "/srv/x".into(),
                    context_name: "start".into(),
                    from: None,
                },
            ),
            event(id, 1, EventKind::TurnUsage(usage.clone())),
        ],
    };
    let mut store = Store::open(&dir).unwrap();
    store.apply_batch(&batch, Utc::now()).unwrap();
    drop(store);
    // What v24 wrote: the same JSON without the fields v25 added.
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    conn.execute_batch(
        "UPDATE events SET event = json_remove(event,
             '$.kind.context_tokens', '$.kind.context_window');
         UPDATE turns SET usage = json_remove(usage, '$.context_tokens', '$.context_window');
         UPDATE sessions SET usage = json_remove(usage,
             '$.context', '$.last_turn.context_tokens', '$.last_turn.context_window',
             '$.total.context_tokens', '$.total.context_window');
         UPDATE protocol_stamp SET version = 24;",
    )
    .unwrap();
    let left: i64 = conn
        .query_row(
            "SELECT count(*) FROM sessions WHERE usage LIKE '%context%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(left, 0, "v24's JSON has no context fields");
    drop(conn);

    let store = Store::open(&dir).unwrap();
    assert_eq!(stamp(&dir), vec![skywave_core::PROTOCOL_VERSION]);
    let session = store.session(id).unwrap().unwrap().usage.unwrap();
    assert_eq!(session.last_turn, usage);
    assert_eq!(session.context, None);
    let turns: Vec<TurnUsage> = store
        .turns()
        .unwrap()
        .into_iter()
        .map(|t| t.usage)
        .collect();
    assert_eq!(turns, std::slice::from_ref(&usage));
    let events: Vec<EventKind> = store
        .session_events(id)
        .unwrap()
        .into_iter()
        .map(|(_, e)| e.kind)
        .collect();
    assert_eq!(events[1], EventKind::TurnUsage(usage));
}

/// A v25 replica's pending approval (its event and the session's pending
/// list) reads as the same plain approval once v26's step ran.
#[test]
fn v26_reads_the_pending_approval_a_v25_replica_holds() {
    let (_tmp, dir) = data_dir();
    let id = SessionId::new();
    let approval = ApprovalId::new();
    let requested = EventKind::ApprovalRequested {
        id: approval,
        call: None,
        tool: "exec".into(),
        summary: "rm -rf target".into(),
        input: None,
        suggestions: Vec::new(),
    };
    let batch = EventBatch {
        from: Seq(1),
        events: vec![
            event(
                id,
                0,
                EventKind::SessionCreated {
                    workdir: "/srv/x".into(),
                    context_name: "start".into(),
                    from: None,
                },
            ),
            event(id, 1, requested.clone()),
        ],
    };
    let mut store = Store::open(&dir).unwrap();
    store.apply_batch(&batch, Utc::now()).unwrap();
    drop(store);
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    conn.execute_batch("UPDATE protocol_stamp SET version = 25;")
        .unwrap();
    drop(conn);

    let store = Store::open(&dir).unwrap();
    assert_eq!(stamp(&dir), vec![skywave_core::PROTOCOL_VERSION]);
    let session = store.session(id).unwrap().unwrap();
    assert_eq!(session.pending_approvals, [approval]);
    let events: Vec<EventKind> = store
        .session_events(id)
        .unwrap()
        .into_iter()
        .map(|(_, e)| e.kind)
        .collect();
    assert_eq!(events[1], requested);
}

/// A v26 replica opens as v27 with no usage limits stored: v26 never
/// asked for them.
#[test]
fn v27_opens_a_v26_replica_without_usage_limits() {
    let (_tmp, dir) = data_dir();
    let id = SessionId::new();
    let created = EventKind::SessionCreated {
        workdir: "/srv/x".into(),
        context_name: "start".into(),
        from: None,
    };
    let batch = EventBatch {
        from: Seq(1),
        events: vec![event(id, 0, created.clone())],
    };
    let mut store = Store::open(&dir).unwrap();
    store.apply_batch(&batch, Utc::now()).unwrap();
    drop(store);
    // What v26 left: no `usage_limits` table, stamped 26.
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    conn.execute_batch(
        "DROP TABLE usage_limits;
         UPDATE protocol_stamp SET version = 26;",
    )
    .unwrap();
    drop(conn);
    set_user_version(&dir, skywave_core::store::SCHEMA_VERSION - 5);

    let mut store = Store::open(&dir).unwrap();
    assert_eq!(stamp(&dir), vec![skywave_core::PROTOCOL_VERSION]);
    assert_eq!(store.usage_limits().unwrap(), None);
    let events: Vec<EventKind> = store
        .session_events(id)
        .unwrap()
        .into_iter()
        .map(|(_, e)| e.kind)
        .collect();
    assert_eq!(events, [created]);
    let read = usage_limits_read();
    store.put_usage_limits(&read).unwrap();
    assert_eq!(store.usage_limits().unwrap(), Some(read));
}

/// A v27 replica opens as v28 with its settings and log as they were and
/// no login expiry: v27 never stored one.
#[test]
fn v28_opens_a_v27_replica_without_a_login_expiry() {
    let (_tmp, dir) = data_dir();
    let id = SessionId::new();
    let created = EventKind::SessionCreated {
        workdir: "/srv/x".into(),
        context_name: "start".into(),
        from: None,
    };
    let batch = EventBatch {
        from: Seq(1),
        events: vec![event(id, 0, created.clone())],
    };
    let mut store = Store::open(&dir).unwrap();
    store.apply_batch(&batch, Utc::now()).unwrap();
    let bypass = ServerFacts {
        bypass_allowed: true,
        ..ServerFacts::default()
    };
    store.set_server_facts(&bypass).unwrap();
    drop(store);
    // What v27 left: stamped 27, no `login_expires` setting.
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    conn.execute("UPDATE protocol_stamp SET version = 27", [])
        .unwrap();
    drop(conn);

    let store = Store::open(&dir).unwrap();
    assert_eq!(stamp(&dir), vec![skywave_core::PROTOCOL_VERSION]);
    assert_eq!(store.settings().unwrap().server, bypass);
    let events: Vec<EventKind> = store
        .session_events(id)
        .unwrap()
        .into_iter()
        .map(|(_, e)| e.kind)
        .collect();
    assert_eq!(events, [created]);
}

/// A v28 replica opens as v29 with its settings and log as they were, and
/// reads the agent's login as offered to no one and not logged out: v28
/// never stored either. The next `hello` says both.
#[test]
fn v29_opens_a_v28_replica_without_the_agent_login_flags() {
    let (_tmp, dir) = data_dir();
    let id = SessionId::new();
    let created = EventKind::SessionCreated {
        workdir: "/srv/x".into(),
        context_name: "start".into(),
        from: None,
    };
    let batch = EventBatch {
        from: Seq(1),
        events: vec![event(id, 0, created.clone())],
    };
    let mut store = Store::open(&dir).unwrap();
    store.apply_batch(&batch, Utc::now()).unwrap();
    let facts = ServerFacts {
        bypass_allowed: true,
        login_expires: Some(at(9)),
        ..ServerFacts::default()
    };
    store.set_server_facts(&facts).unwrap();
    drop(store);
    // What v28 left: stamped 28, neither agent login flag set.
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    conn.execute("UPDATE protocol_stamp SET version = 28", [])
        .unwrap();
    drop(conn);

    let store = Store::open(&dir).unwrap();
    assert_eq!(stamp(&dir), vec![skywave_core::PROTOCOL_VERSION]);
    assert_eq!(store.settings().unwrap().server, facts);
    let events: Vec<EventKind> = store
        .session_events(id)
        .unwrap()
        .into_iter()
        .map(|(_, e)| e.kind)
        .collect();
    assert_eq!(events, [created]);
    let logged_out = ServerFacts {
        agent_logged_out: true,
        ..facts
    };
    store.set_server_facts(&logged_out).unwrap();
    assert_eq!(store.settings().unwrap().server, logged_out);
}

/// A v29 replica opens as v30 with its user messages as they were: v30's
/// `attachments` field is absent from v29 JSON and reads as none.
#[test]
fn v30_opens_a_v29_replica_with_its_user_messages() {
    let (_tmp, dir) = data_dir();
    let id = SessionId::new();
    let created = EventKind::SessionCreated {
        workdir: "/srv/x".into(),
        context_name: "start".into(),
        from: None,
    };
    let message = EventKind::UserMessage {
        device_id: DeviceId::new(),
        text: "look".into(),
        attachments: Vec::new(),
    };
    let batch = EventBatch {
        from: Seq(1),
        events: vec![event(id, 0, created.clone()), event(id, 1, message.clone())],
    };
    let mut store = Store::open(&dir).unwrap();
    store.apply_batch(&batch, Utc::now()).unwrap();
    drop(store);
    // What v29 left: stamped 29.
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    conn.execute("UPDATE protocol_stamp SET version = 29", [])
        .unwrap();
    drop(conn);

    let store = Store::open(&dir).unwrap();
    assert_eq!(stamp(&dir), vec![skywave_core::PROTOCOL_VERSION]);
    let events: Vec<EventKind> = store
        .session_events(id)
        .unwrap()
        .into_iter()
        .map(|(_, e)| e.kind)
        .collect();
    assert_eq!(events, [created, message]);
}

fn usage_limits_read() -> UsageLimitsResult {
    UsageLimitsResult::Read {
        at: Utc.with_ymd_and_hms(2026, 11, 8, 12, 0, 0).unwrap(),
        limits: vec![UsageLimit {
            window: LimitWindow::Session,
            scope: None,
            percent: Some(75),
            severity: Some(LimitSeverity::Warning),
            resets_at: None,
        }],
    }
}

/// The latest answer replaces the one before, reads back after a reopen
/// and in the snapshot, and goes with the replica.
#[test]
fn the_usage_limits_answer_is_kept_until_the_replica_is_reset() {
    let (_tmp, dir) = data_dir();
    let mut store = Store::open(&dir).unwrap();
    assert_eq!(store.load().unwrap().usage_limits, None);
    store
        .put_usage_limits(&UsageLimitsResult::Unavailable)
        .unwrap();
    store.put_usage_limits(&usage_limits_read()).unwrap();
    drop(store);
    let mut store = Store::open(&dir).unwrap();
    assert_eq!(
        store.load().unwrap().usage_limits,
        Some(usage_limits_read())
    );
    store.reset_for_server().unwrap();
    assert_eq!(store.usage_limits().unwrap(), None);
}

#[test]
fn the_replica_is_stamped_with_its_protocol_version_and_refused_under_one_it_cannot_migrate() {
    let (_tmp, dir) = data_dir();
    drop(Store::open(&dir).unwrap());
    assert_eq!(stamp(&dir), vec![skywave_core::PROTOCOL_VERSION]);
    drop(Store::open(&dir).unwrap());
    assert_eq!(stamp(&dir), vec![skywave_core::PROTOCOL_VERSION], "kept");

    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    for other in [
        skywave_proto::MIGRATED_SINCE - 1,
        skywave_core::PROTOCOL_VERSION + 1,
    ] {
        conn.execute("UPDATE protocol_stamp SET version = ?1", [other])
            .unwrap();
        let err = Store::open(&dir).err().unwrap();
        assert!(
            matches!(err, StoreError::ProtocolMismatch { found, current }
                if found == other && current == skywave_core::PROTOCOL_VERSION),
            "{err}"
        );
        assert!(err.unreadable());
        assert_eq!(stamp(&dir), vec![other], "refused untouched");
    }

    // A replica from before the stamp takes this build's.
    conn.execute_batch("DROP TABLE protocol_stamp").unwrap();
    drop(conn);
    set_user_version(&dir, skywave_core::store::SCHEMA_VERSION - 10);
    drop(Store::open(&dir).unwrap());
    assert_eq!(stamp(&dir), vec![skywave_core::PROTOCOL_VERSION]);
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
                context_name: "start".into(),
                from: None,
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
                attachments: Vec::new(),
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
    drop_shell(&dir);
    drop_outbox_delivery_columns(&dir);
    drop_alerted(&dir);
    drop_deleted(&dir);
    drop_usage(&dir);
    drop_archived(&dir);
    drop_titles(&dir);
    drop_unread(&dir);
    drop_drafts(&dir);
    drop_devices(&dir);
    drop_snippets(&dir);
    drop_work(&dir);
    drop_contexts(&dir);
    drop_agent(&dir);
    drop_turns(&dir);
    drop_workdir(&dir);
    drop_import_columns(&dir);
    set_user_version(&dir, 1);
    let store = Store::open(&dir).unwrap();
    assert!(stopped(&store, a));
    assert!(!stopped(&store, b));
}

#[test]
fn sessions_are_ordered_by_their_newest_message_and_backfilled_by_the_migration() {
    let (_tmp, dir) = data_dir();
    let (a, b, c) = (SessionId::new(), SessionId::new(), SessionId::new());
    let created = |id, minute| {
        event(
            id,
            minute,
            EventKind::SessionCreated {
                workdir: "/srv/x".into(),
                context_name: "start".into(),
                from: None,
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
                attachments: Vec::new(),
            },
        )
    };
    let reply = |id, minute| {
        event(
            id,
            minute,
            EventKind::AgentMessage {
                parent: None,
                text: "hello".into(),
                full: None,
            },
        )
    };
    // `b` was written to last; `a` and `c` were active since, but only by
    // the agent; `c` was never written to and sits by its creation.
    let batch = EventBatch {
        from: Seq(1),
        events: vec![
            created(a, 0),
            message(a, 1),
            created(b, 2),
            message(b, 3),
            created(c, 4),
            reply(a, 5),
            reply(c, 6),
        ],
    };
    let mut store = Store::open(&dir).unwrap();
    store.apply_batch(&batch, Utc::now()).unwrap();
    let order = |store: &Store| -> Vec<SessionId> {
        store.sessions().unwrap().iter().map(|s| s.id).collect()
    };
    assert_eq!(order(&store), vec![c, b, a]);
    let summary = store.session(a).unwrap().unwrap();
    assert_eq!(
        (summary.last_seq, summary.last_message_seq),
        (Seq(6), Seq(2))
    );
    drop(store);

    // A replica from before the column: the migration derives it from the log.
    set_user_version(&dir, skywave_core::store::SCHEMA_VERSION - 4);
    let store = Store::open(&dir).unwrap();
    assert_eq!(order(&store), vec![c, b, a]);
    assert_eq!(store.session(c).unwrap().unwrap().last_message_seq, Seq(5));
}

const AGENT: ComposerMode = ComposerMode::Agent;
const SHELL: ComposerMode = ComposerMode::Shell;

/// Rewinds the schema to `version`. A rewind past the list-order column's
/// migration reruns it, so this undoes it here; the last three, moving the
/// server facts, canonicalizing the server addresses and creating the
/// attachment tables, rerun over a current schema as is.
fn set_user_version(dir: &Path, version: i64) {
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    if version < skywave_core::store::SCHEMA_VERSION - 3 {
        conn.execute_batch("ALTER TABLE sessions DROP COLUMN last_message_seq;")
            .unwrap();
    }
    conn.pragma_update(None, "user_version", version).unwrap();
}

/// The schema before `server_facts` kept each fact of the server's `hello`
/// as a `settings` row of its own; the migration moves them into the one
/// row, and a replica that never took a `hello` gets none.
#[test]
fn the_migration_moves_the_hello_settings_rows_into_the_server_facts() {
    let previous_schema = |dir: &Path, rows: &str| {
        drop(Store::open(dir).unwrap());
        let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
        conn.execute_batch(&format!(
            "DROP TABLE server_facts;
             INSERT OR REPLACE INTO settings (key, value) VALUES ('blob_policy', 'eager');
             {rows}"
        ))
        .unwrap();
        drop(conn);
        set_user_version(dir, skywave_core::store::SCHEMA_VERSION - 3);
    };
    let settings_keys = |dir: &Path| -> Vec<String> {
        let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
        let mut stmt = conn
            .prepare("SELECT key FROM settings ORDER BY key")
            .unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };

    let (_tmp, dir) = data_dir();
    // As the previous schema's setters wrote them.
    previous_schema(
        &dir,
        &format!(
            "INSERT INTO settings (key, value) VALUES
                ('shell_enabled', 'false'),
                ('bypass_allowed', 'true'),
                ('login_expires', '{}'),
                ('agent_login_offered', 'true'),
                ('agent_logged_out', 'true');",
            at(7).to_rfc3339()
        ),
    );
    let settings = Store::open(&dir).unwrap().settings().unwrap();
    assert_eq!(
        settings.server,
        ServerFacts {
            shell_enabled: false,
            ..every_fact()
        }
    );
    assert_eq!(settings.blob_policy, BlobPolicy::Eager);
    assert_eq!(settings_keys(&dir), ["blob_policy"], "the rows moved");

    let (_tmp, dir) = data_dir();
    previous_schema(&dir, "");
    assert_eq!(
        Store::open(&dir).unwrap().settings().unwrap().server,
        ServerFacts::default()
    );
    let rows: i64 = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE))
        .unwrap()
        .query_row("SELECT COUNT(*) FROM server_facts", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 0, "no hello, no row");
}

/// The schema before the settings parsed the server address kept it as
/// written: `SetServerAddress` saved it as typed. The migration rewrites
/// every address to its canonical form, drops the ones that do not parse
/// and the others that repeat one, so each address is stored once.
#[test]
fn the_migration_canonicalizes_the_stored_server_addresses() {
    let previous_schema = |dir: &Path, rows: &str| {
        drop(Store::open(dir).unwrap());
        let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
        conn.execute_batch(rows).unwrap();
        drop(conn);
        set_user_version(dir, skywave_core::store::SCHEMA_VERSION - 2);
    };
    let stored = |dir: &Path| -> Vec<(String, String)> {
        let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT key, value FROM settings
                 WHERE key IN ('server_address', 'other_server_addresses') ORDER BY key",
            )
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };

    let (_tmp, dir) = data_dir();
    previous_schema(
        &dir,
        r#"INSERT INTO settings (key, value) VALUES
            ('server_address', ' MyHost '),
            ('other_server_addresses',
             '["myhost:7443", "VPN.example", "h:p", "vpn.example:7443", "[fd00::7]"]');"#,
    );
    drop(Store::open(&dir).unwrap());
    assert_eq!(
        stored(&dir),
        [
            (
                "other_server_addresses".to_owned(),
                r#"["vpn.example:7443","[fd00::7]:7443"]"#.to_owned()
            ),
            ("server_address".to_owned(), "myhost:7443".to_owned()),
        ]
    );

    // Nothing usable: both rows go, and the device has no address to try.
    let (_tmp, dir) = data_dir();
    previous_schema(
        &dir,
        r#"INSERT INTO settings (key, value) VALUES
            ('server_address', 'host:'),
            ('other_server_addresses', '["h:p"]');"#,
    );
    drop(Store::open(&dir).unwrap());
    assert!(stored(&dir).is_empty());

    let (_tmp, dir) = data_dir();
    previous_schema(
        &dir,
        "INSERT INTO settings (key, value) VALUES ('other_server_addresses', 'not json');",
    );
    assert_eq!(
        Store::open(&dir).unwrap().settings().unwrap(),
        Settings::default()
    );
    assert!(stored(&dir).is_empty());
}

#[test]
fn drafts_round_trip_and_blank_deletes() {
    let (_tmp, dir) = data_dir();
    let (a, b) = (SessionId::new(), SessionId::new());
    let draft = |text: &str, cursor| Draft {
        text: text.into(),
        cursor,
        when: RunWhen::Always,
    };
    {
        let store = Store::open(&dir).unwrap();
        store
            .set_draft(a, AGENT, Some(&draft("héllo\nwörld", 7)))
            .unwrap();
        store.set_draft(b, AGENT, Some(&draft("first", 0))).unwrap();
        store
            .set_draft(b, AGENT, Some(&draft("second", 6)))
            .unwrap();
    }
    let store = Store::open(&dir).unwrap();
    let drafts = store.load().unwrap().drafts;
    assert_eq!(drafts.len(), 2);
    assert_eq!(drafts[&(a, AGENT)], draft("héllo\nwörld", 7));
    assert_eq!(
        drafts[&(b, AGENT)],
        draft("second", 6),
        "a save replaces the draft"
    );

    store.set_draft(a, AGENT, None).unwrap();
    store.set_draft(SessionId::new(), AGENT, None).unwrap();
    assert_eq!(
        store.drafts().unwrap().keys().collect::<Vec<_>>(),
        [&(b, AGENT)]
    );
}

#[test]
fn enqueue_message_clears_its_draft_atomically() {
    let (_tmp, dir) = data_dir();
    let (a, b) = (SessionId::new(), SessionId::new());
    let draft = |text: &str| Draft {
        text: text.into(),
        cursor: 0,
        when: RunWhen::Always,
    };
    let store = Store::open(&dir).unwrap();
    store.set_draft(a, AGENT, Some(&draft("to a"))).unwrap();
    store.set_draft(b, AGENT, Some(&draft("to b"))).unwrap();

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
    assert_eq!(
        store.drafts().unwrap().keys().collect::<Vec<_>>(),
        [&(b, AGENT)]
    );
    assert_eq!(store.pending_outbox().unwrap().len(), 2);
}

/// An image for a draft; each `seed` is an image of its own.
fn image(seed: &str) -> AttachedImage {
    AttachedImage {
        sha256: ContentHash::of(seed.as_bytes()),
        media_type: ImageType::Png,
        size: seed.len() as u64,
        width: 4,
        height: 3,
    }
}

#[test]
fn draft_images_keep_their_order_over_a_reopen() {
    let (_tmp, dir) = data_dir();
    let session = SessionId::new();
    let (a, b) = (image("a"), image("b"));
    {
        let store = Store::open(&dir).unwrap();
        store.attach_to_draft(session, &a, at(0)).unwrap();
        store.attach_to_draft(session, &b, at(1)).unwrap();
        // Already held: a no-op.
        store.attach_to_draft(session, &a, at(2)).unwrap();
    }
    let store = Store::open(&dir).unwrap();
    assert_eq!(
        store.load().unwrap().draft_images[&session],
        [a.clone(), b.clone()]
    );

    store.detach_from_draft(session, a.sha256).unwrap();
    assert_eq!(
        store.draft_images().unwrap()[&session],
        std::slice::from_ref(&b)
    );

    // Blank text no longer hides the images: the draft row goes, they stay.
    let draft = Draft {
        text: "half typed".into(),
        cursor: 0,
        when: RunWhen::Always,
    };
    store.set_draft(session, AGENT, Some(&draft)).unwrap();
    store.set_draft(session, AGENT, None).unwrap();
    assert!(store.drafts().unwrap().is_empty());
    assert_eq!(
        store.draft_images().unwrap()[&session],
        std::slice::from_ref(&b)
    );

    // A detached image comes back after the others, not in its old place.
    store.attach_to_draft(session, &a, at(3)).unwrap();
    assert_eq!(store.draft_images().unwrap()[&session], [b, a]);
}

#[test]
fn sending_a_message_drops_its_draft_images() {
    let (_tmp, dir) = data_dir();
    let (a, b) = (SessionId::new(), SessionId::new());
    let store = Store::open(&dir).unwrap();
    store.attach_to_draft(a, &image("to a"), at(0)).unwrap();
    store.attach_to_draft(b, &image("to b"), at(1)).unwrap();
    assert_eq!(store.draft_images().unwrap()[&a], [image("to a")]);

    store.enqueue(&send(a, "with a picture")).unwrap();
    let images = store.draft_images().unwrap();
    assert!(!images.contains_key(&a));
    assert_eq!(images[&b], [image("to b")]);
}

#[test]
fn deleting_a_session_drops_its_draft_images() {
    let (_tmp, dir) = data_dir();
    let (a, b) = (SessionId::new(), SessionId::new());
    let mut store = Store::open(&dir).unwrap();
    store.attach_to_draft(a, &image("to a"), at(0)).unwrap();
    store.attach_to_draft(b, &image("to b"), at(1)).unwrap();
    assert!(store.draft_images().unwrap().contains_key(&a));

    store.delete_session_locally(a, at(2)).unwrap();
    let images = store.draft_images().unwrap();
    assert!(!images.contains_key(&a));
    assert_eq!(images[&b], [image("to b")]);
}

#[test]
fn resetting_for_another_server_drops_every_draft_image() {
    let (_tmp, dir) = data_dir();
    let mut store = Store::open(&dir).unwrap();
    store
        .attach_to_draft(SessionId::new(), &image("a"), at(0))
        .unwrap();
    store
        .attach_to_draft(SessionId::new(), &image("b"), at(1))
        .unwrap();
    assert_eq!(store.draft_images().unwrap().len(), 2);

    store.reset_for_server().unwrap();
    assert!(store.draft_images().unwrap().is_empty());
}

#[test]
fn the_attachment_tables_come_with_the_migration() {
    let (_tmp, dir) = data_dir();
    drop(Store::open(&dir).unwrap());
    drop_attachments(&dir);
    set_user_version(&dir, skywave_core::store::SCHEMA_VERSION - 1);

    let store = Store::open(&dir).unwrap();
    store
        .attach_to_draft(SessionId::new(), &image("a"), at(0))
        .unwrap();
    assert_eq!(store.draft_images().unwrap().len(), 1);
}

#[test]
fn withdraw_anything_not_accepted() {
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
            ("claimed", true),
            ("requeued", true),
            ("refused", true),
            ("undeliverable", true),
            ("accepted", false),
        ]
        .map(|(text, discardable)| (text.to_owned(), discardable))
    );

    // Only what was accepted stays.
    assert!(matches!(
        store.withdraw(accepted.id, None),
        Err(StoreError::NotWithdrawable(id)) if id == accepted.id
    ));
    assert!(matches!(
        store.withdraw(CommandId::new(), None),
        Err(StoreError::NotWithdrawable(_))
    ));
    for command in [&fresh, &claimed, &requeued, &refused, &undeliverable] {
        store.withdraw(command.id, None).unwrap();
    }
    let left: Vec<CommandId> = store
        .outbox()
        .unwrap()
        .iter()
        .map(|e| e.command.id)
        .collect();
    assert_eq!(left, [accepted.id]);

    // An answer to a request still out when it was taken back settles
    // nothing.
    store
        .record_result(claimed.id, &CommandResult::Accepted { seq: Seq(2) }, at(4))
        .unwrap();
    store.record_undeliverable(requeued.id, at(4)).unwrap();
    assert_eq!(store.outbox().unwrap().len(), 1);

    // A session creation is withdrawn the same way; other kinds are not
    // discardable even if never sent.
    let create = Command {
        id: CommandId::new(),
        kind: CommandKind::CreateSession {
            workdir: "/etc".into(),
            conversation: None,
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
    assert!(outbox[1].discardable());
    assert!(!outbox[2].discardable());
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
        when: RunWhen::Always,
    };
    let draft = Draft {
        text: "edit me\n\nolder".into(),
        cursor: 7,
        when: RunWhen::Always,
    };
    let store = Store::open(&dir).unwrap();
    store.enqueue(&command).unwrap();
    store.set_draft(session, AGENT, Some(&older)).unwrap();

    // A draft write that fails after the delete keeps the message queued:
    // the text is never lost.
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER full BEFORE UPDATE ON drafts BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
    )
    .unwrap();
    assert!(
        store
            .withdraw(command.id, Some(((session, AGENT), &draft)))
            .is_err()
    );
    assert_eq!(store.pending_outbox().unwrap().len(), 1);
    assert_eq!(store.drafts().unwrap()[&(session, AGENT)], older);

    // Once it succeeds the text is in the draft only, never in both.
    conn.execute_batch("DROP TRIGGER full").unwrap();
    store
        .withdraw(command.id, Some(((session, AGENT), &draft)))
        .unwrap();
    drop(store);
    let store = Store::open(&dir).unwrap();
    assert!(store.outbox().unwrap().is_empty());
    assert_eq!(store.drafts().unwrap()[&(session, AGENT)], draft);

    // A refused withdraw writes no draft either.
    let other = SessionId::new();
    assert!(
        store
            .withdraw(command.id, Some(((other, AGENT), &draft)))
            .is_err()
    );
    assert!(!store.drafts().unwrap().contains_key(&(other, AGENT)));
}

// ---- local delete ---------------------------------------------------------------

/// The whole log applied, plus what this device keeps for both sessions:
/// drafts of both modes, shell composers, read marks, an answered and an
/// unanswered message each, and session A's blob downloaded.
fn replica_with_both_sessions(dir: &Path, log: &Log) -> (Store, [Command; 4]) {
    let mut store = Store::open(dir).unwrap();
    store.apply_batch(&log.batch(1, 7), Utc::now()).unwrap();
    let draft = Draft {
        text: "later".into(),
        cursor: 0,
        when: RunWhen::Always,
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
        store.set_draft(id, AGENT, Some(&draft)).unwrap();
        store.set_draft(id, SHELL, Some(&draft)).unwrap();
        store.set_composer_mode(id, SHELL).unwrap();
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
    let mut drafts: Vec<_> = after.drafts.keys().copied().collect();
    drafts.sort_by_key(|(_, mode)| *mode == SHELL);
    assert_eq!(drafts, [(log.b, AGENT), (log.b, SHELL)]);
    assert_eq!(after.composer_modes.keys().collect::<Vec<_>>(), [&log.b]);
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
        "composer_modes",
        "read_marks",
        "deleted_sessions",
        "server_facts",
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
    seam.pairing_succeeded(&addr("old.example:7443"), paired_with(&ca))
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
        when: RunWhen::Always,
    };
    store.set_draft(log.a, AGENT, Some(&draft)).unwrap();
    store.set_composer_mode(log.a, ComposerMode::Shell).unwrap();
    seam.hello_received(&hello_saying(every_fact())).unwrap();
    for address in ["vpn.example:7443", "old.example:7443"] {
        core.dispatch(Action::SetServerAddress {
            address: address.into(),
        })
        .unwrap();
    }
    let others = |seam: &skywave_core::Seam| seam.pairing().unwrap().addresses.others().to_vec();
    assert_eq!(others(&seam), [addr("vpn.example:7443")]);
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
    assert_eq!(before.settings.server, every_fact());
    core.dispatch(Action::OpenSession(log.a)).unwrap();
    let shown = |core: &Core| match core.view().screen {
        Screen::Session(session) => (session.agent.bypass_allowed, session.shell.enabled),
        other => panic!("expected the session screen, got {other:?}"),
    };
    assert_eq!(shown(&core), (true, true), "shown before the switch");
    let rows = server_rows(&dir);
    assert!(rows.iter().all(|(_, n)| *n > 0), "{rows:?}");

    // Again with the same server, e.g. after a revocation: all kept.
    seam.pairing_succeeded(&addr("old.example:7443"), paired_with(&ca))
        .unwrap();
    assert_eq!(store.load().unwrap(), before);
    assert_eq!(others(&seam), [addr("vpn.example:7443")]);
    assert_eq!(server_rows(&dir), rows);
    assert!(blob_file.exists());

    // Another server: nothing of the old one is left.
    seam.pairing_succeeded(&addr("new.example:7443"), paired_with(&ca_pem()))
        .unwrap();
    let rows = server_rows(&dir);
    assert!(rows.iter().all(|(_, n)| *n == 0), "{rows:?}");
    let after = store.load().unwrap();
    assert_eq!(after.cursor, Seq::ZERO);
    assert_eq!(
        after.settings,
        Settings {
            blob_policy: BlobPolicy::Eager,
            server: ServerFacts::default(),
        },
        "the blob policy is this device's; everything the old server's hello \
         said, bypass included, was the old server's"
    );
    assert_eq!(
        *seam.pairing().unwrap().addresses.current(),
        addr("new.example:7443")
    );
    assert!(
        others(&seam).is_empty(),
        "the other addresses were the old server's"
    );
    assert!(!blob_file.exists(), "blob files go with their rows");

    // The new server's log starts from the beginning.
    let applied = seam.apply_events(&log.batch(1, 2)).unwrap();
    assert_eq!((applied.cursor, applied.new_events), (Seq(2), 2));

    // The model holds what the store does: the new server said nothing yet.
    let view = core.view();
    assert_eq!(
        (
            view.login_expires,
            view.agent_login_offered,
            view.agent_logged_out
        ),
        (None, false, false)
    );
    core.dispatch(Action::OpenSession(log.a)).unwrap();
    assert_eq!(shown(&core), (false, false), "no bypass, no shell");
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
            context_name: "start".into(),
            from: None,
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
                    event(
                        a,
                        6,
                        EventKind::AgentMessage {
                            parent: None,
                            text: "ok".into(),
                            full: None,
                        },
                    ),
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
    store.reset_for_server().unwrap();
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
    drop_shell(&dir);
    drop_alerted(&dir);
    drop_devices(&dir);
    drop_snippets(&dir);
    drop_work(&dir);
    drop_contexts(&dir);
    drop_agent(&dir);
    drop_turns(&dir);
    drop_workdir(&dir);
    drop_import_columns(&dir);
    set_user_version(&dir, 9);
    let store = Store::open(&dir).unwrap();
    assert_eq!(sorted(store.load().unwrap().alerted), sorted([x, y]));
}

fn drop_devices(dir: &Path) {
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    conn.execute_batch("DROP TABLE devices;").unwrap();
}

/// Undoes the snippets migration.
fn drop_snippets(dir: &Path) {
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    conn.execute_batch("DROP TABLE snippets;").unwrap();
}

/// A replica from protocol v8 holds events without a context, a first
/// session event without its context's name, and messages queued without
/// the context they were written in. The migration rewrites each to the v9
/// shape: the session's first context, named "start".
#[test]
fn a_v8_replica_moves_into_first_contexts() {
    let (_tmp, dir) = data_dir();
    let session = SessionId::new();
    let other = SessionId::new();
    let (queued, finished) = (send(session, "one"), send(other, "two"));
    let rename = Command {
        id: CommandId::new(),
        kind: CommandKind::RenameSession {
            session_id: session,
            name: Some("x".into()),
        },
    };
    {
        let mut store = Store::open(&dir).unwrap();
        let created = |id| {
            event(
                id,
                0,
                EventKind::SessionCreated {
                    workdir: "/srv".into(),
                    context_name: "start".into(),
                    from: None,
                },
            )
        };
        let batch = EventBatch {
            from: Seq(1),
            events: vec![
                created(session),
                created(other),
                event(
                    session,
                    1,
                    EventKind::AgentMessage {
                        parent: None,
                        text: "hi".into(),
                        full: None,
                    },
                ),
            ],
        };
        store.apply_batch(&batch, Utc::now()).unwrap();
        for command in [&queued, &finished, &rename] {
            store.enqueue(command).unwrap();
        }
        store
            .record_result(finished.id, &CommandResult::Accepted { seq: Seq(4) }, at(1))
            .unwrap();
    }
    // Back to the v8 shapes and the schema before the column.
    {
        let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
        conn.execute_batch(
            "UPDATE events SET event = json_remove(event, '$.context', '$.kind.context_name');
             UPDATE outbox SET command = json_remove(command, '$.kind.context');",
        )
        .unwrap();
        let v8: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE json_extract(event, '$.context') IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v8, 3);
    }
    drop_shell(&dir);
    drop_work(&dir);
    drop_contexts(&dir);
    drop_turns(&dir);
    drop_workdir(&dir);
    drop_import_columns(&dir);
    set_user_version(&dir, skywave_core::store::SCHEMA_VERSION - 17);

    let store = Store::open(&dir).unwrap();
    let events = store.session_events(session).unwrap();
    assert_eq!(events.len(), 2);
    for (_, event) in &events {
        assert_eq!(event.context, ContextId::first_of(session));
    }
    assert!(matches!(
        &events[0].1.kind,
        EventKind::SessionCreated { context_name, .. } if context_name == "start"
    ));
    assert_eq!(
        store.session_events(other).unwrap()[0].1.context,
        ContextId::first_of(other)
    );
    // Both messages decode, queued and finished, in their sessions' first
    // context; other commands are left as they were.
    let outbox = store.outbox().unwrap();
    let commands: Vec<&Command> = outbox.iter().map(|e| &e.command).collect();
    assert_eq!(commands, [&queued, &finished, &rename]);
    assert_eq!(outbox[0].status, OutboxStatus::Queued);
    assert_ne!(outbox[1].status, OutboxStatus::Queued);
    for id in [session, other] {
        assert_eq!(
            store.session(id).unwrap().unwrap().context,
            ContextId::first_of(id)
        );
    }
}

/// Undoes the session contexts migration's schema change; the events it
/// rewrote keep their v9 shape, which a rerun leaves as it is.
fn drop_contexts(dir: &Path) {
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    conn.execute_batch("ALTER TABLE sessions DROP COLUMN context;")
        .unwrap();
}

/// Undoes the shell commands migration: no composer modes, one agent
/// draft per session, snippets without a kind.
fn drop_turns(dir: &Path) {
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    conn.execute_batch("DROP TABLE turns;").unwrap();
}

/// Undoes the workdir snapshots migration.
fn drop_workdir(dir: &Path) {
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    conn.execute_batch(
        "DROP TABLE workdir_snapshots;
         DROP TABLE workdir_blob_refs;",
    )
    .unwrap();
}

/// Undoes the importing-flag and adopted-conversation migrations (the last
/// two).
fn drop_import_columns(dir: &Path) {
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    conn.execute_batch(
        "ALTER TABLE sessions DROP COLUMN adopted;
         ALTER TABLE sessions DROP COLUMN importing;",
    )
    .unwrap();
}

/// Undoes the adopted-conversation migration.
fn drop_adopted(dir: &Path) {
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    conn.execute_batch("ALTER TABLE sessions DROP COLUMN adopted;")
        .unwrap();
}

fn drop_shell(dir: &Path) {
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    conn.execute_batch(
        "DROP TABLE composer_modes;
         CREATE TABLE old_drafts (
             session_id TEXT PRIMARY KEY,
             text TEXT NOT NULL,
             cursor INTEGER NOT NULL
         );
         INSERT INTO old_drafts SELECT session_id, text, cursor FROM drafts WHERE mode = 'agent';
         DROP TABLE drafts;
         ALTER TABLE old_drafts RENAME TO drafts;
         ALTER TABLE snippets DROP COLUMN kind;",
    )
    .unwrap();
}

/// Undoes the background work migration.
fn drop_work(dir: &Path) {
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    conn.execute_batch("ALTER TABLE sessions DROP COLUMN work;")
        .unwrap();
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

    store.reset_for_server().unwrap();
    assert!(store.devices().unwrap().is_empty());
}

#[test]
fn snippets_survive_a_reopen_and_a_new_server() {
    let (_tmp, dir) = data_dir();
    let named = Snippet {
        id: SnippetId::new(),
        name: Some("tests".into()),
        text: "run the tests".into(),
        kind: AGENT,
        used: 1,
    };
    let plain = Snippet {
        id: SnippetId::new(),
        name: None,
        text: "commit this".into(),
        kind: AGENT,
        used: 2,
    };
    let store = Store::open(&dir).unwrap();
    store.set_snippet(&named).unwrap();
    store.set_snippet(&plain).unwrap();
    let renamed = Snippet {
        name: Some("check".into()),
        used: 3,
        ..named.clone()
    };
    store.set_snippet(&renamed).unwrap();
    drop(store);

    let mut store = Store::open(&dir).unwrap();
    let mut snippets = store.load().unwrap().snippets;
    snippets.sort_by_key(|s| s.used);
    assert_eq!(snippets, [plain.clone(), renamed]);

    store.reset_for_server().unwrap();
    store.delete_snippet(named.id).unwrap();
    assert_eq!(store.snippets().unwrap(), [plain], "a device preference");
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
        efforts: Vec::new(),
    }];
    let mut store = Store::open(&dir).unwrap();
    let event = |session_id, kind| Event {
        session_id,
        at: chrono::Utc::now(),
        command_id: None,
        context: ContextId::first_of(session_id),
        kind,
    };
    let batch = EventBatch {
        from: Seq(1),
        events: vec![
            event(
                id,
                EventKind::SessionCreated {
                    workdir: "/srv/a".into(),
                    context_name: "start".into(),
                    from: None,
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
                    context_name: "start".into(),
                    from: None,
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

#[test]
fn an_agents_effort_is_kept() {
    use skywave_proto::Effort;
    let (_tmp, dir) = data_dir();
    let id = SessionId::new();
    let mut store = Store::open(&dir).unwrap();
    let event = |kind| Event {
        session_id: id,
        at: chrono::Utc::now(),
        command_id: None,
        context: ContextId::first_of(id),
        kind,
    };
    let batch = EventBatch {
        from: Seq(1),
        events: vec![
            event(EventKind::SessionCreated {
                workdir: "/srv/a".into(),
                context_name: "start".into(),
                from: None,
            }),
            event(EventKind::EffortChanged {
                effort: Some(Effort::XHigh),
                answers: None,
            }),
        ],
    };
    store.apply_batch(&batch, chrono::Utc::now()).unwrap();
    drop(store);
    let store = Store::open(&dir).unwrap();
    let summary = store.session(id).unwrap().unwrap();
    assert_eq!(summary.agent.effort.current, Some(Some(Effort::XHigh)));
}

// ---- shell commands -----------------------------------------------------------

fn run_shell(session_id: SessionId, command: &str, when: RunWhen) -> Command {
    Command {
        id: CommandId::new(),
        kind: CommandKind::RunShell {
            session_id,
            command: command.into(),
            when,
        },
    }
}

#[test]
fn drafts_are_kept_per_mode_with_the_shell_condition() {
    let (_tmp, dir) = data_dir();
    let session = SessionId::new();
    let agent = Draft {
        text: "fix the tests".into(),
        cursor: 3,
        when: RunWhen::Always,
    };
    let shell = Draft {
        text: "make test".into(),
        cursor: 9,
        when: RunWhen::OnFailure,
    };
    {
        let store = Store::open(&dir).unwrap();
        store.set_draft(session, AGENT, Some(&agent)).unwrap();
        store.set_draft(session, SHELL, Some(&shell)).unwrap();
    }
    let store = Store::open(&dir).unwrap();
    let drafts = store.load().unwrap().drafts;
    assert_eq!(drafts.len(), 2, "one per mode");
    assert_eq!(drafts[&(session, AGENT)], agent);
    assert_eq!(drafts[&(session, SHELL)], shell);

    // Deleting one mode's draft leaves the other.
    store.set_draft(session, SHELL, None).unwrap();
    assert_eq!(
        store.drafts().unwrap().keys().collect::<Vec<_>>(),
        [&(session, AGENT)]
    );
}

#[test]
fn a_queued_run_drops_the_shell_draft_and_a_message_the_agent_draft() {
    let (_tmp, dir) = data_dir();
    let session = SessionId::new();
    let draft = |text: &str| Draft {
        text: text.into(),
        cursor: 0,
        when: RunWhen::Always,
    };
    let store = Store::open(&dir).unwrap();
    store
        .set_draft(session, AGENT, Some(&draft("a message")))
        .unwrap();
    store.set_draft(session, SHELL, Some(&draft("ls"))).unwrap();

    store
        .enqueue(&run_shell(session, "ls", RunWhen::OnSuccess))
        .unwrap();
    assert_eq!(
        store.drafts().unwrap().keys().collect::<Vec<_>>(),
        [&(session, AGENT)]
    );

    store
        .set_draft(session, SHELL, Some(&draft("pwd")))
        .unwrap();
    store.enqueue(&send(session, "a message")).unwrap();
    assert_eq!(
        store.drafts().unwrap().keys().collect::<Vec<_>>(),
        [&(session, SHELL)]
    );

    // The run is queued as the head wrote it, and may be taken back.
    let outbox = store.pending_outbox().unwrap();
    assert!(matches!(
        &outbox[0].command.kind,
        CommandKind::RunShell { command, when: RunWhen::OnSuccess, .. } if command == "ls"
    ));
    assert!(outbox[0].discardable());
    store.claim_send(outbox[0].command.id, at(1)).unwrap();
    assert!(
        store.pending_outbox().unwrap()[0].discardable(),
        "unanswered, it may still go"
    );
}

#[test]
fn withdrawing_a_run_keeps_it_as_the_shell_draft() {
    let (_tmp, dir) = data_dir();
    let session = SessionId::new();
    let agent = Draft {
        text: "a message".into(),
        cursor: 0,
        when: RunWhen::Always,
    };
    let run = run_shell(session, "make test", RunWhen::OnSuccess);
    let draft = Draft {
        text: "make test".into(),
        cursor: 9,
        when: RunWhen::OnSuccess,
    };
    let store = Store::open(&dir).unwrap();
    store.set_draft(session, AGENT, Some(&agent)).unwrap();
    store.enqueue(&run).unwrap();
    store
        .withdraw(run.id, Some(((session, SHELL), &draft)))
        .unwrap();
    drop(store);
    let store = Store::open(&dir).unwrap();
    assert!(store.outbox().unwrap().is_empty());
    let drafts = store.drafts().unwrap();
    assert_eq!(drafts[&(session, SHELL)], draft);
    assert_eq!(
        drafts[&(session, AGENT)],
        agent,
        "the agent draft is untouched"
    );
}

#[test]
fn composer_mode_is_kept_per_session() {
    let (_tmp, dir) = data_dir();
    let (a, b) = (SessionId::new(), SessionId::new());
    {
        let store = Store::open(&dir).unwrap();
        store.set_composer_mode(a, SHELL).unwrap();
        store.set_composer_mode(b, SHELL).unwrap();
        store.set_composer_mode(b, AGENT).unwrap();
    }
    let store = Store::open(&dir).unwrap();
    let modes = store.load().unwrap().composer_modes;
    assert_eq!(modes.len(), 1, "no row is the agent");
    assert_eq!(modes[&a], SHELL);
}

#[test]
fn snippets_of_both_kinds_keep_the_same_text() {
    let (_tmp, dir) = data_dir();
    let snippet = |kind, used| Snippet {
        id: SnippetId::new(),
        name: None,
        text: "make test".into(),
        kind,
        used,
    };
    let (prompt, command) = (snippet(AGENT, 1), snippet(SHELL, 2));
    {
        let store = Store::open(&dir).unwrap();
        store.set_snippet(&prompt).unwrap();
        store.set_snippet(&command).unwrap();
    }
    let mut snippets = Store::open(&dir).unwrap().snippets().unwrap();
    snippets.sort_by_key(|s| s.used);
    assert_eq!(snippets, [prompt, command]);
}

/// A server's `hello` saying `facts` of itself.
fn hello_saying(facts: ServerFacts) -> skywave_proto::HelloResponse {
    skywave_proto::HelloResponse {
        protocol_version: skywave_proto::PROTOCOL_VERSION,
        server_id: skywave_proto::ServerId::new(),
        device_id: DeviceId::new(),
        head: Seq::ZERO,
        log: skywave_proto::LogRun {
            log: skywave_proto::LogId::new(),
            run: skywave_proto::RunId::new(),
            since: None,
        },
        devices: Vec::new(),
        shell: facts.shell_enabled,
        bypass: facts.bypass_allowed,
        login_expires: facts.login_expires,
        agent_login_offered: facts.agent_login_offered,
        agent_logged_out: facts.agent_logged_out,
    }
}

/// A server's `hello` with the agent's login as it says it.
fn hello_with_agent_login(offered: bool, logged_out: bool) -> skywave_proto::HelloResponse {
    hello_saying(ServerFacts {
        agent_login_offered: offered,
        agent_logged_out: logged_out,
        ..ServerFacts::default()
    })
}

/// A `hello` keeps whether a device may log the agent in and whether it is
/// logged out, in the replica and the view, each on its own; pairing with
/// another server forgets both.
#[test]
fn a_hello_keeps_the_agent_login_until_another_server_is_paired() {
    let (_tmp, dir) = data_dir();
    let core = Core::open_detached(&dir).unwrap();
    let seam = core.seam();
    seam.pairing_succeeded(&addr("old.example:7443"), paired_with(&ca_pem()))
        .unwrap();
    let kept = |offered: bool, logged_out: bool| {
        let settings = Store::open(&dir).unwrap().settings().unwrap();
        assert_eq!(
            (
                settings.server.agent_login_offered,
                settings.server.agent_logged_out
            ),
            (offered, logged_out),
            "stored"
        );
        let view = core.view();
        assert_eq!(
            (view.agent_login_offered, view.agent_logged_out),
            (offered, logged_out),
            "shown"
        );
    };
    kept(false, false);
    seam.hello_received(&hello_with_agent_login(true, false))
        .unwrap();
    kept(true, false);
    seam.hello_received(&hello_with_agent_login(false, true))
        .unwrap();
    kept(false, true);
    seam.hello_received(&hello_with_agent_login(true, true))
        .unwrap();
    kept(true, true);

    seam.pairing_succeeded(&addr("new.example:7443"), paired_with(&ca_pem()))
        .unwrap();
    kept(false, false);
}

/// Every byte of the replica on disk: the database and its WAL and shared
/// memory files, whichever exist.
fn replica_bytes(dir: &Path) -> Vec<u8> {
    let db = dir.join(skywave_core::store::DB_FILE);
    let mut bytes = Vec::new();
    for suffix in ["", "-wal", "-shm"] {
        let mut path = db.clone().into_os_string();
        path.push(suffix);
        if let Ok(read) = std::fs::read(&path) {
            bytes.extend(read);
        }
    }
    bytes
}

fn holds(bytes: &[u8], text: &str) -> bool {
    bytes
        .windows(text.len())
        .any(|window| window == text.as_bytes())
}

fn login_flow(core: &Core) -> skywave_core::LoginFlowView {
    match core.view().screen {
        skywave_core::Screen::Settings(settings) => settings.login.flow,
        other => panic!("expected settings, got {other:?}"),
    }
}

/// A login run from this device: its `LoggedIn` is stored and shown as a
/// `hello` saying so would be, while the flow, its URL and the code stay
/// in memory: no settings key and no byte of the replica holds them.
#[test]
fn an_agent_login_stores_its_outcome_and_never_its_url_or_code() {
    use skywave_core::{Action, LoginCode, LoginFlowView, LoginOutcome, Request, Route};
    use skywave_proto::{LoginFinishResult, LoginFlowId, LoginStartResult};

    const STATE: &str = "statevalue7f3a91";
    let url = format!("https://claude.example/oauth/authorize?code=true&state={STATE}");
    let code = format!("authcode5be2c4#{STATE}");
    let (_tmp, dir) = data_dir();
    let core = Core::open_detached(&dir).unwrap();
    let seam = core.seam();
    let requests = seam.take_requests().unwrap();
    seam.pairing_succeeded(&addr("srv.example:7443"), paired_with(&ca_pem()))
        .unwrap();
    seam.hello_received(&hello_with_agent_login(true, true))
        .unwrap();
    let asked = || requests.try_iter().collect::<Vec<_>>();
    asked();

    core.dispatch(Action::Navigate(Route::Settings)).unwrap();
    core.dispatch(Action::StartAgentLogin).unwrap();
    let [Request::LoginStart { req }] = asked()[..] else {
        panic!("expected a login start");
    };
    let flow = LoginFlowId::random();
    seam.login_started(
        req,
        Ok(LoginStartResult::Open {
            flow,
            url: url.clone(),
        }),
    );
    core.dispatch(Action::FinishAgentLogin {
        code: LoginCode::new(code.clone()),
    })
    .unwrap();
    let [Request::LoginFinish { req, ref finish }] = asked()[..] else {
        panic!("expected a login finish");
    };
    assert_eq!((finish.flow, finish.code.as_str()), (flow, code.as_str()));
    let expires = Utc.with_ymd_and_hms(2027, 3, 4, 5, 6, 7).unwrap();
    seam.login_finished(
        req,
        Ok(LoginFinishResult::LoggedIn {
            expires: Some(expires),
        }),
    )
    .unwrap();

    // Stored and shown at once, no hello needed.
    assert_eq!(
        login_flow(&core),
        LoginFlowView::Done(LoginOutcome::LoggedIn)
    );
    let view = core.view();
    assert!(!view.agent_logged_out);
    assert_eq!(view.login_expires, Some(expires));
    let settings = Store::open(&dir).unwrap().settings().unwrap();
    assert!(!settings.server.agent_logged_out);
    assert_eq!(settings.server.login_expires, Some(expires));

    let secrets = [url.as_str(), code.as_str(), STATE];
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    let mut rows = conn.prepare("SELECT key, value FROM settings").unwrap();
    let settings: Vec<(String, String)> = rows
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    for (key, value) in &settings {
        for secret in secrets {
            assert!(!value.contains(secret), "settings key {key} holds {secret}");
        }
    }
    drop(rows);
    drop(conn);

    // Positive control: what the login did store is in the bytes, so a
    // byte that is not would be found if it were.
    let stored = expires.to_rfc3339();
    for (when, bytes) in [
        ("open", replica_bytes(&dir)),
        ("closed", {
            drop((core, seam));
            replica_bytes(&dir)
        }),
    ] {
        assert!(holds(&bytes, &stored), "{when}: the expiry is stored");
        for secret in secrets {
            assert!(!holds(&bytes, secret), "{when}: the replica holds {secret}");
        }
    }
}

/// Pairing with another server forgets the login flow: the settings
/// screen starts over, and an answer still on its way from the old server
/// is dropped.
#[test]
fn pairing_with_another_server_forgets_the_agent_login() {
    use skywave_core::{Action, LoginCode, LoginFlowView, Request, Route};
    use skywave_proto::{LoginFinishResult, LoginFlowId, LoginStartResult};

    let (_tmp, dir) = data_dir();
    let core = Core::open_detached(&dir).unwrap();
    let seam = core.seam();
    let requests = seam.take_requests().unwrap();
    seam.pairing_succeeded(&addr("old.example:7443"), paired_with(&ca_pem()))
        .unwrap();
    seam.hello_received(&hello_with_agent_login(true, true))
        .unwrap();
    let asked = || requests.try_iter().collect::<Vec<_>>();
    asked();
    core.dispatch(Action::Navigate(Route::Settings)).unwrap();
    core.dispatch(Action::StartAgentLogin).unwrap();
    let [Request::LoginStart { req }] = asked()[..] else {
        panic!("expected a login start");
    };
    seam.login_started(
        req,
        Ok(LoginStartResult::Open {
            flow: LoginFlowId::random(),
            url: "https://old.example/login".into(),
        }),
    );
    core.dispatch(Action::FinishAgentLogin {
        code: LoginCode::new("a#b"),
    })
    .unwrap();
    let [Request::LoginFinish { req, .. }] = asked()[..] else {
        panic!("expected a login finish");
    };
    assert!(matches!(login_flow(&core), LoginFlowView::Finishing { .. }));

    seam.pairing_succeeded(&addr("new.example:7443"), paired_with(&ca_pem()))
        .unwrap();
    core.dispatch(Action::Navigate(Route::Settings)).unwrap();
    assert_eq!(login_flow(&core), LoginFlowView::Idle);
    seam.login_finished(req, Ok(LoginFinishResult::LoggedIn { expires: None }))
        .unwrap();
    assert_eq!(
        login_flow(&core),
        LoginFlowView::Idle,
        "the late answer is dropped"
    );
    let settings = Store::open(&dir).unwrap().settings().unwrap();
    assert!(
        !settings.server.agent_logged_out,
        "reset, and not set by the late answer"
    );
    seam.hello_received(&hello_with_agent_login(true, true))
        .unwrap();
    seam.login_finished(req, Ok(LoginFinishResult::LoggedIn { expires: None }))
        .unwrap();
    assert!(core.view().agent_logged_out, "still dropped after a hello");
}

/// Every fact a `hello` reports, each set apart from its default.
fn every_fact() -> ServerFacts {
    ServerFacts {
        shell_enabled: true,
        bypass_allowed: true,
        login_expires: Some(at(7)),
        agent_login_offered: true,
        agent_logged_out: true,
    }
}

/// What a `hello` says of the server is kept whole across a reopen, and
/// the next replaces all of it: nothing an earlier one said lingers.
#[test]
fn the_server_facts_are_kept_whole_until_a_hello_replaces_them() {
    let (_tmp, dir) = data_dir();
    let store = Store::open(&dir).unwrap();
    assert_eq!(
        store.settings().unwrap().server,
        ServerFacts::default(),
        "the defaults until a hello"
    );
    store.set_server_facts(&every_fact()).unwrap();
    drop(store);
    let store = Store::open(&dir).unwrap();
    assert_eq!(store.load().unwrap().settings.server, every_fact());
    let bypass_only = ServerFacts {
        bypass_allowed: true,
        ..ServerFacts::default()
    };
    store.set_server_facts(&bypass_only).unwrap();
    assert_eq!(store.settings().unwrap().server, bypass_only);
}

/// Name, type, `NOT NULL`, default and primary key position of each column.
fn table_info(dir: &Path, table: &str) -> Vec<(String, String, bool, Option<String>, i64)> {
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .unwrap();
    stmt.query_map([], |row| {
        Ok((
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
        ))
    })
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

#[test]
fn the_migration_makes_existing_drafts_and_snippets_the_agents() {
    let (_tmp, dir) = data_dir();
    let session = SessionId::new();
    drop(Store::open(&dir).unwrap());
    let fresh = table_info(&dir, "drafts");

    // A replica from before shell commands, holding a draft and a snippet.
    drop_shell(&dir);
    let snippet = SnippetId::new();
    {
        let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
        conn.execute(
            "INSERT INTO drafts (session_id, text, cursor) VALUES (?1, 'half typed', 4)",
            [session.0.to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO snippets (id, name, text, used) VALUES (?1, NULL, 'run the tests', 1)",
            [snippet.0.to_string()],
        )
        .unwrap();
    }
    drop_turns(&dir);
    drop_workdir(&dir);
    drop_import_columns(&dir);
    set_user_version(&dir, skywave_core::store::SCHEMA_VERSION - 15);

    let store = Store::open(&dir).unwrap();
    let snapshot = store.load().unwrap();
    assert_eq!(snapshot.drafts.len(), 1);
    assert_eq!(
        snapshot.drafts[&(session, AGENT)],
        Draft {
            text: "half typed".into(),
            cursor: 4,
            when: RunWhen::Always,
        }
    );
    assert_eq!(snapshot.snippets.len(), 1);
    assert_eq!(
        (snapshot.snippets[0].id, snapshot.snippets[0].kind),
        (snippet, AGENT)
    );
    assert!(snapshot.composer_modes.is_empty());
    drop(store);

    let column = |name: &str, ty: &str, default: Option<&str>, pk| {
        (
            name.to_owned(),
            ty.to_owned(),
            true,
            default.map(str::to_owned),
            pk,
        )
    };
    let expected = vec![
        column("session_id", "TEXT", None, 1),
        column("mode", "TEXT", None, 2),
        column("text", "TEXT", None, 0),
        column("cursor", "INTEGER", None, 0),
        column("when", "TEXT", Some("'always'"), 0),
    ];
    assert_eq!(
        table_info(&dir, "drafts"),
        expected,
        "keyed by session and mode"
    );
    assert_eq!(fresh, expected, "as a fresh replica has it");
}

// ---- workdir answers --------------------------------------------------------

fn diff_blob(size: u64) -> BlobRef {
    BlobRef {
        id: BlobId::new(),
        size,
        sha256: ContentHash::of(&size.to_le_bytes()),
    }
}

fn changes_ask(session: SessionId) -> WorkdirAsk {
    WorkdirAsk::Changes { session }
}

fn file_ask(session: SessionId, path: &str) -> WorkdirAsk {
    WorkdirAsk::File {
        session,
        path: path.into(),
    }
}

/// A change set with one modified file per blob, each diff that blob.
fn changes(blobs: &[&BlobRef]) -> WorkdirAnswer {
    let files = blobs
        .iter()
        .enumerate()
        .map(|(i, blob)| ChangedFile {
            path: format!("f{i}"),
            from: None,
            change: FileChange::Modified,
            added: Some(1),
            removed: Some(1),
            diff: DiffBody::Blob((*blob).clone()),
        })
        .collect::<Vec<_>>();
    WorkdirAnswer::Changes(ChangesResult::Changes(ChangeSet {
        at: Utc.with_ymd_and_hms(2026, 11, 3, 9, 0, 0).unwrap(),
        branch: Some("main".into()),
        head: Some("abc1234".into()),
        total: u32::try_from(files.len()).unwrap(),
        files,
    }))
}

fn file(path: &str, blob: &BlobRef) -> WorkdirAnswer {
    WorkdirAnswer::File(FileResult::File {
        at: Utc.with_ymd_and_hms(2026, 11, 3, 9, 0, 0).unwrap(),
        path: path.into(),
        size: blob.size,
        content: FileBody::Blob(blob.clone()),
    })
}

fn put(store: &mut Store, ask: &WorkdirAsk, answer: &WorkdirAnswer) -> (Vec<BlobId>, Vec<BlobId>) {
    let stored = store
        .put_workdir(ask, answer)
        .unwrap()
        .expect("the session is in the replica");
    (stored.registered, stored.purged)
}

#[test]
fn a_workdir_refresh_replaces_the_answer_and_purges_only_blobs_nothing_names() {
    let (_tmp, dir) = data_dir();
    let log = Log::new();
    let mut store = Store::open(&dir).unwrap();
    store.apply_batch(&log.batch(1, 7), Utc::now()).unwrap();
    let (d1, d2, d3) = (diff_blob(1), diff_blob(2), diff_blob(3));

    let (registered, purged) = put(&mut store, &changes_ask(log.a), &changes(&[&d1, &d2]));
    assert_eq!(registered, [d1.id, d2.id]);
    assert!(purged.is_empty());
    // A file whose content is d2 names it too: known, so not registered.
    let (registered, _) = put(&mut store, &file_ask(log.a, "f1"), &file("f1", &d2));
    assert!(
        registered.is_empty(),
        "an id already known is not registered"
    );
    store
        .set_blob_status(
            d2.id,
            &BlobStatus::Available {
                path: "/b/d2".into(),
            },
        )
        .unwrap();

    // The refresh drops d1, keeps d2, adds d3.
    let refreshed = changes(&[&d2, &d3]);
    let (registered, purged) = put(&mut store, &changes_ask(log.a), &refreshed);
    assert_eq!(registered, [d3.id]);
    assert_eq!(purged, [d1.id]);
    assert_eq!(store.blob(d1.id).unwrap(), None);
    assert_eq!(
        store.blob(d2.id).unwrap().unwrap().status,
        BlobStatus::Available {
            path: "/b/d2".into()
        },
        "a blob the refresh still names stays downloaded"
    );

    // Dropping d2 from the changes keeps it: the file answer names it.
    let (_, purged) = put(&mut store, &changes_ask(log.a), &changes(&[&d3]));
    assert!(purged.is_empty());
    assert!(store.blob(d2.id).unwrap().is_some());
    drop(store);

    // Only the latest answer per key, kept over a reopen; workdir blobs are
    // marked, the event blob is not.
    let store = Store::open(&dir).unwrap();
    let snapshot = store.load().unwrap();
    assert_eq!(snapshot.workdirs.len(), 2);
    assert_eq!(snapshot.workdirs[&changes_ask(log.a)], changes(&[&d3]));
    assert_eq!(snapshot.workdirs[&file_ask(log.a, "f1")], file("f1", &d2));
    let marks: Vec<(BlobId, bool)> = snapshot.blobs.iter().map(|b| (b.id, b.workdir)).collect();
    let mut expected = vec![(log.blob.id, false), (d2.id, true), (d3.id, true)];
    expected.sort();
    assert_eq!(marks, expected, "every blob is listed, workdir ones marked");
}

#[test]
fn a_workdir_answer_without_blobs_purges_the_ones_it_replaced() {
    let (_tmp, dir) = data_dir();
    let log = Log::new();
    let mut store = Store::open(&dir).unwrap();
    store.apply_batch(&log.batch(1, 7), Utc::now()).unwrap();
    let d1 = diff_blob(1);
    put(&mut store, &file_ask(log.a, "x"), &file("x", &d1));

    let gone = WorkdirAnswer::File(FileResult::NotFound);
    let (registered, purged) = put(&mut store, &file_ask(log.a, "x"), &gone);
    assert!(registered.is_empty());
    assert_eq!(purged, [d1.id]);
    assert_eq!(store.load().unwrap().workdirs[&file_ask(log.a, "x")], gone);

    assert!(matches!(
        store.put_workdir(&file_ask(log.a, "x"), &changes(&[])),
        Err(StoreError::WorkdirMismatch)
    ));
}

#[test]
fn two_sessions_on_one_workdir_share_a_blob_and_deleting_one_keeps_it() {
    let (_tmp, dir) = data_dir();
    let log = Log::new();
    let mut store = Store::open(&dir).unwrap();
    store.apply_batch(&log.batch(1, 7), Utc::now()).unwrap();
    let shared = diff_blob(7);
    let (registered, _) = put(&mut store, &changes_ask(log.a), &changes(&[&shared]));
    assert_eq!(registered, [shared.id]);
    let (registered, _) = put(&mut store, &changes_ask(log.b), &changes(&[&shared]));
    assert!(registered.is_empty(), "the server named the same blob");

    let blobs = store.delete_session_locally(log.a, Utc::now()).unwrap();
    assert_eq!(
        blobs,
        [log.blob.id],
        "the shared blob is not the caller's to remove"
    );
    assert!(store.blob(shared.id).unwrap().unwrap().workdir);
    let workdirs = store.load().unwrap().workdirs;
    assert_eq!(workdirs.keys().collect::<Vec<_>>(), [&changes_ask(log.b)]);

    let blobs = store.delete_session_locally(log.b, Utc::now()).unwrap();
    assert_eq!(blobs, [shared.id]);
    assert_eq!(store.blob(shared.id).unwrap(), None);
    assert!(store.load().unwrap().workdirs.is_empty());
}

#[test]
fn reset_for_server_clears_workdir_answers() {
    let (_tmp, dir) = data_dir();
    let log = Log::new();
    let mut store = Store::open(&dir).unwrap();
    store.apply_batch(&log.batch(1, 7), Utc::now()).unwrap();
    let d1 = diff_blob(1);
    put(&mut store, &file_ask(log.b, "x"), &file("x", &d1));

    let mut blobs = store.reset_for_server().unwrap();
    let mut expected = vec![log.blob.id, d1.id];
    blobs.sort();
    expected.sort();
    assert_eq!(blobs, expected);
    let snapshot = store.load().unwrap();
    assert!(snapshot.workdirs.is_empty());
    assert!(snapshot.blobs.is_empty());
    // No ref is left to mark a later blob of the same id as a workdir one.
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    let refs: i64 = conn
        .query_row("SELECT COUNT(*) FROM workdir_blob_refs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(refs, 0);
}

#[test]
fn a_workdir_answer_for_a_deleted_or_unknown_session_writes_nothing() {
    let (_tmp, dir) = data_dir();
    let log = Log::new();
    let mut store = Store::open(&dir).unwrap();
    store.apply_batch(&log.batch(1, 7), Utc::now()).unwrap();
    store.delete_session_locally(log.a, Utc::now()).unwrap();
    let blob = diff_blob(1);

    for session in [log.a, SessionId::new()] {
        let written = store
            .put_workdir(&file_ask(session, "x"), &file("x", &blob))
            .unwrap();
        assert_eq!(written, None);
    }
    assert!(store.load().unwrap().workdirs.is_empty());
    assert_eq!(store.blob(blob.id).unwrap(), None, "no blob row either");

    // The same answer for a session still here is written.
    put(&mut store, &file_ask(log.b, "x"), &file("x", &blob));
    assert_eq!(store.load().unwrap().workdirs.len(), 1);
    assert!(store.blob(blob.id).unwrap().unwrap().workdir);

    // After a re-pair, an old server's answer finds no session.
    store.reset_for_server().unwrap();
    let written = store
        .put_workdir(&file_ask(log.b, "x"), &file("x", &blob))
        .unwrap();
    assert_eq!(written, None);
    assert!(store.load().unwrap().workdirs.is_empty());
}

#[test]
fn the_workdir_migration_keeps_a_populated_replica() {
    let (_tmp, dir) = data_dir();
    let log = Log::new();
    let mut store = Store::open(&dir).unwrap();
    store.apply_batch(&log.batch(1, 7), Utc::now()).unwrap();
    let before = store.load().unwrap();
    drop(store);

    drop_workdir(&dir);
    drop_import_columns(&dir);
    set_user_version(&dir, skywave_core::store::SCHEMA_VERSION - 13);
    let mut store = Store::open(&dir).unwrap();
    let after = store.load().unwrap();
    assert_eq!(after, before, "nothing else changes");
    assert!(after.workdirs.is_empty());
    assert!(after.blobs.iter().all(|b| !b.workdir));

    let d1 = diff_blob(1);
    let (registered, _) = put(&mut store, &changes_ask(log.a), &changes(&[&d1]));
    assert_eq!(registered, [d1.id]);
}

/// An adopted session, its imported history (a call's output a blob) and
/// its `ImportFinished`, as the server logs them; seqs from 1.
fn adopted_log(session: SessionId, output: &BlobRef) -> Vec<Event> {
    let mut finished = event(
        session,
        3,
        EventKind::ImportFinished {
            outcome: ImportOutcome::Imported { items: 2 },
        },
    );
    finished.command_id = Some(CommandId::new());
    let created = Event {
        command_id: None,
        ..event(
            session,
            0,
            EventKind::SessionCreated {
                workdir: "/srv/a".into(),
                context_name: "start".into(),
                from: Some(SessionOrigin::Agent {
                    conversation: "conv-1".into(),
                }),
            },
        )
    };
    vec![
        created,
        event(
            session,
            1,
            EventKind::Imported {
                at: None,
                item: ImportedItem::UserMessage {
                    text: "fix the build".into(),
                    full: None,
                },
            },
        ),
        event(
            session,
            2,
            EventKind::Imported {
                at: None,
                item: ImportedItem::ToolResult(ToolResult {
                    call: None,
                    is_error: false,
                    status: None,
                    preview: "ok".into(),
                    lines: 1,
                    output: Some(Full::Blob(output.clone())),
                }),
            },
        ),
        finished,
    ]
}

#[test]
fn an_import_split_over_pulls_is_importing_until_its_finish_is_stored() {
    let (_tmp, dir) = data_dir();
    let session = SessionId::new();
    let output = BlobRef {
        id: BlobId::new(),
        size: 9000,
        sha256: skywave_proto::ContentHash::of(b""),
    };
    let events = adopted_log(session, &output);
    let mut store = Store::open(&dir).unwrap();
    store
        .apply_batch(
            &EventBatch {
                from: Seq(1),
                events: events[..3].to_vec(),
            },
            Utc::now(),
        )
        .unwrap();
    let summary = |store: &Store| store.load().unwrap().sessions[0].clone();
    let before = summary(&store);
    assert!(before.importing);
    assert_eq!(before.first_prompt.as_deref(), Some("fix the build"));
    assert_eq!(before.last_seq, Seq(1), "imported history is not activity");
    assert_eq!(before.notable, 0);
    assert!(
        store.blob(output.id).unwrap().is_some(),
        "imported blob registered"
    );
    drop(store);

    let mut store = Store::open(&dir).unwrap();
    assert_eq!(summary(&store), before, "kept over a reopen");
    store
        .apply_batch(
            &EventBatch {
                from: Seq(4),
                events: events[3..].to_vec(),
            },
            Utc::now(),
        )
        .unwrap();
    assert!(!summary(&store).importing);
    drop(store);
    assert!(!summary(&Store::open(&dir).unwrap()).importing);
}

#[test]
fn the_importing_migration_keeps_a_populated_replica() {
    let (_tmp, dir) = data_dir();
    let log = Log::new();
    let mut store = Store::open(&dir).unwrap();
    store.apply_batch(&log.batch(1, 7), Utc::now()).unwrap();
    let before = store.load().unwrap();
    drop(store);

    drop_import_columns(&dir);
    set_user_version(&dir, skywave_core::store::SCHEMA_VERSION - 12);
    let after = Store::open(&dir).unwrap().load().unwrap();
    assert_eq!(after, before, "nothing changes");
    assert!(after.sessions.iter().all(|s| !s.importing));
}

#[test]
fn the_adopted_migration_backfills_from_the_session_created() {
    let (_tmp, dir) = data_dir();
    let session = SessionId::new();
    let output = BlobRef {
        id: BlobId::new(),
        size: 9000,
        sha256: skywave_proto::ContentHash::of(b""),
    };
    let log = Log::new();
    let mut store = Store::open(&dir).unwrap();
    store.apply_batch(&log.batch(1, 7), Utc::now()).unwrap();
    store
        .apply_batch(
            &EventBatch {
                from: Seq(8),
                events: adopted_log(session, &output),
            },
            Utc::now(),
        )
        .unwrap();
    let before = store.load().unwrap();
    let adopted = |sessions: &[skywave_core::SessionSummary]| -> Vec<Option<String>> {
        sessions.iter().map(|s| s.adopted.clone()).collect()
    };
    assert_eq!(
        adopted(&before.sessions),
        [Some("conv-1".to_owned()), None, None],
        "the adopted session (newest) names its conversation, the others none"
    );
    drop(store);

    drop_adopted(&dir);
    set_user_version(&dir, skywave_core::store::SCHEMA_VERSION - 11);
    let after = Store::open(&dir).unwrap().load().unwrap();
    assert_eq!(after, before, "backfilled from the stored log");
}

/// `hello`'s log against what the replica has seen of it: the cursor and
/// any accepted command beyond it. A head below that, another log, a run
/// last seen that the server has no record of (or did not answer about),
/// or a later run that started below it lost events the replica holds; a
/// restart that started at it or past it did not. Nothing is kept when it
/// lost them, and the reset forgets the log with the rest of the replica.
#[test]
fn the_server_log_is_checked_against_what_the_replica_has_seen() {
    use skywave_core::LogCheck;
    use skywave_proto::{LogId, LogRun, RunId, RunsSince};

    let mut store = Store::open_in_memory().unwrap();
    let log = Log::new();
    let seven = Seq(log.events.len() as u64);
    assert_eq!(seven, Seq(7));
    store
        .apply_batch(
            &EventBatch {
                from: Seq(1),
                events: log.events,
            },
            Utc::now(),
        )
        .unwrap();
    let id = LogId::new();
    let hello = |run: RunId, since: Option<RunsSince>| LogRun {
        log: id,
        run,
        since,
    };
    let current = Some(RunsSince::Known { next_start: None });
    let next = |start: u64| {
        Some(RunsSince::Known {
            next_start: Some(Seq(start)),
        })
    };
    let lost = |seen: u64| LogCheck::Lost { seen: Seq(seen) };

    // The first `hello` has nothing to compare but the head.
    let a = RunId::new();
    assert_eq!(store.server_run().unwrap(), None);
    assert_eq!(
        store.take_server_log(Seq(6), &hello(a, None)).unwrap(),
        lost(7)
    );
    assert_eq!(
        store.take_server_log(seven, &hello(a, None)).unwrap(),
        LogCheck::Continues
    );
    assert_eq!(store.server_run().unwrap(), Some(a), "asked about next");
    assert_eq!(
        store.take_server_log(Seq(9), &hello(a, current)).unwrap(),
        LogCheck::Continues,
        "the same run, grown"
    );
    assert_eq!(
        store.take_server_log(Seq(5), &hello(a, current)).unwrap(),
        lost(7)
    );

    // A restart that found everything this replica has seen: equal is clean.
    let b = RunId::new();
    assert_eq!(
        store.take_server_log(Seq(8), &hello(b, next(7))).unwrap(),
        LogCheck::Continues
    );
    assert_eq!(store.server_run().unwrap(), Some(b));
    // Some run since started below it: a restore, grown past the cursor
    // again, however many restarts ago.
    assert_eq!(
        store
            .take_server_log(Seq(12), &hello(RunId::new(), next(5)))
            .unwrap(),
        lost(7)
    );
    // No record of the run last seen, or no answer about it.
    for since in [Some(RunsSince::Unknown), None] {
        assert_eq!(
            store
                .take_server_log(Seq(12), &hello(RunId::new(), since))
                .unwrap(),
            lost(7),
            "{since:?}"
        );
    }
    // Another log altogether, however far it reaches.
    let other = LogRun {
        log: LogId::new(),
        ..hello(RunId::new(), current)
    };
    assert_eq!(store.take_server_log(Seq(20), &other).unwrap(), lost(7));
    // Nothing was kept from the refused: the restart's run is still the one.
    assert_eq!(store.server_run().unwrap(), Some(b));
    let restart = hello(b, current);
    assert_eq!(
        store.take_server_log(Seq(8), &restart).unwrap(),
        LogCheck::Continues
    );

    // A command accepted at 9, beyond the cursor, was seen too.
    let command = Command {
        id: CommandId::new(),
        kind: CommandKind::StopSession { session_id: log.a },
    };
    store.enqueue(&command).unwrap();
    store
        .record_result(
            command.id,
            &CommandResult::Accepted { seq: Seq(9) },
            Utc::now(),
        )
        .unwrap();
    assert_eq!(store.take_server_log(Seq(8), &restart).unwrap(), lost(9));
    assert_eq!(
        store.take_server_log(Seq(12), &hello(b, next(8))).unwrap(),
        lost(9)
    );
    assert_eq!(
        store.take_server_log(Seq(12), &hello(b, next(9))).unwrap(),
        LogCheck::Continues
    );

    // The reset keeps this device's drafts and forgets the log.
    let draft = Draft {
        text: "keep me".into(),
        cursor: 0,
        when: RunWhen::Always,
    };
    store
        .set_draft(log.a, ComposerMode::Agent, Some(&draft))
        .unwrap();
    store.reset_for_lost_history().unwrap();
    assert_eq!(store.cursor().unwrap(), Seq::ZERO);
    assert!(store.outbox().unwrap().is_empty());
    assert_eq!(
        store.drafts().unwrap().get(&(log.a, ComposerMode::Agent)),
        Some(&draft)
    );
    assert_eq!(
        store.take_server_log(Seq(3), &other).unwrap(),
        LogCheck::Continues,
        "a fresh replica takes any log"
    );
}

fn blob_ref(size: u64, content: &[u8]) -> BlobRef {
    BlobRef {
        id: BlobId::new(),
        size,
        sha256: ContentHash::of(content),
    }
}

fn cropped(session: SessionId, minute: u32, blob: BlobId, into: &BlobRef, by: DeviceId) -> Event {
    event(
        session,
        minute,
        EventKind::BlobCropped {
            blob,
            holds: BlobUse::ToolOutput,
            into: into.clone(),
            kept: vec![LineSpan { first: 0, count: 2 }],
            lines: 40,
            by,
        },
    )
}

/// `BlobCropped` and `BlobDeleted` keep the blob's fate and purge the row
/// of a blob not held; a crop's `into` registers, unless the same batch
/// crops it in turn, and a blob with a fate never registers again. Fates
/// survive a reopen and go with the session's local delete and with a
/// reset. (A held blob is kept for a local crop: see
/// `blob_fates_keep_a_held_blob_for_its_local_crop`.)
#[test]
fn blob_fates_fold_purge_and_go_with_the_replica() {
    let log = Log::new();
    let (_tmp, dir) = data_dir();
    let mut store = Store::open(&dir).unwrap();
    store.apply_batch(&log.batch(1, 7), Utc::now()).unwrap();

    // A crop of a crop in one batch, the original not downloaded: the
    // middle blob registers and goes.
    let by = DeviceId::new();
    let (b1, b2) = (blob_ref(20, b"b1"), blob_ref(10, b"b2"));
    let applied = store
        .apply_batch(
            &EventBatch {
                from: Seq(8),
                events: vec![
                    cropped(log.a, 8, log.blob.id, &b1, by),
                    cropped(log.a, 9, b1.id, &b2, by),
                ],
            },
            Utc::now(),
        )
        .unwrap();
    assert_eq!(applied.blobs, [b2.id], "only what is left is queued");
    assert_eq!(applied.purged, [log.blob.id, b1.id]);
    assert!(applied.local_crops.is_empty(), "nothing here to crop");
    assert!(store.pending_crops().unwrap().is_empty());
    assert_eq!(store.blob(log.blob.id).unwrap(), None);
    assert_eq!(store.blob(b1.id).unwrap(), None);
    assert_eq!(
        store.blob(b2.id).unwrap().map(|b| b.status),
        Some(BlobStatus::NotDownloaded)
    );
    let fates = store.blob_fates().unwrap();
    assert_eq!(fates.len(), 2);
    assert!(matches!(
        &fates[&log.blob.id],
        BlobFate::Cropped { into, lines: 40, .. } if *into == b1
    ));

    // A replay changes nothing, nor does a later reference to a purged blob.
    let again = store
        .apply_batch(
            &EventBatch {
                from: Seq(8),
                events: vec![
                    cropped(log.a, 8, log.blob.id, &b1, by),
                    cropped(log.a, 9, b1.id, &b2, by),
                    log.events[3].clone(),
                ],
            },
            Utc::now(),
        )
        .unwrap();
    assert!(again.blobs.is_empty() && again.purged.is_empty());
    assert_eq!(
        store.blob(log.blob.id).unwrap(),
        None,
        "not registered again"
    );

    // A delete of a blob not downloaded purges its row too.
    let deleted = event(
        log.a,
        11,
        EventKind::BlobDeleted {
            blob: b2.id,
            holds: BlobUse::ToolOutput,
            by,
        },
    );
    let applied = store
        .apply_batch(
            &EventBatch {
                from: Seq(11),
                events: vec![deleted],
            },
            Utc::now(),
        )
        .unwrap();
    assert_eq!(applied.purged, [b2.id]);
    assert!(store.blobs().unwrap().is_empty());
    drop(store);

    let mut store = Store::open(&dir).unwrap();
    let snapshot = store.load().unwrap();
    assert_eq!(snapshot.fates.len(), 3);
    assert_eq!(snapshot.fates[&b2.id], BlobFate::Deleted { by });

    // A reset clears them with the events.
    assert!(store.reset_for_lost_history().unwrap().is_empty());
    assert!(store.blob_fates().unwrap().is_empty());

    // And so does the local delete of their session.
    store.apply_batch(&log.batch(1, 7), Utc::now()).unwrap();
    store
        .apply_batch(
            &EventBatch {
                from: Seq(8),
                events: vec![cropped(log.a, 8, log.blob.id, &b1, by)],
            },
            Utc::now(),
        )
        .unwrap();
    assert_eq!(store.blob_fates().unwrap().len(), 1);
    store.delete_session_locally(log.a, at(1)).unwrap();
    assert!(store.blob_fates().unwrap().is_empty());
    assert!(
        store.blobs().unwrap().is_empty(),
        "the crop's blob went too"
    );
}

/// A crop of a crop in one batch, the original held (`Available`): the
/// original's row stays for the local crops, both `into`s register but are
/// not queued for a fetch, and the batch names both crops, in log order.
/// The pending crops are derived from the replica (`pending_crops`, the
/// snapshot), each ends once, purging the blob it was made from, and a
/// delete of what a pending crop makes purges the blob it would read.
#[test]
fn blob_fates_keep_a_held_blob_for_its_local_crop() {
    let log = Log::new();
    let (_tmp, dir) = data_dir();
    let mut store = Store::open(&dir).unwrap();
    store.apply_batch(&log.batch(1, 7), Utc::now()).unwrap();
    let held = BlobStatus::Available {
        path: "blobs/x".into(),
    };
    store.set_blob_status(log.blob.id, &held).unwrap();

    let by = DeviceId::new();
    let (b1, b2) = (blob_ref(20, b"b1"), blob_ref(10, b"b2"));
    let applied = store
        .apply_batch(
            &EventBatch {
                from: Seq(8),
                events: vec![
                    cropped(log.a, 8, log.blob.id, &b1, by),
                    cropped(log.a, 9, b1.id, &b2, by),
                ],
            },
            Utc::now(),
        )
        .unwrap();
    let (first, second) = (
        PendingCrop {
            into: b1.id,
            from: log.blob.id,
        },
        PendingCrop {
            into: b2.id,
            from: b1.id,
        },
    );
    assert!(applied.blobs.is_empty(), "nothing queued for a fetch");
    assert!(applied.purged.is_empty(), "nothing purged yet");
    assert_eq!(applied.local_crops, [first, second]);
    assert_eq!(
        store.blob(log.blob.id).unwrap().map(|b| b.status),
        Some(held),
        "the held copy stays"
    );
    for into in [&b1, &b2] {
        assert_eq!(
            store.blob(into.id).unwrap().map(|b| b.status),
            Some(BlobStatus::NotDownloaded)
        );
    }
    assert_eq!(store.pending_crops().unwrap(), [first, second]);
    assert_eq!(store.blob_fates().unwrap().len(), 2);

    // A replay changes nothing.
    let again = store
        .apply_batch(
            &EventBatch {
                from: Seq(8),
                events: vec![
                    cropped(log.a, 8, log.blob.id, &b1, by),
                    cropped(log.a, 9, b1.id, &b2, by),
                ],
            },
            Utc::now(),
        )
        .unwrap();
    assert!(again.blobs.is_empty() && again.purged.is_empty() && again.local_crops.is_empty());
    drop(store);

    // A reopen finds them pending still.
    let mut store = Store::open(&dir).unwrap();
    assert_eq!(store.load().unwrap().pending_crops, [first, second]);

    // The first crop ends, once: the original goes.
    assert_eq!(store.end_local_crop(first).unwrap(), [log.blob.id]);
    assert!(store.end_local_crop(first).unwrap().is_empty());
    assert_eq!(store.blob(log.blob.id).unwrap(), None);
    assert_eq!(store.pending_crops().unwrap(), [second]);

    // A delete of the last blob before its crop ran purges both.
    let applied = store
        .apply_batch(
            &EventBatch {
                from: Seq(10),
                events: vec![event(
                    log.a,
                    10,
                    EventKind::BlobDeleted {
                        blob: b2.id,
                        holds: BlobUse::ToolOutput,
                        by,
                    },
                )],
            },
            Utc::now(),
        )
        .unwrap();
    assert_eq!(applied.purged, [b2.id, b1.id]);
    assert!(applied.local_crops.is_empty());
    assert!(store.pending_crops().unwrap().is_empty());
    assert!(store.end_local_crop(second).unwrap().is_empty());
    assert!(store.blobs().unwrap().is_empty());
}

/// The fates migration adds its table to a populated replica, which then
/// folds fates as a fresh one does.
#[test]
fn the_fates_migration_keeps_a_populated_replica() {
    let log = Log::new();
    let (_tmp, dir) = data_dir();
    let mut store = Store::open(&dir).unwrap();
    store.apply_batch(&log.batch(1, 7), Utc::now()).unwrap();
    drop(store);
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    conn.execute_batch("DROP TABLE blob_fates;").unwrap();
    drop(conn);
    set_user_version(&dir, skywave_core::store::SCHEMA_VERSION - 7);

    let mut store = Store::open(&dir).unwrap();
    assert!(store.blob_fates().unwrap().is_empty());
    let by = DeviceId::new();
    let applied = store
        .apply_batch(
            &EventBatch {
                from: Seq(8),
                events: vec![event(
                    log.a,
                    8,
                    EventKind::BlobDeleted {
                        blob: log.blob.id,
                        holds: BlobUse::ToolOutput,
                        by,
                    },
                )],
            },
            Utc::now(),
        )
        .unwrap();
    assert_eq!(applied.purged, [log.blob.id]);
    assert_eq!(
        store.blob_fates().unwrap()[&log.blob.id],
        BlobFate::Deleted { by }
    );
}

/// A given-up download stays `Failed`, with its reason, over a reopen;
/// any other status written clears the reason, and the blob's row going
/// takes it along.
#[test]
fn a_failed_blob_keeps_its_reason_over_a_reopen() {
    let log = Log::new();
    let (_tmp, dir) = data_dir();
    let mut store = Store::open(&dir).unwrap();
    store.apply_batch(&log.batch(1, 7), Utc::now()).unwrap();
    let failed = BlobStatus::Failed {
        reason: "blob hash mismatch".into(),
    };
    store.set_blob_status(log.blob.id, &failed).unwrap();
    drop(store);

    let mut store = Store::open(&dir).unwrap();
    assert_eq!(store.blob(log.blob.id).unwrap().unwrap().status, failed);
    assert_eq!(store.blobs().unwrap()[0].status, failed);
    assert_eq!(store.load().unwrap().blobs[0].status, failed);

    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    let rows = |conn: &rusqlite::Connection| -> i64 {
        conn.query_row("SELECT count(*) FROM blob_failures", [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(rows(&conn), 1);
    let partial = BlobStatus::Downloading {
        got: 1,
        path: "blobs/x.part".into(),
    };
    store.set_blob_status(log.blob.id, &partial).unwrap();
    assert_eq!(store.blob(log.blob.id).unwrap().unwrap().status, partial);
    assert_eq!(rows(&conn), 0, "a download under way is no failure");
    store
        .set_blob_status(log.blob.id, &BlobStatus::NotDownloaded)
        .unwrap();
    assert_eq!(
        store.blob(log.blob.id).unwrap().unwrap().status,
        BlobStatus::NotDownloaded
    );

    store.set_blob_status(log.blob.id, &failed).unwrap();
    assert_eq!(rows(&conn), 1);
    store.delete_session_locally(log.a, at(2)).unwrap();
    assert!(store.blob(log.blob.id).unwrap().is_none());
    assert_eq!(rows(&conn), 0, "the failure went with the blob's row");
}

/// A stopped download keeps its progress and partial file over a reopen,
/// apart from both `NotDownloaded` and `Downloading`; a failure or a
/// plain `NotDownloaded` written over it clears them.
#[test]
fn a_stopped_download_keeps_its_progress_over_a_reopen() {
    let log = Log::new();
    let (_tmp, dir) = data_dir();
    let mut store = Store::open(&dir).unwrap();
    store.apply_batch(&log.batch(1, 7), Utc::now()).unwrap();
    let stopped = BlobStatus::Stopped {
        got: 3,
        path: "blobs/x.part".into(),
    };
    store.set_blob_status(log.blob.id, &stopped).unwrap();
    drop(store);

    let store = Store::open(&dir).unwrap();
    assert_eq!(store.blob(log.blob.id).unwrap().unwrap().status, stopped);
    assert_eq!(store.load().unwrap().blobs[0].status, stopped);
    let failed = BlobStatus::Failed {
        reason: "gone".into(),
    };
    store.set_blob_status(log.blob.id, &failed).unwrap();
    assert_eq!(store.blob(log.blob.id).unwrap().unwrap().status, failed);
    store.set_blob_status(log.blob.id, &stopped).unwrap();
    store
        .set_blob_status(log.blob.id, &BlobStatus::NotDownloaded)
        .unwrap();
    assert_eq!(
        store.blob(log.blob.id).unwrap().unwrap().status,
        BlobStatus::NotDownloaded
    );
}

/// The failures migration adds its table and trigger to a populated
/// replica, whose blobs read as before and can fail from then on.
#[test]
fn the_failures_migration_keeps_a_populated_replica() {
    let log = Log::new();
    let (_tmp, dir) = data_dir();
    let mut store = Store::open(&dir).unwrap();
    store.apply_batch(&log.batch(1, 7), Utc::now()).unwrap();
    let before = store.blobs().unwrap();
    drop(store);
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    conn.execute_batch(
        "DROP TRIGGER blob_failures_go_with_blobs;
         DROP TABLE blob_failures;",
    )
    .unwrap();
    drop(conn);
    set_user_version(&dir, skywave_core::store::SCHEMA_VERSION - 6);

    let mut store = Store::open(&dir).unwrap();
    assert_eq!(store.blobs().unwrap(), before);
    let failed = BlobStatus::Failed {
        reason: "not found".into(),
    };
    store.set_blob_status(log.blob.id, &failed).unwrap();
    assert_eq!(store.blob(log.blob.id).unwrap().unwrap().status, failed);
    store.delete_session_locally(log.a, at(2)).unwrap();
    let conn = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE)).unwrap();
    let rows: i64 = conn
        .query_row("SELECT count(*) FROM blob_failures", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 0, "the trigger is back");
}
