//! Blob policy and downloads (T15) against a real `skywaved` — HTTPS API
//! and agent supervisor with the fake agent, whose large tool outputs
//! become blobs — in-process on 127.0.0.1 over real TLS. The client talks
//! through a TCP proxy that can cut a connection mid-blob, and through a
//! transport wrapper that records every blob request's offset and can hold
//! blob bodies back.
//!
//! Waits are bounded and driven by `ViewState` snapshots, the server log's
//! notifier or the file system; nothing relies on a fixed sleep.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use chrono::Utc;
use skywave_core::{
    Action, Backoff, BlobBody, BlobDownload, BlobPolicy, BlobState, BlobStatus, BoxFuture,
    Connector, Core, EngineConfig, Entry, FullView, HttpsTransport, Jitter, Link, Screen, Seam,
    TimelineItem, Transport, TransportConfig, TransportError, ViewState, blob_dir,
};
use skywave_proto::{
    BlobId, Command, CommandResult, Event, EventBatch, EventKind, EventsQuery, Full, HelloResponse,
    ImportedItem, Seq, SessionId,
};
use skywaved::agent::fake::{FakeAgent, FakeOutside, FakeScript, Step};
use skywaved::agent::{AdapterEvent, AgentCallId, HistoryItem, HistoryRead, ResultDisplay};
use skywaved::identity::ServerIdentity;
use skywaved::pairing;
use skywaved::server::{Server, ServerOptions};
use skywaved::{EventLog, Storage, Supervisor, SupervisorConfig, SupervisorHandle};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, watch};
use tokio::task::{AbortHandle, JoinHandle};

/// Upper bound for any wait; a passing test never gets near it.
const SOON: Duration = Duration::from_secs(20);

// ---- server -----------------------------------------------------------------

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
        Daemon::start_with_outside(agent, FakeOutside::new()).await
    }

    /// As [`Daemon::start`], an adopting `CreateSession` reading its
    /// conversation from `outside`.
    async fn start_with_outside(agent: FakeAgent, outside: FakeOutside) -> Daemon {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("root/project")).unwrap();
        let data = tmp.path().join("server");
        let storage = Storage::open(&data).unwrap();
        let identity = Arc::new(
            ServerIdentity::bootstrap(&storage.dir, &["localhost".into(), "127.0.0.1".into()])
                .unwrap(),
        );
        let log = EventLog::open(&storage.db, vec![tmp.path().join("root")]).unwrap();
        let options = ServerOptions {
            outside: Arc::new(outside),
            ..ServerOptions::default()
        };
        let server = Server::bind_with(
            "127.0.0.1:0".parse().unwrap(),
            &storage.dir,
            identity.clone(),
            log.clone(),
            options,
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

    fn events(&self) -> Vec<Event> {
        self.log
            .read(&self.storage.db, Seq::ZERO, 5000)
            .unwrap()
            .events
    }

    fn head(&self) -> Seq {
        Seq(self.events().len() as u64)
    }

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
        .unwrap_or_else(|_| {
            panic!(
                "timed out waiting for the log: {what}; log: {:?}",
                self.events().iter().map(|e| &e.kind).collect::<Vec<_>>()
            )
        });
    }

    /// Creates a session from `client`, sends one message and waits until
    /// the turn it starts has ended and the client holds the whole log.
    /// Returns the session.
    async fn session_with_turn(&self, client: &Client) -> SessionId {
        client
            .core
            .dispatch(Action::CreateSession {
                conversation: None,
                workdir: self.tmp.path().join("root/project").display().to_string(),
            })
            .unwrap();
        self.wait_log("the session", |events| !events.is_empty())
            .await;
        let session = self.events()[0].session_id;
        let head = self.head();
        client
            .wait("the session locally", |c, _| {
                c.seam.cursor().unwrap() >= head
            })
            .await;
        self.turn(client, session).await;
        session
    }

    /// Sends a message and waits until its turn has ended and the client
    /// holds the whole log.
    async fn turn(&self, client: &Client, session: SessionId) {
        let turns_before = self.turns_ended();
        client
            .core
            .dispatch(Action::SendMessage {
                session_id: session,
                text: "go".into(),
            })
            .unwrap();
        self.wait_log("the turn to end", |events| {
            turns_ended(events) > turns_before
        })
        .await;
        let head = self.head();
        client
            .wait("the whole log", |c, _| c.seam.cursor().unwrap() >= head)
            .await;
    }

    fn turns_ended(&self) -> usize {
        turns_ended(&self.events())
    }
}

