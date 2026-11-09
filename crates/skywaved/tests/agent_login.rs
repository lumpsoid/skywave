//! Logging the agent in from a device (D49): `POST /v1/agent-login/start`
//! and `/finish` over real TLS on 127.0.0.1, against a real `ClaudeLogin`
//! running tests/support/fake-claude.sh. The login process runs in the
//! test's `work` dir; its markers, request records and credentials are in
//! `login`. Waits are on fifos the fake makes, gates and bounded file
//! waits.

use std::io::Write as _;
use std::net::SocketAddr;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use chrono::{DateTime, Utc};
use http_body_util::{BodyExt, Full};
use hyper::client::conn::http1::{self, SendRequest};
use hyper::header::{self, HeaderValue};
use hyper::{Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use rcgen::{CertificateParams, KeyPair};
use rustls::RootCertStore;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use skywave_proto::{
    HelloResponse, LoginFinish, LoginFinishResult, LoginFlowId, LoginStartResult, PairRequest,
    PairResponse, limits, path,
};
use skywaved::agent::claude_code::ClaudeCodeAdapter;
use skywaved::agent::claude_code::login::{ClaudeLogin, PASTE_WHOLE_CODE};
use skywaved::agent::claude_code::transcripts::ClaudeTranscripts;
use skywaved::agent::fake::Gate;
use skywaved::agent::{
    AgentLogin, LoginSeen, LoginSource, LoginState, LoginStatus, NoAgentLogin, OutsideConversations,
};
use skywaved::config::AgentConfig;
use skywaved::identity::ServerIdentity;
use skywaved::login::LOGIN_FLOW_MAX;
use skywaved::server::{Server, ServerOptions};
use skywaved::{EventLog, Storage, pairing};
use tempfile::TempDir;
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_rustls::TlsConnector;

/// Generous bound for things that should happen at once.
const SOON: Duration = Duration::from_secs(10);
/// The URL the fake CLI answers `claude_authenticate` with.
const FAKE_MANUAL_URL: &str = "https://example.invalid/oauth/authorize?state=fake-state";
/// The `refreshTokenExpiresAt` of the credentials the fake writes.
const FAKE_EXPIRES_MILLIS: i64 = 1_794_445_707_628;

fn fake_expires() -> Option<DateTime<Utc>> {
    DateTime::from_timestamp_millis(FAKE_EXPIRES_MILLIS)
}

// ---------------------------------------------------------------------------
// Captured tracing

/// Everything any test's server logged, at debug and above: one global
/// subscriber for this binary, since the server logs from the runtime's
/// other threads.
fn captured() -> &'static Arc<Mutex<Vec<u8>>> {
    static CAPTURED: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    CAPTURED.get_or_init(|| {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let writer = buffer.clone();
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .with_writer(move || Capture(writer.clone()))
            .init();
        buffer
    })
}

struct Capture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn captured_text() -> String {
    String::from_utf8_lossy(&captured().lock().unwrap()).into_owned()
}

// ---------------------------------------------------------------------------
// Fixture

/// How a fixture's server logs in.
struct Config {
    /// `None`: a `ClaudeLogin` on the fake.
    login: Option<Arc<dyn AgentLogin>>,
    flow_max: Duration,
    finish_timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            login: None,
            flow_max: LOGIN_FLOW_MAX,
            finish_timeout: skywaved::api::LOGIN_FINISH_TIMEOUT,
        }
    }
}

struct Fixture {
    tmp: TempDir,
    storage: Storage,
    identity: Arc<ServerIdentity>,
    log: EventLog,
    /// Where the login process runs.
    work: PathBuf,
    /// The fake's login dir: markers, records, credentials.
    login: PathBuf,
    login_state: LoginState,
    config: Config,
    /// Opens once a start joins another's.
    joined: Gate,
    /// A paired device's certificate and key.
    creds: Option<(String, String)>,
    addr: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<skywaved::Result<()>>>,
}

impl Fixture {
    async fn start() -> Fixture {
        Fixture::start_with(Config::default()).await
    }

