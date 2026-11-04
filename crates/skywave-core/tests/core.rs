//! `Core` end to end without a network: actions and engine notifications
//! (through the `Seam`) in, `ViewState` snapshots out, all persisted.

use std::path::Path;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;

use chrono::{TimeZone, Utc};
use skywave_core::{
    Action, Attention, BlobState, BlobStatus, ComposerMode, Core, CoreError, Delivery, DeviceRef,
    Draft, Entry, FailReason, Fingerprint, Link, Outgoing, PairedCredentials, PairingStatus,
    Request, Route, RunWhen, Screen, Seam, SessionView, SettingsView, ShellAlert, ShellEnd,
    ShellRun, ShellRunState, ShellView, Store, StoreError, TimelineItem, ViewState,
};
use skywave_proto::{
    ApprovalId, BlobId, BlobRef, CommandId, CommandKind, CommandResult, ContextId, Decision,
    DeviceId, Event, EventBatch, EventKind, RejectReason, Seq, SessionId, ToolResult,
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
        reason: skywave_core::LinkFailure::Unreachable,
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
            parent: None,
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
        mode: ComposerMode::Agent,
        text: "half a thöught".into(),
        cursor: 6,
        when: RunWhen::Always,
    })
    .unwrap();
    assert_eq!(drain(&requests), vec![], "drafts never reach the engine");
    drop((core, seam));

    let draft = Some(Draft {
        text: "half a thöught".into(),
        cursor: 6,
        when: RunWhen::Always,
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
        mode: ComposerMode::Agent,
        text: "unsent".into(),
        cursor: 0,
        when: RunWhen::Always,
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
        sha256: skywave_proto::ContentHash::of(b""),
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
        sha256: skywave_proto::ContentHash::of(b""),
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
        mode: ComposerMode::Agent,
        text: "unsent".into(),
        cursor: 0,
        when: RunWhen::Always,
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
    assert!(
        !store
            .load()
            .unwrap()
            .drafts
            .keys()
            .any(|(id, _)| *id == gone)
    );
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

/// The messages of `timeline`, with the `Agent` call each belongs to.
fn messages(timeline: &[TimelineItem]) -> Vec<(Option<skywave_proto::CallId>, String)> {
    timeline
        .iter()
        .filter_map(|item| match item {
            TimelineItem::Logged {
                parent,
                entry: Entry::AgentMessage { text },
                ..
            } => Some((*parent, text.clone())),
            _ => None,
        })
        .collect()
}

#[test]
fn a_claude_subagents_steps_are_logged_under_its_call_and_kept_off_the_main_timeline() {
    use skywave_proto::{AgentSettings, AgentWork, Command, CommandId, CommandResult};
    use skywaved::agent::EventMapper;
    use skywaved::agent::claude_code::protocol::Protocol;
    use skywaved::{EventLog, Storage};

    // The daemon: the `subagent` recording through the Claude Code
    // protocol and the event mapper into a log.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    let workdir = root.join("project");
    std::fs::create_dir_all(&workdir).unwrap();
    let mut storage = Storage::open(tmp.path().join("data")).unwrap();
    let log = EventLog::open(&storage.db, vec![root]).unwrap();
    let create = Command {
        id: CommandId::new(),
        kind: CommandKind::CreateSession {
            workdir: workdir.to_str().unwrap().to_owned(),
            conversation: None,
        },
    };
    let CommandResult::Accepted { .. } = log
        .handle(&mut storage.db, DeviceId::new(), &create)
        .unwrap()
    else {
        panic!("session not created");
    };
    let sid = log.read(&storage.db, Seq::ZERO, 1).unwrap().events[0].session_id;
    let mut mapper = EventMapper::new(
        sid,
        ContextId::first_of(sid),
        AgentSettings::default(),
        AgentWork::default(),
    );
    let recording = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../skywaved/tests/fixtures/claude-code/subagent.ndjson"),
    )
    .unwrap();
    let mut protocol = Protocol::new(uuid::Uuid::new_v4(), false);
    let mut events = protocol.start().events;
    let initialize = protocol_initialize_id(&recording);
    for line in recording.lines().filter(|l| !l.trim().is_empty()) {
        let record: serde_json::Value = serde_json::from_str(line).unwrap();
        let mut msg = record["msg"].clone();
        match record["dir"].as_str().unwrap() {
            "in" if msg["type"] == "user" => {
                let text = msg["message"]["content"].as_str().unwrap().to_owned();
                events.extend(protocol.send_message(text).events);
            }
            "out" => {
                // The CLI answers `initialize` under the id the protocol chose.
                if msg["response"]["request_id"] == initialize {
                    msg["response"]["request_id"] = "skywave_1".into();
                }
                events.extend(protocol.on_line(&msg.to_string()).events);
            }
            // The protocol answers the CLI's hook calls itself.
            _ => {}
        }
    }
    for event in events {
        mapper.apply(&log, &mut storage, event).unwrap();
    }
    let batch = log.read(&storage.db, Seq::ZERO, 1000).unwrap();
    let logged: Vec<(Option<skywave_proto::CallId>, String)> = batch
        .events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::AgentMessage { parent, text } => Some((*parent, text.clone())),
            _ => None,
        })
        .collect();
    let agent_call = batch
        .events
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::ToolCall(call) if call.name == "Agent" => call.call,
            _ => None,
        })
        .expect("the Agent call");
    assert!(
        logged.contains(&(Some(agent_call), "PONG".to_owned())),
        "{logged:?}"
    );

    // The client: the log through the core.
    let dir = tempfile::tempdir().unwrap();
    let (core, seam, _requests, _) = paired_core(dir.path());
    seam.apply_events(&batch).unwrap();
    core.dispatch(Action::OpenSession(sid)).unwrap();
    let main = messages(&session_view(&core.view()).timeline);
    // Positive control: the main agent's own text is on the main timeline.
    assert!(
        main.contains(&(None, "The subagent replied: **PONG**".to_owned())),
        "{main:?}"
    );
    assert!(main.iter().all(|(parent, _)| parent.is_none()), "{main:?}");
    assert!(!main.iter().any(|(_, text)| text == "PONG"), "{main:?}");
    // The subagent's own view has it.
    core.dispatch(Action::ViewSubagent {
        session_id: sid,
        call: Some(agent_call),
    })
    .unwrap();
    assert_eq!(
        messages(&session_view(&core.view()).timeline),
        [(Some(agent_call), "PONG".to_owned())]
    );
}