fn turns_ended(events: &[Event]) -> usize {
    events
        .iter()
        .filter(|e| {
            matches!(
                e.kind,
                EventKind::SessionStateChanged(skywave_proto::SessionState::Idle)
            )
        })
        .count()
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.task.abort();
    }
}

/// A fake agent whose turns each end with a tool result of these outputs.
fn agent_with_outputs(outputs: &[String]) -> FakeAgent {
    let mut script = FakeScript::new();
    for output in outputs {
        script = script.turn([
            Step::tool_call("bash", serde_json::json!({ "cmd": "cat big" })),
            Step::tool_result(output.clone()),
        ]);
    }
    FakeAgent::new(script)
}

/// `len` bytes of printable text, different per `seed`.
fn big_output(len: usize, seed: u8) -> String {
    (0..len)
        .map(|i| {
            if i % 80 == 79 {
                '\n'
            } else {
                char::from(b'a' + ((i / 80 + usize::from(seed)) % 26) as u8)
            }
        })
        .collect()
}

// ---- proxy ------------------------------------------------------------------

/// A TCP proxy in front of the server. Once armed with
/// [`cut_after`](Proxy::cut_after), the first connection that relays more
/// than that many bytes to the client is cut.
struct Proxy {
    addr: String,
    /// Bytes after which to cut, 0 when disarmed.
    cut_after: Arc<AtomicU64>,
    live: Arc<Mutex<Vec<AbortHandle>>>,
    task: JoinHandle<()>,
}

impl Proxy {
    async fn start(upstream: SocketAddr) -> Proxy {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
        let cut_after = Arc::new(AtomicU64::new(0));
        let live = Arc::new(Mutex::new(Vec::<AbortHandle>::new()));
        let (accept_cut, accept_live) = (cut_after.clone(), live.clone());
        let task = tokio::spawn(async move {
            loop {
                let Ok((inbound, _)) = listener.accept().await else {
                    return;
                };
                let cut = accept_cut.clone();
                let pipe = tokio::spawn(async move {
                    let Ok(outbound) = TcpStream::connect(upstream).await else {
                        return;
                    };
                    let (mut client_rx, mut client_tx) = inbound.into_split();
                    let (mut server_rx, mut server_tx) = outbound.into_split();
                    let up = tokio::io::copy(&mut client_rx, &mut server_tx);
                    let down = async {
                        let mut relayed = 0u64;
                        let mut buf = vec![0u8; 16 * 1024];
                        loop {
                            let n = match server_rx.read(&mut buf).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => n,
                            };
                            relayed += n as u64;
                            let limit = cut.load(Ordering::SeqCst);
                            if limit > 0
                                && relayed > limit
                                && cut
                                    .compare_exchange(limit, 0, Ordering::SeqCst, Ordering::SeqCst)
                                    .is_ok()
                            {
                                return;
                            }
                            if client_tx.write_all(&buf[..n]).await.is_err() {
                                return;
                            }
                        }
                    };
                    tokio::select! {
                        _ = up => {}
                        () = down => {}
                    }
                });
                let mut live = accept_live.lock().unwrap();
                live.retain(|conn| !conn.is_finished());
                live.push(pipe.abort_handle());
            }
        });
        Proxy {
            addr,
            cut_after,
            live,
            task,
        }
    }

    fn cut_after(&self, bytes: u64) {
        self.cut_after.store(bytes, Ordering::SeqCst);
    }

    fn armed(&self) -> bool {
        self.cut_after.load(Ordering::SeqCst) > 0
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.task.abort();
        for conn in self.live.lock().unwrap().drain(..) {
            conn.abort();
        }
    }
}

