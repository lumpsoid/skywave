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
    Action, ApprovalStatus, BlobDownload, BlobStatus, BoxFuture, Connector, Core, Decision,
    EngineConfig, Entry, FailReason, HttpsTransport, Link, LinkRejection, MAX_SERVER_ERRORS,
    OutboxStatus, Outgoing, Screen, SessionRow, TimelineItem, Transport, TransportError, ViewState,
};
use skywave_proto::{
    BlobId, Command, CommandId, CommandKind, CommandResult, ConversationsResult, Event, EventBatch,
    EventKind, EventsQuery, HelloResponse, RejectReason, Seq, SessionState,
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
            conversation: None,
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
            |k| matches!(k, EventKind::AgentMessage { text, .. } if text.starts_with("echo:")),
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
    fn hello(
        &self,
        since: Option<skywave_proto::RunId>,
    ) -> BoxFuture<'_, Result<HelloResponse, TransportError>> {
        self.tick();
        self.inner.hello(since)
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
        connector: Connector::new(move |address, identity, config, run| {
            let inner = HttpsTransport::new(address, identity, config.clone(), run.clone())?;
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
            conversation: None,
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
            conversation: None,
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

/// The rejection the link comes to show; `Reconnecting` on the way is
/// allowed (a cut drops the live connection first).
async fn rejection(client: &Client) -> LinkRejection {
    let view = client
        .wait("a rejection", |_, view| {
            matches!(view.link, Link::Rejected(_))
        })
        .await;
    match view.link {
        Link::Rejected(rejection) => rejection,
        other => panic!("not rejected: {other:?}"),
    }
}

/// A server whose CA was regenerated (or another server at the address)
/// fails every handshake the same way, so the link stops on the first
/// one with `ServerChanged` instead of reconnecting forever. Retrying
/// asks again, and once the old server is back it connects.
#[tokio::test(flavor = "multi_thread")]
async fn a_server_with_another_ca_stops_the_link_until_re_paired() {
    let daemon = Daemon::start(FakeAgent::new(FakeScript::new())).await;
    let proxy = proxy(&daemon).await;
    let client = Client::open(config());
    daemon.pair(&client, &proxy.address()).await;

    let other = Daemon::start(FakeAgent::new(FakeScript::new())).await;
    assert_ne!(
        other.identity.ca_fingerprint(),
        daemon.identity.ca_fingerprint()
    );
    proxy.retarget(other.addr);
    proxy.cut();
    let rejected = rejection(&client).await;
    assert_eq!(rejected, LinkRejection::ServerChanged);
    assert!(rejected.needs_repair());

    // Still the other CA: the retry is rejected the same way.
    client.dispatch(Action::RetryNow);
    assert_eq!(rejection(&client).await, LinkRejection::ServerChanged);

    proxy.retarget(daemon.addr);
    client.dispatch(Action::RetryNow);
    client
        .wait("online with the old CA", |_, view| {
            matches!(view.link, Link::Online { .. })
        })
        .await;
}

/// A stored identity that cannot be used for TLS (a damaged key file)
/// can never connect: the link stops with `BadIdentity` and asks to
/// re-pair, which brings it back.
#[tokio::test(flavor = "multi_thread")]
async fn a_damaged_identity_stops_the_link_until_re_paired() {
    let daemon = Daemon::start(FakeAgent::new(FakeScript::new())).await;
    let proxy = proxy(&daemon).await;
    let client = Client::open(config());
    daemon.pair(&client, &proxy.address()).await;

    let dir = client.stop();
    std::fs::write(dir.path().join("identity/device_key.pem"), "not a key").unwrap();
    let client = Client::open_in(dir, config());
    let rejected = rejection(&client).await;
    assert_eq!(rejected, LinkRejection::BadIdentity);
    assert!(rejected.needs_repair());
    client.dispatch(Action::RetryNow);
    assert_eq!(
        client.core.view().link,
        Link::Rejected(LinkRejection::BadIdentity),
        "retrying cannot mend the file"
    );

    client.dispatch(Action::Navigate(skywave_core::Route::Pairing));
    daemon.pair(&client, &proxy.address()).await;
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
                    |k| matches!(k, EventKind::AgentMessage { text, .. } if text.starts_with("echo:")),
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
            conversation: None,
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
        Outgoing::Failed(FailReason::Rejected(RejectReason::ApprovalCancelled))
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

/// A context cleared offline leads the message queued after it: the
/// message names the new context, and once the link is back both go out in
/// order and are accepted, the message landing in the new context.
#[tokio::test(flavor = "multi_thread")]
async fn a_context_cleared_offline_takes_the_message_queued_after_it() {
    let agent = FakeAgent::new(FakeScript::new());
    let daemon = Daemon::start(agent).await;
    let proxy = proxy(&daemon).await;
    let client = Client::open(config());
    daemon.pair(&client, &proxy.address()).await;
    let session = client.create_session(&daemon).await;
    client
        .open_session(session, |s| !s.contexts.is_empty())
        .await;

    proxy.down();
    client
        .wait("the link to drop", |_, view| is_reconnecting(&view.link))
        .await;
    client.dispatch(Action::ClearContext {
        session_id: session,
    });
    client.dispatch(Action::SendMessage {
        session_id: session,
        text: "afresh".into(),
    });
    let outbox = client.outbox();
    let (clear, cleared, name) = match &outbox[1].command.kind {
        CommandKind::ClearContext { context, name, .. } => {
            (outbox[1].command.id, *context, name.clone())
        }
        other => panic!("expected the clear, got {other:?}"),
    };
    let message = match &outbox[2].command.kind {
        CommandKind::SendMessage { context, .. } => {
            assert_eq!(*context, cleared, "written for the new context");
            outbox[2].command.id
        }
        other => panic!("expected the message, got {other:?}"),
    };
    let view = client
        .open_session(session, |s| s.timeline.len() == 1)
        .await;
    assert_eq!(view.context, cleared);
    assert_eq!(view.clear, Some(Outgoing::Queued));

    proxy.up();
    let view = client
        .wait("both accepted and pulled", |c, view| {
            let events = daemon.events();
            view.queued == 0
                && c.cursor() == Seq(events.len() as u64)
                && count(&events, |k| matches!(k, EventKind::AgentMessage { .. })) == 1
        })
        .await;
    let Screen::Session(view) = view.screen else {
        panic!("the session stays open");
    };
    let seq_of = |id: CommandId| match client
        .outbox()
        .into_iter()
        .find(|e| e.command.id == id)
        .map(|e| e.status)
    {
        Some(OutboxStatus::Done(CommandResult::Accepted { seq })) => seq,
        other => panic!("expected accepted, got {other:?}"),
    };
    assert!(seq_of(clear) < seq_of(message), "in the order queued");
    let events = daemon.events();
    let logged = &events[(seq_of(message).0 - 1) as usize];
    assert_eq!(logged.context, cleared);
    assert_eq!(
        events[(seq_of(clear).0 - 1) as usize].kind,
        EventKind::ContextStarted {
            by: client.device(),
            name: name.clone(),
        }
    );
    support::assert_exactly_once(&daemon, &[&client]);

    assert_eq!((view.context, view.clear), (cleared, None));
    // The first context had no message: only the new one is offered.
    assert_eq!(
        view.contexts
            .iter()
            .map(|c| (c.name.as_str(), c.messages, c.current))
            .collect::<Vec<_>>(),
        [(name.as_str(), 1, true)]
    );
    assert!(matches!(
        &view.timeline[0],
        TimelineItem::Logged {
            entry: Entry::ContextStarted { name: shown, by },
            context,
            ..
        } if *shown == name && by.me && *context == cleared
    ));
}

/// A message written while another device cleared the context is refused
/// for it: shown failed and discardable, it goes back to the draft, and
/// sent again it reaches the context now current.
#[tokio::test(flavor = "multi_thread")]
async fn a_message_refused_for_a_changed_context_is_edited_and_resent() {
    let agent = FakeAgent::new(FakeScript::new());
    let daemon = Daemon::start(agent).await;
    let (proxy_a, proxy_b) = (proxy(&daemon).await, proxy(&daemon).await);
    let (a, b) = (Client::open(config()), Client::open(config()));
    daemon.pair(&a, &proxy_a.address()).await;
    daemon.pair(&b, &proxy_b.address()).await;
    let session = a.create_session(&daemon).await;
    daemon.settle(&[&a, &b], SOON).await;
    a.open_session(session, |s| !s.contexts.is_empty()).await;

    proxy_a.down();
    a.wait("a offline", |_, view| is_reconnecting(&view.link))
        .await;
    a.dispatch(Action::SendMessage {
        session_id: session,
        text: "written before".into(),
    });
    let stale = a.outbox()[1].command.id;
    b.dispatch(Action::ClearContext {
        session_id: session,
    });
    let started = |events: &[Event]| {
        events.iter().find_map(|e| match e.kind {
            EventKind::ContextStarted { .. } => Some(e.context),
            _ => None,
        })
    };
    daemon
        .wait_log("b's clear", |events| started(events).is_some())
        .await;
    let fresh = started(&daemon.events()).unwrap();

    proxy_a.up();
    let refused = Outgoing::Failed(FailReason::Rejected(RejectReason::ContextChanged));
    let view = a
        .open_session(session, |s| {
            s.context == fresh
                && s.timeline.iter().any(|item| {
                    matches!(item, TimelineItem::Pending { command_id, outgoing, .. }
                        if *command_id == stale && *outgoing == refused)
                })
        })
        .await;
    assert!(view.timeline.iter().any(|item| matches!(
        item,
        TimelineItem::Pending { command_id, discardable: true, .. } if *command_id == stale
    )));

    a.dispatch(Action::EditQueued(stale));
    let view = a.open_session(session, |s| s.draft.is_some()).await;
    let text = view.draft.unwrap().text;
    assert_eq!(text, "written before");
    a.dispatch(Action::SendMessage {
        session_id: session,
        text,
    });
    let resent = a.outbox().last().unwrap().command.id;
    daemon
        .wait_log("the resend", |events| {
            events.iter().any(|e| e.command_id == Some(resent))
        })
        .await;
    let events = daemon.events();
    let logged = events
        .iter()
        .find(|e| e.command_id == Some(resent))
        .unwrap();
    assert_eq!(logged.context, fresh);
    assert!(matches!(logged.kind, EventKind::UserMessage { .. }));
    assert!(
        !events.iter().any(|e| e.command_id == Some(stale)),
        "the refused one never logged"
    );
}

/// Moving the context messages go to: the core clears it first, then
/// moves it under its own name. Both are accepted in that order, the new
/// session (logged by the daemon, right before the move) opens on its
/// own once the move is pulled, and starts with where it came from; the
/// source goes on in the fresh context with the moved one frozen.
#[tokio::test(flavor = "multi_thread")]
async fn moving_the_current_context_opens_it_in_a_new_session() {
    let agent = FakeAgent::new(FakeScript::new());
    let daemon = Daemon::start(agent).await;
    let proxy = proxy(&daemon).await;
    let client = Client::open(config());
    daemon.pair(&client, &proxy.address()).await;
    let session = client.create_session(&daemon).await;
    let start = skywave_proto::ContextId::first_of(session);
    client.dispatch(Action::SendMessage {
        session_id: session,
        text: "hello".into(),
    });
    daemon
        .wait_log("the agent's answer", |events| {
            count(events, |k| matches!(k, EventKind::AgentMessage { .. })) == 1
        })
        .await;
    let view = client
        .open_session(session, |s| s.contexts.iter().any(|c| c.can_move))
        .await;
    assert_eq!(view.context, start);
    let start_name = view.contexts[0].name.clone();

    client.dispatch(Action::MoveContext {
        session_id: session,
        context: start,
    });
    let outbox = client.outbox();
    let (clear, fresh, fresh_name) = match &outbox[outbox.len() - 2].command.kind {
        CommandKind::ClearContext { context, name, .. } => {
            (outbox[outbox.len() - 2].command.id, *context, name.clone())
        }
        other => panic!("expected the clear first, got {other:?}"),
    };
    let moved = outbox.last().unwrap().command.id;
    assert_eq!(
        outbox.last().unwrap().command.kind,
        CommandKind::MoveContext {
            session_id: session,
            context: start,
            name: start_name.clone(),
        },
        "under its own name"
    );

    let view = client
        .wait("the new session open", |c, view| {
            matches!(&view.screen, Screen::Session(s) if s.id != session)
                && view.queued == 0
                && c.cursor() == daemon.head()
        })
        .await;
    let Screen::Session(new) = view.screen else {
        unreachable!()
    };
    let seq_of = |id: CommandId| match client
        .outbox()
        .into_iter()
        .find(|e| e.command.id == id)
        .map(|e| e.status)
    {
        Some(OutboxStatus::Done(CommandResult::Accepted { seq })) => seq,
        other => panic!("expected accepted, got {other:?}"),
    };
    assert!(seq_of(clear) < seq_of(moved), "in the order queued");
    let events = daemon.events();
    assert_eq!(
        events[(seq_of(moved).0 - 1) as usize].kind,
        EventKind::ContextMoved {
            to: new.id,
            by: client.device(),
        }
    );
    support::assert_exactly_once(&daemon, &[&client]);

    let source_title = |rows: &[SessionRow]| {
        rows.iter()
            .find(|r| r.id == session)
            .map(|r| r.title.clone())
            .unwrap()
    };
    let rows = client.session_list().await;
    assert_eq!(
        rows.iter().map(|r| r.id).collect::<Vec<_>>(),
        [new.id, session],
        "the new session first"
    );
    assert!(matches!(
        new.timeline.first(),
        Some(TimelineItem::Logged {
            entry: Entry::MovedFrom { from, name },
            ..
        }) if from.id == session
            && from.title.as_deref() == Some(source_title(&rows).as_str())
            && *name == start_name
    ));

    let view = client
        .open_session(session, |s| s.contexts.iter().any(|c| c.moved_to.is_some()))
        .await;
    assert_eq!((view.context, view.clear), (fresh, None));
    let row = view.contexts.iter().find(|c| c.id == start).unwrap();
    assert_eq!(row.moved_to.as_ref().map(|to| to.id), Some(new.id));
    assert!(!row.can_move && !row.current && row.outgoing.is_none());
    let dividers: Vec<&Entry> = view
        .timeline
        .iter()
        .filter_map(|item| match item {
            TimelineItem::Logged {
                entry: entry @ (Entry::ContextStarted { .. } | Entry::ContextMoved { .. }),
                ..
            } => Some(entry),
            _ => None,
        })
        .collect();
    assert!(
        matches!(
            &dividers[..],
            [
                Entry::ContextStarted { name, by },
                Entry::ContextMoved { name: moved, to, by: mover },
            ] if *name == fresh_name && by.me && *moved == start_name && to.id == new.id && mover.me
        ),
        "{dividers:?}"
    );
}

/// A move of a context another device made current again before it
/// arrived is refused: shown failed on the context's row, discardable,
/// and messages go where the replica says.
#[tokio::test(flavor = "multi_thread")]
async fn a_move_of_a_context_made_current_meanwhile_is_refused_and_discarded() {
    let agent = FakeAgent::new(FakeScript::new());
    let daemon = Daemon::start(agent).await;
    let (proxy_a, proxy_b) = (proxy(&daemon).await, proxy(&daemon).await);
    let (a, b) = (Client::open(config()), Client::open(config()));
    daemon.pair(&a, &proxy_a.address()).await;
    daemon.pair(&b, &proxy_b.address()).await;
    let session = a.create_session(&daemon).await;
    let start = skywave_proto::ContextId::first_of(session);
    a.dispatch(Action::SendMessage {
        session_id: session,
        text: "hello".into(),
    });
    daemon
        .wait_log("the agent's answer", |events| {
            count(events, |k| matches!(k, EventKind::AgentMessage { .. })) == 1
        })
        .await;
    a.dispatch(Action::ClearContext {
        session_id: session,
    });
    let view = a
        .open_session(session, |s| {
            s.clear.is_none() && s.context != start && s.contexts.iter().any(|c| c.can_move)
        })
        .await;
    let fresh = view.context;

    proxy_a.down();
    a.wait("a offline", |_, view| is_reconnecting(&view.link))
        .await;
    a.dispatch(Action::MoveContext {
        session_id: session,
        context: start,
    });
    let moved = a.outbox().pop().unwrap();
    assert!(
        matches!(moved.command.kind, CommandKind::MoveContext { context, .. } if context == start),
        "a context not current moves alone: {moved:?}"
    );
    let moved = moved.command.id;
    daemon.settle(&[&b], SOON).await;
    b.dispatch(Action::SwitchContext {
        session_id: session,
        context: start,
    });
    daemon
        .wait_log("b's switch", |events| {
            events
                .iter()
                .any(|e| matches!(e.kind, EventKind::ContextSwitched { .. }))
        })
        .await;

    proxy_a.up();
    let refused = Outgoing::Failed(FailReason::Rejected(RejectReason::ContextCurrent));
    let view = a
        .open_session(session, |s| {
            s.context == start
                && s.contexts
                    .iter()
                    .any(|c| c.id == start && c.outgoing == Some(refused))
        })
        .await;
    let row = view.contexts.iter().find(|c| c.id == start).unwrap();
    assert_eq!(
        (row.discard_move, row.moved_to.as_ref()),
        (Some(moved), None)
    );
    assert_ne!(view.context, fresh, "the replica's, b's switch");

    a.dispatch(Action::Discard(moved));
    let view = a
        .open_session(session, |s| {
            s.contexts
                .iter()
                .any(|c| c.id == start && c.outgoing.is_none())
        })
        .await;
    assert_eq!(
        view.contexts
            .iter()
            .find(|c| c.id == start)
            .unwrap()
            .discard_move,
        None
    );
    assert!(
        a.outbox().iter().all(|e| e.command.id != moved),
        "taken back"
    );
    assert!(
        !daemon
            .events()
            .iter()
            .any(|e| matches!(e.kind, EventKind::ContextMoved { .. })),
        "never moved"
    );
}

/// The server's `hello` says whether it runs shell commands; the device
/// keeps that, so a cold start without a link shows it too.
#[tokio::test(flavor = "multi_thread")]
async fn the_shell_flag_from_hello_is_kept_for_offline() {
    let shown = |view: &ViewState| match &view.screen {
        Screen::Session(session) => Some(session.shell.enabled),
        _ => None,
    };
    for shell in [true, false] {
        let agent = FakeAgent::new(FakeScript::new());
        let daemon = Daemon::start_with_shell(agent, shell).await;
        let proxy = proxy(&daemon).await;
        let client = Client::open(config());
        daemon.pair(&client, &proxy.address()).await;
        let session = client.create_session(&daemon).await;
        client.dispatch(Action::OpenSession(session));
        let view = client
            .wait("online", |_, view| matches!(view.link, Link::Online { .. }))
            .await;
        assert_eq!(shown(&view), Some(shell), "as the server said");
        let dir = client.stop();

        let offline = std::thread::spawn(move || {
            let core = Core::open_detached(dir.path()).unwrap();
            core.dispatch(Action::OpenSession(session)).unwrap();
            let view = core.view();
            core.shutdown(SOON).unwrap();
            view
        })
        .join()
        .unwrap();
        assert_eq!(offline.link, Link::Connecting, "never online");
        assert_eq!(shown(&offline), Some(shell), "kept over a restart");
    }
}

/// Answers every send of the message `text` with a 503 of its own, never
/// passing it on, and counts them in `sends`; the `n`th such send of
/// `hold` first waits for a permit of its semaphore. Everything else goes
/// to the real server.
struct ServerErrorFor {
    inner: Arc<dyn Transport>,
    text: &'static str,
    sends: Arc<AtomicUsize>,
    hold: Option<(usize, Arc<tokio::sync::Semaphore>)>,
}

impl Transport for ServerErrorFor {
    fn hello(
        &self,
        since: Option<skywave_proto::RunId>,
    ) -> BoxFuture<'_, Result<HelloResponse, TransportError>> {
        self.inner.hello(since)
    }

    fn send_command<'a>(
        &'a self,
        command: &'a Command,
    ) -> BoxFuture<'a, Result<CommandResult, TransportError>> {
        if !matches!(&command.kind, CommandKind::SendMessage { text, .. } if text == self.text) {
            return self.inner.send_command(command);
        }
        Box::pin(async move {
            let n = self.sends.fetch_add(1, Ordering::SeqCst) + 1;
            if let Some((at, gate)) = &self.hold
                && n == *at
            {
                gate.acquire().await.unwrap().forget();
            }
            Err(TransportError::BadStatus(503))
        })
    }

    fn events(&self, query: EventsQuery) -> BoxFuture<'_, Result<EventBatch, TransportError>> {
        self.inner.events(query)
    }

    fn blob(&self, id: BlobId, from: u64) -> BoxFuture<'_, Result<BlobDownload, TransportError>> {
        self.inner.blob(id, from)
    }
}

