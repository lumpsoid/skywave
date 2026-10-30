//! `Core` end to end without a network: actions and engine notifications
//! (through the `Seam`) in, `ViewState` snapshots out, all persisted.

use std::path::Path;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;

use chrono::{TimeZone, Utc};
use skywave_core::{
    Action, BlobState, BlobStatus, Core, CoreError, Delivery, Draft, Entry, Link, Outgoing,
    PairedCredentials, PairingStatus, Request, Route, Screen, Seam, SessionView, Store, StoreError,
    TimelineItem, ViewState,
};
use skywave_proto::{
    BlobId, BlobRef, CommandKind, CommandResult, DeviceId, Event, EventBatch, EventKind, Seq,
    SessionId, ToolResult,
};

fn assert_owned_snapshot<T: Clone + Send + Sync + 'static>() {}

#[test]
fn view_state_is_owned_clone_and_send() {
    assert_owned_snapshot::<ViewState>();
    assert_owned_snapshot::<Core>();
    assert_owned_snapshot::<Seam>();
}

/// Every snapshot a subscriber received.
#[derive(Clone, Default)]
struct Views(Arc<Mutex<Vec<ViewState>>>);

impl Views {
    fn subscribe(core: &Core) -> Views {
        let views = Views::default();
        let sink = views.clone();
        core.subscribe(move |view| sink.0.lock().unwrap().push(view));
        views
    }

    fn count(&self) -> usize {
        self.0.lock().unwrap().len()
    }

    fn last(&self) -> ViewState {
        self.0.lock().unwrap().last().cloned().expect("a snapshot")
    }
}

fn credentials() -> PairedCredentials {
    PairedCredentials {
        device_id: DeviceId::new(),
        device_key_pem: "KEY".into(),
        device_cert_pem: "CERT".into(),
        ca_cert_pem: "CA".into(),
    }
}

fn drain(requests: &Receiver<Request>) -> Vec<Request> {
    let mut out = Vec::new();
    loop {
        match requests.try_recv() {
            Ok(request) => out.push(request),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => return out,
        }
    }
}

/// A core over `dir` paired through the seam, with its request channel.
fn paired_core(dir: &Path) -> (Core, Seam, Receiver<Request>, DeviceId) {
    let core = Core::open_detached(dir).unwrap();
    let seam = core.seam();
    let requests = seam.take_requests().unwrap();
    core.dispatch(Action::Pair {
        address: "srv:7443".into(),
        code: "123456".into(),
        fingerprint: "ab:cd".into(),
    })
    .unwrap();
    let creds = credentials();
    let device = creds.device_id;
    seam.pairing_succeeded("srv:7443", creds).unwrap();
    drain(&requests);
    (core, seam, requests, device)
}

fn event(session_id: SessionId, minute: u32, kind: EventKind) -> Event {
    Event {
        session_id,
        at: Utc.with_ymd_and_hms(2026, 10, 29, 11, minute, 0).unwrap(),
        kind,
    }
}

fn session_view(view: &ViewState) -> &SessionView {
    match &view.screen {
        Screen::Session(session) => session,
        other => panic!("expected the session screen, got {other:?}"),
    }
}

#[test]
fn pairing_runs_through_the_seam_and_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let core = Core::open_detached(dir.path()).unwrap();
    let views = Views::subscribe(&core);
    assert_eq!(views.count(), 1, "current snapshot right away");
    assert!(matches!(views.last().screen, Screen::Pairing(_)));

    let seam = core.seam();
    let requests = seam.take_requests().unwrap();
    assert!(seam.take_requests().is_none(), "one receiver");
    assert!(seam.identity().is_none());

    let pair = Action::Pair {
        address: "srv:7443".into(),
        code: "000000".into(),
        fingerprint: "ab:cd".into(),
    };
    core.dispatch(pair.clone()).unwrap();
    assert_eq!(
        drain(&requests),
        vec![Request::Pair {
            address: "srv:7443".into(),
            code: "000000".into(),
            fingerprint: "ab:cd".into(),
        }]
    );
    let pairing_status = |view: ViewState| match view.screen {
        Screen::Pairing(pairing) => pairing.status,
        other => panic!("expected pairing, got {other:?}"),
    };
    assert_eq!(
        pairing_status(views.last()),
        PairingStatus::InProgress {
            address: "srv:7443".into()
        }
    );

    seam.pairing_failed("wrong code");
    assert_eq!(
        pairing_status(views.last()),
        PairingStatus::Failed {
            address: "srv:7443".into(),
            reason: "wrong code".into(),
        }
    );

    core.dispatch(pair).unwrap();
    let creds = credentials();
    let device = creds.device_id;
    seam.pairing_succeeded("srv:7443", creds).unwrap();
    assert_eq!(drain(&requests).last(), Some(&Request::Connect));
    let view = views.last();
    assert_eq!(view.link, Link::Connecting);
    assert!(matches!(view.screen, Screen::SessionList(_)));
    assert_eq!(seam.identity().unwrap().device_id(), device);
    drop((core, seam));

    // Cold start: paired, address remembered, straight to the session list.
    let core = Core::open_detached(dir.path()).unwrap();
    assert!(matches!(core.view().screen, Screen::SessionList(_)));
    let seam = core.seam();
    assert_eq!(seam.identity().unwrap().device_id(), device);
    assert_eq!(
        seam.settings().unwrap().server_address.as_deref(),
        Some("srv:7443")
    );
}