    async fn start_with(config: Config) -> Fixture {
        captured();
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("work");
        let login = tmp.path().join("login");
        std::fs::create_dir(&work).unwrap();
        std::fs::create_dir(&login).unwrap();
        let storage = Storage::open(tmp.path().join("data")).unwrap();
        let identity =
            Arc::new(ServerIdentity::bootstrap(&storage.dir, &["localhost".into()]).unwrap());
        let log = EventLog::open(&storage.db, vec![work.clone()]).unwrap();
        let mut f = Fixture {
            tmp,
            storage,
            identity,
            log,
            work,
            login,
            login_state: LoginState::new(),
            config,
            joined: Gate::new(),
            creds: None,
            addr: "127.0.0.1:0".parse().unwrap(),
            stop: None,
            task: None,
        };
        f.serve().await;
        f
    }

    /// The `ClaudeLogin` on the fake, `other_login` as given.
    fn claude(&self, other_login: bool) -> ClaudeLogin {
        let adapter = ClaudeCodeAdapter::new(AgentConfig {
            command: Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/fake-claude.sh"),
            args: vec![
                "--fake-login-dir".to_owned(),
                self.login.to_str().unwrap().to_owned(),
            ],
            allow_bypass: false,
        })
        .with_grace_period(Duration::from_secs(5));
        ClaudeLogin::new(
            self.login.join(".credentials.json"),
            other_login,
            adapter,
            self.work.clone(),
        )
    }

    /// Binds a server, with flows of its own, and serves it.
    async fn serve(&mut self) {
        let login = match &self.config.login {
            Some(login) => login.clone(),
            None => Arc::new(self.claude(false)),
        };
        let options = ServerOptions {
            login,
            login_state: self.login_state.clone(),
            login_flow_max: self.config.flow_max,
            login_finish_timeout: self.config.finish_timeout,
            joined_hook: Some(Arc::new({
                let joined = self.joined.clone();
                move || joined.open()
            })),
            ..ServerOptions::default()
        };
        let server = Server::bind_with(
            "127.0.0.1:0".parse().unwrap(),
            &self.storage.dir,
            self.identity.clone(),
            self.log.clone(),
            options,
        )
        .await
        .unwrap();
        self.addr = server.local_addr();
        let (stop, stopped) = oneshot::channel::<()>();
        self.task = Some(tokio::spawn(server.serve(async {
            let _ = stopped.await;
        })));
        self.stop = Some(stop);
    }

    /// Stops the server and serves a new one on the same dirs.
    async fn restart(&mut self) {
        self.stop.take().unwrap().send(()).unwrap();
        self.task.take().unwrap().await.unwrap().unwrap();
        self.serve().await;
    }

    /// A connection of the one paired device, pairing it first.
    async fn device(&mut self) -> Client {
        if self.creds.is_none() {
            let issued =
                pairing::issue_code(self.storage.db.conn(), Utc::now(), pairing::DEFAULT_TTL)
                    .unwrap();
            let key = KeyPair::generate().unwrap();
            let req = PairRequest {
                code: issued.code.to_string(),
                csr_pem: CertificateParams::default()
                    .serialize_request(&key)
                    .unwrap()
                    .pem()
                    .unwrap(),
                device_name: "phone".into(),
            };
            let mut anonymous = Client::connect(self.addr, self.identity.ca_cert_der(), None).await;
            let reply = anonymous.send(Method::POST, path::PAIR, json(&req)).await;
            assert_eq!(reply.0, StatusCode::OK);
            let paired: PairResponse = serde_json::from_slice(&reply.1).unwrap();
            self.creds = Some((paired.device_cert_pem, key.serialize_pem()));
        }
        Client::connect(self.addr, self.identity.ca_cert_der(), self.creds.as_ref()).await
    }

    fn mark(&self, marker: &str) {
        std::fs::write(self.login.join(marker), "").unwrap();
    }

    fn unmark(&self, marker: &str) {
        std::fs::remove_file(self.login.join(marker)).unwrap();
    }

    /// How many lines the fake wrote to `name` in the login dir; 0 for no
    /// file.
    fn lines(&self, name: &str) -> usize {
        std::fs::read_to_string(self.login.join(name)).map_or(0, |t| t.lines().count())
    }

    /// Lets the fake's held `claude_authenticate` answer.
    async fn cue_authenticate(&self) {
        appears(&self.login.join("authenticates.txt")).await;
        cue(&self.login.join("auth.fifo")).await;
    }