/// A client whose sends of the message `text` all get a 503
/// ([`ServerErrorFor`]).
fn server_error_for(
    text: &'static str,
    sends: Arc<AtomicUsize>,
    hold: Option<(usize, Arc<tokio::sync::Semaphore>)>,
) -> Client {
    Client::open(EngineConfig {
        connector: Connector::new(move |address, identity, config, run| {
            let inner = HttpsTransport::new(address, identity, config.clone(), run.clone())?;
            Ok(Arc::new(ServerErrorFor {
                inner: Arc::new(inner),
                text,
                sends: sends.clone(),
                hold: hold.clone(),
            }) as Arc<dyn Transport>)
        }),
        ..config()
    })
}

/// The pending item for the command `id` in `session`'s timeline, as
/// (outgoing, discardable).
fn pending(session: &skywave_core::SessionView, id: CommandId) -> Option<(Outgoing, bool)> {
    session.timeline.iter().find_map(|item| match item {
        TimelineItem::Pending {
            command_id,
            outgoing,
            discardable,
            ..
        } if *command_id == id => Some((*outgoing, *discardable)),
        _ => None,
    })
}

/// A message the server answers with a 5xx every time is sent
/// `MAX_SERVER_ERRORS` times, then, as the log never shows it, fails as
/// undeliverable instead of holding back the queue: the message behind it
/// goes out, the failed one is never sent again, and the user can discard
/// it.
#[tokio::test(flavor = "multi_thread")]
async fn a_message_the_server_keeps_failing_becomes_undeliverable_and_the_next_goes_out() {
    let agent = FakeAgent::new(FakeScript::new());
    let daemon = Daemon::start(agent.clone()).await;
    let proxy = proxy(&daemon).await;
    let sends = Arc::new(AtomicUsize::new(0));
    let client = server_error_for("poison", sends.clone(), None);
    daemon.pair(&client, &proxy.address()).await;
    let session = client.create_session(&daemon).await;

    for text in ["poison", "after"] {
        client.dispatch(Action::SendMessage {
            session_id: session,
            text: text.into(),
        });
    }
    let ids: Vec<CommandId> = client.outbox()[1..].iter().map(|e| e.command.id).collect();
    let (poison, after) = (ids[0], ids[1]);

    client
        .wait("the poison to fail and the next to go out", |c, _| {
            let outbox = c.outbox();
            let status = |id| {
                outbox
                    .iter()
                    .find(|e| e.command.id == id)
                    .unwrap()
                    .status
                    .clone()
            };
            status(poison) == OutboxStatus::Undeliverable
                && matches!(
                    status(after),
                    OutboxStatus::Done(CommandResult::Accepted { .. })
                )
        })
        .await;
    assert_eq!(
        sends.load(Ordering::SeqCst),
        MAX_SERVER_ERRORS as usize,
        "sent up to the limit, then settled from the log"
    );
    daemon
        .wait_log("the echo", |events| {
            count(
                events,
                |k| matches!(k, EventKind::AgentMessage { text, .. } if text.starts_with("echo:")),
            ) == 1
        })
        .await;
    assert_eq!(
        user_messages(&daemon.events()),
        vec![(after, "after".to_owned())]
    );

    let view = client
        .open_session(session, |s| pending(s, poison).is_some())
        .await;
    assert_eq!(
        pending(&view, poison),
        Some((Outgoing::Failed(FailReason::Undeliverable), true)),
        "shown failed, and the user may take it back"
    );
    client.dispatch(Action::Discard(poison));
    client
        .open_session(session, |s| pending(s, poison).is_none())
        .await;
    assert!(client.outbox().iter().all(|e| e.command.id != poison));
    // Nothing woke the sender for it again.
    client
        .wait("caught up", |c, _| c.cursor() == daemon.head())
        .await;
    assert_eq!(sends.load(Ordering::SeqCst), MAX_SERVER_ERRORS as usize);
}