#[test]
fn offline_send_is_persisted_first_and_appears_once() {
    let dir = tempfile::tempdir().unwrap();
    let (core, seam, requests, device) = paired_core(dir.path());
    let views = Views::subscribe(&core);
    let sid = SessionId::new();
    seam.apply_events(&EventBatch {
        from: Seq(1),
        events: vec![event(
            sid,
            0,
            EventKind::SessionCreated {
                workdir: "/srv/a".into(),
            },
        )],
    })
    .unwrap();
    core.dispatch(Action::OpenSession(sid)).unwrap();
    seam.set_link(Link::Reconnecting {
        attempt: 1,
        next_in: std::time::Duration::from_secs(2),
        offline_since: std::time::Instant::now(),
    });

    core.dispatch(Action::SendMessage {
        session_id: sid,
        text: "fix the build".into(),
    })
    .unwrap();
    assert_eq!(drain(&requests), vec![Request::OutboxChanged]);
    // By the time the engine hears of it, it is on disk.
    let on_disk = Store::open(dir.path()).unwrap().pending_outbox().unwrap();
    assert_eq!(on_disk.len(), 1);
    let command = on_disk[0].command.clone();
    assert_eq!(
        command.kind,
        CommandKind::SendMessage {
            session_id: sid,
            text: "fix the build".into()
        }
    );
    let view = views.last();
    assert_eq!(view.queued, 1);
    let timeline = &session_view(&view).timeline;
    let queued = delivery(&timeline[0]);
    assert_eq!((queued.tries, queued.first_sent_at), (0, None));
    assert_eq!(
        *timeline,
        vec![TimelineItem::Pending {
            command_id: command.id,
            text: "fix the build".into(),
            outgoing: Outgoing::Queued,
            discardable: true,
            delivery: Some(queued),
        }]
    );

    // Reconnect: the sender drains the outbox.
    seam.set_link(Link::Online { latency_ms: 80 });
    let pending = seam.pending_outbox().unwrap();
    assert_eq!(pending.len(), 1);
    assert!(seam.command_sending(command.id).unwrap());
    assert!(matches!(
        session_view(&views.last()).timeline[0],
        TimelineItem::Pending {
            outgoing: Outgoing::Sending,
            ..
        }
    ));
    seam.command_result(command.id, &CommandResult::Accepted { seq: Seq(2) })
        .unwrap();
    seam.apply_events(&EventBatch {
        from: Seq(2),
        events: vec![event(
            sid,
            1,
            EventKind::UserMessage {
                command_id: command.id,
                device_id: device,
                text: "fix the build".into(),
            },
        )],
    })
    .unwrap();
    let live = views.last();
    assert_eq!(live.queued, 0);
    let timeline = &session_view(&live).timeline;
    // Sent once, answered after; all by this device's clock.
    let done = delivery(&timeline[0]);
    let (Some(sent), Some(answered)) = (done.first_sent_at, done.answered_at) else {
        panic!("sent and answered: {done:?}");
    };
    assert_eq!(done.tries, 1);
    assert!(done.queued_at <= sent && sent <= answered);
    assert_eq!(
        *timeline,
        vec![TimelineItem::Logged {
            seq: Seq(2),
            at: event(sid, 1, EventKind::TurnInterrupted).at,
            entry: Entry::UserMessage {
                command_id: command.id,
                text: "fix the build".into(),
                from: device,
                own: true,
                outgoing: Some(Outgoing::Accepted(Seq(2))),
                delivery: Some(done),
            },
        }]
    );
    drop((core, seam));

    // Cold start renders the same thing from the replica alone.
    let core = Core::open_detached(dir.path()).unwrap();
    core.dispatch(Action::OpenSession(sid)).unwrap();
    let cold = core.view();
    assert_eq!(cold.screen, live.screen);
    assert_eq!(cold.link, Link::Connecting);
}