/// The request id of the `initialize` the recording's driver wrote.
fn protocol_initialize_id(recording: &str) -> serde_json::Value {
    recording
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .find(|r| r["dir"] == "in" && r["msg"]["request"]["subtype"] == "initialize")
        .map(|r| r["msg"]["request_id"].clone())
        .unwrap()
}

/// A core over `dir` paired, with one session `sid` in its replica, open.
fn core_with_session(dir: &Path) -> (Core, Seam, Receiver<Request>, SessionId) {
    let (core, seam, requests, _) = paired_core(dir);
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
    (core, seam, requests, sid)
}

/// The shell runs queued in the replica, in send order: (id, command, when).
fn queued_runs(dir: &Path) -> Vec<(skywave_proto::CommandId, String, RunWhen)> {
    Store::open(dir)
        .unwrap()
        .pending_outbox()
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.command.kind {
            CommandKind::RunShell { command, when, .. } => Some((e.command.id, command, when)),
            _ => None,
        })
        .collect()
}

fn run_shell(session_id: SessionId, command: &str, when: RunWhen) -> Action {
    Action::RunShell {
        session_id,
        command: command.into(),
        when,
    }
}

#[test]
fn a_shell_run_is_queued_as_the_head_wrote_it() {
    let dir = tempfile::tempdir().unwrap();
    let (core, _seam, requests, sid) = core_with_session(dir.path());

    // Blank, or for an unknown session: nothing is queued.
    core.dispatch(run_shell(sid, " \n\t", RunWhen::OnSuccess))
        .unwrap();
    core.dispatch(run_shell(SessionId::new(), "ls", RunWhen::Always))
        .unwrap();
    assert_eq!(drain(&requests), vec![]);
    assert!(queued_runs(dir.path()).is_empty());

    // The text and the condition go out verbatim, untrimmed.
    core.dispatch(run_shell(sid, "  make test \n", RunWhen::OnFailure))
        .unwrap();
    assert_eq!(drain(&requests), vec![Request::OutboxChanged]);
    let runs = queued_runs(dir.path());
    assert_eq!(runs.len(), 1);
    assert_eq!(
        (runs[0].1.as_str(), runs[0].2),
        ("  make test \n", RunWhen::OnFailure)
    );
    let stored = Store::open(dir.path()).unwrap().pending_outbox().unwrap();
    assert!(matches!(
        stored[0].command.kind,
        CommandKind::RunShell { session_id, .. } if session_id == sid
    ));

    // A stop is queued as asked for, naming the run.
    core.dispatch(Action::StopShell {
        session_id: sid,
        run: runs[0].0,
    })
    .unwrap();
    let stored = Store::open(dir.path()).unwrap().pending_outbox().unwrap();
    assert!(matches!(
        stored[1].command.kind,
        CommandKind::StopShell { session_id, run } if session_id == sid && run == runs[0].0
    ));
}