/// A server error for one command keeps the connection: the pull goes on
/// and applies another device's events while the command is still being
/// resent, and the link never shows `Reconnecting`.
#[tokio::test(flavor = "multi_thread")]
async fn a_server_error_on_a_send_does_not_stop_the_pull() {
    let agent = FakeAgent::new(FakeScript::new());
    let daemon = Daemon::start(agent).await;
    let (proxy_a, proxy_b) = (proxy(&daemon).await, proxy(&daemon).await);
    let sends = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let a = server_error_for("poison", sends.clone(), Some((2, gate.clone())));
    let b = Client::open(config());
    daemon.pair(&a, &proxy_a.address()).await;
    daemon.pair(&b, &proxy_b.address()).await;
    let session = a.create_session(&daemon).await;
    daemon.settle(&[&a, &b], SOON).await;

    let links = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = links.clone();
    a.core
        .subscribe(move |view| sink.lock().unwrap().push(view.link));
    a.dispatch(Action::SendMessage {
        session_id: session,
        text: "poison".into(),
    });
    let poison = a.outbox().last().unwrap().command.id;
    // The second claim comes after the first send got its 503, on the same
    // connection; that send then waits at the gate.
    a.wait("the resend", |c, _| {
        c.outbox()
            .iter()
            .any(|e| e.command.id == poison && e.tries == 2)
    })
    .await;
    assert_eq!(
        links
            .lock()
            .unwrap()
            .iter()
            .filter(|l| is_reconnecting(l))
            .count(),
        0
    );

    // Another device makes the log grow; a pulls it meanwhile.
    b.create_session(&daemon).await;
    a.wait("a to pull b's session", |c, _| c.cursor() == daemon.head())
        .await;
    let entry = a
        .outbox()
        .into_iter()
        .find(|e| e.command.id == poison)
        .unwrap();
    assert_eq!(entry.status, OutboxStatus::Sending, "still being sent");
    assert_eq!(entry.tries, 2);
    assert!(sends.load(Ordering::SeqCst) <= 2);

    gate.add_permits(1);
    a.wait("the poison to fail", |c, _| {
        c.outbox()
            .iter()
            .any(|e| e.command.id == poison && e.status == OutboxStatus::Undeliverable)
    })
    .await;
    let links = links.lock().unwrap().clone();
    assert!(!links.is_empty());
    assert!(
        links.iter().all(|link| !is_reconnecting(link)),
        "the connection held: {links:?}"
    );
}