/// An own message's delivery, pending or logged.
fn delivery(item: &TimelineItem) -> Delivery {
    match item {
        TimelineItem::Pending {
            delivery: Some(delivery),
            ..
        }
        | TimelineItem::Logged {
            entry:
                Entry::UserMessage {
                    delivery: Some(delivery),
                    ..
                },
            ..
        } => *delivery,
        other => panic!("expected an own message with its delivery, got {other:?}"),
    }
}

#[test]
fn draft_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (core, seam, requests, _) = paired_core(dir.path());
    let sid = SessionId::new();
    seam.apply_events(&EventBatch {
        from: Seq(1),
        events: vec![event(
            sid,
            0,
            EventKind::SessionCreated {
                workdir: "/srv/a".into(),
            },
        )],
    })
    .unwrap();
    core.dispatch(Action::SaveDraft {
        session_id: sid,
        text: "half a thöught".into(),
        cursor: 6,
    })
    .unwrap();
    assert_eq!(drain(&requests), vec![], "drafts never reach the engine");
    drop((core, seam));

    let draft = Some(Draft {
        text: "half a thöught".into(),
        cursor: 6,
    });
    let core = Core::open_detached(dir.path()).unwrap();
    let Screen::SessionList(list) = core.view().screen else {
        panic!("expected the session list");
    };
    assert!(list.sessions[0].has_draft);
    core.dispatch(Action::OpenSession(sid)).unwrap();
    assert_eq!(session_view(&core.view()).draft, draft);

    // A send that cannot be stored keeps the draft, in the view and on disk.
    let send = Action::SendMessage {
        session_id: sid,
        text: "half a thöught".into(),
    };
    let conn = rusqlite::Connection::open(dir.path().join(skywave_core::store::DB_FILE)).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER full BEFORE INSERT ON outbox BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
    )
    .unwrap();
    assert!(core.dispatch(send.clone()).is_err());
    assert_eq!(session_view(&core.view()).draft, draft);
    assert_eq!(Store::open(dir.path()).unwrap().drafts().unwrap().len(), 1);

    conn.execute_batch("DROP TRIGGER full").unwrap();
    core.dispatch(send).unwrap();
    assert_eq!(session_view(&core.view()).draft, None);
    drop(core);
    let core = Core::open_detached(dir.path()).unwrap();
    core.dispatch(Action::OpenSession(sid)).unwrap();
    assert_eq!(
        session_view(&core.view()).draft,
        None,
        "sent with the message"
    );
}

#[test]
fn pairing_with_another_server_drops_the_old_sessions_from_the_view() {
    let dir = tempfile::tempdir().unwrap();
    let (core, seam, requests, _) = paired_core(dir.path());
    let created = |id, minute| {
        event(
            id,
            minute,
            EventKind::SessionCreated {
                workdir: "/srv/a".into(),
            },
        )
    };
    let old = SessionId::new();
    seam.apply_events(&EventBatch {
        from: Seq(1),
        events: vec![
            created(old, 0),
            event(old, 1, EventKind::AgentMessage { text: "hi".into() }),
        ],
    })
    .unwrap();
    core.dispatch(Action::SaveDraft {
        session_id: old,
        text: "unsent".into(),
        cursor: 0,
    })
    .unwrap();
    let rows = |core: &Core| match core.view().screen {
        Screen::SessionList(list) => list
            .sessions
            .iter()
            .map(|row| (row.id, row.has_draft, row.unread))
            .collect::<Vec<_>>(),
        other => panic!("expected the session list, got {other:?}"),
    };
    assert_eq!(rows(&core), vec![(old, true, 1)]);
    core.dispatch(Action::OpenSession(old)).unwrap();

    // Another CA is another server.
    let creds = PairedCredentials {
        ca_cert_pem: "ANOTHER CA".into(),
        ..credentials()
    };
    seam.pairing_succeeded("other:7443", creds).unwrap();
    assert_eq!(drain(&requests), vec![Request::Connect]);
    assert_eq!(rows(&core), vec![], "nothing of the old server shows");

    // The new server's log starts at seq 1; only its sessions show.
    let new = SessionId::new();
    seam.apply_events(&EventBatch {
        from: Seq(1),
        events: vec![created(new, 2)],
    })
    .unwrap();
    assert_eq!(rows(&core), vec![(new, false, 0)]);
    drop((core, seam));
    let core = Core::open_detached(dir.path()).unwrap();
    assert_eq!(rows(&core), vec![(new, false, 0)], "and after a restart");
}