#[test]
fn offline_runs_keep_their_order_and_go_back_into_the_shell_draft() {
    let dir = tempfile::tempdir().unwrap();
    let (core, seam, _requests, sid) = core_with_session(dir.path());
    let agent_draft = Draft {
        text: "a message".into(),
        cursor: 2,
        when: RunWhen::Always,
    };
    core.dispatch(Action::SaveDraft {
        session_id: sid,
        mode: ComposerMode::Agent,
        text: agent_draft.text.clone(),
        cursor: agent_draft.cursor,
        when: RunWhen::Always,
    })
    .unwrap();
    for (command, when) in [
        ("cargo build", RunWhen::Always),
        ("cargo test", RunWhen::OnSuccess),
        ("git status", RunWhen::OnFailure),
    ] {
        core.dispatch(run_shell(sid, command, when)).unwrap();
    }
    let runs = queued_runs(dir.path());
    let texts = |runs: &[(skywave_proto::CommandId, String, RunWhen)]| {
        runs.iter().map(|r| r.1.clone()).collect::<Vec<_>>()
    };
    assert_eq!(texts(&runs), ["cargo build", "cargo test", "git status"]);

    core.dispatch(Action::Discard(runs[0].0)).unwrap();
    let left = queued_runs(dir.path());
    assert_eq!(texts(&left), ["cargo test", "git status"]);

    // The shell draft gets the run's text and condition; the agent draft
    // stays as it was.
    core.dispatch(Action::SaveDraft {
        session_id: sid,
        mode: ComposerMode::Shell,
        text: "ls".into(),
        cursor: 2,
        when: RunWhen::Always,
    })
    .unwrap();
    let rev = session_view(&core.view()).draft_rev;
    core.dispatch(Action::EditQueued(left[0].0)).unwrap();
    let shell_draft = Draft {
        text: "cargo test\nls".into(),
        cursor: "cargo test".len(),
        when: RunWhen::OnSuccess,
    };
    let view = core.view();
    let view = session_view(&view);
    assert_eq!(view.shell_draft, Some(shell_draft.clone()));
    assert_eq!(view.draft, Some(agent_draft.clone()));
    assert!(view.draft_rev > rev, "heads refill their composer");
    assert_eq!(texts(&queued_runs(dir.path())), ["git status"]);

    // On disk too.
    drop((core, seam));
    let core = Core::open_detached(dir.path()).unwrap();
    core.dispatch(Action::OpenSession(sid)).unwrap();
    let view = core.view();
    assert_eq!(session_view(&view).shell_draft, Some(shell_draft));
    assert_eq!(session_view(&view).draft, Some(agent_draft));
}

#[test]
fn drafts_per_mode_and_the_composer_mode_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (core, seam, requests, sid) = core_with_session(dir.path());
    assert_eq!(session_view(&core.view()).mode, ComposerMode::Agent);
    let save = |mode, text: &str, when| Action::SaveDraft {
        session_id: sid,
        mode,
        text: text.into(),
        cursor: 1,
        when,
    };
    core.dispatch(save(ComposerMode::Agent, "hello", RunWhen::OnFailure))
        .unwrap();
    core.dispatch(save(ComposerMode::Shell, "ls -l", RunWhen::OnSuccess))
        .unwrap();
    core.dispatch(Action::SetComposerMode {
        session_id: sid,
        mode: ComposerMode::Shell,
    })
    .unwrap();
    assert_eq!(drain(&requests), vec![], "all of it stays on this device");
    drop((core, seam));

    let core = Core::open_detached(dir.path()).unwrap();
    core.dispatch(Action::OpenSession(sid)).unwrap();
    let view = core.view();
    let view = session_view(&view);
    assert_eq!(view.mode, ComposerMode::Shell);
    let draft = |text: &str, when| {
        Some(Draft {
            text: text.into(),
            cursor: 1,
            when,
        })
    };
    assert_eq!(
        view.draft,
        draft("hello", RunWhen::Always),
        "an agent draft runs on no condition"
    );
    assert_eq!(view.shell_draft, draft("ls -l", RunWhen::OnSuccess));

    // A shell draft with a condition picked before typing is kept blank;
    // a blank one with none goes.
    core.dispatch(save(ComposerMode::Shell, "", RunWhen::OnFailure))
        .unwrap();
    assert_eq!(
        session_view(&core.view()).shell_draft,
        Some(Draft {
            text: String::new(),
            cursor: 0,
            when: RunWhen::OnFailure,
        })
    );
    core.dispatch(save(ComposerMode::Shell, " ", RunWhen::Always))
        .unwrap();
    assert_eq!(session_view(&core.view()).shell_draft, None);

    // Back to the agent, kept too.
    core.dispatch(Action::SetComposerMode {
        session_id: sid,
        mode: ComposerMode::Agent,
    })
    .unwrap();
    drop(core);
    let core = Core::open_detached(dir.path()).unwrap();
    core.dispatch(Action::OpenSession(sid)).unwrap();
    assert_eq!(session_view(&core.view()).mode, ComposerMode::Agent);
}

#[test]
fn snippets_of_both_kinds_with_the_same_text_coexist() {
    let dir = tempfile::tempdir().unwrap();
    let (core, seam, _requests, _sid) = core_with_session(dir.path());
    for kind in [
        ComposerMode::Agent,
        ComposerMode::Shell,
        ComposerMode::Shell,
    ] {
        core.dispatch(Action::SaveSnippet {
            name: String::new(),
            text: "make test".into(),
            kind,
        })
        .unwrap();
    }
    drop((core, seam));
    let core = Core::open_detached(dir.path()).unwrap();
    let mut kinds: Vec<_> = core
        .view()
        .snippets
        .iter()
        .map(|s| (s.text.clone(), s.kind == ComposerMode::Shell))
        .collect();
    kinds.sort();
    assert_eq!(
        kinds,
        [
            ("make test".to_owned(), false),
            ("make test".to_owned(), true)
        ],
        "one per kind"
    );
}

