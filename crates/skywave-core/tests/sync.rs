//! The sync engine (T14) against a real `skywaved` — HTTPS API and agent
//! supervisor with the fake agent — in-process on 127.0.0.1 over real TLS.
//! The client reaches the server through the link harness proxy
//! (`skywave-testkit`) on a clean profile, which the tests take down and
//! whose connections they cut; `tests/link.rs` runs it on bad links.
//!
//! Waits are bounded and driven by the client's `ViewState` snapshots or
//! the server log's notifier; nothing relies on a fixed sleep, except the
//! one quiet period that checks a rejected device stays silent.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use chrono::Utc;
use skywave_core::{
    Action, ApprovalStatus, BlobDownload, BoxFuture, Connector, Core, Decision, EngineConfig,
    Entry, FailReason, HttpsTransport, Link, LinkRejection, OutboxStatus, Outgoing, Screen,
    SessionRow, TimelineItem, Transport, TransportError, ViewState,
};
use skywave_proto::{
    BlobId, Command, CommandId, CommandKind, CommandResult, Event, EventBatch, EventKind,
    EventsQuery, HelloResponse, RejectReason, Seq, SessionState,
};
use skywave_testkit::{LinkProfile, LinkProxy, seed};
use skywaved::agent::fake::{FakeAgent, FakeScript, Gate, Step};
use skywaved::devices;
use support::{
    Client, Daemon, SOON, assert_same_log, assert_same_view, config, count, is_reconnecting,
    user_messages,
};

/// A clean link the tests can take down and whose connections they can cut.
async fn proxy(daemon: &Daemon) -> LinkProxy {
    LinkProxy::start(daemon.addr, LinkProfile::clean(), seed()).await
}

// ---- tests ------------------------------------------------------------------

/// A fresh device is `Unpaired`, not connecting to nothing; pairing starts
/// the link, which goes on to `Online`, and it never claims `Unpaired`
/// again.
#[tokio::test(flavor = "multi_thread")]
async fn an_unpaired_device_is_unpaired_until_it_pairs() {
    let agent = FakeAgent::new(FakeScript::new());
    let daemon = Daemon::start(agent.clone()).await;
    let proxy = proxy(&daemon).await;
    let client = Client::open(config());
    assert_eq!(client.core.view().link, Link::Unpaired);
    client.dispatch(Action::RetryNow);
    assert_eq!(client.core.view().link, Link::Unpaired);

    let links = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = links.clone();
    client
        .core
        .subscribe(move |view| sink.lock().unwrap().push(view.link));
    daemon.pair(&client, &proxy.address()).await;

    let links = links.lock().unwrap().clone();
    assert_eq!(links.first(), Some(&Link::Unpaired));
    let paired = links
        .iter()
        .position(|link| *link != Link::Unpaired)
        .unwrap();
    assert!(
        links[paired..].iter().all(|link| *link != Link::Unpaired),
        "{links:?}"
    );
    assert!(
        matches!(
            links[paired],
            Link::Connecting | Link::CatchingUp { .. } | Link::Online { .. }
        ),
        "{links:?}"
    );
    assert!(
        matches!(links.last(), Some(Link::Online { .. })),
        "{links:?}"
    );
}