#[test]
fn gap_is_refused_and_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (core, seam, _requests, _) = paired_core(dir.path());
    let before = core.view();
    let err = seam
        .apply_events(&EventBatch {
            from: Seq(5),
            events: vec![event(
                SessionId::new(),
                0,
                EventKind::SessionCreated {
                    workdir: "/srv/x".into(),
                },
            )],
        })
        .unwrap_err();
    assert!(matches!(
        err,
        CoreError::Store(StoreError::Gap {
            cursor: Seq::ZERO,
            from: Seq(5)
        })
    ));
    assert_eq!(core.view(), before);
    assert_eq!(seam.cursor().unwrap(), Seq::ZERO);
}

#[test]
fn blob_progress_is_persisted_and_shown() {
    let dir = tempfile::tempdir().unwrap();
    let (core, seam, requests, _) = paired_core(dir.path());
    let sid = SessionId::new();
    let blob = BlobRef {
        id: BlobId::new(),
        size: 9000,
    };
    seam.apply_events(&EventBatch {
        from: Seq(1),
        events: vec![
            event(
                sid,
                0,
                EventKind::SessionCreated {
                    workdir: "/srv/a".into(),
                },
            ),
            event(
                sid,
                1,
                EventKind::ToolResult(ToolResult {
                    call: None,
                    is_error: false,
                    status: None,
                    preview: "log".into(),
                    lines: 1,
                    output: Some(blob.clone()),
                }),
            ),
        ],
    })
    .unwrap();
    core.dispatch(Action::OpenSession(sid)).unwrap();
    core.dispatch(Action::DownloadBlob(blob.id)).unwrap();
    assert_eq!(drain(&requests), vec![Request::DownloadBlob(blob.id)]);

    let output_state = |core: &Core| match &session_view(&core.view()).timeline[0] {
        TimelineItem::Logged {
            entry: Entry::ToolResult {
                output: Some(blob), ..
            },
            ..
        } => blob.state.clone(),
        other => panic!("expected the tool result, got {other:?}"),
    };
    assert_eq!(output_state(&core), BlobState::Requested { size: 9000 });
    let partial = seam.data_dir().join("blobs").join("partial");
    seam.blob_status(
        blob.id,
        BlobStatus::Downloading {
            got: 1000,
            path: partial.clone(),
        },
    )
    .unwrap();
    assert_eq!(
        output_state(&core),
        BlobState::Downloading {
            got: 1000,
            size: 9000,
            // A detached core has no link.
            paused: true,
        }
    );
    assert!(
        seam.blob_status(BlobId::new(), BlobStatus::NotDownloaded)
            .is_err()
    );
    drop((core, seam));

    let core = Core::open_detached(dir.path()).unwrap();
    core.dispatch(Action::OpenSession(sid)).unwrap();
    assert_eq!(
        output_state(&core),
        BlobState::Downloading {
            got: 1000,
            size: 9000,
            // A detached core has no link.
            paused: true,
        }
    );
}