/// A paired core with one session `sid` in its replica, open, and a log to
/// feed it shell events with.
struct ShellLog {
    core: Core,
    seam: Seam,
    dir: tempfile::TempDir,
    sid: SessionId,
    me: DeviceId,
    next: u64,
}

impl ShellLog {
    fn new() -> ShellLog {
        let dir = tempfile::tempdir().unwrap();
        let (core, seam, _requests, me) = paired_core(dir.path());
        let sid = SessionId::new();
        let mut log = ShellLog {
            core,
            seam,
            dir,
            sid,
            me,
            next: 1,
        };
        log.batch(vec![(
            0,
            None,
            EventKind::SessionCreated {
                workdir: "/srv/a".into(),
                context_name: "start".into(),
                from: None,
            },
        )]);
        log.core.dispatch(Action::OpenSession(sid)).unwrap();
        log
    }

    /// Delivers `events` as one batch: each at that many seconds past noon,
    /// logged by the command named with it.
    fn batch(&mut self, events: Vec<(i64, Option<CommandId>, EventKind)>) {
        let from = Seq(self.next);
        self.next += events.len() as u64;
        let noon = Utc.with_ymd_and_hms(2026, 10, 29, 12, 0, 0).unwrap();
        let events = events
            .into_iter()
            .map(|(secs, command_id, kind)| Event {
                session_id: self.sid,
                at: noon + chrono::Duration::seconds(secs),
                command_id,
                context: ContextId::first_of(self.sid),
                kind,
            })
            .collect();
        self.seam
            .apply_events(&EventBatch { from, events })
            .unwrap();
    }

    /// Queues a run from this device and returns its id.
    fn run(&self, command: &str) -> CommandId {
        self.core
            .dispatch(run_shell(self.sid, command, RunWhen::Always))
            .unwrap();
        queued_runs(self.dir.path()).last().unwrap().0
    }

    fn shell(&self) -> ShellView {
        session_view(&self.core.view()).shell.clone()
    }

    fn attention(&self) -> Attention {
        self.core.view().attention
    }
}

fn requested(command: &str, by: DeviceId) -> EventKind {
    EventKind::ShellRequested {
        command: command.into(),
        when: RunWhen::Always,
        by,
    }
}

fn started(run: CommandId, cwd: &str) -> EventKind {
    EventKind::ShellStarted {
        run,
        cwd: cwd.into(),
    }
}

/// A finish with no output that started at `started_at` seconds past noon.
fn finished(
    run: CommandId,
    by: DeviceId,
    started_at: Option<i64>,
    command_line: &str,
    end: ShellEnd,
) -> EventKind {
    let noon = Utc.with_ymd_and_hms(2026, 10, 29, 12, 0, 0).unwrap();
    EventKind::ShellFinished {
        run,
        by,
        started_at: started_at.map(|s| noon + chrono::Duration::seconds(s)),
        command_line: command_line.into(),
        end,
        cwd: None,
        preview: String::new(),
        lines: 0,
        output: None,
        truncated: false,
    }
}

fn run_of(shell: &ShellView, run: CommandId) -> &ShellRun {
    shell
        .runs
        .iter()
        .find(|r| r.run == run)
        .expect("the run is shown")
}