/// F8: commands made while the server is unreachable stay `Queued`, then
/// drain oldest first once it is back, each exactly once in the log.
#[tokio::test(flavor = "multi_thread")]
async fn offline_queue_drains_exactly_once() {
    let agent = FakeAgent::new(FakeScript::new());
    let daemon = Daemon::start(agent.clone()).await;
    let proxy = proxy(&daemon).await;
    let client = Client::open(config());
    daemon.pair(&client, &proxy.address()).await;
    let session = client.create_session(&daemon).await;

    proxy.down();
    client
        .wait("the link to drop", |_, view| is_reconnecting(&view.link))
        .await;

    let texts = ["first", "second", "third"];
    for text in texts {
        client
            .core
            .dispatch(Action::SendMessage {
                session_id: session,
                text: text.into(),
            })
            .unwrap();
    }
    client
        .core
        .dispatch(Action::CreateSession {
            workdir: daemon.workdir(),
        })
        .unwrap();
    let queued = client.seam.pending_outbox().unwrap();
    assert_eq!(queued.len(), 4);
    assert!(
        queued.iter().all(|e| e.status == OutboxStatus::Queued),
        "nothing is sent while offline: {queued:?}"
    );
    assert_eq!(client.core.view().queued, 4);
    // Give the engine a few failed reconnects with the work queued.
    client
        .wait(
            "a few retries",
            |_, view| matches!(view.link, Link::Reconnecting { attempt, .. } if attempt >= 3),
        )
        .await;
    assert!(user_messages(&daemon.events()).is_empty());

    proxy.up();
    let echoed = |events: &[Event]| {
        count(
            events,
            |k| matches!(k, EventKind::AgentMessage { text } if text.starts_with("echo:")),
        ) == texts.len()
            && events
                .iter()
                .rev()
                .find(|e| e.session_id == session)
                .map(|e| &e.kind)
                == Some(&EventKind::SessionStateChanged(SessionState::Idle))
    };
    let view = client
        .wait(
            "the queue to drain and the replica to catch up",
            |c, view| {
                let events = daemon.events();
                view.queued == 0
                    && echoed(&events)
                    && c.cursor() == Seq(events.len() as u64)
                    && matches!(view.link, Link::Online { .. })
            },
        )
        .await;
    assert_eq!(view.queued, 0);

    let events = daemon.events();
    assert!(client.seam.pending_outbox().unwrap().is_empty());
    let outbox = client.outbox();
    let messages: Vec<(CommandId, String)> = outbox
        .iter()
        .filter_map(|e| match &e.command.kind {
            CommandKind::SendMessage { text, .. } => Some((e.command.id, text.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        user_messages(&events),
        messages,
        "each message exactly once, in the order sent"
    );
    assert_eq!(
        count(&events, |k| matches!(k, EventKind::SessionCreated { .. })),
        2
    );
    for entry in &outbox {
        let OutboxStatus::Done(CommandResult::Accepted { seq }) = entry.status else {
            panic!("expected accepted: {entry:?}");
        };
        assert!(seq.0 >= 1 && seq.0 <= events.len() as u64);
    }
    let delivered: Vec<String> = agent
        .inputs(session)
        .into_iter()
        .filter_map(|input| match input {
            skywaved::agent::fake::FakeInput::Message(text) => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(delivered, texts);
    assert_same_log(&events, &client.local_events());
}

/// F9: the link dies while the agent works — mid-turn, and again and again
/// while catching up in small batches — and the replica ends up equal to
/// the server log.
#[tokio::test(flavor = "multi_thread")]
async fn dropped_link_catches_up_without_gaps() {
    let gate = Gate::new();
    let mut turn: Vec<Step> = (0..30).map(|i| Step::text(format!("early {i}"))).collect();
    turn.push(Step::Wait(gate.clone()));
    turn.extend((0..150).map(|i| Step::text(format!("late {i}"))));
    let agent = FakeAgent::new(FakeScript::new().turn(turn));
    let daemon = Daemon::start(agent).await;
    let proxy = proxy(&daemon).await;
    let client = Client::open(config());
    daemon.pair(&client, &proxy.address()).await;
    let session = client.create_session(&daemon).await;

    client
        .core
        .dispatch(Action::SendMessage {
            session_id: session,
            text: "go".into(),
        })
        .unwrap();
    let early_done =
        |events: &[Event]| count(events, |k| matches!(k, EventKind::AgentMessage { .. })) == 30;
    daemon.wait_log("the early messages", early_done).await;
    client
        .wait("the early messages locally", |c, _| {
            c.cursor() == Seq(daemon.events().len() as u64)
        })
        .await;

    // The link dies while the agent is mid-turn; the agent goes on.
    proxy.down();
    client
        .wait("the link to drop", |_, view| is_reconnecting(&view.link))
        .await;
    gate.open();
    let finished = |events: &[Event]| {
        count(events, |k| matches!(k, EventKind::AgentMessage { .. })) == 180
            && events.last().map(|e| &e.kind)
                == Some(&EventKind::SessionStateChanged(SessionState::Idle))
    };
    daemon.wait_log("the turn to finish", finished).await;
    let head = Seq(daemon.events().len() as u64);
    let offline_cursor = client.cursor();
    assert!(offline_cursor < head);

    // Back up, but cut the connection every few batches of the catch-up.
    proxy.up();
    let mut cuts = 0;
    let mut saw_catching_up = false;
    loop {
        let from = client.cursor();
        let view = client
            .wait("catch-up progress", |c, _| {
                let cursor = c.cursor();
                cursor >= head || cursor.0 >= from.0 + 14
            })
            .await;
        saw_catching_up |= matches!(view.link, Link::CatchingUp { .. });
        if client.cursor() >= head {
            break;
        }
        proxy.cut();
        cuts += 1;
    }
    assert!(cuts >= 3, "cut the link {cuts} times while catching up");
    assert!(saw_catching_up, "the link showed CatchingUp");
    client
        .wait("online", |_, view| matches!(view.link, Link::Online { .. }))
        .await;

    assert_eq!(client.cursor(), head);
    assert_same_log(&daemon.events(), &client.local_events());
}

/// Counts every request made through the wrapped transport.
struct Counting {
    inner: Arc<dyn Transport>,
    calls: Arc<AtomicUsize>,
}

impl Counting {
    fn tick(&self) {
        self.calls.fetch_add(1, Ordering::SeqCst);
    }
}

impl Transport for Counting {
    fn hello(&self) -> BoxFuture<'_, Result<HelloResponse, TransportError>> {
        self.tick();
        self.inner.hello()
    }

    fn send_command<'a>(
        &'a self,
        command: &'a Command,
    ) -> BoxFuture<'a, Result<CommandResult, TransportError>> {
        self.tick();
        self.inner.send_command(command)
    }

    fn events(&self, query: EventsQuery) -> BoxFuture<'_, Result<EventBatch, TransportError>> {
        self.tick();
        self.inner.events(query)
    }

    fn blob(&self, id: BlobId, from: u64) -> BoxFuture<'_, Result<BlobDownload, TransportError>> {
        self.tick();
        self.inner.blob(id, from)
    }
}