// ---- transport spy ------------------------------------------------------------

/// What the spy saw, shared with the test.
#[derive(Clone, Default)]
struct Spy {
    /// `from` of every blob request, in order.
    froms: Arc<Mutex<Vec<u64>>>,
    /// While `false`, blob bodies stop after their first chunk.
    flowing: Option<watch::Receiver<bool>>,
}

impl Spy {
    fn holding(flowing: watch::Receiver<bool>) -> Spy {
        Spy {
            flowing: Some(flowing),
            ..Spy::default()
        }
    }

    fn froms(&self) -> Vec<u64> {
        self.froms.lock().unwrap().clone()
    }

    fn connector(&self) -> Connector {
        let spy = self.clone();
        Connector::new(move |address, identity, config| {
            let inner = HttpsTransport::new(address, identity, config.clone())?;
            Ok(Arc::new(Spied {
                inner: Arc::new(inner),
                spy: spy.clone(),
            }) as Arc<dyn Transport>)
        })
    }
}

struct Spied {
    inner: Arc<dyn Transport>,
    spy: Spy,
}

impl Transport for Spied {
    fn expect_run(&self, run: skywave_proto::RunId) {
        self.inner.expect_run(run);
    }

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
        self.inner.events(query)
    }

    fn blob(&self, id: BlobId, from: u64) -> BoxFuture<'_, Result<BlobDownload, TransportError>> {
        Box::pin(async move {
            self.spy.froms.lock().unwrap().push(from);
            let mut download = self.inner.blob(id, from).await?;
            if let Some(flowing) = &self.spy.flowing {
                download.body = Box::new(Held {
                    inner: download.body,
                    flowing: flowing.clone(),
                    chunks: 0,
                });
            }
            Ok(download)
        })
    }
}

/// A blob body that passes its first chunk, then waits for `flowing`.
struct Held {
    inner: Box<dyn BlobBody>,
    flowing: watch::Receiver<bool>,
    chunks: usize,
}

impl BlobBody for Held {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Bytes>, TransportError>> {
        Box::pin(async move {
            if self.chunks > 0 {
                let _ = self.flowing.wait_for(|flowing| *flowing).await;
            }
            self.chunks += 1;
            self.inner.next_chunk().await
        })
    }
}

// ---- client -----------------------------------------------------------------

struct Client {
    core: Core,
    seam: Seam,
    views: watch::Receiver<ViewState>,
}

impl Client {
    fn open(dir: &Path, spy: &Spy) -> Client {
        let config = EngineConfig {
            transport: TransportConfig {
                connect_timeout: SOON,
                request_timeout: SOON,
                ..TransportConfig::default()
            },
            wait_secs: 1,
            backoff: Backoff {
                base: Duration::from_millis(20),
                cap: Duration::from_millis(200),
                jitter: Jitter::random(),
            },
            device_name: Some("blob test".into()),
            connector: spy.connector(),
            ..EngineConfig::default()
        };
        let core = Core::open_with(dir, config).unwrap();
        let (tx, views) = watch::channel(core.view());
        core.subscribe(move |view| {
            tx.send_replace(view);
        });
        Client {
            seam: core.seam(),
            core,
            views,
        }
    }

    /// Ends the core and its engine, as the process dying would, so the
    /// data dir can be opened again (one core per data dir).
    fn stop(self) {
        let Client { core, seam, views } = self;
        drop((seam, views));
        // Off the runtime: a shutdown refuses to block a runtime thread.
        std::thread::spawn(move || core.shutdown(SOON))
            .join()
            .unwrap()
            .unwrap();
    }