#[test]
fn shell_runs_fold_from_the_log_and_this_devices_outbox() {
    let mut log = ShellLog::new();
    let other = DeviceId::new();
    let me = log.me;
    let noon = Utc.with_ymd_and_hms(2026, 10, 29, 12, 0, 0).unwrap();
    let at = |s: i64| noon + chrono::Duration::seconds(s);
    assert_eq!(log.shell(), ShellView::default(), "nothing yet");

    // This device's runs: one logged, one sent without an answer, one
    // refused and one still queued here.
    let logged = log.run("make test");
    let sending = log.run("cargo build");
    let refused = log.run("bad");
    let queued = log.run("ls");
    log.seam.command_sending(logged).unwrap();
    log.seam
        .command_result(logged, &CommandResult::Accepted { seq: Seq(2) })
        .unwrap();
    log.batch(vec![(1, Some(logged), requested("make test", me))]);
    log.seam.command_sending(refused).unwrap();
    log.seam
        .command_result(
            refused,
            &CommandResult::Rejected {
                reason: RejectReason::BadShellCommand,
            },
        )
        .unwrap();
    log.seam.command_sending(sending).unwrap();

    // Another device's run, started, with output coming in.
    let theirs = CommandId::new();
    log.batch(vec![
        (2, Some(theirs), requested("sleep 30", other)),
        (3, None, started(theirs, "/srv/a")),
        (
            4,
            None,
            EventKind::ShellOutput {
                run: theirs,
                tail: "one".into(),
                lines: 1,
                bytes: 4,
            },
        ),
    ]);
    let shell = log.shell();
    let order: Vec<CommandId> = shell.runs.iter().map(|r| r.run).collect();
    assert_eq!(
        order,
        [logged, theirs, sending, refused, queued],
        "logged in log order, then this device's in send order"
    );
    assert_eq!(shell.cwd.as_deref(), Some("/srv/a"));

    let mine = run_of(&shell, logged);
    assert_eq!(mine.state, ShellRunState::Queued);
    assert_eq!(mine.requested_at, Some(at(1)));
    assert_eq!(mine.outgoing, Some(Outgoing::Accepted(Seq(2))));
    assert!(!mine.discardable);
    assert!(mine.by.me);

    let run = run_of(&shell, theirs);
    assert_eq!(run.state, ShellRunState::Running);
    assert_eq!(run.started_at, Some(at(3)));
    assert_eq!((run.tail_or_preview.as_str(), run.lines), ("one", 1));
    assert_eq!(
        (run.by.id, run.by.me, run.outgoing),
        (other, false, None),
        "another device's"
    );
    assert!(!run.stopping);

    let run = run_of(&shell, sending);
    assert_eq!(
        (&run.state, run.outgoing, run.discardable),
        (&ShellRunState::Pending, Some(Outgoing::Sending), false)
    );
    assert!(run.delivery.is_some());
    let run = run_of(&shell, refused);
    assert_eq!(
        (&run.state, run.outgoing, run.discardable),
        (
            &ShellRunState::Pending,
            Some(Outgoing::Failed(FailReason::Rejected(
                RejectReason::BadShellCommand
            ))),
            true
        ),
        "a refused run stays in sight to take back"
    );
    let run = run_of(&shell, queued);
    assert_eq!(
        (&run.state, run.outgoing, run.discardable, run.by.me),
        (&ShellRunState::Pending, Some(Outgoing::Queued), true, true)
    );

    // The tail follows the newest output; another device's stop is logged.
    log.batch(vec![
        (
            5,
            None,
            EventKind::ShellOutput {
                run: theirs,
                tail: "one\ntwo".into(),
                lines: 2,
                bytes: 8,
            },
        ),
        (
            6,
            Some(CommandId::new()),
            EventKind::ShellStopRequested {
                run: theirs,
                by: other,
            },
        ),
    ]);
    let shell = log.shell();
    let run = run_of(&shell, theirs);
    assert_eq!((run.tail_or_preview.as_str(), run.lines), ("one\ntwo", 2));
    assert!(run.stopping, "a logged stop");
    assert!(!run_of(&shell, logged).stopping);

    // This device's stop on its way counts; a refused one does not.
    log.core
        .dispatch(Action::StopShell {
            session_id: log.sid,
            run: logged,
        })
        .unwrap();
    log.core
        .dispatch(Action::StopShell {
            session_id: log.sid,
            run: sending,
        })
        .unwrap();
    let stops: Vec<CommandId> = log
        .seam
        .pending_outbox()
        .unwrap()
        .into_iter()
        .filter(|e| matches!(e.command.kind, CommandKind::StopShell { .. }))
        .map(|e| e.command.id)
        .collect();
    log.seam.command_sending(stops[1]).unwrap();
    log.seam
        .command_result(
            stops[1],
            &CommandResult::Rejected {
                reason: RejectReason::UnknownRun,
            },
        )
        .unwrap();
    let shell = log.shell();
    assert!(run_of(&shell, logged).stopping, "own stop queued");
    assert!(!run_of(&shell, sending).stopping, "own stop refused");

    // Finished: the preview, the blob and the cwd it ended in.
    let blob = BlobRef {
        id: BlobId::new(),
        size: 9000,
        sha256: skywave_proto::ContentHash::of(b""),
    };
    log.batch(vec![(
        40,
        None,
        EventKind::ShellFinished {
            run: theirs,
            by: other,
            started_at: Some(at(3)),
            command_line: "sleep 30".into(),
            end: ShellEnd::Stopped,
            cwd: Some("/srv/b".into()),
            preview: "two".into(),
            lines: 900,
            output: Some(blob.clone()),
            truncated: true,
        },
    )]);
    let shell = log.shell();
    let run = run_of(&shell, theirs);
    assert_eq!(run.state, ShellRunState::Finished(ShellEnd::Stopped));
    assert_eq!(run.finished_at, Some(at(40)));
    assert_eq!((run.tail_or_preview.as_str(), run.lines), ("two", 900));
    assert_eq!(
        run.output.as_ref().map(|b| (b.id, b.state.clone())),
        Some((blob.id, BlobState::NotDownloaded { size: 9000 }))
    );
    assert!(run.truncated);
    assert_eq!(shell.cwd.as_deref(), Some("/srv/b"), "where it ended");

    // A later start says where the next run is.
    log.batch(vec![(41, None, started(logged, "/srv/a"))]);
    assert_eq!(log.shell().cwd.as_deref(), Some("/srv/a"));

    // The shell view is session-wide: narrowing the timeline leaves it.
    let before = log.shell();
    log.core
        .dispatch(Action::ViewContext {
            session_id: log.sid,
            context: Some(ContextId::first_of(log.sid)),
        })
        .unwrap();
    assert_eq!(log.shell(), before);
}

