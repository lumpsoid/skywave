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
    Action, Backoff, BlobBody, BlobChange, BlobDownload, BlobPolicy, BlobState, BlobStatus,
    BoxFuture, Connector, Core, DeviceRef, EngineConfig, Entry, FullView, HttpsTransport, Jitter,
    Link, Screen, Seam, Store, TimelineItem, Transport, TransportConfig, TransportError, ViewState,
    blob_dir,
};
use skywave_proto::{
    BlobId, BlobRef, Command, CommandId, CommandKind, CommandResult, Event, EventBatch, EventKind,
    EventsQuery, Full, HelloResponse, ImportedItem, LineSpan, Seq, SessionId,
};
use skywave_testkit::Rng;
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

/// `len` bytes of random hex text in lines, from `seed`: text, so it
/// compresses, but only about by half, so its frame stays large on the
/// wire where [`big_output`]'s shrinks to almost nothing.
fn noisy_output(len: usize, seed: u64) -> String {
    let mut rng = Rng::new(seed);
    (0..len)
        .map(|i| {
            if i % 80 == 79 {
                '\n'
            } else {
                char::from(b"0123456789abcdef"[rng.below(16) as usize])
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
    /// Bytes relayed to the client, every connection together.
    down: Arc<AtomicU64>,
    live: Arc<Mutex<Vec<AbortHandle>>>,
    task: JoinHandle<()>,
}

impl Proxy {
    async fn start(upstream: SocketAddr) -> Proxy {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
        let cut_after = Arc::new(AtomicU64::new(0));
        let down_total = Arc::new(AtomicU64::new(0));
        let live = Arc::new(Mutex::new(Vec::<AbortHandle>::new()));
        let (accept_cut, accept_down, accept_live) =
            (cut_after.clone(), down_total.clone(), live.clone());
        let task = tokio::spawn(async move {
            loop {
                let Ok((inbound, _)) = listener.accept().await else {
                    return;
                };
                let cut = accept_cut.clone();
                let down_total = accept_down.clone();
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
                            down_total.fetch_add(n as u64, Ordering::SeqCst);
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
            down: down_total,
            live,
            task,
        }
    }

    fn cut_after(&self, bytes: u64) {
        self.cut_after.store(bytes, Ordering::SeqCst);
    }

    /// Bytes relayed to the client so far.
    fn down(&self) -> u64 {
        self.down.load(Ordering::SeqCst)
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
    /// The blob of every blob request, in order.
    ids: Arc<Mutex<Vec<BlobId>>>,
    /// While `false`, blob bodies stop after their first chunk.
    flowing: Option<watch::Receiver<bool>>,
    /// Held bodies a download let go of, by blob.
    dropped: Arc<Mutex<Vec<BlobId>>>,
    /// The newest transport the engine built, to send commands the core
    /// has no action for yet.
    transport: Arc<Mutex<Option<Arc<dyn Transport>>>>,
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

    fn ids(&self) -> Vec<BlobId> {
        self.ids.lock().unwrap().clone()
    }

    fn dropped(&self) -> Vec<BlobId> {
        self.dropped.lock().unwrap().clone()
    }

    /// Sends `kind` to the server as this client, and its answer.
    async fn send(&self, kind: CommandKind) -> CommandResult {
        let transport = self
            .transport
            .lock()
            .unwrap()
            .clone()
            .expect("a transport: the client is paired");
        transport
            .send_command(&Command {
                id: CommandId::new(),
                kind,
            })
            .await
            .unwrap()
    }

    fn connector(&self) -> Connector {
        let spy = self.clone();
        Connector::new(move |address, identity, config, run| {
            let inner: Arc<dyn Transport> = Arc::new(HttpsTransport::new(
                address,
                identity,
                config.clone(),
                run.clone(),
            )?);
            *spy.transport.lock().unwrap() = Some(inner.clone());
            Ok(Arc::new(Spied {
                inner,
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
            self.spy.ids.lock().unwrap().push(id);
            let mut download = self.inner.blob(id, from).await?;
            if let Some(flowing) = &self.spy.flowing {
                download.body = Box::new(Held {
                    inner: download.body,
                    flowing: flowing.clone(),
                    chunks: 0,
                    id,
                    dropped: self.spy.dropped.clone(),
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
    id: BlobId,
    dropped: Arc<Mutex<Vec<BlobId>>>,
}

impl Drop for Held {
    fn drop(&mut self) {
        self.dropped.lock().unwrap().push(self.id);
    }
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

    /// The tool outputs as the open session shows them, in order.
    fn shown_outputs(view: &ViewState) -> Vec<FullView> {
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
                            output: Some(output),
                            ..
                        },
                    ..
                } => Some(output.clone()),
                _ => None,
            })
            .collect()
    }

    /// Waits until the open session shows its first tool output deleted,
    /// and returns who it says deleted it.
    async fn wait_tombstone(&self) -> DeviceRef {
        let view = self
            .wait("the tombstone", |_, view| {
                matches!(
                    Client::shown_outputs(view).first(),
                    Some(FullView::Deleted { .. })
                )
            })
            .await;
        match &Client::shown_outputs(&view)[0] {
            FullView::Deleted { by } => by.clone(),
            _ => unreachable!(),
        }
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

/// D45: a blob comes compressed, far smaller on the wire than its size,
/// and lands as the tool's output, its hash checked on the decoded bytes.
#[tokio::test(flavor = "multi_thread")]
async fn a_download_comes_compressed() {
    let output = big_output(3 * 1024 * 1024, 15);
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

    let before = proxy.down();
    client.core.dispatch(Action::DownloadBlob(id)).unwrap();
    let path = client.wait_available(0).await;
    let wire = proxy.down() - before;
    assert!(
        wire < output.len() as u64 / 20,
        "{wire} bytes on the wire for {} of output",
        output.len()
    );
    assert_eq!(spy.froms(), [0]);
    assert_eq!(std::fs::read(&path).unwrap(), output.as_bytes());
}

/// An interrupted download resumes: the connection carrying the blob is cut
/// mid-body, the next try asks for the rest with `Range`, and the file ends
/// up byte for byte the tool's output. The body is compressed (D45), and
/// the range counts decoded bytes: the cut came after 1 MiB on the wire,
/// and more than that of the output had landed.
#[tokio::test(flavor = "multi_thread")]
async fn interrupted_download_resumes() {
    let output = noisy_output(3 * 1024 * 1024, 1);
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
    assert!(
        froms[1] > 1024 * 1024,
        "resumed from the decoded offset, past the wire's cut: {froms:?}"
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

// ---- server deletes and crops ---------------------------------------------------

/// `output` cropped to `keep`, as the server makes it.
fn cropped(output: &[u8], keep: &[LineSpan]) -> Vec<u8> {
    let mut out = Vec::new();
    skywave_proto::crop::crop(output, &mut out, keep).unwrap();
    out
}

fn line_count(output: &[u8]) -> u32 {
    u32::try_from(skywave_proto::crop::count(output).unwrap()).unwrap()
}

/// Waits until the open session holds the whole log, then shows it.
async fn open_at_head(daemon: &Daemon, client: &Client, session: SessionId) {
    let head = daemon.head();
    client
        .wait("the whole log", |c, _| c.seam.cursor().unwrap() >= head)
        .await;
    client.core.dispatch(Action::OpenSession(session)).unwrap();
}

/// A server delete purges the copy of every device, the one that decided
/// it and another alike, and leaves the tombstone; a device that never
/// fetched it shows the same and has nothing to download. A fresh device
/// pulling the whole log, and a replica reset, fold to the same view.
#[tokio::test(flavor = "multi_thread")]
async fn a_server_delete_purges_every_copy_and_leaves_the_tombstone() {
    let output = big_output(200 * 1024, 7);
    let daemon = Daemon::start(agent_with_outputs(std::slice::from_ref(&output))).await;
    let proxy = Proxy::start(daemon.addr).await;
    let dirs: Vec<TempDir> = (0..4).map(|_| tempfile::tempdir().unwrap()).collect();
    let spies: Vec<Spy> = (0..4).map(|_| Spy::default()).collect();
    let (mine, theirs, never) = (
        Client::open(dirs[0].path(), &spies[0]),
        Client::open(dirs[1].path(), &spies[1]),
        Client::open(dirs[2].path(), &spies[2]),
    );
    for client in [&mine, &theirs, &never] {
        daemon.pair(client, &proxy).await;
    }
    let session = daemon.session_with_turn(&mine).await;
    for client in [&mine, &theirs, &never] {
        open_at_head(&daemon, client, session).await;
    }
    let view = never
        .wait("the tool output", |_, view| {
            Client::shown_blobs(view).len() == 1
        })
        .await;
    let id = Client::shown_blobs(&view)[0].0;
    let mut copies = Vec::new();
    for client in [&mine, &theirs] {
        client.core.dispatch(Action::DownloadBlob(id)).unwrap();
        copies.push(client.wait_available(0).await);
    }

    assert!(matches!(
        spies[0].send(CommandKind::DeleteBlob { blob: id }).await,
        CommandResult::Accepted { .. }
    ));
    for (client, me) in [(&mine, true), (&theirs, false), (&never, false)] {
        assert_eq!(client.wait_tombstone().await.me, me);
        assert!(client.seam.blob(id).unwrap().is_none(), "the row is gone");
    }
    for copy in &copies {
        assert!(!copy.exists(), "{copy:?} is gone");
    }
    // Nothing to download: the view offers no blob.
    assert_eq!(Client::shown_outputs(&never.core.view())[0].blob(), None);
    assert_eq!(spies[0].ids(), [id]);
    assert_eq!(spies[1].ids(), [id]);

    // A device paired since pulls the whole log and shows the same.
    let fresh = Client::open(dirs[3].path(), &spies[3]);
    daemon.pair(&fresh, &proxy).await;
    open_at_head(&daemon, &fresh, session).await;
    assert!(!fresh.wait_tombstone().await.me);

    // So does a replica reset, pulling it all again.
    never.stop();
    Store::reset_replica(dirs[2].path()).unwrap();
    let never = Client::open(dirs[2].path(), &spies[2]);
    open_at_head(&daemon, &never, session).await;
    assert!(!never.wait_tombstone().await.me);
    assert!(spies[2].ids().is_empty(), "it never fetched it");
}

/// A crop arriving mid-download stops the download at its next report
/// (the lease), without a retry and leaving no file; the session then shows
/// the cropped blob, not downloaded, with what the crop kept.
#[tokio::test(flavor = "multi_thread")]
async fn a_crop_mid_download_stops_it_and_shows_the_cropped_blob() {
    let output = big_output(1024 * 1024, 8);
    let daemon = Daemon::start(agent_with_outputs(std::slice::from_ref(&output))).await;
    let proxy = Proxy::start(daemon.addr).await;
    let dir = tempfile::tempdir().unwrap();
    let (flowing, held) = watch::channel(false);
    let spy = Spy::holding(held);
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
    client.core.dispatch(Action::DownloadBlob(id)).unwrap();
    client
        .wait(
            "progress recorded",
            |c, _| matches!(c.status(id), BlobStatus::Downloading { got, .. } if got > 0),
        )
        .await;

    let keep = [LineSpan { first: 3, count: 4 }];
    assert!(matches!(
        spy.send(CommandKind::CropBlob {
            blob: id,
            keep: keep.to_vec(),
        })
        .await,
        CommandResult::Accepted { .. }
    ));
    let expected = cropped(output.as_bytes(), &keep);
    let view = client
        .wait("the cropped blob", |_, view| {
            matches!(
                Client::shown_outputs(view).first(),
                Some(FullView::Blob(blob)) if blob.id != id
            )
        })
        .await;
    let FullView::Blob(shown) = &Client::shown_outputs(&view)[0] else {
        unreachable!()
    };
    assert_eq!(
        shown.state,
        BlobState::NotDownloaded {
            size: expected.len() as u64
        }
    );
    let Some(BlobChange::Cropped { kept, of, by }) = &shown.change else {
        panic!("expected the crop: {shown:?}");
    };
    assert_eq!(
        (*kept, *of, by.me),
        (4, line_count(output.as_bytes()), true)
    );
    assert!(client.seam.blob(id).unwrap().is_none(), "the row is gone");
    assert!(
        !part_file(dir.path(), id).exists(),
        "the partial file is gone"
    );

    // The stalled body goes on; the download's next report finds the blob
    // purged and lets go.
    flowing.send_replace(true);
    tokio::time::timeout(SOON, async {
        while !spy.dropped().contains(&id) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("timed out waiting for the download to let go");
    // Asked for, the cropped blob downloads: the server's crop, byte for
    // byte, and nothing of the old one was fetched again.
    let into = shown.id;
    client.core.dispatch(Action::DownloadBlob(into)).unwrap();
    let path = client.wait_available(0).await;
    assert_eq!(std::fs::read(path).unwrap(), expected);
    assert_eq!(spy.ids(), [id, into]);
    assert!(!part_file(dir.path(), id).exists());
    assert!(!blob_dir(dir.path()).join(id.0.to_string()).exists());
}

/// Under `Eager`, a fresh device pulling a crop of a crop in one batch
/// registers and purges the original and the middle blob in that batch, so
/// it queues and fetches only the last; the note counts what that crop
/// kept of the original's lines.
#[tokio::test(flavor = "multi_thread")]
async fn a_fresh_eager_device_fetches_only_what_a_crop_of_a_crop_left() {
    let output = big_output(64 * 1024, 9);
    let daemon = Daemon::start(agent_with_outputs(std::slice::from_ref(&output))).await;
    let proxy = Proxy::start(daemon.addr).await;
    let dirs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
    let (spy, eager_spy) = (Spy::default(), Spy::default());
    let client = Client::open(dirs[0].path(), &spy);
    daemon.pair(&client, &proxy).await;
    let session = daemon.session_with_turn(&client).await;
    let original = daemon
        .events()
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::ToolResult(result) => result.output.as_ref().and_then(Full::blob).cloned(),
            _ => None,
        })
        .expect("the tool output is a blob");
    let (first, second) = (
        [LineSpan {
            first: 0,
            count: 20,
        }],
        [LineSpan { first: 5, count: 2 }],
    );
    assert!(matches!(
        spy.send(CommandKind::CropBlob {
            blob: original.id,
            keep: first.to_vec(),
        })
        .await,
        CommandResult::Accepted { .. }
    ));
    let middle = daemon
        .events()
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::BlobCropped { into, .. } => Some(into.id),
            _ => None,
        })
        .expect("the first crop");
    assert!(matches!(
        spy.send(CommandKind::CropBlob {
            blob: middle,
            keep: second.to_vec(),
        })
        .await,
        CommandResult::Accepted { .. }
    ));

    let eager = Client::open(dirs[1].path(), &eager_spy);
    eager
        .core
        .dispatch(Action::SetBlobPolicy(BlobPolicy::Eager))
        .unwrap();
    daemon.pair(&eager, &proxy).await;
    open_at_head(&daemon, &eager, session).await;
    let path = eager.wait_available(0).await;
    let view = eager.core.view();
    let FullView::Blob(shown) = &Client::shown_outputs(&view)[0] else {
        unreachable!()
    };
    let expected = cropped(&cropped(output.as_bytes(), &first), &second);
    assert_eq!(std::fs::read(path).unwrap(), expected);
    assert_eq!(eager_spy.ids(), [shown.id], "only the last crop is fetched");
    assert!(eager.seam.blob(original.id).unwrap().is_none());
    assert!(eager.seam.blob(middle).unwrap().is_none());
    let Some(BlobChange::Cropped { kept, of, by }) = &shown.change else {
        panic!("expected the crop: {shown:?}");
    };
    assert_eq!(
        (*kept, *of, by.me),
        (2, line_count(output.as_bytes()), false)
    );
}

// ---- local crops ------------------------------------------------------------------

/// A session whose one tool printed `output`, with `mine` (in its own
/// directory) the device that sends deletes and crops through `spy`.
struct Cropping {
    daemon: Daemon,
    proxy: Proxy,
    _mine_dir: TempDir,
    /// Kept running: it decided the session's turn.
    _mine: Client,
    spy: Spy,
    session: SessionId,
    output: String,
    /// The output's blob.
    original: BlobRef,
}

impl Cropping {
    async fn start(output: String) -> Cropping {
        let daemon = Daemon::start(agent_with_outputs(std::slice::from_ref(&output))).await;
        let proxy = Proxy::start(daemon.addr).await;
        let mine_dir = tempfile::tempdir().unwrap();
        let spy = Spy::default();
        let mine = Client::open(mine_dir.path(), &spy);
        daemon.pair(&mine, &proxy).await;
        let session = daemon.session_with_turn(&mine).await;
        let original = daemon
            .events()
            .iter()
            .find_map(|e| match &e.kind {
                EventKind::ToolResult(result) => {
                    result.output.as_ref().and_then(Full::blob).cloned()
                }
                _ => None,
            })
            .expect("the tool output is a blob");
        Cropping {
            daemon,
            proxy,
            _mine_dir: mine_dir,
            _mine: mine,
            spy,
            session,
            output,
            original,
        }
    }

    /// A device in `dir` under `policy` that holds the output's blob,
    /// `Available`, and its spy.
    async fn holder(&self, dir: &Path, policy: BlobPolicy) -> (Client, Spy) {
        let spy = Spy::default();
        let client = Client::open(dir, &spy);
        client.core.dispatch(Action::SetBlobPolicy(policy)).unwrap();
        self.daemon.pair(&client, &self.proxy).await;
        open_at_head(&self.daemon, &client, self.session).await;
        // Eager fetches it unasked. Asking too would queue it again once
        // fetched, and a core stopped while that claim reads the file
        // system fails the blob (a known shutdown race, not a crop's).
        if policy == BlobPolicy::OnDemand {
            client
                .core
                .dispatch(Action::DownloadBlob(self.original.id))
                .unwrap();
        }
        let path = client.wait_available(0).await;
        assert_eq!(std::fs::read(path).unwrap(), self.output.as_bytes());
        assert_eq!(spy.ids(), [self.original.id]);
        (client, spy)
    }

    /// Crops `blob` on the server to `keep`, from `mine`; the blob it made.
    async fn crop(&self, blob: BlobId, keep: &[LineSpan]) -> BlobRef {
        assert!(matches!(
            self.spy
                .send(CommandKind::CropBlob {
                    blob,
                    keep: keep.to_vec(),
                })
                .await,
            CommandResult::Accepted { .. }
        ));
        self.daemon
            .events()
            .iter()
            .find_map(|e| match &e.kind {
                EventKind::BlobCropped { blob: of, into, .. } if *of == blob => Some(into.clone()),
                _ => None,
            })
            .expect("the crop is logged")
    }

    /// Applies the log past `dir`'s cursor in one batch, through a core
    /// with no engine: no downloader runs. Returns what the batch applied.
    fn apply_detached(&self, dir: &Path) -> skywave_core::Applied {
        let core = Core::open_detached(dir).unwrap();
        let seam = core.seam();
        let cursor = seam.cursor().unwrap();
        let events = self.daemon.events()[cursor.0 as usize..].to_vec();
        let applied = seam
            .apply_events(&EventBatch {
                from: Seq(cursor.0 + 1),
                events,
            })
            .unwrap();
        drop(seam);
        std::thread::spawn(move || core.shutdown(SOON))
            .join()
            .unwrap()
            .unwrap();
        applied
    }
}

/// Waits until the open session shows `into` available in place of the
/// output, and `old`'s row is gone. Returns `into`'s file.
async fn wait_local_crop(client: &Client, into: BlobId, old: BlobId) -> PathBuf {
    let view = client
        .wait("the cropped blob made here", |c, view| {
            matches!(
                Client::shown_outputs(view).first(),
                Some(FullView::Blob(blob))
                    if blob.id == into && matches!(blob.state, BlobState::Available { .. })
            ) && c.seam.blob(old).unwrap().is_none()
        })
        .await;
    let FullView::Blob(shown) = &Client::shown_outputs(&view)[0] else {
        unreachable!()
    };
    let BlobState::Available { path } = &shown.state else {
        unreachable!()
    };
    path.clone()
}

/// Whether `id` left no file in `dir`, complete or partial.
fn no_files(dir: &Path, id: BlobId) -> bool {
    !part_file(dir, id).exists() && !blob_dir(dir).join(id.0.to_string()).exists()
}

/// A device that holds a blob makes the server's crop of it itself, even
/// under `Eager` and though it did not decide the crop: the cropped blob is
/// available, the server's bytes exactly, the old blob is purged, and the
/// cropped blob was never requested (an eager download racing the crop
/// would have been).
#[tokio::test(flavor = "multi_thread")]
async fn a_holder_crops_locally_and_fetches_nothing() {
    let c = Cropping::start(big_output(200 * 1024, 10)).await;
    let dir = tempfile::tempdir().unwrap();
    let (theirs, spy) = c.holder(dir.path(), BlobPolicy::Eager).await;

    let keep = [LineSpan { first: 2, count: 5 }];
    let into = c.crop(c.original.id, &keep).await;
    let path = wait_local_crop(&theirs, into.id, c.original.id).await;
    assert_eq!(
        std::fs::read(path).unwrap(),
        cropped(c.output.as_bytes(), &keep)
    );
    assert!(no_files(dir.path(), c.original.id));
    assert!(!part_file(dir.path(), into.id).exists());
    assert_eq!(
        spy.ids(),
        [c.original.id],
        "the cropped blob is never fetched"
    );
}

/// A held copy that no longer hashes to what it was cannot make the crop:
/// under `Eager` the server's crop is fetched, once, and the old blob is
/// purged. (Under `OnDemand` nothing is fetched: the unit test
/// `a_failed_local_crop_fetches_nothing_unless_wanted` checks that once
/// the worker let go, which this test cannot see.)
#[tokio::test(flavor = "multi_thread")]
async fn a_tampered_copy_is_fetched_under_eager() {
    let c = Cropping::start(big_output(200 * 1024, 11)).await;
    let dir = tempfile::tempdir().unwrap();
    let (client, _) = c.holder(dir.path(), BlobPolicy::Eager).await;
    let BlobStatus::Available { path } = client.status(c.original.id) else {
        unreachable!()
    };
    client.stop();
    // The same lines, other bytes on every one: the crop's hash cannot
    // match. (Bytes outside the kept lines would still crop right.)
    let bytes = std::fs::read(&path).unwrap();
    let tampered: Vec<u8> = bytes
        .iter()
        .map(|b| match b {
            b'\n' => b'\n',
            b'x' => b'y',
            _ => b'x',
        })
        .collect();
    std::fs::write(&path, tampered).unwrap();
    let keep = [LineSpan { first: 1, count: 3 }];
    let into = c.crop(c.original.id, &keep).await;

    let spy = Spy::default();
    let eager = Client::open(dir.path(), &spy);
    open_at_head(&c.daemon, &eager, c.session).await;
    let path = wait_local_crop(&eager, into.id, c.original.id).await;
    assert_eq!(
        std::fs::read(path).unwrap(),
        cropped(c.output.as_bytes(), &keep)
    );
    assert!(no_files(dir.path(), c.original.id));
    assert_eq!(spy.ids(), [into.id], "fetched exactly once");
}

/// A crop of a crop arriving in one batch, the original held: both crops
/// run here, in log order, and neither the middle blob nor the last is
/// fetched.
#[tokio::test(flavor = "multi_thread")]
async fn a_crop_of_a_crop_in_one_batch_runs_both_locally() {
    let c = Cropping::start(big_output(64 * 1024, 12)).await;
    let dir = tempfile::tempdir().unwrap();
    let (theirs, _) = c.holder(dir.path(), BlobPolicy::Eager).await;
    theirs.stop();
    let (first, second) = (
        [LineSpan {
            first: 0,
            count: 20,
        }],
        [LineSpan { first: 5, count: 2 }],
    );
    let middle = c.crop(c.original.id, &first).await;
    let last = c.crop(middle.id, &second).await;

    let spy = Spy::default();
    let theirs = Client::open(dir.path(), &spy);
    open_at_head(&c.daemon, &theirs, c.session).await;
    let path = wait_local_crop(&theirs, last.id, middle.id).await;
    assert_eq!(
        std::fs::read(path).unwrap(),
        cropped(&cropped(c.output.as_bytes(), &first), &second)
    );
    assert!(theirs.seam.blob(c.original.id).unwrap().is_none());
    assert!(no_files(dir.path(), c.original.id));
    assert!(no_files(dir.path(), middle.id));
    assert!(spy.ids().is_empty(), "nothing fetched: {:?}", spy.ids());
}

/// A crop applied with no engine running stays pending in the replica; the
/// next engine queues it at startup, and it runs: the cropped blob made
/// here, the old one gone, nothing fetched.
#[tokio::test(flavor = "multi_thread")]
async fn a_pending_local_crop_runs_after_a_restart() {
    let c = Cropping::start(big_output(200 * 1024, 13)).await;
    let dir = tempfile::tempdir().unwrap();
    let (theirs, _) = c.holder(dir.path(), BlobPolicy::OnDemand).await;
    theirs.stop();
    let keep = [LineSpan { first: 4, count: 6 }];
    let into = c.crop(c.original.id, &keep).await;

    let applied = c.apply_detached(dir.path());
    let pending = skywave_core::PendingCrop {
        into: into.id,
        from: c.original.id,
    };
    assert_eq!(applied.local_crops, [pending]);
    assert!(!applied.blobs.contains(&into.id), "not queued for a fetch");
    {
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.load().unwrap().pending_crops, [pending]);
    }

    let spy = Spy::default();
    let theirs = Client::open(dir.path(), &spy);
    open_at_head(&c.daemon, &theirs, c.session).await;
    let path = wait_local_crop(&theirs, into.id, c.original.id).await;
    assert_eq!(
        std::fs::read(path).unwrap(),
        cropped(c.output.as_bytes(), &keep)
    );
    assert!(no_files(dir.path(), c.original.id));
    assert!(spy.ids().is_empty(), "nothing fetched: {:?}", spy.ids());
    assert!(
        Store::open(dir.path())
            .unwrap()
            .pending_crops()
            .unwrap()
            .is_empty()
    );
}

/// The server deletes a crop's blob while this device's local crop of it
/// is still pending: the old blob and the cropped one both go, with every
/// file, and nothing is left to run.
#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_pending_crops_blob_purges_both() {
    let c = Cropping::start(big_output(200 * 1024, 14)).await;
    let dir = tempfile::tempdir().unwrap();
    let (theirs, _) = c.holder(dir.path(), BlobPolicy::Eager).await;
    theirs.stop();
    let into = c
        .crop(c.original.id, &[LineSpan { first: 0, count: 3 }])
        .await;
    let applied = c.apply_detached(dir.path());
    assert_eq!(
        applied.local_crops.len(),
        1,
        "the crop is pending: {applied:?}"
    );
    // A `.part` the crop may have begun.
    std::fs::write(part_file(dir.path(), into.id), b"begun").unwrap();

    assert!(matches!(
        c.spy.send(CommandKind::DeleteBlob { blob: into.id }).await,
        CommandResult::Accepted { .. }
    ));
    let applied = c.apply_detached(dir.path());
    assert_eq!(applied.purged, [into.id, c.original.id]);
    assert!(applied.local_crops.is_empty());
    for id in [c.original.id, into.id] {
        assert!(no_files(dir.path(), id), "{id:?} left a file");
    }

    let spy = Spy::default();
    let theirs = Client::open(dir.path(), &spy);
    open_at_head(&c.daemon, &theirs, c.session).await;
    assert!(!theirs.wait_tombstone().await.me);
    assert!(theirs.seam.blob(c.original.id).unwrap().is_none());
    assert!(theirs.seam.blob(into.id).unwrap().is_none());
    assert!(theirs.seam.pending_crops().unwrap().is_empty());
    assert!(spy.ids().is_empty());
}
