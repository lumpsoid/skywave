//! `Core` end to end without a network: actions and engine notifications
//! (through the `Seam`) in, `ViewState` snapshots out, all persisted.

use std::path::Path;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;

use chrono::{TimeZone, Utc};
use skywave_core::{
    Action, BlobState, BlobStatus, Core, CoreError, Delivery, DeviceRef, Draft, Entry, Fingerprint,
    Link, Outgoing, PairedCredentials, PairingStatus, Request, Route, Screen, Seam, SessionView,
    SettingsView, Store, StoreError, TimelineItem, ViewState,
};
use skywave_proto::{
    ApprovalId, BlobId, BlobRef, CommandKind, CommandResult, ContextId, Decision, DeviceId, Event,
    EventBatch, EventKind, Seq, SessionId, ToolResult,
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
        command_id: kind.logged_by_command().then(skywave_proto::CommandId::new),
        context: ContextId::first_of(session_id),
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
    assert_eq!(views.last().link, Link::Unpaired);

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
    // Nothing to retry while unpaired, even mid-pairing.
    core.dispatch(Action::RetryNow).unwrap();
    assert!(drain(&requests).is_empty());
    assert_eq!(views.last().link, Link::Unpaired);
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

fn settings_view(core: &Core) -> SettingsView {
    core.dispatch(Action::Navigate(Route::Settings)).unwrap();
    match core.view().screen {
        Screen::Settings(view) => view,
        other => panic!("expected settings, got {other:?}"),
    }
}

#[test]
fn a_new_server_address_reconnects_keeps_the_pairing_and_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (core, seam, requests, device) = paired_core(dir.path());
    core.dispatch(Action::CreateSession {
        workdir: "/srv/a".into(),
        conversation: None,
    })
    .unwrap();
    drain(&requests);

    core.dispatch(Action::SetServerAddress {
        address: " 10.0.0.7:7443 ".into(),
    })
    .unwrap();
    assert_eq!(drain(&requests), vec![Request::Connect]);
    let settings = settings_view(&core);
    assert_eq!(settings.server_address.as_deref(), Some("10.0.0.7:7443"));
    assert_eq!(settings.device_id, Some(device));
    drop((core, seam));

    let core = Core::open_detached(dir.path()).unwrap();
    let settings = settings_view(&core);
    assert_eq!(settings.server_address.as_deref(), Some("10.0.0.7:7443"));
    assert_eq!(settings.device_id, Some(device), "no re-pairing");
    core.dispatch(Action::Navigate(Route::SessionList)).unwrap();
    match core.view().screen {
        Screen::SessionList(list) => assert_eq!(list.creating.len(), 1, "the outbox is kept"),
        other => panic!("expected the session list, got {other:?}"),
    }
}

#[test]
fn settings_show_the_pinned_server_fingerprint_also_after_restart() {
    let key = rcgen::KeyPair::generate().unwrap();
    let ca = rcgen::CertificateParams::new(Vec::new())
        .unwrap()
        .self_signed(&key)
        .unwrap();
    let expected = Fingerprint::of_der(ca.der());

    let dir = tempfile::tempdir().unwrap();
    let core = Core::open_detached(dir.path()).unwrap();
    let seam = core.seam();
    let credentials = PairedCredentials {
        ca_cert_pem: ca.pem(),
        ..credentials()
    };
    seam.pairing_succeeded("srv:7443", credentials).unwrap();
    assert_eq!(settings_view(&core).server_fingerprint, Some(expected));
    drop((core, seam));

    let core = Core::open_detached(dir.path()).unwrap();
    let settings = settings_view(&core);
    assert_eq!(settings.server_fingerprint, Some(expected));
    assert_eq!(
        settings.server_fingerprint.unwrap().to_string(),
        expected.to_string(),
        "the colon-separated form skywaved prints"
    );
}