#[test]
fn every_shell_end_shows_as_its_run_finished() {
    let mut log = ShellLog::new();
    let other = DeviceId::new();
    let ends = [
        ShellEnd::Exited { code: 0 },
        ShellEnd::Exited { code: 2 },
        ShellEnd::Signalled { signal: 9 },
        ShellEnd::Stopped,
        ShellEnd::Skipped,
        ShellEnd::Lost,
        ShellEnd::NotStarted {
            reason: "no shell".into(),
        },
    ];
    let mut runs = Vec::new();
    for (i, end) in ends.iter().enumerate() {
        let run = CommandId::new();
        let s = i as i64 * 10;
        // A skipped run never starts; the rest do.
        let start = (*end != ShellEnd::Skipped).then_some(s + 1);
        let mut events = vec![(s, Some(run), requested("x", other))];
        if let Some(start) = start {
            events.push((start, None, started(run, "/srv/a")));
        }
        events.push((s + 2, None, finished(run, other, start, "x", end.clone())));
        log.batch(events);
        runs.push(run);
    }
    let shell = log.shell();
    let states: Vec<ShellRunState> = runs
        .iter()
        .map(|run| run_of(&shell, *run).state.clone())
        .collect();
    let expected: Vec<ShellRunState> = ends.into_iter().map(ShellRunState::Finished).collect();
    assert_eq!(states, expected);
    let skipped = run_of(&shell, runs[4]);
    assert_eq!(skipped.started_at, None, "a skipped run never started");
}

#[test]
fn shell_commands_are_newest_first_without_repeats() {
    let mut log = ShellLog::new();
    let other = DeviceId::new();
    let me = log.me;
    let (a, b, c) = (CommandId::new(), CommandId::new(), CommandId::new());
    log.batch(vec![
        (0, Some(a), requested("ls", other)),
        (1, Some(b), requested("make", me)),
        (2, Some(c), requested("make", other)),
    ]);
    log.run("ls");
    log.run("cargo test");
    assert_eq!(log.shell().commands(), ["cargo test", "ls", "make", "ls"]);
}

#[test]
fn a_long_run_of_this_device_alerts_once_wherever_it_finished() {
    let mut log = ShellLog::new();
    let me = log.me;
    let title = session_view(&log.core.view()).title.clone();
    assert_eq!(log.attention().shell_alerts, 0);

    // Away from the session, catching up; the request and the start come
    // in an earlier batch than the finish.
    log.core.dispatch(Action::Back).unwrap();
    log.seam.set_link(Link::CatchingUp {
        have: Seq(1),
        server: Seq(9),
    });
    let run = CommandId::new();
    log.batch(vec![
        (0, Some(run), requested("make test\nmake lint", me)),
        (1, None, started(run, "/srv/a")),
    ]);
    assert_eq!(log.attention().shell_alerts, 0, "not finished yet");
    log.batch(vec![(
        13,
        None,
        finished(run, me, Some(1), "make test", ShellEnd::Exited { code: 1 }),
    )]);
    let attention = log.attention();
    assert_eq!(attention.shell_alerts, 1, "12 s");
    assert_eq!(
        attention.last_shell,
        Some(ShellAlert {
            session_id: log.sid,
            title,
            command_line: "make test".into(),
            end: ShellEnd::Exited { code: 1 },
        })
    );
    log.seam.set_link(Link::Online { latency_ms: 5 });
    assert_eq!(log.attention().shell_alerts, 1, "once");

    // Too short, another device's, never started: no alert.
    let other = DeviceId::new();
    let (short, theirs, skipped) = (CommandId::new(), CommandId::new(), CommandId::new());
    log.batch(vec![
        (20, Some(short), requested("ls", me)),
        (21, None, started(short, "/srv/a")),
        (
            24,
            None,
            finished(short, me, Some(21), "ls", ShellEnd::Exited { code: 0 }),
        ),
        (30, Some(theirs), requested("sleep 12", other)),
        (31, None, started(theirs, "/srv/a")),
        (
            43,
            None,
            finished(
                theirs,
                other,
                Some(31),
                "sleep 12",
                ShellEnd::Exited { code: 0 },
            ),
        ),
        (50, Some(skipped), requested("deploy", me)),
        (
            90,
            None,
            finished(skipped, me, None, "deploy", ShellEnd::Skipped),
        ),
    ]);
    assert_eq!(log.attention().shell_alerts, 1);

    // On screen in shell mode: no alert. In agent mode it does.
    log.core.dispatch(Action::OpenSession(log.sid)).unwrap();
    log.core
        .dispatch(Action::SetComposerMode {
            session_id: log.sid,
            mode: ComposerMode::Shell,
        })
        .unwrap();
    let watched = CommandId::new();
    log.batch(vec![
        (100, Some(watched), requested("make", me)),
        (101, None, started(watched, "/srv/a")),
        (
            120,
            None,
            finished(watched, me, Some(101), "make", ShellEnd::Exited { code: 0 }),
        ),
    ]);
    assert_eq!(log.attention().shell_alerts, 1, "watched in shell mode");
    log.core
        .dispatch(Action::SetComposerMode {
            session_id: log.sid,
            mode: ComposerMode::Agent,
        })
        .unwrap();
    let unwatched = CommandId::new();
    log.batch(vec![
        (130, Some(unwatched), requested("make", me)),
        (131, None, started(unwatched, "/srv/a")),
        (
            141,
            None,
            finished(
                unwatched,
                me,
                Some(131),
                "make",
                ShellEnd::Exited { code: 0 },
            ),
        ),
    ]);
    assert_eq!(
        log.attention().shell_alerts,
        2,
        "open in agent mode, exactly 10 s"
    );
}