#[test]
fn deleted_session_stays_gone_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (core, seam, requests, _) = paired_core(dir.path());
    let (gone, kept) = (SessionId::new(), SessionId::new());
    let blob = BlobRef {
        id: BlobId::new(),
        size: 3,
    };
    let created = |id, minute| {
        event(
            id,
            minute,
            EventKind::SessionCreated {
                workdir: "/srv/a".into(),
            },
        )
    };
    let archived = EventKind::SessionArchived {
        archived: true,
        by: DeviceId::new(),
    };
    seam.apply_events(&EventBatch {
        from: Seq(1),
        events: vec![
            created(gone, 0),
            created(kept, 1),
            event(
                gone,
                2,
                EventKind::ToolResult(ToolResult {
                    call: None,
                    is_error: false,
                    status: None,
                    preview: "log".into(),
                    lines: 1,
                    output: Some(blob.clone()),
                }),
            ),
            event(gone, 3, archived.clone()),
        ],
    })
    .unwrap();
    // Downloaded, and a stale part file besides.
    let blobs = skywave_core::blob_dir(dir.path());
    std::fs::create_dir_all(&blobs).unwrap();
    let (done, part) = (
        blobs.join(blob.id.0.to_string()),
        blobs.join(format!("{}.part", blob.id.0)),
    );
    std::fs::write(&done, "abc").unwrap();
    std::fs::write(&part, "a").unwrap();
    seam.blob_status(blob.id, BlobStatus::Available { path: done.clone() })
        .unwrap();
    core.dispatch(Action::SaveDraft {
        session_id: gone,
        text: "unsent".into(),
        cursor: 0,
    })
    .unwrap();

    core.dispatch(Action::Navigate(Route::ArchivedSessions))
        .unwrap();
    core.dispatch(Action::DeleteSessionLocally { session_id: gone })
        .unwrap();
    assert_eq!(drain(&requests), vec![Request::BlobsPurged(vec![blob.id])]);
    assert!(!done.exists() && !part.exists(), "its files are gone");
    let listed = |core: &Core| match core.view().screen {
        Screen::SessionList(list) => (list.sessions.len(), list.other_count),
        other => panic!("expected the session list, got {other:?}"),
    };
    assert_eq!(listed(&core), (0, 1));

    // The log goes on: the session is used elsewhere, which would bring it
    // back. The cursor moves; the session stays gone.
    let again = |seam: &Seam, from| {
        seam.apply_events(&EventBatch {
            from: Seq(from),
            events: vec![event(gone, 10, archived.clone())],
        })
        .unwrap()
    };
    assert_eq!(again(&seam, 5).cursor, Seq(5));
    assert_eq!(listed(&core), (0, 1));
    drop((core, seam));

    let core = Core::open_detached(dir.path()).unwrap();
    let seam = core.seam();
    assert_eq!(again(&seam, 6).cursor, Seq(6));
    core.dispatch(Action::Navigate(Route::ArchivedSessions))
        .unwrap();
    assert_eq!(listed(&core), (0, 1));
    core.dispatch(Action::OpenSession(gone)).unwrap();
    assert_eq!(listed(&core), (0, 1), "it does not open");
    let store = Store::open(dir.path()).unwrap();
    assert!(store.session_events(gone).unwrap().is_empty());
    assert_eq!(store.blob(blob.id).unwrap(), None);
    assert!(!store.load().unwrap().drafts.contains_key(&gone));
}

#[test]
fn callbacks_may_dispatch_and_every_subscriber_ends_on_the_latest_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let (core, seam, _requests, _) = paired_core(dir.path());

    // A callback that reacts to a snapshot by dispatching must not deadlock.
    let reacting = Views::default();
    let sink = reacting.clone();
    let handle = core.clone();
    core.subscribe(move |view| {
        let on_settings = matches!(view.screen, Screen::Settings(_));
        sink.0.lock().unwrap().push(view);
        if !on_settings {
            handle.dispatch(Action::Navigate(Route::Settings)).unwrap();
        }
    });
    assert!(matches!(core.view().screen, Screen::Settings(_)));
    assert!(matches!(reacting.last().screen, Screen::Settings(_)));

    let watching = Views::subscribe(&core);
    let threads: Vec<_> = (0..4)
        .map(|t| {
            let seam = seam.clone();
            thread::spawn(move || {
                for i in 0..50 {
                    seam.set_link(Link::Online {
                        latency_ms: t * 1000 + i,
                    });
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    let latest = core.view();
    assert_eq!(watching.last(), latest);
    assert_eq!(reacting.last(), latest);

    // An unsubscribed callback hears nothing more.
    let gone = Views::default();
    let sink = gone.clone();
    let id = core.subscribe(move |view| sink.0.lock().unwrap().push(view));
    core.unsubscribe(id);
    seam.set_link(Link::Connecting);
    assert_eq!(gone.count(), 1);
}