/// F3: a revoked device sees `Rejected(Revoked)` and the engine stops
/// talking to the server; re-pairing brings it back and drains the queue.
#[tokio::test(flavor = "multi_thread")]
async fn revoked_device_is_rejected_until_it_re_pairs() {
    let daemon = Daemon::start(FakeAgent::new(FakeScript::new())).await;
    let proxy = proxy(&daemon).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let client = Client::open(EngineConfig {
        connector: Connector::new(move |address, identity, config| {
            let inner = HttpsTransport::new(address, identity, config.clone())?;
            Ok(Arc::new(Counting {
                inner: Arc::new(inner),
                calls: counter.clone(),
            }) as Arc<dyn Transport>)
        }),
        ..config()
    });
    daemon.pair(&client, &proxy.address()).await;
    let device = client.seam.identity().unwrap().device_id();

    devices::revoke(daemon.storage.db.conn(), device, Utc::now()).unwrap();
    client
        .core
        .dispatch(Action::CreateSession {
            workdir: daemon.workdir(),
        })
        .unwrap();
    client
        .wait("Rejected(Revoked)", |_, view| {
            view.link == Link::Rejected(LinkRejection::Revoked)
        })
        .await;

    // More work does not wake the engine; time passing does not either.
    let made = calls.load(Ordering::SeqCst);
    client
        .core
        .dispatch(Action::CreateSession {
            workdir: daemon.workdir(),
        })
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        made,
        "no requests after Rejected"
    );
    assert_eq!(
        client.core.view().link,
        Link::Rejected(LinkRejection::Revoked)
    );
    assert_eq!(client.core.view().queued, 2, "local data stays");
    assert_eq!(
        count(&daemon.events(), |k| matches!(
            k,
            EventKind::SessionCreated { .. }
        )),
        0
    );

    // Re-pairing connects again with the new identity and drains.
    client
        .core
        .dispatch(Action::Navigate(skywave_core::Route::Pairing))
        .unwrap();
    daemon.pair(&client, &proxy.address()).await;
    assert_ne!(client.seam.identity().unwrap().device_id(), device);
    client
        .wait("the queue to drain", |c, view| {
            view.queued == 0 && c.cursor() == Seq(daemon.events().len() as u64)
        })
        .await;
    assert_eq!(
        count(&daemon.events(), |k| matches!(
            k,
            EventKind::SessionCreated { .. }
        )),
        2
    );
}