/// What [`replica_to_reset`] stored: the log it applied, its session and
/// blob, and the device it paired as.
struct ToReset {
    dir: tempfile::TempDir,
    log: EventBatch,
    sid: SessionId,
    blob: BlobId,
    device: DeviceId,
}

/// A paired replica with one archived session whose tool output is a
/// downloaded blob, a draft for that session and a snippet; closed.
fn replica_to_reset() -> ToReset {
    let dir = tempfile::tempdir().unwrap();
    let (core, seam, _requests, device) = paired_core(dir.path());
    let sid = SessionId::new();
    let blob = BlobRef {
        id: BlobId::new(),
        size: 3,
        sha256: skywave_proto::ContentHash::of(b""),
    };
    let log = EventBatch {
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
            event(
                sid,
                2,
                EventKind::SessionArchived {
                    archived: true,
                    by: DeviceId::new(),
                },
            ),
        ],
    };
    seam.apply_events(&log).unwrap();
    let file = skywave_core::blob_dir(dir.path()).join(blob.id.0.to_string());
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, "abc").unwrap();
    seam.blob_status(blob.id, BlobStatus::Available { path: file })
        .unwrap();
    core.dispatch(Action::SaveDraft {
        session_id: sid,
        mode: ComposerMode::Agent,
        text: "unsent".into(),
        cursor: 0,
        when: RunWhen::Always,
    })
    .unwrap();
    core.dispatch(Action::SaveSnippet {
        name: String::new(),
        text: "make test".into(),
        kind: ComposerMode::Shell,
    })
    .unwrap();
    drop((core, seam));
    ToReset {
        dir,
        log,
        sid,
        blob: blob.id,
        device,
    }
}

fn sql(dir: &Path, statements: &str) {
    rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE))
        .unwrap()
        .execute_batch(statements)
        .unwrap();
}

fn unreadable_reason(dir: &Path) -> String {
    match Core::open_detached(dir) {
        Err(CoreError::ReplicaUnreadable { reason }) => reason,
        Err(other) => panic!("expected an unreadable replica, got {other}"),
        Ok(_) => panic!("expected an unreadable replica, but it opened"),
    }
}

#[test]
fn a_replica_of_another_protocol_version_is_reset_keeping_what_is_this_devices() {
    let r = replica_to_reset();
    let dir = r.dir.path();
    let blob_file = skywave_core::blob_dir(dir).join(r.blob.0.to_string());
    // Positive control: as written, it opens.
    drop(Core::open_detached(dir).unwrap());

    let other = skywave_core::PROTOCOL_VERSION + 1;
    sql(dir, &format!("UPDATE protocol_stamp SET version = {other}"));
    let reason = unreadable_reason(dir);
    assert!(
        reason.contains(&format!("protocol version {other}")),
        "{reason}"
    );
    assert!(blob_file.exists(), "refused untouched");

    // Not beside an open core.
    sql(
        dir,
        &format!(
            "UPDATE protocol_stamp SET version = {}",
            skywave_core::PROTOCOL_VERSION
        ),
    );
    let open = Core::open_detached(dir).unwrap();
    assert!(matches!(
        Core::reset_replica(dir),
        Err(CoreError::Lock(skywave_core::LockError::InUse { .. }))
    ));
    drop(open);
    sql(dir, &format!("UPDATE protocol_stamp SET version = {other}"));

    Core::reset_replica(dir).unwrap();
    assert!(!blob_file.exists(), "the blob files go");
    let store = Store::open(dir).unwrap();
    let kept = store.load().unwrap();
    assert_eq!(kept.cursor, Seq::ZERO);
    assert!(kept.sessions.is_empty() && kept.blobs.is_empty() && kept.outbox.is_empty());
    assert!(store.session_events(r.sid).unwrap().is_empty());
    assert_eq!(kept.drafts.len(), 1, "drafts stay");
    assert_eq!(kept.snippets.len(), 1, "snippets stay");
    assert_eq!(kept.settings.server_address.as_deref(), Some("srv:7443"));
    drop(store);

    // It opens paired, and syncs the log again from the start.
    let core = Core::open_detached(dir).unwrap();
    let seam = core.seam();
    assert_eq!(seam.identity().unwrap().device_id(), r.device);
    assert_eq!(seam.apply_events(&r.log).unwrap().cursor, Seq(3));
    core.dispatch(Action::Navigate(Route::ArchivedSessions))
        .unwrap();
    let Screen::SessionList(list) = core.view().screen else {
        panic!("expected the session list");
    };
    assert_eq!(list.sessions.len(), 1);
    assert_eq!(list.sessions[0].id, r.sid);
    assert!(list.sessions[0].has_draft, "its draft is back with it");
    assert_eq!(
        seam.blob(r.blob).unwrap().map(|b| b.status),
        Some(BlobStatus::NotDownloaded)
    );
}