// ---- a log that went backwards ----------------------------------------------

/// Where the link stopped because the server's log lost history, if it did.
fn history_lost(link: &Link) -> Option<(Seq, Seq)> {
    match link {
        Link::Rejected(LinkRejection::HistoryLost { have, server }) => Some((*have, *server)),
        _ => None,
    }
}

/// Logs a session the way another device would, straight through the
/// server's command handling, so it needs no connection.
fn create_on_server(daemon: &mut Daemon, device: skywave_core::DeviceId) {
    let command = Command {
        id: CommandId::new(),
        kind: CommandKind::CreateSession {
            workdir: daemon.workdir(),
            conversation: None,
        },
    };
    let result = daemon
        .log
        .handle(&mut daemon.storage.db, device, &command)
        .unwrap();
    assert!(
        matches!(result, CommandResult::Accepted { .. }),
        "{result:?}"
    );
}

/// The server comes back from a backup older than what the device pulled:
/// its head is below the cursor. The link stops on `hello` with
/// `Rejected(HistoryLost)` instead of long-polling past a head that will
/// not come, the replica stays as it was (the only copy of what was lost),
/// and the reset starts it over from the restored log, after which the
/// link syncs as usual.
#[tokio::test(flavor = "multi_thread")]
async fn a_log_restored_from_an_older_backup_stops_the_link_until_the_replica_is_reset() {
    let agent = FakeAgent::new(FakeScript::new());
    let daemon = Daemon::start(agent.clone()).await;
    let proxy = proxy(&daemon).await;
    let client = Client::open(config());
    daemon.pair(&client, &proxy.address()).await;
    let kept = client.create_session(&daemon).await;
    daemon.settle(&[&client], SOON).await;
    let backup = daemon.backup();
    let backed_up = daemon.head();
    let lost = client.create_session(&daemon).await;
    daemon.settle(&[&client], SOON).await;
    let have = client.cursor();
    assert!(have > backed_up, "{have:?} over {backed_up:?}");
    let before = client.local_events();

    let daemon = daemon.restore(backup, &proxy, agent.clone()).await;
    assert_eq!(daemon.head(), backed_up);
    let view = client
        .wait("the link to stop on the lost history", |_, view| {
            history_lost(&view.link).is_some()
        })
        .await;
    assert_eq!(history_lost(&view.link), Some((have, backed_up)));
    assert_eq!(client.local_events(), before, "the replica stays as it was");
    assert!(client.sessions().contains(&lost));
    // Only the reset goes on from here: a retry changes nothing.
    client.dispatch(Action::RetryNow);
    assert!(history_lost(&client.core.view().link).is_some());

    client.dispatch(Action::ResetReplica);
    client
        .wait("the reset replica to sync the restored log", |c, view| {
            matches!(view.link, Link::Online { .. }) && c.cursor() == daemon.head()
        })
        .await;
    assert_same_log(&daemon.events(), &client.local_events());
    let sessions = client.sessions();
    assert!(sessions.contains(&kept), "{sessions:?}");
    assert!(!sessions.contains(&lost), "{sessions:?}");
    // And on from there as usual.
    let next = client.create_session(&daemon).await;
    daemon.settle(&[&client], SOON).await;
    assert!(client.sessions().contains(&next));
    assert_same_log(&daemon.events(), &client.local_events());
}

