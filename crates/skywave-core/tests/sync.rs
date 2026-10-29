//! The sync engine (T14) against a real `skywaved` — HTTPS API and agent
//! supervisor with the fake agent — in-process on 127.0.0.1 over real TLS.
//! The client reaches the server through a small TCP proxy the tests can
//! take down and whose connections they can cut.
//!
//! Waits are bounded and driven by the client's `ViewState` snapshots or
//! the server log's notifier; nothing relies on a fixed sleep, except the
//! one quiet period that checks a rejected device stays silent.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use skywave_core::{
    Action, Backoff, BlobDownload, BoxFuture, Connector, Core, EngineConfig, HttpsTransport,
    Jitter, Link, LinkRejection, OutboxStatus, Screen, Seam, Store, Transport, TransportConfig,
    TransportError, ViewState,
};
use skywave_proto::{
    BlobId, Command, CommandId, CommandKind, CommandResult, Event, EventBatch, EventKind,
    EventsQuery, HelloResponse, Seq, SessionId, SessionState,
};
use skywaved::agent::fake::{FakeAgent, FakeScript, Gate, Step};
use skywaved::identity::ServerIdentity;
use skywaved::pairing;
use skywaved::server::Server;
use skywaved::{EventLog, Storage, Supervisor, SupervisorConfig, SupervisorHandle, devices};
use tempfile::TempDir;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, watch};
use tokio::task::{AbortHandle, JoinHandle};

/// Upper bound for any wait; a passing test never gets near it.
const SOON: Duration = Duration::from_secs(20);

// ---- server -----------------------------------------------------------------

/// `skywaved` in-process: API server plus supervisor with the fake agent.
struct Daemon {
    tmp: TempDir,
    storage: Storage,
    identity: Arc<ServerIdentity>,
    log: EventLog,
    addr: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<skywaved::Result<()>>,
    _supervisor: SupervisorHandle,
}

impl Daemon {
    async fn start(agent: FakeAgent) -> Daemon {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("root/project")).unwrap();
        let data = tmp.path().join("server");
        let storage = Storage::open(&data).unwrap();
        let identity = Arc::new(
            ServerIdentity::bootstrap(&storage.dir, &["localhost".into(), "127.0.0.1".into()])
                .unwrap(),
        );
        let log = EventLog::open(&storage.db, vec![tmp.path().join("root")]).unwrap();
        let server = Server::bind(
            "127.0.0.1:0".parse().unwrap(),
            &storage.dir,
            identity.clone(),
            log.clone(),
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(server.serve(async {
            let _ = stopped.await;
        }));
        let supervisor = Supervisor::start(agent, &data, log.clone(), SupervisorConfig::default())
            .await
            .unwrap();
        Daemon {
            tmp,
            storage,
            identity,
            log,
            addr,
            stop: Some(stop),
            task,
            _supervisor: supervisor,
        }
    }

    fn workdir(&self) -> String {
        self.tmp.path().join("root/project").display().to_string()
    }

    /// The whole log, seq 1 first.
    fn events(&self) -> Vec<Event> {
        self.log
            .read(&self.storage.db, Seq::ZERO, 5000)
            .unwrap()
            .events
    }

    /// Pairs `client` through its engine: the head's `Pair` action, with
    /// what `skywaved pair` would print.
    async fn pair(&self, client: &Client, proxy: &Proxy) {
        let code = pairing::issue_code(self.storage.db.conn(), Utc::now(), pairing::DEFAULT_TTL)
            .unwrap()
            .code;
        client
            .core
            .dispatch(Action::Pair {
                address: proxy.addr.clone(),
                code: code.to_string(),
                fingerprint: self.identity.ca_fingerprint().to_string(),
            })
            .unwrap();
        client
            .wait("paired and online", |_, view| {
                matches!(view.screen, Screen::SessionList(_))
                    && matches!(view.link, Link::Online { .. })
            })
            .await;
    }

    /// Waits until the log satisfies `done`.
    async fn wait_log(&self, what: &str, done: impl Fn(&[Event]) -> bool) {
        let mut head = self.log.subscribe();
        tokio::time::timeout(SOON, async {
            loop {
                head.borrow_and_update();
                if done(&self.events()) {
                    return;
                }
                head.changed().await.unwrap();
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for the log: {what}"));
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.task.abort();
    }
}

// ---- proxy ------------------------------------------------------------------

/// A TCP proxy in front of the server. While down it accepts and drops
/// every connection; `cut` kills the live ones.
struct Proxy {
    /// `127.0.0.1:<port>`, as the client pairs and connects.
    addr: String,
    up: Arc<AtomicBool>,
    live: Arc<Mutex<Vec<AbortHandle>>>,
    task: JoinHandle<()>,
}

impl Proxy {
    async fn start(upstream: SocketAddr) -> Proxy {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
        let up = Arc::new(AtomicBool::new(true));
        let live = Arc::new(Mutex::new(Vec::<AbortHandle>::new()));
        let (accept_up, accept_live) = (up.clone(), live.clone());
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut inbound, _)) = listener.accept().await else {
                    return;
                };
                if !accept_up.load(Ordering::SeqCst) {
                    continue;
                }
                let pipe = tokio::spawn(async move {
                    if let Ok(mut outbound) = TcpStream::connect(upstream).await {
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                    }
                });
                let mut live = accept_live.lock().unwrap();
                live.retain(|conn| !conn.is_finished());
                live.push(pipe.abort_handle());
            }
        });
        Proxy {
            addr,
            up,
            live,
            task,
        }
    }

    /// Kills every live connection.
    fn cut(&self) {
        for conn in self.live.lock().unwrap().drain(..) {
            conn.abort();
        }
    }

    fn down(&self) {
        self.up.store(false, Ordering::SeqCst);
        self.cut();
    }

    fn up(&self) {
        self.up.store(true, Ordering::SeqCst);
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.task.abort();
        self.cut();
    }
}