#[test]
fn a_stored_row_that_no_longer_decodes_makes_the_replica_unreadable() {
    let r = replica_to_reset();
    let dir = r.dir.path();
    sql(dir, "UPDATE sessions SET state = '\"no_such_state\"'");
    let reason = unreadable_reason(dir);
    assert!(!reason.is_empty());
    Core::reset_replica(dir).unwrap();
    let core = Core::open_detached(dir).unwrap();
    assert_eq!(core.seam().identity().unwrap().device_id(), r.device);
}

#[test]
fn a_setting_this_build_cannot_read_is_dropped_in_place_and_the_rest_kept() {
    let r = replica_to_reset();
    let dir = r.dir.path();
    sql(
        dir,
        "INSERT OR REPLACE INTO settings (key, value) VALUES ('blob_policy', 'sometimes');
         INSERT OR REPLACE INTO settings (key, value) VALUES ('shell_enabled', 'maybe');
         INSERT OR REPLACE INTO settings (key, value) VALUES ('from_a_newer_build', 'x');",
    );
    unreadable_reason(dir);
    Core::reset_replica(dir).unwrap();
    let core = Core::open_detached(dir).unwrap();
    let seam = core.seam();
    let settings = seam.settings().unwrap();
    assert_eq!(settings.server_address.as_deref(), Some("srv:7443"));
    assert_eq!(settings.blob_policy, skywave_core::BlobPolicy::default());
    assert!(!settings.shell_enabled);
    drop((core, seam));
    let store = Store::open(dir).unwrap();
    let kept = store.load().unwrap();
    assert_eq!(kept.drafts.len(), 1, "reset in place: drafts stay");
    assert_eq!(kept.snippets.len(), 1, "reset in place: snippets stay");
    let other: String = rusqlite::Connection::open(dir.join(skywave_core::store::DB_FILE))
        .unwrap()
        .query_row(
            "SELECT value FROM settings WHERE key = 'from_a_newer_build'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(other, "x", "a newer build's key stays");
}

#[test]
fn a_replica_that_cannot_be_reset_in_place_is_created_again_with_its_settings() {
    // A newer schema: nothing of it can be trusted but the plain settings,
    // and of those only what this build reads.
    let r = replica_to_reset();
    let dir = r.dir.path();
    sql(
        dir,
        "INSERT OR REPLACE INTO settings (key, value) VALUES ('blob_policy', 'sometimes');
         PRAGMA user_version = 999;",
    );
    // A new database a crashed reset left behind, and its hot journal.
    std::fs::write(dir.join("core.db.new"), b"half").unwrap();
    std::fs::write(dir.join("core.db.new-journal"), b"half").unwrap();
    unreadable_reason(dir);
    Core::reset_replica(dir).unwrap();
    assert!(!dir.join("core.db.new").exists() && !dir.join("core.db.new-journal").exists());
    let core = Core::open_detached(dir).unwrap();
    let seam = core.seam();
    assert_eq!(seam.identity().unwrap().device_id(), r.device);
    assert_eq!(
        seam.settings().unwrap().server_address.as_deref(),
        Some("srv:7443")
    );
    drop((core, seam));
    assert!(Store::open(dir).unwrap().drafts().unwrap().is_empty());

    // Not a database at all: created again from nothing.
    std::fs::write(dir.join(skywave_core::store::DB_FILE), b"not sqlite").unwrap();
    for wal in ["core.db-wal", "core.db-shm"] {
        let _ = std::fs::remove_file(dir.join(wal));
    }
    unreadable_reason(dir);
    Core::reset_replica(dir).unwrap();
    let core = Core::open_detached(dir).unwrap();
    assert_eq!(core.seam().identity().unwrap().device_id(), r.device);
}

#[test]
fn a_session_with_an_event_that_no_longer_decodes_can_be_deleted() {
    let r = replica_to_reset();
    let dir = r.dir.path();
    // The tool result, which names the blob, no longer decodes.
    sql(
        dir,
        "UPDATE events SET event = '{\"bogus\": 1}' WHERE seq = 2",
    );
    let core = Core::open_detached(dir).unwrap();
    let requests = core.seam().take_requests().unwrap();
    core.dispatch(Action::OpenSession(r.sid)).unwrap_err();
    core.dispatch(Action::Navigate(Route::ArchivedSessions))
        .unwrap();
    core.dispatch(Action::DeleteSessionLocally { session_id: r.sid })
        .unwrap();
    // Its blob is unknown without the event: it stays, until a reset.
    assert_eq!(drain(&requests), vec![Request::BlobsPurged(vec![])]);
    let store = Store::open(dir).unwrap();
    assert!(store.session(r.sid).unwrap().is_none());
    assert!(store.session_events(r.sid).unwrap().is_empty());
    assert!(store.load().unwrap().deleted.contains(&r.sid));
}