/// The restored log grows past the device's cursor before the device is
/// back: the head alone no longer gives it away, but the run that started
/// below the cursor does, so nothing of the new history stacks onto the
/// old.
#[tokio::test(flavor = "multi_thread")]
async fn a_restored_log_grown_past_the_cursor_still_stops_the_link() {
    let agent = FakeAgent::new(FakeScript::new());
    let daemon = Daemon::start(agent.clone()).await;
    let proxy = proxy(&daemon).await;
    let client = Client::open(config());
    daemon.pair(&client, &proxy.address()).await;
    client.create_session(&daemon).await;
    daemon.settle(&[&client], SOON).await;
    let backup = daemon.backup();
    let backed_up = daemon.head();
    client.create_session(&daemon).await;
    daemon.settle(&[&client], SOON).await;
    let have = client.cursor();
    let before = client.local_events();

    proxy.down();
    let mut daemon = daemon.restore(backup, &proxy, agent.clone()).await;
    while daemon.head() <= have {
        create_on_server(&mut daemon, client.device());
    }
    let server = daemon.head();
    proxy.up();
    let view = client
        .wait("the link to stop on the lost history", |_, view| {
            history_lost(&view.link).is_some()
        })
        .await;
    assert_eq!(history_lost(&view.link), Some((have, server)));
    assert!(
        server > have && have > backed_up,
        "{backed_up:?} {have:?} {server:?}"
    );
    assert_eq!(client.local_events(), before, "nothing stacked");
}