/// Messages taken back while offline — discarded, or pulled into the draft
/// to edit — never reach the server once the link is back; the rest still
/// drains.
#[tokio::test(flavor = "multi_thread")]
async fn withdrawn_message_never_reaches_the_server() {
    let agent = FakeAgent::new(FakeScript::new());
    let daemon = Daemon::start(agent.clone()).await;
    let proxy = proxy(&daemon).await;
    let client = Client::open(config());
    daemon.pair(&client, &proxy.address()).await;
    let session = client.create_session(&daemon).await;

    proxy.down();
    client
        .wait("the link to drop", |_, view| is_reconnecting(&view.link))
        .await;
    for text in ["keep", "drop", "edit"] {
        client.dispatch(Action::SendMessage {
            session_id: session,
            text: text.into(),
        });
    }
    let ids: Vec<CommandId> = client.outbox()[1..].iter().map(|e| e.command.id).collect();
    let view = client
        .open_session(session, |s| s.timeline.len() == 3)
        .await;
    assert!(
        view.timeline.iter().all(|item| matches!(
            item,
            TimelineItem::Pending {
                discardable: true,
                ..
            }
        )),
        "never sent, so each can be taken back: {:?}",
        view.timeline
    );
    client.dispatch(Action::Discard(ids[1]));
    client.dispatch(Action::EditQueued(ids[2]));
    let view = client
        .open_session(session, |s| s.timeline.len() == 1)
        .await;
    assert_eq!(view.draft.map(|d| d.text).as_deref(), Some("edit"));
    assert_eq!(client.core.view().queued, 1);

    proxy.up();
    client
        .wait("the queue to drain and the echo to arrive", |c, view| {
            let events = daemon.events();
            view.queued == 0
                && count(
                    &events,
                    |k| matches!(k, EventKind::AgentMessage { text } if text.starts_with("echo:")),
                ) == 1
                && c.cursor() == Seq(events.len() as u64)
        })
        .await;
    assert_eq!(
        user_messages(&daemon.events()),
        vec![(ids[0], "keep".to_owned())],
        "only the message left in the outbox went out"
    );
    let delivered: Vec<String> = agent
        .inputs(session)
        .into_iter()
        .filter_map(|input| match input {
            skywaved::agent::fake::FakeInput::Message(text) => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(delivered, ["keep"]);
    let left: Vec<CommandId> = client.outbox()[1..].iter().map(|e| e.command.id).collect();
    assert_eq!(left, vec![ids[0]]);
}

/// Suspending stops the network loops without losing work; resuming syncs
/// what was queued meanwhile.
#[tokio::test(flavor = "multi_thread")]
async fn suspend_pauses_and_resume_syncs() {
    let daemon = Daemon::start(FakeAgent::new(FakeScript::new())).await;
    let proxy = proxy(&daemon).await;
    let client = Client::open(config());
    daemon.pair(&client, &proxy.address()).await;

    client.core.suspend();
    client
        .core
        .dispatch(Action::CreateSession {
            workdir: daemon.workdir(),
        })
        .unwrap();
    // A quiet period: the suspended engine sends nothing.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(client.core.view().queued, 1);
    assert!(daemon.events().is_empty());

    client.core.resume();
    client
        .wait("the session after resume", |_, view| {
            view.queued == 0
                && matches!(&view.screen, Screen::SessionList(list) if list.sessions.len() == 1)
        })
        .await;
    assert_same_log(&daemon.events(), &client.local_events());
}

/// Two devices rename one session while one of them is offline: each shows
/// its own rename at once, then both converge on the last one in log order,
/// and neither reorders the list.
#[tokio::test(flavor = "multi_thread")]
async fn renames_converge_on_the_last_in_log_order() {
    let agent = FakeAgent::new(FakeScript::new());
    let daemon = Daemon::start(agent).await;
    let (proxy_a, proxy_b) = (proxy(&daemon).await, proxy(&daemon).await);
    let (a, b) = (Client::open(config()), Client::open(config()));
    daemon.pair(&a, &proxy_a.address()).await;
    daemon.pair(&b, &proxy_b.address()).await;
    let older = a.create_session(&daemon).await;
    let newer = a.create_session(&daemon).await;
    daemon.settle(&[&a, &b], SOON).await;
    let order = |rows: &[SessionRow]| rows.iter().map(|r| r.id).collect::<Vec<_>>();
    assert_eq!(order(&b.session_list().await), vec![newer, older]);

    // B renames while offline; A renames online, so A's lands first.
    proxy_b.down();
    b.wait("b offline", |_, view| is_reconnecting(&view.link))
        .await;
    b.dispatch(Action::RenameSession {
        session_id: older,
        name: "from b".into(),
    });
    let row = |rows: Vec<SessionRow>| rows.into_iter().find(|r| r.id == older).unwrap();
    let shown = row(b.session_list().await);
    assert_eq!(shown.title, "from b", "shown at once");
    assert_eq!(shown.rename, Some(Outgoing::Queued));
    a.dispatch(Action::RenameSession {
        session_id: older,
        name: "from a".into(),
    });
    let renames =
        |events: &[Event]| count(events, |k| matches!(k, EventKind::SessionRenamed { .. }));
    daemon
        .wait_log("a's rename", |events| renames(events) == 1)
        .await;
    proxy_b.up();
    daemon
        .wait_log("b's rename", |events| renames(events) == 2)
        .await;
    daemon.settle(&[&a, &b], SOON).await;

    for client in [&a, &b] {
        let rows = client.session_list().await;
        assert_eq!(order(&rows), vec![newer, older], "a rename is not activity");
        let renamed = row(rows);
        assert_eq!(renamed.title, "from b");
        assert!(renamed.named);
        assert_eq!(renamed.rename, None);
        assert_eq!(renamed.unread, 0);
    }
    assert_same_view(&a, &b).await;
}

/// One device archives a session online while the other is offline; the
/// offline one archives another session and writes to both. Each shows its
/// own changes at once, then both converge: the messages, logged after the
/// archiving, bring both sessions back, and archiving alone never reorders
/// the list.
#[tokio::test(flavor = "multi_thread")]
async fn archiving_converges_and_a_message_brings_a_session_back() {
    let agent = FakeAgent::new(FakeScript::new());
    let daemon = Daemon::start(agent).await;
    let (proxy_a, proxy_b) = (proxy(&daemon).await, proxy(&daemon).await);
    let (a, b) = (Client::open(config()), Client::open(config()));
    daemon.pair(&a, &proxy_a.address()).await;
    daemon.pair(&b, &proxy_b.address()).await;
    let older = a.create_session(&daemon).await;
    let newer = a.create_session(&daemon).await;
    let kept = a.create_session(&daemon).await;
    daemon.settle(&[&a, &b], SOON).await;
    let ids = |rows: Vec<SessionRow>| rows.iter().map(|r| r.id).collect::<Vec<_>>();
    let archive = |session_id| Action::SetSessionArchived {
        session_id,
        archived: true,
    };
    let archived =
        |events: &[Event]| count(events, |k| matches!(k, EventKind::SessionArchived { .. }));

    // A archives online: both devices move it, and nothing else moves.
    a.dispatch(archive(older));
    assert_eq!(ids(a.archived_list().await), vec![older], "shown at once");
    daemon
        .wait_log("a's archiving", |events| archived(events) == 1)
        .await;
    daemon.settle(&[&a, &b], SOON).await;
    for client in [&a, &b] {
        assert_eq!(ids(client.session_list().await), vec![kept, newer]);
        assert_eq!(ids(client.archived_list().await), vec![older]);
    }

    // B, offline, archives `newer` and then writes to it and to `older`.
    proxy_b.down();
    b.wait("b offline", |_, view| is_reconnecting(&view.link))
        .await;
    b.dispatch(archive(newer));
    assert_eq!(ids(b.archived_list().await), vec![newer, older]);
    for session_id in [newer, older] {
        b.dispatch(Action::SendMessage {
            session_id,
            text: "back to work".into(),
        });
    }
    assert!(
        b.archived_list().await.is_empty(),
        "the messages win locally"
    );
    assert_eq!(ids(b.session_list().await), vec![kept, newer, older]);
    proxy_b.up();
    daemon
        .wait_log("b's commands", |events| {
            archived(events) == 2 && user_messages(events).len() == 2
        })
        .await;
    daemon.settle(&[&a, &b], SOON).await;

    for client in [&a, &b] {
        assert!(client.archived_list().await.is_empty());
        let mut active = ids(client.session_list().await);
        active.sort_by_key(|id| id.0);
        let mut all = vec![older, newer, kept];
        all.sort_by_key(|id| id.0);
        assert_eq!(active, all);
    }
    assert_same_view(&a, &b).await;
}

/// An answer given while the server was down, to an approval the server
/// cancelled meanwhile (its agent was shut down with the daemon): the server
/// refuses it as `ApprovalCancelled`, and the device shows it did not apply,
/// on the settled item, with no way to answer again.
#[tokio::test(flavor = "multi_thread")]
async fn an_answer_to_an_approval_cancelled_while_offline_is_shown_refused() {
    let agent = FakeAgent::new(
        FakeScript::new().turn([Step::approval(
            "req-1",
            "write",
            serde_json::json!({ "path": "a.txt" }),
        )
        .on_approve([Step::tool_result("written")])]),
    );
    let mut daemon = Daemon::start(agent.clone()).await;
    let proxy = proxy(&daemon).await;
    let client = Client::open(config());
    daemon.pair(&client, &proxy.address()).await;
    let session = client.create_session(&daemon).await;
    client.dispatch(Action::SendMessage {
        session_id: session,
        text: "write a.txt".into(),
    });
    let view = client
        .open_session(session, |s| s.pending_approval.is_some())
        .await;
    let approval = view.pending_approval.unwrap().id;

    // The phone answers while the server is down; the answer is queued.
    proxy.down();
    client
        .wait("the link to drop", |_, view| is_reconnecting(&view.link))
        .await;
    client.dispatch(Action::ResolveApproval {
        id: approval,
        decision: Decision::Approve,
        rule: None,
    });
    // Meanwhile the server cancels the approval, as a restart does.
    let db = &mut daemon.storage.db;
    daemon
        .log
        .cancel_approval(db, approval, "interrupted")
        .unwrap()
        .unwrap();

    proxy.up();
    let view = client
        .wait("the answer's result and the cancel", |c, _| {
            c.outbox().iter().any(|e| {
                matches!(e.command.kind, CommandKind::ResolveApproval { .. })
                    && matches!(e.status, OutboxStatus::Done(_))
            }) && c.cursor() >= daemon.head()
        })
        .await;
    let answers = |c: &Client| {
        c.outbox()
            .into_iter()
            .filter(|e| matches!(e.command.kind, CommandKind::ResolveApproval { .. }))
            .collect::<Vec<_>>()
    };
    // Refused as too late, not as answered elsewhere.
    assert_eq!(
        answers(&client)[0].status,
        OutboxStatus::Done(CommandResult::Rejected {
            reason: RejectReason::ApprovalCancelled
        })
    );

    // The prompt is gone; the settled item keeps the answer, refused.
    let Screen::Session(view) = view.screen else {
        panic!("not on the session")
    };
    assert_eq!(view.pending_approval, None);
    let (status, answer) = view
        .timeline
        .iter()
        .find_map(|item| match item {
            TimelineItem::Logged {
                entry:
                    Entry::Approval {
                        id, status, answer, ..
                    },
                ..
            } if *id == approval => Some((status.clone(), answer.clone())),
            _ => None,
        })
        .expect("the approval's item");
    assert_eq!(
        status,
        ApprovalStatus::Cancelled {
            reason: "interrupted".into()
        }
    );
    let answer = answer.expect("this device's answer on the item");
    assert_eq!(answer.decision, Decision::Approve);
    assert_eq!(
        answer.outgoing,
        Outgoing::Failed(FailReason::ApprovalCancelled)
    );
    assert!(!answer.retryable, "a cancelled approval takes no answer");

    // Answering again changes nothing.
    client.dispatch(Action::ResolveApproval {
        id: approval,
        decision: Decision::Deny,
        rule: None,
    });
    assert_eq!(answers(&client).len(), 1);
}

/// An approval that came in while no core of this device ran, pulled by a
/// catch-up that ended before the link was online (a background pull cut
/// short), alerts once the next core is online and caught up; the core
/// after that does not alert for it again.
#[tokio::test(flavor = "multi_thread")]
async fn an_approval_pulled_while_offline_alerts_once_after_a_cold_start() {
    let agent = FakeAgent::new(
        FakeScript::new().turn([Step::approval(
            "req-1",
            "write",
            serde_json::json!({ "path": "a.txt" }),
        )
        .on_approve([Step::tool_result("written")])]),
    );
    let daemon = Daemon::start(agent.clone()).await;
    let (proxy_a, proxy_b) = (proxy(&daemon).await, proxy(&daemon).await);
    let (a, b) = (Client::open(config()), Client::open(config()));
    daemon.pair(&a, &proxy_a.address()).await;
    daemon.pair(&b, &proxy_b.address()).await;
    let dir = a.stop();

    // Another device starts a turn that waits on an approval.
    let session = b.create_session(&daemon).await;
    b.dispatch(Action::SendMessage {
        session_id: session,
        text: "write a.txt".into(),
    });
    daemon
        .wait_log("the approval request", |events| {
            count(events, |k| matches!(k, EventKind::ApprovalRequested { .. })) == 1
        })
        .await;

    // A core without a link pulls the log, then stops: never online.
    let log = daemon.events();
    let dir = std::thread::spawn(move || {
        let core = Core::open_detached(dir.path()).unwrap();
        let seam = core.seam();
        let cursor = seam.cursor().unwrap();
        seam.apply_events(&EventBatch {
            from: cursor.next(),
            events: log[usize::try_from(cursor.0).unwrap()..].to_vec(),
        })
        .unwrap();
        assert_eq!(core.view().attention.approvals.len(), 1);
        assert_eq!(core.view().attention.alerts, 0, "not online: no alert");
        drop(seam);
        core.shutdown(SOON).unwrap();
        dir
    })
    .join()
    .unwrap();

    let online_and_caught_up = |c: &Client, view: &ViewState| {
        matches!(view.link, Link::Online { .. }) && c.cursor() >= daemon.head()
    };
    let a = Client::open_in(dir, config());
    let view = a.wait("a online again", online_and_caught_up).await;
    assert_eq!(view.attention.alerts, 1, "news to this device");
    assert_eq!(view.attention.approvals.len(), 1);
    assert_eq!(view.attention.approvals[0].session_id, session);

    // Alerted on: the next start stays quiet.
    let a = Client::open_in(a.stop(), config());
    let view = a.wait("a online once more", online_and_caught_up).await;
    assert_eq!(view.attention.alerts, 0, "alerted before");
    assert_eq!(view.attention.approvals.len(), 1, "still waiting");
}
