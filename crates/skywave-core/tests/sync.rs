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
    Action, BlobDownload, BoxFuture, Connector, EngineConfig, HttpsTransport, Link, LinkRejection,
    OutboxStatus, Outgoing, Screen, SessionRow, TimelineItem, Transport, TransportError,
};
use skywave_proto::{
    BlobId, Command, CommandId, CommandKind, CommandResult, Event, EventBatch, EventKind,
    EventsQuery, HelloResponse, Seq, SessionState,
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