/// As above, and the server restarted once more before the device is back,
/// so the run it meets started past its cursor. The record of runs still
/// has the run the device last saw, and the one after it started below
/// the cursor.
#[tokio::test(flavor = "multi_thread")]
async fn a_restored_log_grown_and_restarted_again_still_stops_the_link() {
    let agent = FakeAgent::new(FakeScript::new());
    let daemon = Daemon::start(agent.clone()).await;
    let proxy = proxy(&daemon).await;
    let client = Client::open(config());
    daemon.pair(&client, &proxy.address()).await;
    client.create_session(&daemon).await;
    daemon.settle(&[&client], SOON).await;
    let backup = daemon.backup();
    client.create_session(&daemon).await;
    daemon.settle(&[&client], SOON).await;
    let have = client.cursor();
    let before = client.local_events();

    proxy.down();
    let mut daemon = daemon.restore(backup, &proxy, agent.clone()).await;
    while daemon.head() <= have {
        create_on_server(&mut daemon, client.device());
    }
    let restart = daemon.backup();
    let daemon = daemon.restore(restart, &proxy, agent.clone()).await;
    let server = daemon.head();
    assert!(server > have, "the run the device meets starts past it");
    proxy.up();
    let view = client
        .wait("the link to stop on the lost history", |_, view| {
            history_lost(&view.link).is_some()
        })
        .await;
    assert_eq!(history_lost(&view.link), Some((have, server)));
    assert_eq!(client.local_events(), before, "nothing stacked");
}

/// Another log behind the same server, as far as the cursor from the
/// start: only the log id tells, and it stops the link too. The same
/// database with its own id is a plain restart, which carries on
/// (`a_restart_with_the_whole_log_carries_on`).
#[tokio::test(flavor = "multi_thread")]
async fn another_log_behind_the_same_server_stops_the_link() {
    let agent = FakeAgent::new(FakeScript::new());
    let daemon = Daemon::start(agent.clone()).await;
    let proxy = proxy(&daemon).await;
    let client = Client::open(config());
    daemon.pair(&client, &proxy.address()).await;
    client.create_session(&daemon).await;
    daemon.settle(&[&client], SOON).await;
    let have = client.cursor();
    let backup = daemon.backup();
    rusqlite::Connection::open(backup.path().join("server/skywave.db"))
        .unwrap()
        .execute(
            "UPDATE log_identity SET log_id = ?1",
            [skywave_proto::LogId::new().0.to_string()],
        )
        .unwrap();

    let daemon = daemon.restore(backup, &proxy, agent.clone()).await;
    assert_eq!(daemon.head(), have, "as far as the replica");
    let view = client
        .wait("the link to stop on the other log", |_, view| {
            history_lost(&view.link).is_some()
        })
        .await;
    assert_eq!(history_lost(&view.link), Some((have, have)));
}

/// A server restarted with its whole log (a new run that starts at the
/// head the device has) is a reconnect like any other: online again,
/// nothing reset, and the next command goes through.
#[tokio::test(flavor = "multi_thread")]
async fn a_restart_with_the_whole_log_carries_on() {
    let agent = FakeAgent::new(FakeScript::new());
    let daemon = Daemon::start(agent.clone()).await;
    let proxy = proxy(&daemon).await;
    let client = Client::open(config());
    daemon.pair(&client, &proxy.address()).await;
    let first = client.create_session(&daemon).await;
    daemon.settle(&[&client], SOON).await;
    let before = client.local_events();
    let backup = daemon.backup();

    let daemon = daemon.restore(backup, &proxy, agent.clone()).await;
    let second = client.create_session(&daemon).await;
    daemon.settle(&[&client], SOON).await;
    let link = client.core.view().link;
    assert!(matches!(link, Link::Online { .. }), "{link:?}");
    assert_eq!(client.local_events()[..before.len()], before[..]);
    let sessions = client.sessions();
    assert_eq!(sessions.len(), 2, "{sessions:?}");
    assert!(sessions.contains(&first) && sessions.contains(&second));
    assert_same_log(&daemon.events(), &client.local_events());
}