#[test]
fn a_pinned_pem_without_a_certificate_has_no_fingerprint() {
    let dir = tempfile::tempdir().unwrap();
    let (core, _seam, _requests, _device) = paired_core(dir.path());
    assert_eq!(settings_view(&core).server_fingerprint, None);
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
                context_name: "start".into(),
                from: None,
            },
        )],
    })
    .unwrap();
    core.dispatch(Action::OpenSession(sid)).unwrap();
    seam.set_link(Link::Reconnecting {
        attempt: 1,
        retry_at: chrono::Utc::now() + chrono::TimeDelta::seconds(2),
        offline_since: chrono::Utc::now(),
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
            context: ContextId::first_of(sid),
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
            context: ContextId::first_of(sid),
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
        events: vec![Event {
            command_id: Some(command.id),
            ..event(
                sid,
                1,
                EventKind::UserMessage {
                    device_id: device,
                    text: "fix the build".into(),
                },
            )
        }],
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
            context: ContextId::first_of(sid),
            entry: Entry::UserMessage {
                command_id: command.id,
                text: "fix the build".into(),
                from: DeviceRef {
                    id: device,
                    name: None,
                    me: true,
                },
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
                context_name: "start".into(),
                from: None,
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
                context_name: "start".into(),
                from: None,
            },
        )
    };
    let old = SessionId::new();
    seam.apply_events(&EventBatch {
        from: Seq(1),
        events: vec![
            created(old, 0),
            event(
                old,
                1,
                EventKind::AgentMessage {
                    parent: None,
                    text: "hi".into(),
                },
            ),
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
                    context_name: "start".into(),
                    from: None,
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
                    context_name: "start".into(),
                    from: None,
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
fn an_approval_no_core_alerted_on_alerts_once_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (core, seam, _requests, _) = paired_core(dir.path());
    let sid = SessionId::new();
    let (news, answered, resolved) = (ApprovalId::new(), ApprovalId::new(), ApprovalId::new());
    let requested = |minute, id| {
        event(
            sid,
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
    };
    // Pulled while never online: nobody was alerted.
    seam.apply_events(&EventBatch {
        from: Seq(1),
        events: vec![
            event(
                sid,
                0,
                EventKind::SessionCreated {
                    workdir: "/srv/a".into(),
                    context_name: "start".into(),
                    from: None,
                },
            ),
            requested(1, news),
            requested(2, answered),
            requested(3, resolved),
        ],
    })
    .unwrap();
    core.dispatch(Action::ResolveApproval {
        id: answered,
        decision: Decision::Deny,
        rule: None,
    })
    .unwrap();
    assert_eq!(core.view().attention.alerts, 0);
    drop((core, seam));

    let ids = |view: &ViewState| {
        view.attention
            .approvals
            .iter()
            .map(|a| a.id)
            .collect::<Vec<_>>()
    };
    let core = Core::open_detached(dir.path()).unwrap();
    let seam = core.seam();
    assert_eq!(core.view().attention.alerts, 0, "nothing before online");
    seam.set_link(Link::CatchingUp {
        have: Seq(4),
        server: Seq(5),
    });
    seam.apply_events(&EventBatch {
        from: Seq(5),
        events: vec![event(
            sid,
            4,
            EventKind::ApprovalResolved {
                id: resolved,
                decision: Decision::Approve,
                by: DeviceId::new(),
                rule: None,
            },
        )],
    })
    .unwrap();
    assert_eq!(core.view().attention.alerts, 0, "nor while catching up");
    seam.set_link(Link::Online { latency_ms: 5 });
    let view = core.view();
    assert_eq!(view.attention.alerts, 1, "one alert, for the news only");
    assert_eq!(ids(&view), vec![news]);
    drop((core, seam));

    // Alerted on: a later start stays quiet.
    let core = Core::open_detached(dir.path()).unwrap();
    core.seam().set_link(Link::Online { latency_ms: 5 });
    let view = core.view();
    assert_eq!(view.attention.alerts, 0);
    assert_eq!(ids(&view), vec![news]);
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
                context_name: "start".into(),
                from: None,
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

/// Publishes a link change when dropped.
struct PublishOnDrop(Seam);

impl Drop for PublishOnDrop {
    fn drop(&mut self) {
        self.0.set_link(Link::Online { latency_ms: 7 });
    }
}

#[test]
fn a_snapshot_published_during_unsubscribe_still_reaches_the_others() {
    let dir = tempfile::tempdir().unwrap();
    let core = Core::open_detached(dir.path()).unwrap();
    let watching = Views::subscribe(&core);

    // `unsubscribe` drops the removed callback while it holds the
    // subscriber list, so this publish finds the list taken — exactly the
    // race with a publish from another thread, made deterministic. It
    // leaves its snapshot to the holder, which must deliver it.
    let guard = PublishOnDrop(core.seam());
    let id = core.subscribe(move |_| {
        let _ = &guard;
    });
    core.unsubscribe(id);

    assert_eq!(core.view().link, Link::Online { latency_ms: 7 });
    assert_eq!(watching.last(), core.view());
}

/// Between the ack of this device's switch and the pull of its event, the
/// replica still names the old context: a message sent then goes to the
/// switch's context all the same, or the server would refuse it.
#[test]
fn a_message_after_an_acked_switch_not_yet_pulled_goes_to_the_switch_context() {
    let dir = tempfile::tempdir().unwrap();
    let (core, seam, _requests, me) = paired_core(dir.path());
    let views = Views::subscribe(&core);
    let sid = SessionId::new();
    let first = ContextId::first_of(sid);
    let later = ContextId::new();
    let elsewhere = DeviceId::new();
    seam.apply_events(&EventBatch {
        from: Seq(1),
        events: vec![
            event(
                sid,
                0,
                EventKind::SessionCreated {
                    workdir: "/srv/a".into(),
                    context_name: "start".into(),
                    from: None,
                },
            ),
            Event {
                context: later,
                ..event(
                    sid,
                    1,
                    EventKind::ContextStarted {
                        by: elsewhere,
                        name: "velvet walrus".into(),
                    },
                )
            },
        ],
    })
    .unwrap();
    core.dispatch(Action::OpenSession(sid)).unwrap();
    let sent_to = |text: &str| {
        core.dispatch(Action::SendMessage {
            session_id: sid,
            text: text.into(),
        })
        .unwrap();
        let outbox = seam.pending_outbox().unwrap();
        match &outbox.last().unwrap().command.kind {
            CommandKind::SendMessage {
                context, text: t, ..
            } if t == text => *context,
            other => panic!("expected the message, got {other:?}"),
        }
    };
    // The control: before the switch, messages go to the newer context.
    assert_eq!(session_view(&views.last()).context, later);
    assert_eq!(sent_to("before"), later);

    core.dispatch(Action::SwitchContext {
        session_id: sid,
        context: first,
    })
    .unwrap();
    let switch = seam
        .pending_outbox()
        .unwrap()
        .into_iter()
        .find(|e| matches!(e.command.kind, CommandKind::SwitchContext { .. }))
        .unwrap()
        .command
        .id;
    assert!(seam.command_sending(switch).unwrap());
    seam.command_result(switch, &CommandResult::Accepted { seq: Seq(4) })
        .unwrap();
    // Acked at seq 4, the replica at seq 2: the window.
    assert_eq!(seam.cursor().unwrap(), Seq(2));
    let view = session_view(&views.last()).clone();
    assert_eq!(view.context, first);
    assert_eq!(view.switch, Some(Outgoing::Accepted(Seq(4))));
    assert_eq!(sent_to("in the window"), first);

    // Pulled: the replica says the same, and the switch is settled.
    seam.apply_events(&EventBatch {
        from: Seq(3),
        events: vec![
            Event {
                command_id: Some(skywave_proto::CommandId::new()),
                context: later,
                ..event(
                    sid,
                    2,
                    EventKind::UserMessage {
                        device_id: elsewhere,
                        text: "x".into(),
                    },
                )
            },
            Event {
                command_id: Some(switch),
                context: first,
                ..event(sid, 3, EventKind::ContextSwitched { by: me })
            },
        ],
    })
    .unwrap();
    let view = session_view(&views.last()).clone();
    assert_eq!((view.context, view.switch), (first, None));
    assert_eq!(sent_to("after"), first);
}

/// Moving the current context through the core: the clear and the move go
/// out in that order, and the new session opens, loaded from the store,
/// only once the move's own event is pulled, the pull split between the
/// new session and the move.
#[test]
fn a_moved_context_opens_its_new_session_once_the_move_is_pulled() {
    let dir = tempfile::tempdir().unwrap();
    let (core, seam, _requests, me) = paired_core(dir.path());
    let views = Views::subscribe(&core);
    let sid = SessionId::new();
    let start = ContextId::first_of(sid);
    seam.apply_events(&EventBatch {
        from: Seq(1),
        events: vec![
            event(
                sid,
                0,
                EventKind::SessionCreated {
                    workdir: "/srv/a".into(),
                    context_name: "start".into(),
                    from: None,
                },
            ),
            event(
                sid,
                1,
                EventKind::UserMessage {
                    device_id: DeviceId::new(),
                    text: "fix it".into(),
                },
            ),
            event(
                sid,
                2,
                EventKind::SessionStateChanged(skywave_proto::SessionState::AgentWorking),
            ),
        ],
    })
    .unwrap();
    core.dispatch(Action::OpenSession(sid)).unwrap();
    core.dispatch(Action::MoveContext {
        session_id: sid,
        context: start,
    })
    .unwrap();
    let outbox = seam.pending_outbox().unwrap();
    let [clear, moved] = &outbox[..] else {
        panic!("expected a clear and a move: {outbox:?}");
    };
    let CommandKind::ClearContext {
        context: fresh,
        name,
        ..
    } = clear.command.kind.clone()
    else {
        panic!("expected the clear first, got {clear:?}");
    };
    assert_eq!(
        moved.command.kind,
        CommandKind::MoveContext {
            session_id: sid,
            context: start,
            name: "start".into(),
        }
    );
    let (clear, moved) = (clear.command.id, moved.command.id);
    for (command, seq) in [(clear, Seq(4)), (moved, Seq(6))] {
        assert!(seam.command_sending(command).unwrap());
        seam.command_result(command, &CommandResult::Accepted { seq })
            .unwrap();
    }

    let new = SessionId::new();
    seam.apply_events(&EventBatch {
        from: Seq(4),
        events: vec![
            Event {
                command_id: Some(clear),
                context: fresh,
                ..event(sid, 3, EventKind::ContextStarted { by: me, name })
            },
            Event {
                command_id: None,
                ..event(
                    new,
                    4,
                    EventKind::SessionCreated {
                        workdir: "/srv/a".into(),
                        context_name: "start".into(),
                        from: Some(skywave_proto::SessionOrigin::Context(
                            skywave_proto::ContextRef {
                                session: sid,
                                context: start,
                            },
                        )),
                    },
                )
            },
        ],
    })
    .unwrap();
    assert_eq!(
        session_view(&views.last()).id,
        sid,
        "the move is not in yet"
    );

    seam.apply_events(&EventBatch {
        from: Seq(6),
        events: vec![Event {
            command_id: Some(moved),
            context: start,
            ..event(sid, 5, EventKind::ContextMoved { to: new, by: me })
        }],
    })
    .unwrap();
    let view = session_view(&views.last()).clone();
    assert_eq!(view.id, new);
    assert!(
        matches!(
            view.timeline.first(),
            Some(TimelineItem::Logged {
                entry: Entry::MovedFrom { from, name },
                ..
            }) if from.id == sid && from.title.as_deref() == Some("fix it") && name == "start"
        ),
        "{:?}",
        view.timeline
    );
}