    /// Lets the fake's held `claude_oauth_callback` answer.
    async fn cue_callback(&self) {
        appears(&self.login.join("callbacks.txt")).await;
        cue(&self.login.join("callback.fifo")).await;
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

/// Waits until `path` exists.
async fn appears(path: &Path) {
    tokio::time::timeout(SOON, async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{} never appeared", path.display()));
}

/// Writes a line to the fifo at `path` once its reader has it open. Opened
/// without blocking, so a fifo nobody reads fails the test instead of
/// hanging it.
async fn cue(path: &Path) {
    tokio::time::timeout(SOON, async {
        loop {
            let opened = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(path);
            match opened {
                Ok(mut fifo) => {
                    fifo.write_all(b"go\n").unwrap();
                    return;
                }
                // No reader yet (ENXIO), or no fifo yet.
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("nobody read {}", path.display()));
}

struct Client {
    sender: SendRequest<Full<Bytes>>,
}

impl Client {
    async fn connect(addr: SocketAddr, ca_der: &[u8], creds: Option<&(String, String)>) -> Client {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut roots = RootCertStore::empty();
        roots.add(CertificateDer::from(ca_der.to_vec())).unwrap();
        let builder = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots);
        let config = match creds {
            None => builder.with_no_client_auth(),
            Some((cert_pem, key_pem)) => {
                let chain = CertificateDer::pem_slice_iter(cert_pem.as_bytes())
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap();
                let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes()).unwrap();
                builder.with_client_auth_cert(chain, key).unwrap()
            }
        };
        let tcp = TcpStream::connect(addr).await.unwrap();
        let tls = TlsConnector::from(Arc::new(config))
            .connect(ServerName::try_from("localhost").unwrap(), tcp)
            .await
            .unwrap();
        let (sender, conn) = http1::handshake(TokioIo::new(tls)).await.unwrap();
        tokio::spawn(conn);
        Client { sender }
    }

    async fn send(&mut self, method: Method, uri: &str, body: Bytes) -> (StatusCode, Bytes) {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, "localhost");
        if !body.is_empty() {
            req = req.header(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
        }
        self.sender.ready().await.unwrap();
        let resp = self
            .sender
            .send_request(req.body(Full::new(body)).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        (status, resp.into_body().collect().await.unwrap().to_bytes())
    }

    async fn hello(&mut self) -> HelloResponse {
        let (status, body) = self.send(Method::GET, path::HELLO, Bytes::new()).await;
        assert_eq!(status, StatusCode::OK);
        serde_json::from_slice(&body).unwrap()
    }

    async fn start(&mut self) -> LoginStartResult {
        let (status, body) = self
            .send(Method::POST, path::AGENT_LOGIN_START, Bytes::new())
            .await;
        assert_eq!(status, StatusCode::OK);
        serde_json::from_slice(&body).unwrap()
    }

    /// Starts a login that must open, and gives its flow.
    async fn open(&mut self) -> LoginFlowId {
        match self.start().await {
            LoginStartResult::Open { flow, url } => {
                assert_eq!(url, FAKE_MANUAL_URL);
                flow
            }
            other => panic!("not open: {other:?}"),
        }
    }

    async fn finish_status(&mut self, flow: LoginFlowId, code: &str) -> (StatusCode, Bytes) {
        let body = json(&LoginFinish {
            flow,
            code: code.to_owned(),
        });
        self.send(Method::POST, path::AGENT_LOGIN_FINISH, body)
            .await
    }

    async fn finish(&mut self, flow: LoginFlowId, code: &str) -> LoginFinishResult {
        let (status, body) = self.finish_status(flow, code).await;
        assert_eq!(status, StatusCode::OK);
        serde_json::from_slice(&body).unwrap()
    }
}

fn json(value: &impl serde::Serialize) -> Bytes {
    serde_json::to_vec(value).unwrap().into()
}

fn logged_in() -> LoginFinishResult {
    LoginFinishResult::LoggedIn {
        expires: fake_expires(),
    }
}

/// Files under `dir`, at any depth, whose name ends in `suffix`.
fn files_ending(dir: &Path, suffix: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if entry.file_type().unwrap().is_dir() {
            found.extend(files_ending(&path, suffix));
        } else if path.to_string_lossy().ends_with(suffix) {
            found.push(path);
        }
    }
    found
}

// ---------------------------------------------------------------------------
// Verification 3, 13, 14: the happy path, and nothing of it kept

const SECRETS: [&str; 3] = ["fake-state", "good", "example.invalid"];

#[tokio::test(flavor = "multi_thread")]
async fn a_device_logs_the_agent_in_and_hello_shows_it() {
    let mut f = Fixture::start().await;
    f.mark("logged-out");
    f.login_state
        .apply(false, LoginSource::Start, &LoginSeen::started(0));
    let mut device = f.device().await;
    let hello = device.hello().await;
    assert!(hello.agent_logged_out);
    assert!(hello.agent_login_offered);
    assert_eq!(hello.login_expires, None);

    let flow = device.open().await;
    assert_eq!(device.finish(flow, "good#fake-state").await, logged_in());
    let hello = device.hello().await;
    assert_eq!(hello.login_expires, fake_expires());
    assert!(!hello.agent_logged_out);
    assert_eq!(f.login_state.status(), LoginStatus::LoggedIn);
    assert_eq!(f.login_state.successes(), 1);
    assert!(!f.login.join("logged-out").exists());
    // The process ends with the flow.
    appears(&f.work.join("term.txt")).await;

    // Verification 13: neither the URL, its state nor the code is stored
    // or logged.
    let mut kinds = f
        .storage
        .db
        .conn()
        .prepare("SELECT kind FROM events")
        .unwrap();
    let kinds: Vec<String> = kinds
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    for kind in &kinds {
        for secret in SECRETS {
            assert!(!kind.contains(secret), "{secret:?} stored in {kind}");
        }
    }
    let data = f.tmp.path().join("data");
    for file in files_ending(&data, ".db")
        .into_iter()
        .chain(files_ending(&data, ".db-wal"))
    {
        let bytes = std::fs::read(&file).unwrap();
        for secret in ["fake-state", "example.invalid", "good#"] {
            assert!(
                !bytes.windows(secret.len()).any(|w| w == secret.as_bytes()),
                "{secret:?} in {}",
                file.display()
            );
        }
    }
    // A finish the server refuses logs its reason: the capture sees the
    // server's own threads.
    let flow = device.open().await;
    assert_eq!(
        device.finish(flow, "nohash").await,
        LoginFinishResult::Failed {
            reason: PASTE_WHOLE_CODE.into()
        }
    );
    assert_eq!(device.finish(flow, "bad#x").await, failed_400());
    let logged = captured_text();
    assert!(logged.contains("login finish failed"), "{logged}");
    assert!(logged.contains(PASTE_WHOLE_CODE), "{logged}");
    for secret in SECRETS {
        assert!(!logged.contains(secret), "{secret:?} logged:\n{logged}");
    }

    // Verification 14: the login process leaves no conversation behind.
    let transcripts = ClaudeTranscripts::new(f.login.join("projects"));
    let listed = transcripts
        .list(
            std::slice::from_ref(&f.work),
            &Default::default(),
            100,
            Instant::now() + SOON,
        )
        .unwrap();
    assert!(listed.conversations.is_empty(), "{listed:?}");
    assert_eq!(files_ending(f.tmp.path(), ".jsonl"), Vec::<PathBuf>::new());
}

// ---------------------------------------------------------------------------
// Verification 4, 5: joining and busy

#[tokio::test(flavor = "multi_thread")]
async fn a_second_start_joins_the_open_flow() {
    let mut f = Fixture::start().await;
    let mut device = f.device().await;
    let flow = device.open().await;
    assert_eq!(device.open().await, flow);
    assert_eq!(f.lines("authenticates.txt"), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn racing_starts_run_one_login() {
    let mut f = Fixture::start().await;
    f.mark("auth-slow");
    let mut first = f.device().await;
    let mut second = f.device().await;
    let first = tokio::spawn(async move { first.start().await });
    let second = tokio::spawn(async move { second.start().await });
    // Both in flight before the agent answers either.
    tokio::time::timeout(SOON, f.joined.opened())
        .await
        .expect("no start joined");
    assert!(!first.is_finished() && !second.is_finished());
    f.cue_authenticate().await;
    let (first, second) = (first.await.unwrap(), second.await.unwrap());
    assert!(
        matches!(&first, LoginStartResult::Open { url, .. } if url == FAKE_MANUAL_URL),
        "{first:?}"
    );
    assert_eq!(first, second);
    assert_eq!(f.lines("authenticates.txt"), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_start_while_a_finish_runs_is_busy() {
    let mut f = Fixture::start().await;
    f.mark("callback-slow");
    let mut device = f.device().await;
    let flow = device.open().await;
    let finish = tokio::spawn({
        let mut device = f.device().await;
        async move { device.finish(flow, "good#fake-state").await }
    });
    appears(&f.login.join("callbacks.txt")).await;
    assert_eq!(device.start().await, LoginStartResult::Busy);
    f.cue_callback().await;
    assert_eq!(finish.await.unwrap(), logged_in());
}

// ---------------------------------------------------------------------------
// Verification 6, 7, 8: one finish per flow

/// The first finish gives up waiting (503) but goes on; the retry collects
/// its outcome, and the agent got one code.
#[tokio::test(flavor = "multi_thread")]
async fn a_finish_that_timed_out_is_collected_by_its_retry() {
    let mut f = Fixture::start_with(Config {
        finish_timeout: Duration::from_millis(500),
        ..Config::default()
    })
    .await;
    f.mark("callback-slow");
    let mut device = f.device().await;
    let flow = device.open().await;
    let (status, _) = device.finish_status(flow, "good#fake-state").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(f.lines("callbacks.txt"), 1);
    assert!(!f.work.join("term.txt").exists());

    f.cue_callback().await;
    let retried = tokio::time::timeout(SOON, async {
        loop {
            let (status, body) = device.finish_status(flow, "good#fake-state").await;
            if status != StatusCode::SERVICE_UNAVAILABLE {
                assert_eq!(status, StatusCode::OK);
                break serde_json::from_slice::<LoginFinishResult>(&body).unwrap();
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(retried, logged_in());
    assert_eq!(device.finish(flow, "other#code").await, logged_in());
    assert_eq!(f.lines("callbacks.txt"), 1);
    assert_eq!(f.login_state.successes(), 1);
}

fn failed_400() -> LoginFinishResult {
    LoginFinishResult::Failed {
        reason: "Request failed with status code 400".into(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_code_ends_the_flow_and_a_new_start_opens_another() {
    let mut f = Fixture::start().await;
    let mut device = f.device().await;
    let flow = device.open().await;
    assert_eq!(device.finish(flow, "bad#fake-state").await, failed_400());
    assert_eq!(device.finish(flow, "good#fake-state").await, failed_400());
    assert_eq!(f.lines("callbacks.txt"), 1);
    assert!(!f.login.join(".credentials.json").exists());
    assert_eq!(f.login_state.status(), LoginStatus::Unknown);

    let next = device.open().await;
    assert_ne!(next, flow);
    assert_eq!(f.lines("authenticates.txt"), 2);
    assert_eq!(device.finish(next, "good#fake-state").await, logged_in());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_misshapen_paste_leaves_the_flow_open() {
    let mut f = Fixture::start().await;
    let mut device = f.device().await;
    let flow = device.open().await;
    assert_eq!(
        device.finish(flow, "nohash").await,
        LoginFinishResult::Failed {
            reason: PASTE_WHOLE_CODE.into()
        }
    );
    assert!(!f.login.join("callbacks.txt").exists());
    assert_eq!(device.finish(flow, "good#fake-state").await, logged_in());
    assert_eq!(f.lines("callbacks.txt"), 1);
}

// ---------------------------------------------------------------------------
// Verification 8b: the slot is never locked while the agent answers

#[tokio::test(flavor = "multi_thread")]
async fn hello_answers_while_a_start_waits_for_the_agent() {
    let mut f = Fixture::start().await;
    f.mark("auth-slow");
    let mut starter = f.device().await;
    let mut device = f.device().await;
    let start = tokio::spawn(async move { starter.start().await });
    // The start is with the agent, which answers only on the cue.
    appears(&f.login.join("authenticates.txt")).await;
    let hello = tokio::time::timeout(Duration::from_secs(2), device.hello()).await;
    let started = start.is_finished();
    f.cue_authenticate().await;
    hello.expect("hello waited for the login start");
    assert!(!started);
    assert!(matches!(
        start.await.unwrap(),
        LoginStartResult::Open { .. }
    ));
}

// ---------------------------------------------------------------------------
// Verification 9, 10: flows the server no longer has

#[tokio::test(flavor = "multi_thread")]
async fn a_flow_the_server_never_had_or_lost_in_a_restart_is_none() {
    let mut f = Fixture::start().await;
    let mut device = f.device().await;
    assert_eq!(
        device
            .finish(LoginFlowId::random(), "good#fake-state")
            .await,
        LoginFinishResult::NoFlow
    );
    let flow = device.open().await;
    appears(&f.login.join("commands-sent")).await;
    f.restart().await;
    // The old server's flow went with it, and its process.
    appears(&f.work.join("term.txt")).await;
    let mut device = f.device().await;
    assert_eq!(
        device.finish(flow, "good#fake-state").await,
        LoginFinishResult::NoFlow
    );
    assert_eq!(f.lines("callbacks.txt"), 0);
}

/// With no time to live, the hello after a start drops its flow: the
/// process stops before any finish comes, and the finish finds none.
#[tokio::test(flavor = "multi_thread")]
async fn a_hello_drops_a_flow_left_open_too_long() {
    let mut f = Fixture::start_with(Config {
        flow_max: Duration::ZERO,
        ..Config::default()
    })
    .await;
    let mut device = f.device().await;
    let flow = device.open().await;
    // Its big unasked line read, the fake waits in `read`, where a SIGTERM
    // reaches its trap (one blocked writing to a full pipe would only be
    // killed after the grace, and write no term.txt).
    appears(&f.login.join("commands-sent")).await;
    assert!(!f.work.join("term.txt").exists());
    device.hello().await;
    appears(&f.work.join("term.txt")).await;
    assert_eq!(
        device.finish(flow, "good#fake-state").await,
        LoginFinishResult::NoFlow
    );
    assert_eq!(f.lines("callbacks.txt"), 0);
}

// ---------------------------------------------------------------------------
// Verification 11, 12: starts that fail

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_or_crashed_start_says_why_and_leaves_no_flow() {
    let mut f = Fixture::start().await;
    let mut device = f.device().await;
    f.mark("auth-refused");
    assert_eq!(
        device.start().await,
        LoginStartResult::Failed {
            reason: "Claude.ai login is disabled by your organization".into()
        }
    );
    f.unmark("auth-refused");
    f.mark("auth-crash");
    let LoginStartResult::Failed { reason } = device.start().await else {
        panic!("expected a failure");
    };
    assert!(reason.contains("OAuth is unavailable"), "{reason}");
    f.unmark("auth-crash");
    // Idle again: the next start asks the agent anew.
    device.open().await;
    assert_eq!(f.lines("authenticates.txt"), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_login_not_offered_is_unavailable() {
    let mut f = Fixture::start_with(Config {
        login: Some(Arc::new(NoAgentLogin)),
        ..Config::default()
    })
    .await;
    let mut device = f.device().await;
    assert_eq!(device.start().await, LoginStartResult::Unavailable);

    let mut f = Fixture::start().await;
    f.config.login = Some(Arc::new(f.claude(true)));
    f.restart().await;
    let mut device = f.device().await;
    assert_eq!(device.start().await, LoginStartResult::Unavailable);
    // Nothing was started.
    assert!(!f.work.join("argv.txt").exists());
}

// ---------------------------------------------------------------------------
// Verification 15: the code's cap

#[tokio::test(flavor = "multi_thread")]
async fn a_code_over_the_cap_is_a_bad_request() {
    let mut f = Fixture::start().await;
    let mut device = f.device().await;
    let flow = device.open().await;
    let long = format!("{}#fake-state", "x".repeat(limits::LOGIN_CODE_MAX_BYTES));
    let (status, _) = device.finish_status(flow, &long).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(f.lines("callbacks.txt"), 0);
    // At the cap it reaches the agent.
    let at_cap = format!(
        "{}#fake-state",
        "x".repeat(limits::LOGIN_CODE_MAX_BYTES - "#fake-state".len())
    );
    assert_eq!(device.finish(flow, &at_cap).await, failed_400());
    assert_eq!(f.lines("callbacks.txt"), 1);
}