    /// Snapshots older than the call — the delivery thread may not have
    /// caught up with a dispatch yet — do not count.
    async fn wait(&self, what: &str, done: impl Fn(&Client, &ViewState) -> bool) -> ViewState {
        let mut views = self.views.clone();
        tokio::time::timeout(SOON, async {
            let mut caught_up = false;
            loop {
                let view = views.borrow_and_update().clone();
                caught_up = caught_up || view == self.core.view();
                if caught_up && done(self, &view) {
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

    /// The tool outputs' blobs as the open session shows them, in order.
    fn shown_blobs(view: &ViewState) -> Vec<(BlobId, BlobState)> {
        let Screen::Session(session) = &view.screen else {
            return Vec::new();
        };
        session
            .timeline
            .iter()
            .filter_map(|item| match item {
                TimelineItem::Logged {
                    entry:
                        Entry::ToolResult {
                            output: Some(FullView::Blob(blob)),
                            ..
                        },
                    ..
                } => Some((blob.id, blob.state.clone())),
                _ => None,
            })
            .collect()
    }

    fn status(&self, id: BlobId) -> BlobStatus {
        self.seam.blob(id).unwrap().unwrap().status
    }

    /// Waits until the open session shows blob `n` as available and returns
    /// its file.
    async fn wait_available(&self, n: usize) -> PathBuf {
        let view = self
            .wait("the blob to be available", |_, view| {
                matches!(
                    Client::shown_blobs(view).get(n),
                    Some((_, BlobState::Available { .. }))
                )
            })
            .await;
        match &Client::shown_blobs(&view)[n].1 {
            BlobState::Available { path } => path.clone(),
            _ => unreachable!(),
        }
    }
}

fn part_file(dir: &Path, id: BlobId) -> PathBuf {
    blob_dir(dir).join(format!("{}.part", id.0))
}

/// Waits until the `.part` file of `id` holds some bytes.
async fn wait_part(dir: &Path, id: BlobId) -> u64 {
    let part = part_file(dir, id);
    tokio::time::timeout(SOON, async {
        loop {
            match std::fs::metadata(&part) {
                Ok(meta) if meta.len() > 0 => return meta.len(),
                _ => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    })
    .await
    .expect("timed out waiting for the partial file")
}

// ---- tests ------------------------------------------------------------------

/// F11: with the default `OnDemand`, a blob stays `NotDownloaded` until the
/// user asks for it; then it downloads and its file holds exactly the
/// tool's output.
#[tokio::test(flavor = "multi_thread")]
async fn on_demand_downloads_only_when_asked() {
    let output = big_output(300 * 1024, 0);
    let daemon = Daemon::start(agent_with_outputs(std::slice::from_ref(&output))).await;
    let proxy = Proxy::start(daemon.addr).await;
    let dir = tempfile::tempdir().unwrap();
    let spy = Spy::default();
    let client = Client::open(dir.path(), &spy);
    daemon.pair(&client, &proxy).await;
    let session = daemon.session_with_turn(&client).await;

    client.core.dispatch(Action::OpenSession(session)).unwrap();
    let view = client
        .wait("the session with its tool output", |_, view| {
            Client::shown_blobs(view).len() == 1
        })
        .await;
    let (id, state) = Client::shown_blobs(&view).remove(0);
    assert_eq!(
        state,
        BlobState::NotDownloaded {
            size: output.len() as u64
        }
    );
    assert_eq!(
        client.seam.settings().unwrap().blob_policy,
        BlobPolicy::OnDemand
    );
    assert!(matches!(view.link, Link::Online { .. }), "synced: {view:?}");
    assert!(spy.froms().is_empty(), "nothing downloads unasked");

    client.core.dispatch(Action::DownloadBlob(id)).unwrap();
    let path = client.wait_available(0).await;
    assert!(path.is_absolute(), "{path:?}");
    assert_eq!(path, blob_dir(dir.path()).join(id.0.to_string()));
    assert_eq!(std::fs::read(&path).unwrap(), output.as_bytes());
    assert_eq!(spy.froms(), [0]);
    assert!(!part_file(dir.path(), id).exists());
}

/// An interrupted download resumes: the connection carrying the blob is cut
/// mid-body, the next try asks for the rest with `Range`, and the file ends
/// up byte for byte the tool's output.
#[tokio::test(flavor = "multi_thread")]
async fn interrupted_download_resumes() {
    let output = big_output(3 * 1024 * 1024, 1);
    let daemon = Daemon::start(agent_with_outputs(std::slice::from_ref(&output))).await;
    let proxy = Proxy::start(daemon.addr).await;
    let dir = tempfile::tempdir().unwrap();
    let spy = Spy::default();
    let client = Client::open(dir.path(), &spy);
    daemon.pair(&client, &proxy).await;
    let session = daemon.session_with_turn(&client).await;
    client.core.dispatch(Action::OpenSession(session)).unwrap();
    let view = client
        .wait("the tool output", |_, view| {
            Client::shown_blobs(view).len() == 1
        })
        .await;
    let id = Client::shown_blobs(&view)[0].0;

    proxy.cut_after(1024 * 1024);
    client.core.dispatch(Action::DownloadBlob(id)).unwrap();
    let path = client.wait_available(0).await;
    assert!(!proxy.armed(), "the proxy cut a connection");
    let froms = spy.froms();
    assert_eq!(froms[0], 0);
    assert!(
        froms.len() >= 2 && froms[1] > 0 && froms[1] < output.len() as u64,
        "the second try resumed with a range: {froms:?}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), output.as_bytes());
}

/// D6/F10 with `Eager`: blobs download in the background right after their
/// metadata, and a download that is held back delays neither event sync
/// nor `Online` — metadata synced counts as synced.
#[tokio::test(flavor = "multi_thread")]
async fn eager_downloads_in_the_background_without_blocking_sync() {
    let outputs = [big_output(512 * 1024, 2), big_output(64 * 1024, 3)];
    let daemon = Daemon::start(agent_with_outputs(&outputs)).await;
    let proxy = Proxy::start(daemon.addr).await;
    let dir = tempfile::tempdir().unwrap();
    let (flowing, held) = watch::channel(false);
    let spy = Spy::holding(held);
    let client = Client::open(dir.path(), &spy);
    daemon.pair(&client, &proxy).await;
    client
        .core
        .dispatch(Action::SetBlobPolicy(BlobPolicy::Eager))
        .unwrap();

    // The first turn's blob starts downloading on its own, and stalls.
    let session = daemon.session_with_turn(&client).await;
    client.core.dispatch(Action::OpenSession(session)).unwrap();
    let view = client
        .wait("the first blob downloading", |_, view| {
            matches!(
                Client::shown_blobs(view).first(),
                Some((_, BlobState::Downloading { .. }))
            )
        })
        .await;
    let first = Client::shown_blobs(&view)[0].0;
    wait_part(dir.path(), first).await;

    // A second turn syncs completely while the first blob is still held.
    daemon.turn(&client, session).await;
    let view = client
        .wait("online with both tool outputs", |_, view| {
            matches!(view.link, Link::Online { .. }) && Client::shown_blobs(view).len() == 2
        })
        .await;
    assert_eq!(client.seam.cursor().unwrap(), daemon.head());
    assert!(
        matches!(
            Client::shown_blobs(&view)[0].1,
            BlobState::Downloading { .. }
        ),
        "the first blob is still downloading: {view:?}"
    );
    assert!(matches!(
        client.status(first),
        BlobStatus::Downloading { .. }
    ));

    // Let the bodies flow: both blobs arrive without being asked for.
    flowing.send_replace(true);
    let first_path = client.wait_available(0).await;
    let second_path = client.wait_available(1).await;
    assert_eq!(std::fs::read(first_path).unwrap(), outputs[0].as_bytes());
    assert_eq!(std::fs::read(second_path).unwrap(), outputs[1].as_bytes());
    assert_eq!(spy.froms(), [0, 0], "each blob fetched once");
}

/// An adopted conversation's imported blobs are blobs like any other:
/// `Eager` fetches them in the background, `OnDemand` leaves them until
/// asked for.
#[tokio::test(flavor = "multi_thread")]
async fn imported_blobs_download_under_eager_and_wait_under_on_demand() {
    let output = big_output(200 * 1024, 5);
    let outside = FakeOutside::new();
    let daemon =
        Daemon::start_with_outside(FakeAgent::new(FakeScript::new()), outside.clone()).await;
    let workdir = daemon.tmp.path().join("root/project");
    outside.conversation(
        &std::fs::canonicalize(&workdir).unwrap(),
        "conv-1",
        HistoryRead::Read {
            items: vec![
                HistoryItem::User {
                    at: None,
                    text: "show it".into(),
                },
                HistoryItem::Agent {
                    at: None,
                    event: AdapterEvent::ToolCall {
                        call: Some(AgentCallId::from("toolu_1")),
                        parent: None,
                        name: "Bash".into(),
                        input: serde_json::json!({ "command": "cat big" }),
                        display: None,
                    },
                },
                HistoryItem::Agent {
                    at: None,
                    event: AdapterEvent::ToolResult {
                        call: Some(AgentCallId::from("toolu_1")),
                        result: ResultDisplay {
                            is_error: false,
                            status: None,
                            preview: None,
                            output: output.clone(),
                        },
                    },
                },
            ],
        },
    );
    let proxy = Proxy::start(daemon.addr).await;
    let (eager_dir, lazy_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (eager_spy, lazy_spy) = (Spy::default(), Spy::default());
    let eager = Client::open(eager_dir.path(), &eager_spy);
    let lazy = Client::open(lazy_dir.path(), &lazy_spy);
    daemon.pair(&eager, &proxy).await;
    daemon.pair(&lazy, &proxy).await;
    eager
        .core
        .dispatch(Action::SetBlobPolicy(BlobPolicy::Eager))
        .unwrap();

    eager
        .core
        .dispatch(Action::CreateSession {
            conversation: Some("conv-1".into()),
            workdir: workdir.display().to_string(),
        })
        .unwrap();
    daemon
        .wait_log("the import", |events| {
            events
                .iter()
                .any(|e| matches!(e.kind, EventKind::ImportFinished { .. }))
        })
        .await;
    let blob = daemon
        .events()
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::Imported {
                item: ImportedItem::ToolResult(result),
                ..
            } => result.output.as_ref().and_then(Full::blob).cloned(),
            _ => None,
        })
        .expect("the imported output is a blob");
    let head = daemon.head();

    // Eager: fetched without being asked for, byte for byte.
    eager
        .wait("the imported blob downloaded", |c, _| {
            matches!(
                c.seam.blob(blob.id).unwrap().map(|b| b.status),
                Some(BlobStatus::Available { .. })
            )
        })
        .await;
    let BlobStatus::Available { path } = eager.status(blob.id) else {
        unreachable!()
    };
    assert_eq!(std::fs::read(path).unwrap(), output.as_bytes());
    assert_eq!(eager_spy.froms(), [0]);

    // On demand: synced to the head, the blob registered and left alone.
    lazy.wait("the whole log", |c, view| {
        c.seam.cursor().unwrap() >= head && matches!(view.link, Link::Online { .. })
    })
    .await;
    assert_eq!(lazy.status(blob.id), BlobStatus::NotDownloaded);
    assert!(lazy_spy.froms().is_empty(), "nothing downloads unasked");
}

/// Starts downloading the one blob of a turn whose tool printed `output`,
/// lets the body stall after its first chunk until that chunk is recorded
/// as progress, then stops the core. Returns the blob and its recorded
/// progress.
async fn stop_mid_download(daemon: &Daemon, proxy: &Proxy, dir: &Path) -> (BlobId, u64) {
    let (_flowing, held) = watch::channel(false);
    let client = Client::open(dir, &Spy::holding(held));
    daemon.pair(&client, proxy).await;
    let session = daemon.session_with_turn(&client).await;
    client.core.dispatch(Action::OpenSession(session)).unwrap();
    let view = client
        .wait("the tool output", |_, view| {
            Client::shown_blobs(view).len() == 1
        })
        .await;
    let id = Client::shown_blobs(&view)[0].0;
    client.core.dispatch(Action::DownloadBlob(id)).unwrap();
    // The stalled body still reports what it got.
    client
        .wait(
            "progress recorded",
            |c, _| matches!(c.status(id), BlobStatus::Downloading { got, .. } if got > 0),
        )
        .await;
    let BlobStatus::Downloading { got, .. } = client.status(id) else {
        unreachable!()
    };
    // The core goes away mid-download.
    client.stop();
    (id, got)
}

/// A download interrupted by a restart resumes from its `.part` file when
/// the core opens again.
#[tokio::test(flavor = "multi_thread")]
async fn restart_resumes_from_the_part_file() {
    let output = big_output(1024 * 1024, 4);
    let daemon = Daemon::start(agent_with_outputs(std::slice::from_ref(&output))).await;
    let proxy = Proxy::start(daemon.addr).await;
    let dir = tempfile::tempdir().unwrap();
    let (id, got) = stop_mid_download(&daemon, &proxy, dir.path()).await;
    assert!(wait_part(dir.path(), id).await >= got);

    let spy = Spy::default();
    let client = Client::open(dir.path(), &spy);
    client
        .wait("the blob to be available after the restart", |c, _| {
            matches!(c.status(id), BlobStatus::Available { .. })
        })
        .await;
    let BlobStatus::Available { path } = client.status(id) else {
        unreachable!()
    };
    let froms = spy.froms();
    assert!(
        !froms.is_empty() && froms[0] > 0,
        "resumed from the partial file: {froms:?}"
    );
    assert_eq!(std::fs::read(path).unwrap(), output.as_bytes());
}

/// A `.part` file longer than the recorded progress (as a power loss can
/// leave it: the length kept, the data past the last fsync zeros) is not
/// trusted past that progress: the restart resumes from the recorded count
/// and the blob is the tool's output, byte for byte.
#[tokio::test(flavor = "multi_thread")]
async fn restart_resumes_from_the_recorded_progress_not_the_file_length() {
    let output = big_output(1024 * 1024, 6);
    let daemon = Daemon::start(agent_with_outputs(std::slice::from_ref(&output))).await;
    let proxy = Proxy::start(daemon.addr).await;
    let dir = tempfile::tempdir().unwrap();
    let (id, got) = stop_mid_download(&daemon, &proxy, dir.path()).await;

    // Zeros past the recorded progress, as if the length survived a crash
    // that lost the data.
    let part = part_file(dir.path(), id);
    let file = std::fs::OpenOptions::new().write(true).open(&part).unwrap();
    file.set_len(got + 256 * 1024).unwrap();
    drop(file);

    let spy = Spy::default();
    let client = Client::open(dir.path(), &spy);
    client
        .wait("the blob to be available after the restart", |c, _| {
            matches!(c.status(id), BlobStatus::Available { .. })
        })
        .await;
    let BlobStatus::Available { path } = client.status(id) else {
        unreachable!()
    };
    assert_eq!(spy.froms(), [got], "resumed from the recorded progress");
    assert!(
        std::fs::read(path).unwrap() == output.as_bytes(),
        "the blob is the tool's output, without the zeros"
    );
}