// ---- client -----------------------------------------------------------------

/// A core with its engine, its snapshots on a watch channel.
struct Client {
    dir: TempDir,
    core: Core,
    seam: Seam,
    views: watch::Receiver<ViewState>,
}

impl Client {
    fn open(config: EngineConfig) -> Client {
        let dir = tempfile::tempdir().unwrap();
        let core = Core::open_with(dir.path(), config).unwrap();
        let (tx, views) = watch::channel(core.view());
        core.subscribe(move |view| {
            tx.send_replace(view);
        });
        Client {
            dir,
            seam: core.seam(),
            core,
            views,
        }
    }

    /// Waits until `done` holds for the latest snapshot (and whatever else
    /// it checks), re-checking on every new snapshot.
    async fn wait(&self, what: &str, done: impl Fn(&Client, &ViewState) -> bool) -> ViewState {
        let mut views = self.views.clone();
        tokio::time::timeout(SOON, async {
            loop {
                let view = views.borrow_and_update().clone();
                if done(self, &view) {
                    return view;
                }
                views.changed().await.unwrap();
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "timed out waiting for {what}; last view: {:?}",
                self.core.view()
            )
        })
    }

    fn cursor(&self) -> Seq {
        self.seam.cursor().unwrap()
    }

    /// Every event in the local replica, seq order.
    fn local_events(&self) -> Vec<(Seq, Event)> {
        local_events(self.dir.path())
    }

    /// Creates a session and waits until the replica has it and the
    /// command has its result.
    async fn create_session(&self, daemon: &Daemon) -> SessionId {
        self.core
            .dispatch(Action::CreateSession {
                workdir: daemon.workdir(),
            })
            .unwrap();
        let view = self
            .wait("the session in the list", |_, view| match &view.screen {
                Screen::SessionList(list) => list.sessions.len() == 1 && view.queued == 0,
                _ => false,
            })
            .await;
        let Screen::SessionList(list) = view.screen else {
            unreachable!()
        };
        list.sessions[0].id
    }
}

fn local_events(dir: &Path) -> Vec<(Seq, Event)> {
    let store = Store::open(dir).unwrap();
    let mut events: Vec<(Seq, Event)> = store
        .sessions()
        .unwrap()
        .into_iter()
        .flat_map(|s| store.session_events(s.id).unwrap())
        .collect();
    events.sort_by_key(|(seq, _)| *seq);
    events
}

/// Fast retries, short long-polls, small batches.
fn config() -> EngineConfig {
    EngineConfig {
        transport: TransportConfig {
            connect_timeout: SOON,
            request_timeout: SOON,
            ..TransportConfig::default()
        },
        wait_secs: 1,
        limit: Some(7),
        backoff: Backoff {
            base: Duration::from_millis(20),
            cap: Duration::from_millis(200),
            jitter: Jitter::random(),
        },
        device_name: Some("sync test".into()),
        ..EngineConfig::default()
    }
}

fn is_reconnecting(link: &Link) -> bool {
    matches!(link, Link::Reconnecting { .. })
}

fn user_messages(events: &[Event]) -> Vec<(CommandId, String)> {
    events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::UserMessage {
                command_id, text, ..
            } => Some((*command_id, text.clone())),
            _ => None,
        })
        .collect()
}

fn count(events: &[Event], what: impl Fn(&EventKind) -> bool) -> usize {
    events.iter().filter(|e| what(&e.kind)).count()
}

/// The server log and the replica hold the same events at the same seqs.
fn assert_same_log(server: &[Event], local: &[(Seq, Event)]) {
    let seqs: Vec<u64> = local.iter().map(|(seq, _)| seq.0).collect();
    let expected: Vec<u64> = (1..=server.len() as u64).collect();
    assert_eq!(seqs, expected, "no gaps, no duplicates");
    for ((seq, mine), theirs) in local.iter().zip(server) {
        assert_eq!(mine, theirs, "event {seq:?}");
    }
}

// ---- tests ------------------------------------------------------------------

/// F8: commands made while the server is unreachable stay `Queued`, then
/// drain oldest first once it is back, each exactly once in the log.
#[tokio::test(flavor = "multi_thread")]
async fn offline_queue_drains_exactly_once() {
    let agent = FakeAgent::new(FakeScript::new());
    let daemon = Daemon::start(agent.clone()).await;
    let proxy = Proxy::start(daemon.addr).await;
    let client = Client::open(config());
    daemon.pair(&client, &proxy).await;
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
    let outbox = Store::open(client.dir.path()).unwrap().outbox().unwrap();
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
    let proxy = Proxy::start(daemon.addr).await;
    let client = Client::open(config());
    daemon.pair(&client, &proxy).await;
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
    let proxy = Proxy::start(daemon.addr).await;
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
    daemon.pair(&client, &proxy).await;
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
    daemon.pair(&client, &proxy).await;
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

/// Suspending stops the network loops without losing work; resuming syncs
/// what was queued meanwhile.
#[tokio::test(flavor = "multi_thread")]
async fn suspend_pauses_and_resume_syncs() {
    let daemon = Daemon::start(FakeAgent::new(FakeScript::new())).await;
    let proxy = Proxy::start(daemon.addr).await;
    let client = Client::open(config());
    daemon.pair(&client, &proxy).await;

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