/// A link whose every request opens a fresh connection, and whose
/// long-poll waits as long as the server lets it: the one in flight stays
/// on the server it reached while the next request goes wherever the proxy
/// points.
fn fresh_connections() -> EngineConfig {
    let mut config = config();
    config.transport.max_idle = 0;
    config.wait_secs = 60;
    config
}

/// Queues a session the device creates, without waiting for it, and
/// returns its command.
fn queue_session(client: &Client, daemon: &Daemon) -> CommandId {
    client.dispatch(Action::CreateSession {
        conversation: None,
        workdir: daemon.workdir(),
    });
    client.outbox().last().unwrap().command.id
}

/// The server is rolled back under a connection that never closed (a VM
/// snapshot restored, say): the long-poll still hangs on the old run while
/// a command goes out over a fresh connection to the rolled-back one. That
/// run refuses it, as it names the run the replica is synced with, so the
/// stale log never takes it; the link reconnects, and the connection's
/// `hello` finds the lost history.
#[tokio::test(flavor = "multi_thread")]
async fn a_command_reaching_a_rolled_back_run_is_refused_and_the_link_stops() {
    let agent = FakeAgent::new(FakeScript::new());
    let old = Daemon::start(agent.clone()).await;
    let proxy = proxy(&old).await;
    let client = Client::open(fresh_connections());
    old.pair(&client, &proxy.address()).await;
    client.create_session(&old).await;
    old.settle(&[&client], SOON).await;
    let backup = old.backup();
    let backed_up = old.head();
    client.create_session(&old).await;
    old.settle(&[&client], SOON).await;
    let have = client.cursor();
    let before = client.local_events();

    // The old server runs on, so the long-poll on it neither ends nor fails.
    let rolled_back = Daemon::start_from(backup, agent.clone()).await;
    proxy.retarget(rolled_back.addr);
    let command = queue_session(&client, &rolled_back);
    let view = client
        .wait("the link to stop on the lost history", |_, view| {
            history_lost(&view.link).is_some()
        })
        .await;
    assert_eq!(history_lost(&view.link), Some((have, backed_up)));
    assert_eq!(rolled_back.head(), backed_up, "the stale log took nothing");
    assert!(!rolled_back.command_results().contains_key(&command));
    assert!(!old.command_results().contains_key(&command));
    let entry = client
        .outbox()
        .into_iter()
        .find(|e| e.command.id == command)
        .unwrap();
    // Sent, and refused as meant for another run: not given up on.
    assert_eq!(
        (entry.status, entry.tries),
        (OutboxStatus::Sending, 1),
        "{command:?}"
    );
    assert_eq!(client.local_events(), before, "nothing stacked");
}

/// As above, but the server restarted with its whole log: the refused
/// command is a reconnect like any other, whose `hello` finds the log goes
/// on, and the command is sent again and taken once.
#[tokio::test(flavor = "multi_thread")]
async fn a_command_reaching_a_restarted_run_is_sent_again_after_its_hello() {
    let agent = FakeAgent::new(FakeScript::new());
    let old = Daemon::start(agent.clone()).await;
    let proxy = proxy(&old).await;
    let client = Client::open(fresh_connections());
    old.pair(&client, &proxy.address()).await;
    client.create_session(&old).await;
    old.settle(&[&client], SOON).await;
    let backup = old.backup();

    let restarted = Daemon::start_from(backup, agent.clone()).await;
    proxy.retarget(restarted.addr);
    let command = queue_session(&client, &restarted);
    client
        .wait("the session in the replica", |c, _| {
            c.created_session(command).is_some()
        })
        .await;
    restarted.settle(&[&client], SOON).await;
    let entry = client
        .outbox()
        .into_iter()
        .find(|e| e.command.id == command)
        .unwrap();
    assert_eq!(entry.tries, 2, "refused once, then taken");
    assert!(restarted.command_results().contains_key(&command));
    assert!(!old.command_results().contains_key(&command));
    let link = client.core.view().link;
    assert!(matches!(link, Link::Online { .. }), "{link:?}");
    assert_same_log(&restarted.events(), &client.local_events());
}

/// How each blob download and conversation listing went, in the order
/// answered: `("blob" | "conversations", outcome)`.
type Answers = tokio::sync::watch::Sender<Vec<(&'static str, Result<(), TransportError>)>>;

/// Holds the link's long-polls back on the device, before they leave, while
/// `hold` is set; `held` counts the polls it has held.
#[derive(Default)]
struct PollHold {
    hold: tokio::sync::watch::Sender<bool>,
    held: tokio::sync::watch::Sender<usize>,
}

impl PollHold {
    /// Holds every poll from now on and waits until one is held: from then
    /// on the link has no request in flight anywhere until
    /// [`release`](Self::release), as long as its outbox is empty.
    async fn park(&self) {
        let held = *self.held.borrow();
        let mut seen = self.held.subscribe();
        self.hold.send_replace(true);
        tokio::time::timeout(SOON, seen.wait_for(|n| *n > held))
            .await
            .expect("timed out waiting for a poll to hold")
            .unwrap();
    }

    fn release(&self) {
        self.hold.send_replace(false);
    }
}

/// Passes everything on, noting how every blob download and conversation
/// listing went, and holding long-polls as `polls` says.
struct Answered {
    inner: Arc<dyn Transport>,
    answers: Arc<Answers>,
    polls: Arc<PollHold>,
}

impl Answered {
    fn note<T>(&self, what: &'static str, result: &Result<T, TransportError>) {
        let outcome = result.as_ref().map(|_| ()).map_err(Clone::clone);
        self.answers.send_modify(|all| all.push((what, outcome)));
    }
}

impl Transport for Answered {
    fn hello(
        &self,
        since: Option<skywave_proto::RunId>,
    ) -> BoxFuture<'_, Result<HelloResponse, TransportError>> {
        self.inner.hello(since)
    }

    fn send_command<'a>(
        &'a self,
        command: &'a Command,
    ) -> BoxFuture<'a, Result<CommandResult, TransportError>> {
        self.inner.send_command(command)
    }

    fn events(&self, query: EventsQuery) -> BoxFuture<'_, Result<EventBatch, TransportError>> {
        Box::pin(async move {
            let mut released = self.polls.hold.subscribe();
            if *released.borrow_and_update() {
                self.polls.held.send_modify(|n| *n += 1);
                let _ = released.wait_for(|hold| !*hold).await;
            }
            self.inner.events(query).await
        })
    }

    fn blob(&self, id: BlobId, from: u64) -> BoxFuture<'_, Result<BlobDownload, TransportError>> {
        Box::pin(async move {
            let result = self.inner.blob(id, from).await;
            self.note("blob", &result);
            result
        })
    }

    fn list_conversations(&self) -> BoxFuture<'_, Result<ConversationsResult, TransportError>> {
        Box::pin(async move {
            let result = self.inner.list_conversations().await;
            self.note("conversations", &result);
            result
        })
    }
}

/// Waits until a `what` answer satisfies `answered`.
async fn wait_answer(
    answers: &Answers,
    what: &'static str,
    answered: impl Fn(&Result<(), TransportError>) -> bool,
) {
    let mut seen = answers.subscribe();
    tokio::time::timeout(
        SOON,
        seen.wait_for(|all| all.iter().any(|(w, r)| *w == what && answered(r))),
    )
    .await
    .unwrap_or_else(|_| {
        panic!(
            "timed out waiting for a {what} answer: {:?}",
            answers.borrow()
        )
    })
    .unwrap();
}

/// The blob downloader and the side transport (listings, workdir reads)
/// name the run the link expects, from the one shared value the link sets.
/// Restarted while the link has no request out (its long-poll held on the
/// device), the new run refuses a blob download and a conversation listing
/// that name the old run; once the released poll, refused the same way,
/// has the link meet the new run, the blob, retried, comes down, and a
/// listing goes through.
#[tokio::test(flavor = "multi_thread")]
async fn blobs_and_listings_follow_the_run_the_link_expects() {
    let output = "a tool output line\n".repeat(20_000);
    let agent = FakeAgent::new(FakeScript::new().turn([
        Step::tool_call("bash", serde_json::json!({ "cmd": "cat big" })),
        Step::tool_result(output.clone()),
    ]));
    let old = Daemon::start(agent.clone()).await;
    let proxy = proxy(&old).await;
    let answers = Arc::new(Answers::new(Vec::new()));
    let polls = Arc::new(PollHold::default());
    let (noted, holding) = (answers.clone(), polls.clone());
    // Fresh connections: no request reaches the old server over a
    // connection pooled before the restart.
    let mut engine = config();
    engine.transport.max_idle = 0;
    let client = Client::open(EngineConfig {
        connector: Connector::new(move |address, identity, config, run| {
            let inner = HttpsTransport::new(address, identity, config.clone(), run.clone())?;
            Ok(Arc::new(Answered {
                inner: Arc::new(inner),
                answers: noted.clone(),
                polls: holding.clone(),
            }) as Arc<dyn Transport>)
        }),
        ..engine
    });
    old.pair(&client, &proxy.address()).await;
    let session = client.create_session(&old).await;
    let idle = |events: &[Event]| {
        count(events, |kind| {
            matches!(kind, EventKind::SessionStateChanged(SessionState::Idle))
        })
    };
    let idle_before = idle(&old.events());
    client.dispatch(Action::SendMessage {
        session_id: session,
        text: "go".into(),
    });
    old.wait_log("the turn to end", |events| idle(events) > idle_before)
        .await;
    old.settle(&[&client], SOON).await;
    let blobs = client.seam.blobs().unwrap();
    assert_eq!(blobs.len(), 1, "{blobs:?}");
    let id = blobs[0].id;
    client.session_list().await;
    assert!(answers.borrow().is_empty(), "nothing asked yet");

    // The link parked with nothing out (the outbox is empty, the poll held
    // before it leaves), so it still expects the old run whatever the proxy
    // points at. Then a clean restart with the whole log and its blobs.
    polls.park().await;
    assert_eq!(client.core.view().queued, 0);
    let backup = old.backup();
    let blob_files = backup.path().join("server/blobs");
    std::fs::create_dir_all(&blob_files).unwrap();
    for file in std::fs::read_dir(old.tmp.path().join("server/blobs")).unwrap() {
        let file = file.unwrap();
        std::fs::copy(file.path(), blob_files.join(file.file_name())).unwrap();
    }
    let restarted = Daemon::start_from(backup, agent.clone()).await;
    proxy.retarget(restarted.addr);

    client.dispatch(Action::ListConversations);
    client.dispatch(Action::DownloadBlob(id));
    let run_changed = |r: &Result<(), TransportError>| r == &Err(TransportError::RunChanged);
    wait_answer(&answers, "conversations", run_changed).await;
    wait_answer(&answers, "blob", run_changed).await;
    let refused = answers.borrow().clone();
    assert!(
        refused.iter().all(|(_, r)| run_changed(r)),
        "nothing reached the new run naming the old one: {refused:?}"
    );

    // The released poll reaches the new run naming the old one, is
    // refused, and the next connection's `hello` has the link meet the new
    // run, which every transport then names.
    polls.release();
    let command = queue_session(&client, &restarted);
    client
        .wait("the session in the replica", |c, _| {
            c.created_session(command).is_some()
        })
        .await;
    let view = client
        .wait("the blob downloaded", |c, _| {
            matches!(
                c.seam.blob(id).unwrap().unwrap().status,
                BlobStatus::Available { .. }
            )
        })
        .await;
    assert!(matches!(view.link, Link::Online { .. }), "{view:?}");
    let BlobStatus::Available { path } = client.seam.blob(id).unwrap().unwrap().status else {
        unreachable!()
    };
    assert_eq!(std::fs::read(path).unwrap(), output.as_bytes());
    client.dispatch(Action::ListConversations);
    wait_answer(&answers, "conversations", Result::is_ok).await;
    let all = answers.borrow().clone();
    assert_eq!(
        all.iter().rev().find(|(w, _)| *w == "blob"),
        Some(&("blob", Ok(()))),
        "{all:?}"
    );
}
