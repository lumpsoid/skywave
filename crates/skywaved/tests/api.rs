//! The mTLS HTTPS API (T06), over real TLS on 127.0.0.1.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use chrono::{DateTime, Utc};
use http_body_util::{BodyExt, Full};
use hyper::client::conn::http1::{self, SendRequest};
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::{Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use rcgen::{CertificateParams, KeyPair};
use rustls::RootCertStore;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use serde::de::DeserializeOwned;
use skywave_proto::{
    Attachment, BlobId, BlobRef, BlobStorage, BlobUse, ChangesResult, Command, CommandId,
    CommandKind, CommandResult, ContentHash, ContextId, ConversationsResult, DeviceId, DeviceInfo,
    DiffBody, DirEntry, DirsResult, EntryKind, EventBatch, EventKind, FileBody, FileChange,
    FileResult, HelloResponse, ImageType, ImportOutcome, LimitSeverity, LimitWindow, LineSpan,
    ListResult, LogResult, PROTOCOL_VERSION, PairRequest, PairResponse, RUN_HEADER, RejectReason,
    RepoListing, RunId, RunsSince, Seq, SessionId, SessionOrigin, SessionState, ShellEnd,
    ShellRunStorage, StorageReport, ToolResult, UPLOAD_OFFSET_HEADER, UploadResult, UsageLimit,
    UsageLimitsResult, Worktree, WorktreeList, WriteFileRequest, WriteFileResult, limits, path,
};
use skywaved::agent::claude_code::ClaudeCodeAdapter;
use skywaved::agent::claude_code::login::ClaudeLogin;
use skywaved::agent::fake::Gate;
use skywaved::agent::{
    AgentLogin, LimitsError, LoginAttempt, LoginError, LoginSeen, LoginSource, LoginState,
    NoAgentLogin, UsageLimitsSource,
};
use skywaved::blobs::BlobStore;
use skywaved::config::AgentConfig;
use skywaved::crop::CropHook;
use skywaved::disk::{Disk, FreeSpace};
use skywaved::identity::ServerIdentity;
use skywaved::server::{ConnectionLimits, Server, ServerOptions};
use skywaved::workdir::Git;
use skywaved::{EventLog, Storage, devices, pairing};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpSocket, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_rustls::TlsConnector;

/// Generous bound for things that should happen at once.
const SOON: Duration = Duration::from_secs(10);

struct Fixture {
    _tmp: TempDir,
    storage: Storage,
    identity: Arc<ServerIdentity>,
    log: EventLog,
    root: PathBuf,
    addr: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<skywaved::Result<()>>>,
}

/// A device's credentials: certificate and key PEM.
struct Credentials {
    id: DeviceId,
    cert_pem: String,
    key_pem: String,
}

impl Fixture {
    async fn start() -> Fixture {
        Fixture::start_with_shell(false).await
    }

    /// [`Fixture::start`] with shell commands on or off.
    async fn start_with_shell(shell: bool) -> Fixture {
        Fixture::start_with(shell, false, isolated_git(), ConnectionLimits::DEFAULT).await
    }

    /// [`Fixture::start`] with bypass mode allowed or not.
    async fn start_with_bypass(bypass: bool) -> Fixture {
        Fixture::start_with(false, bypass, isolated_git(), ConnectionLimits::DEFAULT).await
    }

    /// [`Fixture::start`] with workdir changes running git as `git` says.
    async fn start_with_git(git: Git) -> Fixture {
        Fixture::start_with(false, false, git, ConnectionLimits::DEFAULT).await
    }

    /// [`Fixture::start`] with connection limits `limits`.
    async fn start_with_limits(limits: ConnectionLimits) -> Fixture {
        Fixture::start_with(false, false, isolated_git(), limits).await
    }

    async fn start_with(shell: bool, bypass: bool, git: Git, limits: ConnectionLimits) -> Fixture {
        let options = ServerOptions {
            git,
            limits,
            ..ServerOptions::default()
        };
        Fixture::start_configured(|log| log.with_shell(shell).with_bypass(bypass), options).await
    }

    /// [`Fixture::start`] with shell commands on, the log watching `disk`
    /// and every crop running `hook`.
    async fn start_cropping(disk: Disk, hook: Option<Arc<dyn CropHook>>) -> Fixture {
        let options = ServerOptions {
            crop_hook: hook,
            ..ServerOptions::default()
        };
        Fixture::start_cropping_with(disk, options).await
    }

    /// [`Fixture::start_cropping`] with `options` (git isolated).
    async fn start_cropping_with(disk: Disk, options: ServerOptions) -> Fixture {
        let options = ServerOptions {
            git: isolated_git(),
            ..options
        };
        Fixture::start_configured(|log| log.with_shell(true).with_disk(disk), options).await
    }

    async fn start_configured(
        configure: impl FnOnce(EventLog) -> EventLog,
        options: ServerOptions,
    ) -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(root.join("project")).unwrap();
        let storage = Storage::open(tmp.path().join("data")).unwrap();
        let identity =
            Arc::new(ServerIdentity::bootstrap(&storage.dir, &["localhost".into()]).unwrap());
        let log = configure(EventLog::open(&storage.db, vec![root.clone()]).unwrap());
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
        Fixture {
            _tmp: tmp,
            storage,
            identity,
            log,
            root,
            addr,
            stop: Some(stop),
            task: Some(task),
        }
    }

    async fn anonymous(&self) -> Client {
        Client::connect(self.addr, self.identity.ca_cert_der(), None)
            .await
            .unwrap()
    }

    async fn client(&self, creds: &Credentials) -> Client {
        Client::connect(self.addr, self.identity.ca_cert_der(), Some(creds))
            .await
            .unwrap()
    }

    /// Pairs a new device through `POST /v1/pair`, like a real client.
    async fn pair(&self) -> Credentials {
        self.pair_as("laptop").await
    }

    /// [`Fixture::pair`] with the device name `name`.
    async fn pair_as(&self, name: &str) -> Credentials {
        let issued =
            pairing::issue_code(self.storage.db.conn(), Utc::now(), pairing::DEFAULT_TTL).unwrap();
        let key = KeyPair::generate().unwrap();
        let req = PairRequest {
            code: issued.code.to_string(),
            csr_pem: CertificateParams::default()
                .serialize_request(&key)
                .unwrap()
                .pem()
                .unwrap(),
            device_name: name.into(),
        };
        let resp = self
            .anonymous()
            .await
            .send(Method::POST, path::PAIR, &[], json(&req))
            .await;
        assert_eq!(resp.status, StatusCode::OK);
        let paired: PairResponse = resp.json();
        assert_eq!(paired.ca_cert_pem, self.identity.ca_cert_pem());
        Credentials {
            id: paired.device_id,
            cert_pem: paired.device_cert_pem,
            key_pem: key.serialize_pem(),
        }
    }

    fn create_session_command(&self) -> Command {
        Command {
            id: CommandId::new(),
            kind: CommandKind::CreateSession {
                workdir: self.root.join("project").display().to_string(),
                conversation: None,
                worktree: None,
            },
        }
    }

    /// Appends an agent message from outside the API, as the supervisor will.
    fn append(&mut self, session: SessionId, text: &str) -> Seq {
        self.log
            .append(
                &mut self.storage.db,
                session,
                ContextId::first_of(session),
                EventKind::AgentMessage {
                    parent: None,
                    text: text.into(),
                    full: None,
                },
            )
            .unwrap()
    }

    async fn stop(&mut self) {
        let _ = self.stop.take().unwrap().send(());
        tokio::time::timeout(SOON, self.task.take().unwrap())
            .await
            .expect("server stops promptly")
            .unwrap()
            .unwrap();
    }
}

/// Every test ends by checking that every row names a context of its own
/// session.
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
        if !std::thread::panicking() {
            let violations = self.storage.db.context_stamp_violations().unwrap();
            assert!(violations.is_empty(), "context stamps: {violations:#?}");
        }
    }
}

struct Client {
    sender: SendRequest<Full<Bytes>>,
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl Reply {
    fn json<T: DeserializeOwned>(&self) -> T {
        serde_json::from_slice(&self.body).unwrap()
    }

    fn header(&self, name: header::HeaderName) -> &str {
        self.headers
            .get(&name)
            .unwrap_or_else(|| panic!("missing {name}"))
            .to_str()
            .unwrap()
    }
}

fn client_config(ca_der: &[u8], creds: Option<&Credentials>) -> rustls::ClientConfig {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = RootCertStore::empty();
    roots.add(CertificateDer::from(ca_der.to_vec())).unwrap();
    let builder = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots);
    match creds {
        None => builder.with_no_client_auth(),
        Some(creds) => {
            let chain = CertificateDer::pem_slice_iter(creds.cert_pem.as_bytes())
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            let key = PrivateKeyDer::from_pem_slice(creds.key_pem.as_bytes()).unwrap();
            builder.with_client_auth_cert(chain, key).unwrap()
        }
    }
}

impl Client {
    async fn connect(
        addr: SocketAddr,
        ca_der: &[u8],
        creds: Option<&Credentials>,
    ) -> std::io::Result<Client> {
        let connector = TlsConnector::from(Arc::new(client_config(ca_der, creds)));
        let tcp = TcpStream::connect(addr).await?;
        let tls = connector
            .connect(ServerName::try_from("localhost").unwrap(), tcp)
            .await?;
        let (sender, conn) = http1::handshake(TokioIo::new(tls))
            .await
            .map_err(std::io::Error::other)?;
        tokio::spawn(conn);
        Ok(Client { sender })
    }

    async fn try_send(
        &mut self,
        method: Method,
        uri: &str,
        headers: &[(header::HeaderName, &str)],
        body: Bytes,
    ) -> hyper::Result<Reply> {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, "localhost");
        if !body.is_empty() {
            req = req.header(header::CONTENT_TYPE, "application/json");
        }
        for (name, value) in headers {
            req = req.header(name, HeaderValue::from_str(value).unwrap());
        }
        self.sender.ready().await?;
        let resp = self
            .sender
            .send_request(req.body(Full::new(body)).unwrap())
            .await?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = resp.into_body().collect().await?.to_bytes();
        Ok(Reply {
            status,
            headers,
            body,
        })
    }

    async fn send(
        &mut self,
        method: Method,
        uri: &str,
        headers: &[(header::HeaderName, &str)],
        body: Bytes,
    ) -> Reply {
        self.try_send(method, uri, headers, body).await.unwrap()
    }

    async fn get(&mut self, uri: &str) -> Reply {
        self.send(Method::GET, uri, &[], Bytes::new()).await
    }

    async fn command(&mut self, command: &Command) -> CommandResult {
        let reply = self
            .send(Method::POST, path::COMMANDS, &[], json(command))
            .await;
        assert_eq!(reply.status, StatusCode::OK);
        reply.json()
    }

    async fn events(&mut self, after: Seq, wait: u32) -> EventBatch {
        let reply = self.get(&events_uri(after, wait)).await;
        assert_eq!(reply.status, StatusCode::OK);
        reply.json()
    }
}

fn json(value: &impl serde::Serialize) -> Bytes {
    serde_json::to_vec(value).unwrap().into()
}

fn events_uri(after: Seq, wait: u32) -> String {
    format!("{}?after={}&wait={wait}", path::EVENTS, after.0)
}

fn session_of(result: &CommandResult, batch: &EventBatch) -> SessionId {
    let CommandResult::Accepted { seq } = result else {
        panic!("not accepted: {result:?}");
    };
    batch
        .iter()
        .find(|(s, _)| s == seq)
        .map(|(_, e)| e.session_id)
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_client_cert_only_pairing_answers() {
    let f = Fixture::start().await;
    let mut anon = f.anonymous().await;
    let blob = path::blob(BlobId::new());
    for (method, uri) in [
        (Method::GET, path::HELLO),
        (Method::POST, path::COMMANDS),
        (Method::GET, "/v1/events?after=0"),
        (Method::GET, path::CONVERSATIONS),
        (Method::GET, blob.as_str()),
    ] {
        let reply = anon.send(method.clone(), uri, &[], Bytes::new()).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{method} {uri}");
        assert!(reply.body.is_empty());
    }
    let command = json(&f.create_session_command());
    let reply = anon.send(Method::POST, path::COMMANDS, &[], command).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(f.log.head(), Seq::ZERO, "nothing was appended");
    assert_eq!(anon.get("/v1/other").await.status, StatusCode::NOT_FOUND);

    // Pairing works on a connection without a certificate.
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    assert_eq!(device.get(path::HELLO).await.status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread")]
async fn hello_names_the_caller_and_records_last_seen() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    assert_eq!(
        devices::get(f.storage.db.conn(), creds.id)
            .unwrap()
            .unwrap()
            .last_seen,
        None
    );
    let reply = f.client(&creds).await.get(path::HELLO).await;
    assert_eq!(reply.status, StatusCode::OK);
    let hello: HelloResponse = reply.json();
    assert_eq!(
        hello,
        HelloResponse {
            protocol_version: PROTOCOL_VERSION,
            server_id: f.identity.server_id(),
            device_id: creds.id,
            head: Seq::ZERO,
            log: f.log.run(&f.storage.db, None).unwrap(),
            devices: vec![DeviceInfo {
                id: creds.id,
                name: "laptop".into(),
            }],
            shell: false,
            bypass: false,
            login_expires: None,
            agent_login_offered: false,
            agent_logged_out: false,
        }
    );
    let device = devices::get(f.storage.db.conn(), creds.id)
        .unwrap()
        .unwrap();
    assert!(device.last_seen.is_some());
    // Telling a device whether the agent is logged in started no process
    // of the agent's (which would leave a `pid.txt`): it is learned from
    // session processes only. The walk does see files (the database).
    let files = files_under(f._tmp.path());
    assert!(files.iter().any(|p| p.ends_with("skywave.db")), "{files:?}");
    assert!(!files.iter().any(|p| p.ends_with("pid.txt")), "{files:?}");
}

/// Every file under `dir`, recursively.
fn files_under(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut dirs = vec![dir.to_owned()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files
}

/// `hello`'s log is the database's: its id stays with the database, and
/// every open of it is a new run, recorded with the head it found. Asked
/// about a run (`since`), it says whether the record has it and the lowest
/// start of the runs after it.
#[tokio::test(flavor = "multi_thread")]
async fn hello_reports_the_log_and_what_became_of_a_run() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut client = f.client(&creds).await;
    let first: HelloResponse = client.get(path::HELLO).await.json();
    assert_eq!(first.log.log, f.storage.db.log_id().unwrap());
    assert_eq!(first.log.since, None, "asked about no run");
    let since = |run: RunId| format!("{}?since={}", path::HELLO, run.0);
    let current: HelloResponse = client.get(&since(first.log.run)).await.json();
    assert_eq!(
        current.log.since,
        Some(RunsSince::Known { next_start: None }),
        "the current run"
    );
    let created = client.command(&f.create_session_command()).await;
    assert_eq!(created, CommandResult::Accepted { seq: Seq(1) });
    let later: HelloResponse = client.get(&since(first.log.run)).await.json();
    assert_eq!(later.log, current.log, "appending is not a new run");
    assert_eq!(later.head, Seq(1));
    let unknown: HelloResponse = client.get(&since(RunId::new())).await.json();
    assert_eq!(unknown.log.since, Some(RunsSince::Unknown));
    let bad = client.get(&format!("{}?since=nope", path::HELLO)).await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);

    let reopened = EventLog::open(&f.storage.db, vec![f.root.clone()]).unwrap();
    let again = reopened.run(&f.storage.db, Some(first.log.run)).unwrap();
    assert_eq!(again.log, first.log.log, "the same database");
    assert_ne!(again.run, first.log.run, "a new run");
    assert_eq!(
        again.since,
        Some(RunsSince::Known {
            next_start: Some(Seq(1))
        }),
        "started from the head it found"
    );

    let other = Fixture::start().await;
    let other = other
        .log
        .run(&other.storage.db, Some(first.log.run))
        .unwrap();
    assert_ne!(other.log, first.log.log, "another database");
    assert_eq!(other.since, Some(RunsSince::Unknown));
}

/// A request naming the run it expects ([`RUN_HEADER`]) goes through when
/// that is this run, and is refused with 412 when it is another, before
/// its handler: a command is not handled, so a resend later is not a
/// duplicate. Without the header nothing is checked; a malformed one is a
/// 400.
#[tokio::test(flavor = "multi_thread")]
async fn a_request_for_another_run_is_refused_untouched() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut client = f.client(&creds).await;
    let run = f.log.run_id();
    let this = run.0.to_string();
    let other = RunId::new().0.to_string();
    fn named(run: &str) -> [(header::HeaderName, &str); 1] {
        [(header::HeaderName::from_static(RUN_HEADER), run)]
    }

    let hello = client
        .send(Method::GET, path::HELLO, &named(&this), Bytes::new())
        .await;
    assert_eq!(hello.status, StatusCode::OK);
    assert_eq!(hello.json::<HelloResponse>().log.run, run);
    let command = f.create_session_command();
    let refused = client
        .send(Method::POST, path::COMMANDS, &named(&other), json(&command))
        .await;
    assert_eq!(refused.status, StatusCode::PRECONDITION_FAILED);
    assert_eq!(f.log.head(), Seq::ZERO, "not handled");
    let taken = client
        .send(Method::POST, path::COMMANDS, &named(&this), json(&command))
        .await;
    assert_eq!(taken.status, StatusCode::OK);
    assert_eq!(
        taken.json::<CommandResult>(),
        CommandResult::Accepted { seq: Seq(1) },
        "the first time it is handled"
    );
    // Every route, the read-only ones too: a blob that does not exist and
    // queries their handlers would refuse are refused as another run's
    // first.
    for uri in [
        path::HELLO.to_owned(),
        events_uri(Seq::ZERO, 0),
        path::blob(BlobId::new()),
        path::DIRS.to_owned(),
        path::CONVERSATIONS.to_owned(),
        path::WORKDIR_CHANGES.to_owned(),
        path::WORKDIR_LOG.to_owned(),
        path::WORKDIR_LIST.to_owned(),
        path::WORKDIR_FILE.to_owned(),
        path::WORKTREES.to_owned(),
    ] {
        let reply = client
            .send(Method::GET, &uri, &named(&other), Bytes::new())
            .await;
        assert_eq!(reply.status, StatusCode::PRECONDITION_FAILED, "{uri}");
    }
    let since = format!("{}?since={}", path::HELLO, run.0);
    let asked = client
        .send(Method::GET, &since, &named(&other), Bytes::new())
        .await;
    assert_eq!(
        asked.status,
        StatusCode::PRECONDITION_FAILED,
        "checked whatever the query"
    );
    let bare = client.get(&events_uri(Seq::ZERO, 0)).await;
    assert_eq!(bare.status, StatusCode::OK, "no header, no check");
    let bad = client
        .send(Method::GET, path::HELLO, &named("nope"), Bytes::new())
        .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);

    let mut stranger = Client::connect(f.addr, f.identity.ca_cert_der(), None)
        .await
        .unwrap();
    let unauthenticated = stranger
        .send(Method::GET, path::HELLO, &named(&other), Bytes::new())
        .await;
    assert_eq!(
        unauthenticated.status,
        StatusCode::UNAUTHORIZED,
        "authentication first"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn hello_reports_whether_shell_commands_are_on() {
    for shell in [false, true] {
        let f = Fixture::start_with_shell(shell).await;
        let creds = f.pair().await;
        let hello: HelloResponse = f.client(&creds).await.get(path::HELLO).await.json();
        assert_eq!(hello.shell, shell);
        assert!(!hello.bypass, "apart from bypass");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn hello_reports_whether_bypass_is_allowed() {
    for bypass in [false, true] {
        let f = Fixture::start_with_bypass(bypass).await;
        let creds = f.pair().await;
        let hello: HelloResponse = f.client(&creds).await.get(path::HELLO).await.json();
        assert_eq!(hello.bypass, bypass);
        assert!(!hello.shell, "apart from shell commands");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn hello_names_every_paired_device_revoked_ones_too() {
    let f = Fixture::start().await;
    let laptop = f.pair_as("laptop").await;
    let phone = f.pair_as("Pixel 8").await;
    devices::revoke(f.storage.db.conn(), phone.id, Utc::now()).unwrap();
    let hello: HelloResponse = f.client(&laptop).await.get(path::HELLO).await.json();
    let mut named: Vec<(DeviceId, String)> =
        hello.devices.into_iter().map(|d| (d.id, d.name)).collect();
    named.sort();
    let mut expected = vec![
        (laptop.id, "laptop".to_owned()),
        (phone.id, "Pixel 8".to_owned()),
    ];
    expected.sort();
    assert_eq!(named, expected);
}

#[tokio::test(flavor = "multi_thread")]
async fn revocation_applies_to_the_next_request_on_a_live_connection() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    assert_eq!(device.get(path::HELLO).await.status, StatusCode::OK);

    devices::revoke(f.storage.db.conn(), creds.id, Utc::now()).unwrap();
    let reply = device.get(path::HELLO).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert!(reply.body.is_empty());
    let command = json(&f.create_session_command());
    let reply = device
        .send(Method::POST, path::COMMANDS, &[], command)
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(f.log.head(), Seq::ZERO);

    // A new connection fares no better.
    let reply = f.client(&creds).await.get(path::HELLO).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_ca_signed_cert_not_in_the_registry_is_forbidden() {
    let f = Fixture::start().await;
    let key = KeyPair::generate().unwrap();
    let csr = CertificateParams::default()
        .serialize_request(&key)
        .unwrap()
        .pem()
        .unwrap();
    let cert = f.identity.sign_device_csr(&csr, DeviceId::new()).unwrap();
    let creds = Credentials {
        id: DeviceId::new(),
        cert_pem: cert.cert_pem,
        key_pem: key.serialize_pem(),
    };
    let reply = f.client(&creds).await.get(path::HELLO).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cert_from_another_ca_fails_the_handshake() {
    let f = Fixture::start().await;
    let key = KeyPair::generate().unwrap();
    let cert = CertificateParams::new(vec!["intruder".into()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    let creds = Credentials {
        id: DeviceId::new(),
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
    };
    // With TLS 1.3 the client learns of the rejection on first use.
    let failed = match Client::connect(f.addr, f.identity.ca_cert_der(), Some(&creds)).await {
        Err(_) => true,
        Ok(mut client) => client
            .try_send(Method::GET, path::HELLO, &[], Bytes::new())
            .await
            .is_err(),
    };
    assert!(failed);
}

#[tokio::test(flavor = "multi_thread")]
async fn commands_are_deduplicated_and_rejections_are_results() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;

    let create = f.create_session_command();
    let first = device.command(&create).await;
    assert_eq!(first, CommandResult::Accepted { seq: Seq(1) });
    assert_eq!(device.command(&create).await, first);
    // Another connection, same command id: same answer.
    assert_eq!(f.client(&creds).await.command(&create).await, first);

    let unknown = Command {
        id: CommandId::new(),
        kind: CommandKind::SendMessage {
            session_id: SessionId::new(),
            context: ContextId::first_of(SessionId::new()),
            text: "hi".into(),
            attachments: Vec::new(),
        },
    };
    let rejected = CommandResult::Rejected {
        reason: RejectReason::UnknownSession,
    };
    assert_eq!(device.command(&unknown).await, rejected);
    assert_eq!(device.command(&unknown).await, rejected);

    let batch = device.events(Seq::ZERO, 0).await;
    assert_eq!(batch.from, Seq(1));
    assert_eq!(batch.events.len(), 1, "one copy in the log");

    // A user message records the device that sent it.
    let session = session_of(&first, &batch);
    let send = Command {
        id: CommandId::new(),
        kind: CommandKind::SendMessage {
            session_id: session,
            context: ContextId::first_of(session),
            text: "hello".into(),
            attachments: Vec::new(),
        },
    };
    assert_eq!(
        device.command(&send).await,
        CommandResult::Accepted { seq: Seq(2) }
    );
    let batch = device.events(Seq(1), 0).await;
    let EventKind::UserMessage { device_id, .. } = &batch.events[0].kind else {
        panic!("{batch:?}");
    };
    assert_eq!(*device_id, creds.id);

    for body in [&b"{not json"[..], br#"{"id":"x"}"#, b""] {
        let reply = device
            .send(Method::POST, path::COMMANDS, &[], Bytes::from_static(body))
            .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    }
    assert_eq!(f.log.head(), Seq(2));
}

/// Adopting an outside conversation over the wire: the trimmed id comes
/// back in the `SessionCreated`, and the command names the
/// `ImportFinished` after it (`Unavailable`: this server reads no outside
/// conversations); a bad id is a result, not an error, and stays one on
/// resend.
#[tokio::test(flavor = "multi_thread")]
async fn create_session_adopts_a_conversation_by_its_id() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let adopt = |conversation: &str| Command {
        id: CommandId::new(),
        kind: CommandKind::CreateSession {
            workdir: f.root.join("project").display().to_string(),
            conversation: Some(conversation.to_owned()),
            worktree: None,
        },
    };

    let bad = adopt("not an id");
    let rejected = CommandResult::Rejected {
        reason: RejectReason::BadConversation,
    };
    assert_eq!(device.command(&bad).await, rejected);
    assert_eq!(device.command(&bad).await, rejected);
    assert_eq!(f.log.head(), Seq::ZERO);

    let good = adopt(" 9b2c1e4a-5f3d-4c8e-a1b2-c3d4e5f6a7b8 ");
    let accepted = device.command(&good).await;
    assert_eq!(accepted, CommandResult::Accepted { seq: Seq(2) });
    assert_eq!(device.command(&good).await, accepted);
    let batch = device.events(Seq::ZERO, 0).await;
    assert_eq!(batch.events.len(), 2);
    assert_eq!(batch.events[1].command_id, Some(good.id));
    assert_eq!(
        batch.events[1].kind,
        EventKind::ImportFinished {
            outcome: ImportOutcome::Unavailable
        }
    );
    let event = &batch.events[0];
    assert_eq!(event.command_id, None);
    let EventKind::SessionCreated { from, .. } = &event.kind else {
        panic!("{batch:?}");
    };
    assert_eq!(
        from,
        &Some(SessionOrigin::Agent {
            conversation: "9b2c1e4a-5f3d-4c8e-a1b2-c3d4e5f6a7b8".into(),
        })
    );
}

/// Makes `dir` a repository with one commit on `main`: a worktree starts
/// from the commit.
fn make_repo(dir: &std::path::Path) {
    setup_git(dir, &["init", "-q"]);
    setup_git(dir, &["commit", "-q", "--allow-empty", "-m", "first"]);
}

/// Whether the repository at `dir` has the local branch `branch`.
fn has_branch(dir: &std::path::Path, branch: &str) -> bool {
    let full = format!("refs/heads/{branch}");
    std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--verify", "--quiet", full.as_str()])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap_or_else(|e| panic!("this test needs `git` on PATH: {e}"))
        .status
        .success()
}

/// A `CreateSession` of the fixture's `project` on a new worktree `branch`.
fn worktree_command(f: &Fixture, branch: &str) -> Command {
    Command {
        id: CommandId::new(),
        kind: CommandKind::CreateSession {
            workdir: f.root.join("project").display().to_string(),
            conversation: None,
            worktree: Some(branch.into()),
        },
    }
}

/// How many worktree intents the database holds.
fn intents(f: &Fixture) -> i64 {
    f.storage
        .db
        .conn()
        .query_row("SELECT COUNT(*) FROM worktree_intents", [], |r| r.get(0))
        .unwrap()
}

/// A `CreateSession` naming a worktree makes it next to the repository and
/// starts the session there. Resending it answers the same and logs nothing
/// more.
#[tokio::test(flavor = "multi_thread")]
async fn create_session_makes_a_worktree_and_runs_there() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let project = f.root.join("project");
    make_repo(&project);
    let command = worktree_command(&f, "feat/x");

    let accepted = device.command(&command).await;
    assert_eq!(accepted, CommandResult::Accepted { seq: Seq(1) });
    assert_eq!(device.command(&command).await, accepted);
    assert_eq!(f.log.head(), Seq(1));
    assert_eq!(intents(&f), 0);
    let batch = device.events(Seq::ZERO, 0).await;
    assert_eq!(batch.events.len(), 1);
    let event = &batch.events[0];
    assert_eq!(event.command_id, Some(command.id));
    let EventKind::SessionCreated {
        workdir,
        from,
        worktree,
        ..
    } = &event.kind
    else {
        panic!("{batch:?}");
    };
    let made = f.root.join("project.worktrees/feat-x");
    assert!(made.is_dir());
    assert_eq!(from, &None);
    assert_eq!(
        workdir,
        &std::fs::canonicalize(&made).unwrap().display().to_string()
    );
    assert_eq!(
        worktree,
        &Some(Worktree {
            repo: std::fs::canonicalize(&project)
                .unwrap()
                .display()
                .to_string(),
            branch: "feat/x".into(),
        })
    );
}

/// A worktree on a branch that already exists is refused and makes no
/// directory. The same create on a free branch then goes through.
#[tokio::test(flavor = "multi_thread")]
async fn create_session_refuses_an_existing_branch() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    make_repo(&f.root.join("project"));

    assert_eq!(
        device.command(&worktree_command(&f, "main")).await,
        rejected(RejectReason::BranchExists)
    );
    assert_eq!(f.log.head(), Seq::ZERO);
    assert!(!f.root.join("project.worktrees").exists());
    // The check comes before the record: a pre-existing branch is never recorded.
    assert_eq!(intents(&f), 0);
    // Positive control: a free branch from the same fixture is created.
    let free = device.command(&worktree_command(&f, "feat/x")).await;
    assert_eq!(accepted_seq(&free), Seq(1));
    assert!(f.root.join("project.worktrees/feat-x").is_dir());
}

/// A worktree needs a repository: a plain directory is refused and nothing
/// is created.
#[tokio::test(flavor = "multi_thread")]
async fn create_session_refuses_a_worktree_outside_a_repository() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;

    assert_eq!(
        device.command(&worktree_command(&f, "feat/x")).await,
        rejected(RejectReason::NotARepository)
    );
    assert_eq!(f.log.head(), Seq::ZERO);
    assert!(!f.root.join("project.worktrees").exists());
    // Positive control: once the directory is a repository, the same command
    // is accepted.
    make_repo(&f.root.join("project"));
    let made = device.command(&worktree_command(&f, "feat/x")).await;
    assert_eq!(accepted_seq(&made), Seq(1));
}

/// D30: a worktree with a conversation is refused before anything is
/// created, the branch included.
#[tokio::test(flavor = "multi_thread")]
async fn create_session_refuses_a_worktree_with_a_conversation() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let project = f.root.join("project");
    make_repo(&project);
    let adopting = Command {
        id: CommandId::new(),
        kind: CommandKind::CreateSession {
            workdir: project.display().to_string(),
            conversation: Some("9b2c1e4a-5f3d-4c8e-a1b2-c3d4e5f6a7b8".into()),
            worktree: Some("feat/x".into()),
        },
    };

    assert_eq!(
        device.command(&adopting).await,
        rejected(RejectReason::BadConversation)
    );
    assert_eq!(f.log.head(), Seq::ZERO);
    assert!(!f.root.join("project.worktrees").exists());
    assert!(!has_branch(&project, "feat/x"));
    // Positive control: the same branch without the conversation is created.
    let made = device.command(&worktree_command(&f, "feat/x")).await;
    assert_eq!(accepted_seq(&made), Seq(1));
    assert!(has_branch(&project, "feat/x"));
}

/// A worktree made for a session the log then refuses is discarded with its
/// branch. Were the branch left behind, creating it again would be refused
/// with `BranchExists`.
#[tokio::test(flavor = "multi_thread")]
async fn a_worktree_whose_session_is_rejected_is_discarded() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let project = f.root.join("project");
    make_repo(&project);

    // A stopping server refuses new work after the worktree is made.
    f.log.set_stopping(true);
    assert_eq!(
        device.command(&worktree_command(&f, "feat/x")).await,
        rejected(RejectReason::ServerStopping)
    );
    assert_eq!(f.log.head(), Seq::ZERO);
    assert!(!f.root.join("project.worktrees/feat-x").exists());
    assert!(!has_branch(&project, "feat/x"));
    // Positive control: running again, the same branch is created.
    f.log.set_stopping(false);
    let made = device.command(&worktree_command(&f, "feat/x")).await;
    assert_eq!(accepted_seq(&made), Seq(1));
}

/// A worktree add that fails after it recorded its intent (a failing
/// `post-checkout` hook) is refused and undoes the add. Its intent row goes
/// with the refusal, so no row is left for a branch that is gone.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_worktree_add_leaves_no_intent() {
    use std::os::unix::fs::PermissionsExt;

    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let project = f.root.join("project");
    make_repo(&project);
    let hooks = f.root.join("hooks");
    std::fs::create_dir(&hooks).unwrap();
    let hook = hooks.join("post-checkout");
    std::fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    setup_git(
        &project,
        &["config", "core.hooksPath", hooks.to_str().unwrap()],
    );

    assert_eq!(
        device.command(&worktree_command(&f, "feat/x")).await,
        rejected(RejectReason::WorktreeFailed)
    );
    assert_eq!(intents(&f), 0);
    assert!(!has_branch(&project, "feat/x"));
    // Positive control: without the hook the same branch is created.
    setup_git(&project, &["config", "--unset", "core.hooksPath"]);
    let made = device.command(&worktree_command(&f, "feat/x")).await;
    assert_eq!(accepted_seq(&made), Seq(1));
    assert_eq!(intents(&f), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn removing_the_worktree_of_an_unknown_session_is_refused() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let session = new_session(&f, &mut device).await;

    let remove = |session_id| Command {
        id: CommandId::new(),
        kind: CommandKind::RemoveWorktree { session_id },
    };
    assert_eq!(
        device.command(&remove(SessionId::new())).await,
        rejected(RejectReason::UnknownSession)
    );
    // Positive control: the same command for a known session is not refused
    // as unknown, so the refusal above is about the session id.
    assert_ne!(
        device.command(&remove(session)).await,
        rejected(RejectReason::UnknownSession)
    );
}

/// A `RemoveWorktree` of `session`, under a new command id.
fn remove_command(session: SessionId) -> Command {
    Command {
        id: CommandId::new(),
        kind: CommandKind::RemoveWorktree {
            session_id: session,
        },
    }
}

/// Stands for a supervisor that has handled every event logged so far:
/// this fixture runs none, so its cursor stays at 0.
fn supervisor_caught_up(f: &Fixture) {
    f.storage
        .db
        .conn()
        .execute(
            "UPDATE supervisor_cursor SET seq = \
             (SELECT COALESCE(MAX(seq), 0) FROM events) WHERE id = 1",
            [],
        )
        .unwrap();
}

/// A merged, clean worktree is removed with its branch, and the removal is
/// logged. Resending the same command answers the same. A new command for the
/// same session is accepted too: the removal is already done.
#[tokio::test(flavor = "multi_thread")]
async fn removing_a_merged_clean_worktree_deletes_it_and_its_branch() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let project = f.root.join("project");
    make_repo(&project);
    let created = device.command(&worktree_command(&f, "feat/x")).await;
    let session = session_of(&created, &device.events(Seq::ZERO, 0).await);
    let dir = f.root.join("project.worktrees/feat-x");
    assert!(dir.is_dir());

    let command = remove_command(session);
    let accepted = device.command(&command).await;
    let seq = accepted_seq(&accepted);
    assert!(!dir.exists());
    assert!(!has_branch(&project, "feat/x"));
    let batch = device.events(Seq::ZERO, 0).await;
    let removed = batch.iter().find(|(s, _)| *s == seq).map(|(_, e)| e);
    let removed = removed.expect("the accepted removal is in the log");
    assert!(
        matches!(removed.kind, EventKind::WorktreeRemoved { .. }),
        "{removed:?}"
    );
    assert_eq!(device.command(&command).await, accepted);
    let again = device.command(&remove_command(session)).await;
    assert!(accepted_seq(&again).0 > seq.0, "{again:?}");
}

/// Work for a session whose worktree was removed is refused `WorkdirNotFound`:
/// its directory is gone, or may belong to another worktree since. Control for
/// it, a rename, is still accepted.
#[tokio::test(flavor = "multi_thread")]
async fn work_for_a_removed_worktree_session_is_refused() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let project = f.root.join("project");
    make_repo(&project);
    let created = device.command(&worktree_command(&f, "feat/x")).await;
    let session = session_of(&created, &device.events(Seq::ZERO, 0).await);
    let context = ContextId::first_of(session);
    // Positive control: before the removal, the same message is accepted.
    accepted_seq(
        &device
            .command(&message_command(session, context, "hello", Vec::new()))
            .await,
    );
    supervisor_caught_up(&f);
    accepted_seq(&device.command(&remove_command(session)).await);

    assert_eq!(
        device
            .command(&message_command(session, context, "again", Vec::new()))
            .await,
        rejected(RejectReason::WorkdirNotFound)
    );
    let model = Command {
        id: CommandId::new(),
        kind: CommandKind::SetModel {
            session_id: session,
            model: "opus".into(),
        },
    };
    assert_eq!(
        device.command(&model).await,
        rejected(RejectReason::WorkdirNotFound)
    );
    let rename = Command {
        id: CommandId::new(),
        kind: CommandKind::RenameSession {
            session_id: session,
            name: Some("renamed".into()),
        },
    };
    accepted_seq(&device.command(&rename).await);
}

/// A plain session in a removed worktree's directory is refused
/// `WorkdirNotFound` too: the directory is gone for it as well. Control for it,
/// before the removal, is accepted.
#[tokio::test(flavor = "multi_thread")]
async fn work_for_a_session_sharing_a_removed_worktree_is_refused() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let project = f.root.join("project");
    make_repo(&project);
    let created = device.command(&worktree_command(&f, "feat/x")).await;
    let owner = session_of(&created, &device.events(Seq::ZERO, 0).await);
    let dir = f.root.join("project.worktrees/feat-x");
    let plain = Command {
        id: CommandId::new(),
        kind: CommandKind::CreateSession {
            workdir: dir.display().to_string(),
            conversation: None,
            worktree: None,
        },
    };
    let made = device.command(&plain).await;
    let other = session_of(&made, &device.events(Seq::ZERO, 0).await);
    let context = ContextId::first_of(other);
    // Positive control: before the removal, the other session's message is accepted.
    accepted_seq(
        &device
            .command(&message_command(other, context, "hello", Vec::new()))
            .await,
    );
    supervisor_caught_up(&f);
    accepted_seq(&device.command(&remove_command(owner)).await);

    assert_eq!(
        device
            .command(&message_command(other, context, "again", Vec::new()))
            .await,
        rejected(RejectReason::WorkdirNotFound)
    );
}

/// A worktree made at a removed worktree's path again makes the directory
/// usable again, so a session sharing it works once more. Until then it is
/// refused.
#[tokio::test(flavor = "multi_thread")]
async fn a_recreated_worktree_lets_a_sharing_session_work_again() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let project = f.root.join("project");
    make_repo(&project);
    let created = device.command(&worktree_command(&f, "feat/x")).await;
    let owner = session_of(&created, &device.events(Seq::ZERO, 0).await);
    let dir = f.root.join("project.worktrees/feat-x");
    let plain = Command {
        id: CommandId::new(),
        kind: CommandKind::CreateSession {
            workdir: dir.display().to_string(),
            conversation: None,
            worktree: None,
        },
    };
    let made = device.command(&plain).await;
    let other = session_of(&made, &device.events(Seq::ZERO, 0).await);
    let context = ContextId::first_of(other);
    accepted_seq(&device.command(&remove_command(owner)).await);
    assert_eq!(
        device
            .command(&message_command(other, context, "refused", Vec::new()))
            .await,
        rejected(RejectReason::WorkdirNotFound)
    );

    accepted_seq(&device.command(&worktree_command(&f, "feat/x")).await);
    assert!(dir.is_dir());
    accepted_seq(
        &device
            .command(&message_command(other, context, "again", Vec::new()))
            .await,
    );
}

/// Uncommitted work keeps the worktree and its branch. Once the file is gone,
/// the same session's removal goes through.
#[tokio::test(flavor = "multi_thread")]
async fn a_dirty_worktree_is_not_removed() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let project = f.root.join("project");
    make_repo(&project);
    let created = device.command(&worktree_command(&f, "feat/x")).await;
    let session = session_of(&created, &device.events(Seq::ZERO, 0).await);
    let dir = f.root.join("project.worktrees/feat-x");
    let scratch = dir.join("notes.txt");
    std::fs::write(&scratch, "untracked").unwrap();

    assert_eq!(
        device.command(&remove_command(session)).await,
        rejected(RejectReason::WorktreeDirty)
    );
    assert!(dir.is_dir());
    assert!(has_branch(&project, "feat/x"));
    // Positive control: once the file is gone, the same worktree is removed.
    std::fs::remove_file(&scratch).unwrap();
    accepted_seq(&device.command(&remove_command(session)).await);
    assert!(!dir.exists());
    assert!(!has_branch(&project, "feat/x"));
}

/// A branch with a commit its `HEAD` lacks is not merged, so the worktree and
/// branch stay. Once the branch is merged, the removal goes through.
#[tokio::test(flavor = "multi_thread")]
async fn an_unmerged_worktree_branch_is_not_removed() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let project = f.root.join("project");
    make_repo(&project);
    let created = device.command(&worktree_command(&f, "feat/x")).await;
    let session = session_of(&created, &device.events(Seq::ZERO, 0).await);
    let dir = f.root.join("project.worktrees/feat-x");
    setup_git(&dir, &["commit", "-q", "--allow-empty", "-m", "ahead"]);

    assert_eq!(
        device.command(&remove_command(session)).await,
        rejected(RejectReason::BranchUnmerged)
    );
    assert!(dir.is_dir());
    assert!(has_branch(&project, "feat/x"));
    // Positive control: once the branch is merged, the removal goes through.
    setup_git(&project, &["merge", "-q", "--ff-only", "feat/x"]);
    accepted_seq(&device.command(&remove_command(session)).await);
    assert!(!dir.exists());
    assert!(!has_branch(&project, "feat/x"));
}

/// A session that runs in no worktree has nothing to remove.
#[tokio::test(flavor = "multi_thread")]
async fn a_plain_session_has_no_worktree_to_remove() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let plain = new_session(&f, &mut device).await;

    assert_eq!(
        device.command(&remove_command(plain)).await,
        rejected(RejectReason::NotAWorktree)
    );
    // Positive control: a session that does run in a worktree is removed, so
    // the refusal above is about the session, not about the command.
    make_repo(&f.root.join("project"));
    let created = device.command(&worktree_command(&f, "feat/x")).await;
    let session = session_of(&created, &device.events(Seq::ZERO, 0).await);
    accepted_seq(&device.command(&remove_command(session)).await);
}

/// A running turn keeps the worktree: the refusal comes before git runs. Once
/// the turn is over, the removal goes through.
#[tokio::test(flavor = "multi_thread")]
async fn a_worktree_is_not_removed_while_a_turn_runs() {
    let mut f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let project = f.root.join("project");
    make_repo(&project);
    let created = device.command(&worktree_command(&f, "feat/x")).await;
    let session = session_of(&created, &device.events(Seq::ZERO, 0).await);
    let dir = f.root.join("project.worktrees/feat-x");
    let context = f
        .log
        .session(&f.storage.db, session)
        .unwrap()
        .unwrap()
        .context;
    let working = EventKind::SessionStateChanged(SessionState::AgentWorking);
    f.log
        .append(&mut f.storage.db, session, context, working)
        .unwrap();

    assert_eq!(
        device.command(&remove_command(session)).await,
        rejected(RejectReason::TurnRunning)
    );
    assert!(dir.is_dir());
    assert!(has_branch(&project, "feat/x"));
    // Positive control: once the turn is idle, the same worktree is removed.
    let idle = EventKind::SessionStateChanged(SessionState::Idle);
    f.log
        .append(&mut f.storage.db, session, context, idle)
        .unwrap();
    accepted_seq(&device.command(&remove_command(session)).await);
    assert!(!dir.exists());
}

/// A message the supervisor has not handled yet keeps the worktree: the
/// refusal comes before git runs. Once the supervisor has handled it, the
/// removal goes through.
#[tokio::test(flavor = "multi_thread")]
async fn a_worktree_is_not_removed_while_a_message_waits_for_the_supervisor() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let project = f.root.join("project");
    make_repo(&project);
    let created = device.command(&worktree_command(&f, "feat/x")).await;
    let session = session_of(&created, &device.events(Seq::ZERO, 0).await);
    let context = ContextId::first_of(session);
    let dir = f.root.join("project.worktrees/feat-x");
    accepted_seq(
        &device
            .command(&message_command(session, context, "hello", Vec::new()))
            .await,
    );

    assert_eq!(
        device.command(&remove_command(session)).await,
        rejected(RejectReason::TurnRunning)
    );
    assert!(dir.is_dir());
    // Positive control: once the supervisor has handled the message, the
    // same worktree is removed.
    supervisor_caught_up(&f);
    accepted_seq(&device.command(&remove_command(session)).await);
    assert!(!dir.exists());
}

/// A message handed to its agent, whose turn has not started, keeps the
/// worktree. Once the handed row is gone, the removal goes through.
#[tokio::test(flavor = "multi_thread")]
async fn a_worktree_is_not_removed_while_a_handed_message_waits_for_its_turn() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let project = f.root.join("project");
    make_repo(&project);
    let created = device.command(&worktree_command(&f, "feat/x")).await;
    let session = session_of(&created, &device.events(Seq::ZERO, 0).await);
    let context = ContextId::first_of(session);
    let dir = f.root.join("project.worktrees/feat-x");
    let message = accepted_seq(
        &device
            .command(&message_command(session, context, "hello", Vec::new()))
            .await,
    );
    supervisor_caught_up(&f);
    f.storage
        .db
        .conn()
        .execute(
            "INSERT INTO supervisor_owed (seq, session_id, context, state) \
             SELECT seq, session_id, context, 'waiting' FROM events WHERE seq = ?1",
            [i64::try_from(message.0).unwrap()],
        )
        .unwrap();

    assert_eq!(
        device.command(&remove_command(session)).await,
        rejected(RejectReason::TurnRunning)
    );
    assert!(dir.is_dir());
    // Positive control: once nothing is owed, the same worktree is removed.
    f.storage
        .db
        .conn()
        .execute("DELETE FROM supervisor_owed", [])
        .unwrap();
    accepted_seq(&device.command(&remove_command(session)).await);
    assert!(!dir.exists());
}

/// A held session's events after its held seq are not handled yet, though the
/// cursor is past them, so a message there keeps the worktree. Once the held
/// row is gone, the removal goes through.
#[tokio::test(flavor = "multi_thread")]
async fn a_worktree_is_not_removed_while_a_held_message_waits() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let project = f.root.join("project");
    make_repo(&project);
    let created = device.command(&worktree_command(&f, "feat/x")).await;
    let session = session_of(&created, &device.events(Seq::ZERO, 0).await);
    let context = ContextId::first_of(session);
    let dir = f.root.join("project.worktrees/feat-x");
    let message = accepted_seq(
        &device
            .command(&message_command(session, context, "hello", Vec::new()))
            .await,
    );
    supervisor_caught_up(&f);
    f.storage
        .db
        .conn()
        .execute(
            "INSERT INTO supervisor_held (session_id, seq) VALUES (?1, ?2)",
            rusqlite::params![session.0.to_string(), i64::try_from(message.0 - 1).unwrap()],
        )
        .unwrap();

    assert_eq!(
        device.command(&remove_command(session)).await,
        rejected(RejectReason::TurnRunning)
    );
    assert!(dir.is_dir());
    // Positive control: once the session is no longer held, the same worktree
    // is removed.
    f.storage
        .db
        .conn()
        .execute("DELETE FROM supervisor_held", [])
        .unwrap();
    accepted_seq(&device.command(&remove_command(session)).await);
    assert!(!dir.exists());
}

/// A shell command queued or running in the worktree keeps it: the refusal
/// comes before git runs. Once the run has finished, the removal goes through.
#[tokio::test(flavor = "multi_thread")]
async fn a_worktree_is_not_removed_while_a_shell_command_waits() {
    let f = Fixture::start_with_shell(true).await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let project = f.root.join("project");
    make_repo(&project);
    let created = device.command(&worktree_command(&f, "feat/x")).await;
    let session = session_of(&created, &device.events(Seq::ZERO, 0).await);
    let dir = f.root.join("project.worktrees/feat-x");
    let run = Command {
        id: CommandId::new(),
        kind: CommandKind::RunShell {
            session_id: session,
            command: "true".into(),
            when: Default::default(),
        },
    };
    accepted_seq(&device.command(&run).await);

    assert_eq!(
        device.command(&remove_command(session)).await,
        rejected(RejectReason::ShellRunning)
    );
    assert!(dir.is_dir());
    assert!(has_branch(&project, "feat/x"));
    // Positive control: once the run has finished, the same worktree is
    // removed. Nothing runs the command here, so the finish is stored by hand.
    f.storage
        .db
        .conn()
        .execute(
            "UPDATE shell_runs SET finished_seq = (SELECT MAX(seq) FROM events)",
            [],
        )
        .unwrap();
    accepted_seq(&device.command(&remove_command(session)).await);
    assert!(!dir.exists());
}

/// `git worktree list --porcelain` of the repository at `dir`.
fn worktree_list(dir: &std::path::Path) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["worktree", "list", "--porcelain"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap_or_else(|e| panic!("this test needs `git` on PATH: {e}"));
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A session whose worktree was removed does not remove a worktree made at
/// the same path since: its removal is already done, so git does not run.
#[tokio::test(flavor = "multi_thread")]
async fn a_removed_worktree_session_does_not_remove_a_recreated_worktree() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let project = f.root.join("project");
    make_repo(&project);
    let first = device.command(&worktree_command(&f, "feat/x")).await;
    let old = session_of(&first, &device.events(Seq::ZERO, 0).await);
    accepted_seq(&device.command(&remove_command(old)).await);
    let second = device.command(&worktree_command(&f, "feat/x")).await;
    let current = session_of(&second, &device.events(Seq::ZERO, 0).await);
    let dir = f.root.join("project.worktrees/feat-x");
    assert_ne!(old, current);

    accepted_seq(&device.command(&remove_command(old)).await);
    assert!(dir.is_dir());
    assert!(has_branch(&project, "feat/x"));
    // Positive control: the session that owns the new worktree removes it.
    accepted_seq(&device.command(&remove_command(current)).await);
    assert!(!dir.exists());
}

/// A turn in another session of the worktree's directory keeps the worktree,
/// as one in the session that owns it does.
#[tokio::test(flavor = "multi_thread")]
async fn a_worktree_is_not_removed_while_another_session_there_runs_a_turn() {
    let mut f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let project = f.root.join("project");
    make_repo(&project);
    let created = device.command(&worktree_command(&f, "feat/x")).await;
    let owner = session_of(&created, &device.events(Seq::ZERO, 0).await);
    let dir = f.root.join("project.worktrees/feat-x");
    let plain = Command {
        id: CommandId::new(),
        kind: CommandKind::CreateSession {
            workdir: dir.display().to_string(),
            conversation: None,
            worktree: None,
        },
    };
    let made = device.command(&plain).await;
    let other = session_of(&made, &device.events(Seq::ZERO, 0).await);
    let context = f
        .log
        .session(&f.storage.db, other)
        .unwrap()
        .unwrap()
        .context;
    let working = EventKind::SessionStateChanged(SessionState::AgentWorking);
    f.log
        .append(&mut f.storage.db, other, context, working)
        .unwrap();

    assert_eq!(
        device.command(&remove_command(owner)).await,
        rejected(RejectReason::TurnRunning)
    );
    assert!(dir.is_dir());
    assert!(has_branch(&project, "feat/x"));
    // Positive control: once the other session is idle, the removal goes through.
    let idle = EventKind::SessionStateChanged(SessionState::Idle);
    f.log
        .append(&mut f.storage.db, other, context, idle)
        .unwrap();
    accepted_seq(&device.command(&remove_command(owner)).await);
    assert!(!dir.exists());
}

/// `-d` checks the branch's upstream, not HEAD. A branch merged into HEAD is
/// removed even when its upstream does not have it.
#[tokio::test(flavor = "multi_thread")]
async fn a_merged_worktree_branch_with_an_unmerged_upstream_is_removed() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let project = f.root.join("project");
    make_repo(&project);
    setup_git(&project, &["branch", "base"]);
    let created = device.command(&worktree_command(&f, "feat/x")).await;
    let session = session_of(&created, &device.events(Seq::ZERO, 0).await);
    let dir = f.root.join("project.worktrees/feat-x");
    setup_git(&dir, &["commit", "-q", "--allow-empty", "-m", "work"]);
    setup_git(&project, &["merge", "-q", "--ff-only", "feat/x"]);
    setup_git(&project, &["branch", "--set-upstream-to=base", "feat/x"]);

    accepted_seq(&device.command(&remove_command(session)).await);
    assert!(!dir.exists());
    assert!(!has_branch(&project, "feat/x"));
}

/// A worktree whose directory was deleted by hand is pruned from the
/// repository, and its branch is deleted with it.
#[tokio::test(flavor = "multi_thread")]
async fn a_worktree_deleted_by_hand_is_removed() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let project = f.root.join("project");
    make_repo(&project);
    let created = device.command(&worktree_command(&f, "feat/x")).await;
    let session = session_of(&created, &device.events(Seq::ZERO, 0).await);
    let dir = f.root.join("project.worktrees/feat-x");
    std::fs::remove_dir_all(&dir).unwrap();

    accepted_seq(&device.command(&remove_command(session)).await);
    assert!(!has_branch(&project, "feat/x"));
    assert!(!worktree_list(&project).contains("project.worktrees/feat-x"));
    // Positive control: the same branch name is free again.
    accepted_seq(&device.command(&worktree_command(&f, "feat/x")).await);
    assert!(dir.is_dir());
}

/// A worktree detached on a commit its branch lacks keeps the worktree: the
/// commit is not merged into HEAD.
#[tokio::test(flavor = "multi_thread")]
async fn a_worktree_with_detached_commits_is_not_removed() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let project = f.root.join("project");
    make_repo(&project);
    let created = device.command(&worktree_command(&f, "feat/x")).await;
    let session = session_of(&created, &device.events(Seq::ZERO, 0).await);
    let dir = f.root.join("project.worktrees/feat-x");
    setup_git(&dir, &["checkout", "-q", "--detach"]);
    setup_git(&dir, &["commit", "-q", "--allow-empty", "-m", "lost"]);

    assert_eq!(
        device.command(&remove_command(session)).await,
        rejected(RejectReason::BranchUnmerged)
    );
    assert!(dir.is_dir());
    assert!(has_branch(&project, "feat/x"));
    // Positive control: back on the branch, whose commit is merged, the
    // removal goes through.
    setup_git(&dir, &["checkout", "-q", "feat/x"]);
    accepted_seq(&device.command(&remove_command(session)).await);
    assert!(!dir.exists());
}

/// `status.showUntrackedFiles = no` hides untracked files from a plain `git
/// status`. The removal lists them anyway, so the worktree is kept.
#[tokio::test(flavor = "multi_thread")]
async fn untracked_files_hidden_by_config_keep_the_worktree() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let project = f.root.join("project");
    make_repo(&project);
    let created = device.command(&worktree_command(&f, "feat/x")).await;
    let session = session_of(&created, &device.events(Seq::ZERO, 0).await);
    let dir = f.root.join("project.worktrees/feat-x");
    setup_git(&project, &["config", "status.showUntrackedFiles", "no"]);
    let scratch = dir.join("notes.txt");
    std::fs::write(&scratch, "untracked").unwrap();

    assert_eq!(
        device.command(&remove_command(session)).await,
        rejected(RejectReason::WorktreeDirty)
    );
    assert!(dir.is_dir());
    assert!(has_branch(&project, "feat/x"));
    // Positive control: once the file is gone, the removal goes through.
    std::fs::remove_file(&scratch).unwrap();
    accepted_seq(&device.command(&remove_command(session)).await);
    assert!(!dir.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn events_page_with_limit_and_reject_a_bad_query() {
    let mut f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let created = device.command(&f.create_session_command()).await;
    let session = session_of(&created, &device.events(Seq::ZERO, 0).await);
    for i in 0..4 {
        f.append(session, &format!("m{i}"));
    }

    let reply = device.get("/v1/events?after=1&limit=2").await;
    let batch: EventBatch = reply.json();
    assert_eq!(batch.from, Seq(2));
    assert_eq!(batch.events.len(), 2);
    assert_eq!(batch.last(), Some(Seq(3)));
    // Past the head: empty, `from = after + 1`.
    let batch = device.events(Seq(100), 0).await;
    assert_eq!((batch.from, batch.events.len()), (Seq(101), 0));

    for uri in ["/v1/events", "/v1/events?after=x", "/v1/events?after=-1"] {
        assert_eq!(
            device.get(uri).await.status,
            StatusCode::BAD_REQUEST,
            "{uri}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_after_past_any_log_is_a_400_and_the_server_keeps_answering() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;

    // The highest seq a log can hold is still a cursor like any other.
    let last = i64::MAX as u64;
    let batch = device.events(Seq(last), 0).await;
    assert_eq!((batch.from, batch.events.len()), (Seq(last + 1), 0));

    // Past it, no log has that position: a client error, never a 500 (a
    // panic there used to poison the API's database lock for good).
    for after in [last + 1, u64::MAX] {
        let uri = events_uri(Seq(after), 0);
        assert_eq!(
            device.get(&uri).await.status,
            StatusCode::BAD_REQUEST,
            "{uri}"
        );
    }
    assert_eq!(device.get(path::HELLO).await.status, StatusCode::OK);
    let batch = device.events(Seq::ZERO, 0).await;
    assert_eq!(batch.from, Seq(1));
}

#[tokio::test(flavor = "multi_thread")]
async fn long_poll_returns_when_an_event_is_appended() {
    let mut f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let created = device.command(&f.create_session_command()).await;
    let session = session_of(&created, &device.events(Seq::ZERO, 0).await);

    // Events already there: no wait, even with a long `wait`.
    let started = Instant::now();
    let batch = device.events(Seq::ZERO, 60).await;
    assert_eq!(batch.events.len(), 1);
    assert!(started.elapsed() < SOON);

    let mut waiter = f.client(&creds).await;
    let poll = waiter.events(Seq(1), 60);
    tokio::pin!(poll);
    // Nothing to return yet, so it is still waiting.
    assert!(
        tokio::time::timeout(Duration::from_millis(200), &mut poll)
            .await
            .is_err()
    );
    let seq = f.append(session, "news");
    let batch = tokio::time::timeout(SOON, poll)
        .await
        .expect("woken by the append");
    assert_eq!(batch.from, Seq(2));
    assert_eq!(batch.last(), Some(seq));
    assert_eq!(
        batch.events[0].kind,
        EventKind::AgentMessage {
            parent: None,
            text: "news".into(),
            full: None,
        }
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn long_poll_times_out_with_an_empty_batch() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;

    let started = Instant::now();
    let batch = device.events(Seq::ZERO, 1).await;
    let waited = started.elapsed();
    assert_eq!(batch.from, Seq(1));
    assert!(batch.events.is_empty());
    assert!(waited >= Duration::from_millis(900), "waited {waited:?}");
    assert!(waited < SOON, "waited {waited:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_ends_a_long_poll() {
    let mut f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    // The device's first request records `last_seen` once it passes
    // authentication, so seeing it set means the poll is in its handler.
    let poll = tokio::spawn(async move { device.events(Seq::ZERO, 60).await });
    tokio::time::timeout(SOON, async {
        while devices::get(f.storage.db.conn(), creds.id)
            .unwrap()
            .unwrap()
            .last_seen
            .is_none()
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("poll reached the server");

    let started = Instant::now();
    f.stop().await;
    let batch = tokio::time::timeout(SOON, poll)
        .await
        .expect("long-poll answered")
        .unwrap();
    assert!(batch.events.is_empty());
    // Well inside the 60 s wait and the server's 5 s grace period.
    assert!(started.elapsed() < Duration::from_secs(4));
}

#[tokio::test(flavor = "multi_thread")]
async fn blobs_download_whole_or_by_range() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let content: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let size = content.len();
    let blob = BlobStore::new(f.storage.dir.blobs_dir())
        .put(f.storage.db.conn(), &content[..])
        .unwrap();
    let uri = path::blob(blob.id);

    let reply = device.get(&uri).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.header(header::ACCEPT_RANGES), "bytes");
    assert_eq!(reply.header(header::CONTENT_LENGTH), size.to_string());
    assert_eq!(&reply.body[..], &content[..]);

    // An interrupted download resumes from what it has.
    let have = 123_456;
    let reply = device
        .send(
            Method::GET,
            &uri,
            &[(header::RANGE, &format!("bytes={have}-"))],
            Bytes::new(),
        )
        .await;
    assert_eq!(reply.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        reply.header(header::CONTENT_RANGE),
        format!("bytes {have}-{}/{size}", size - 1)
    );
    assert_eq!(
        reply.header(header::CONTENT_LENGTH),
        (size - have).to_string()
    );
    assert_eq!(&reply.body[..], &content[have..]);

    for (range, want, content_range) in [
        (
            "bytes=10-19",
            &content[10..20],
            format!("bytes 10-19/{size}"),
        ),
        (
            "bytes=-5",
            &content[size - 5..],
            format!("bytes {}-{}/{size}", size - 5, size - 1),
        ),
    ] {
        let reply = device
            .send(Method::GET, &uri, &[(header::RANGE, range)], Bytes::new())
            .await;
        assert_eq!(reply.status, StatusCode::PARTIAL_CONTENT, "{range}");
        assert_eq!(reply.header(header::CONTENT_RANGE), content_range);
        assert_eq!(&reply.body[..], want, "{range}");
    }

    let reply = device
        .send(
            Method::GET,
            &uri,
            &[(header::RANGE, &format!("bytes={size}-"))],
            Bytes::new(),
        )
        .await;
    assert_eq!(reply.status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(
        reply.header(header::CONTENT_RANGE),
        format!("bytes */{size}")
    );

    assert_eq!(
        device.get(&path::blob(BlobId::new())).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        device.get("/v1/blobs/not-a-uuid").await.status,
        StatusCode::BAD_REQUEST
    );
}

/// `size` bytes of an image: a PNG signature and filler.
fn png(size: usize) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.resize(size, 0x42);
    bytes
}

/// An image is staged under its hash: `HEAD` says whether it is, and a
/// second `PUT` of it is answered as stored, not rewritten.
#[tokio::test(flavor = "multi_thread")]
async fn an_attachment_is_staged_once_and_reported_as_staged() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let bytes = png(1000);
    let uri = path::attachment(ContentHash::of(&bytes));
    let stored = UploadResult::Stored { size: 1000 };

    let head = device.send(Method::HEAD, &uri, &[], Bytes::new()).await;
    assert_eq!(head.status, StatusCode::NOT_FOUND);
    let reply = device
        .send(Method::PUT, &uri, &[], Bytes::from(bytes.clone()))
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json::<UploadResult>(), stored);
    let staged = f
        .storage
        .dir
        .attachments_dir()
        .join(ContentHash::of(&bytes).to_hex());
    assert_eq!(std::fs::read(&staged).unwrap(), bytes);

    let head = device.send(Method::HEAD, &uri, &[], Bytes::new()).await;
    assert_eq!(head.status, StatusCode::NO_CONTENT);
    let again = device
        .send(Method::PUT, &uri, &[], Bytes::from(bytes))
        .await;
    assert_eq!(again.json::<UploadResult>(), stored);
}

/// A body is staged only under the hash its path names, and any file will
/// do; a refusal leaves no file, temp files included.
#[tokio::test(flavor = "multi_thread")]
async fn a_staged_upload_must_hash_to_its_path_and_may_be_any_file() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let image = png(1000);
    let other = path::attachment(ContentHash::of(b"other"));
    let reply = device
        .send(Method::PUT, &other, &[], Bytes::from(image.clone()))
        .await;
    assert_eq!(reply.json::<UploadResult>(), UploadResult::HashMismatch);
    assert!(files_under(&f.storage.dir.attachments_dir()).is_empty());

    let pdf = b"%PDF-1.4 not an image".to_vec();
    let uri = path::attachment(ContentHash::of(&pdf));
    let reply = device
        .send(Method::PUT, &uri, &[], Bytes::from(pdf.clone()))
        .await;
    assert_eq!(
        reply.json::<UploadResult>(),
        UploadResult::Stored {
            size: pdf.len() as u64,
        }
    );
    let head = device.send(Method::HEAD, &uri, &[], Bytes::new()).await;
    assert_eq!(head.status, StatusCode::NO_CONTENT, "the pdf is staged");

    let uri = path::attachment(ContentHash::of(&image));
    let reply = device
        .send(Method::PUT, &uri, &[], Bytes::from(image))
        .await;
    assert!(
        matches!(reply.json::<UploadResult>(), UploadResult::Stored { .. }),
        "control: the image under its own hash is stored"
    );
}

/// An upload over `upload_max` is `TooLarge` as JSON, not a 413, and leaves
/// no file, temp file included; a body of exactly the cap is stored. The
/// oversize request carries `Expect: 100-continue` and no body, as the device
/// sends it; a client that sends the body unasked can lose the early answer to
/// a reset (task 37b41c).
#[tokio::test(flavor = "multi_thread")]
async fn an_oversize_upload_is_refused_as_json() {
    const CAP: u64 = 1 << 20;
    let options = ServerOptions {
        git: isolated_git(),
        upload_max: CAP,
        ..ServerOptions::default()
    };
    let f = Fixture::start_configured(|log| log, options).await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let big = png(CAP as usize + 1);
    let uri = path::attachment(ContentHash::of(&big));
    let mut tls = tls_from(&f, LOCAL, Some(&creds), false).await.unwrap();
    let head = format!(
        "PUT {uri} HTTP/1.1\r\nhost: localhost\r\nexpect: 100-continue\r\ncontent-length: {}\r\n\r\n",
        CAP + 1
    );
    tls.write_all(head.as_bytes()).await.unwrap();
    tls.flush().await.unwrap();
    let response = tokio::time::timeout(SOON, read_full_response(&mut tls))
        .await
        .expect("answered before the body");
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("\"too_large\""), "{response}");
    assert!(
        files_under(&f.storage.dir.attachments_dir()).is_empty(),
        "nothing is left, temp file included"
    );

    let exact = png(CAP as usize);
    let uri = path::attachment(ContentHash::of(&exact));
    let reply = device
        .send(Method::PUT, &uri, &[], Bytes::from(exact))
        .await;
    assert_eq!(
        reply.json::<UploadResult>(),
        UploadResult::Stored { size: CAP },
        "control: a body of exactly the cap is stored"
    );

    // Past axum's default 2 MB body limit, which is off for this route: a
    // 3 MiB file is stored under the default cap.
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let three_mib = png(3 << 20);
    let uri = path::attachment(ContentHash::of(&three_mib));
    let reply = device
        .send(Method::PUT, &uri, &[], Bytes::from(three_mib))
        .await;
    assert_eq!(
        reply.json::<UploadResult>(),
        UploadResult::Stored { size: 3 << 20 }
    );
}

/// On a low disk an upload is `DiskLow` and stores nothing; once the disk
/// has room again the same upload is stored.
#[tokio::test(flavor = "multi_thread")]
async fn an_upload_on_a_low_disk_is_refused() {
    let free = Arc::new(Free::default());
    free.0.store(10, std::sync::atomic::Ordering::SeqCst);
    let disk = Disk::new(free.clone(), 10);
    let f = Fixture::start_cropping(disk.clone(), None).await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let bytes = png(1000);
    let uri = path::attachment(ContentHash::of(&bytes));

    free.0.store(9, std::sync::atomic::Ordering::SeqCst);
    assert!(disk.check());
    let reply = device
        .send(Method::PUT, &uri, &[], Bytes::from(bytes.clone()))
        .await;
    assert_eq!(reply.json::<UploadResult>(), UploadResult::DiskLow);
    assert!(files_under(&f.storage.dir.attachments_dir()).is_empty());

    free.0.store(10, std::sync::atomic::Ordering::SeqCst);
    assert!(!disk.check());
    let reply = device
        .send(Method::PUT, &uri, &[], Bytes::from(bytes))
        .await;
    assert_eq!(
        reply.json::<UploadResult>(),
        UploadResult::Stored { size: 1000 },
        "control: with room again, the upload is stored"
    );
}

/// A staging path that is not a hash is a bad request, for both methods.
#[tokio::test(flavor = "multi_thread")]
async fn a_bad_attachment_path_is_a_bad_request() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let uri = "/v1/attachments/xyz";
    let put = device
        .send(Method::PUT, uri, &[], Bytes::from(png(1000)))
        .await;
    assert_eq!(put.status, StatusCode::BAD_REQUEST);
    let head = device.send(Method::HEAD, uri, &[], Bytes::new()).await;
    assert_eq!(head.status, StatusCode::BAD_REQUEST);
}

/// The header that names an upload's offset, in a `PUT` or a `HEAD` answer.
fn upload_offset_header() -> header::HeaderName {
    header::HeaderName::from_static(UPLOAD_OFFSET_HEADER)
}

/// The partial an interrupted upload of `hash` leaves in `f`'s staging.
fn partial_of(f: &Fixture, hash: ContentHash) -> PathBuf {
    f.storage
        .dir
        .attachments_dir()
        .join(format!(".part-{}", hash.to_hex()))
}

/// An interrupted upload resumes: `HEAD` names the bytes held in the offset
/// header, and a `PUT` from that offset completes the file.
#[tokio::test(flavor = "multi_thread")]
async fn an_upload_resumes_at_the_offset_head_reports() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let bytes = png(10_000);
    let hash = ContentHash::of(&bytes);
    let uri = path::attachment(hash);
    std::fs::write(partial_of(&f, hash), &bytes[..4000]).unwrap();

    let head = device.send(Method::HEAD, &uri, &[], Bytes::new()).await;
    assert_eq!(head.status, StatusCode::NOT_FOUND);
    assert_eq!(head.header(upload_offset_header()), "4000");

    let reply = device
        .send(
            Method::PUT,
            &uri,
            &[(upload_offset_header(), "4000")],
            Bytes::from(bytes[4000..].to_vec()),
        )
        .await;
    assert_eq!(
        reply.json::<UploadResult>(),
        UploadResult::Stored { size: 10_000 }
    );
    let staged = f.storage.dir.attachments_dir().join(hash.to_hex());
    assert_eq!(std::fs::read(&staged).unwrap(), bytes);
    assert!(!partial_of(&f, hash).exists(), "the partial is gone");

    let head = device.send(Method::HEAD, &uri, &[], Bytes::new()).await;
    assert_eq!(head.status, StatusCode::NO_CONTENT);
    assert!(head.headers.get(upload_offset_header()).is_none());
}

/// A `PUT` naming an offset other than the partial's length is an
/// `OffsetMismatch` and writes nothing; one naming no offset restarts it.
#[tokio::test(flavor = "multi_thread")]
async fn a_put_at_another_offset_is_an_offset_mismatch_and_zero_restarts() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let bytes = png(10_000);
    let hash = ContentHash::of(&bytes);
    let uri = path::attachment(hash);
    std::fs::write(partial_of(&f, hash), &bytes[..4000]).unwrap();

    let reply = device
        .send(
            Method::PUT,
            &uri,
            &[(upload_offset_header(), "5000")],
            Bytes::from(bytes[5000..].to_vec()),
        )
        .await;
    assert_eq!(
        reply.json::<UploadResult>(),
        UploadResult::OffsetMismatch { have: 4000 }
    );
    assert_eq!(
        std::fs::read(partial_of(&f, hash)).unwrap(),
        &bytes[..4000],
        "the partial is as it was"
    );

    let reply = device
        .send(Method::PUT, &uri, &[], Bytes::from(bytes))
        .await;
    assert_eq!(
        reply.json::<UploadResult>(),
        UploadResult::Stored { size: 10_000 }
    );
}

/// A body cut off midway keeps the bytes that arrived, and the `PUT` from the
/// offset `HEAD` then reports completes the file.
#[tokio::test(flavor = "multi_thread")]
async fn an_upload_cut_off_midway_keeps_its_partial_and_resumes() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let bytes = png(10_000);
    let hash = ContentHash::of(&bytes);
    let uri = path::attachment(hash);

    let mut tls = tls_from(&f, LOCAL, Some(&creds), false).await.unwrap();
    let request = format!("PUT {uri} HTTP/1.1\r\nhost: localhost\r\ncontent-length: 10000\r\n\r\n");
    tls.write_all(request.as_bytes()).await.unwrap();
    tls.write_all(&bytes[..4000]).await.unwrap();
    tls.flush().await.unwrap();
    drop(tls);

    // The server reads the body as it arrives, so poll until the bytes show.
    let held = tokio::time::timeout(SOON, async {
        loop {
            let head = device.send(Method::HEAD, &uri, &[], Bytes::new()).await;
            if head.headers.get(upload_offset_header()) == Some(&HeaderValue::from_static("4000")) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(held.is_ok(), "the server holds the 4000 bytes that arrived");

    let reply = tokio::time::timeout(
        SOON,
        device.send(
            Method::PUT,
            &uri,
            &[(upload_offset_header(), "4000")],
            Bytes::from(bytes[4000..].to_vec()),
        ),
    )
    .await
    .expect("the resumed PUT is answered");
    assert_eq!(
        reply.json::<UploadResult>(),
        UploadResult::Stored { size: 10_000 }
    );
    let staged = f.storage.dir.attachments_dir().join(hash.to_hex());
    assert_eq!(std::fs::read(&staged).unwrap(), bytes);
}

/// The upload lock and the upload outlive the handler hyper drops when the
/// client leaves after its body: the file work runs on its own task.
#[tokio::test(flavor = "multi_thread")]
async fn a_put_dropped_after_its_body_keeps_the_upload_lock_until_staged() {
    let entered = Gate::new();
    let release = Gate::new();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let _open = OpenOnDrop(release.clone());
    let hook = {
        let (entered, release, calls) = (entered.clone(), release.clone(), calls.clone());
        move || {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            entered.open();
            release.wait_blocking();
        }
    };
    let options = ServerOptions {
        upload_hook: Some(Arc::new(hook)),
        ..ServerOptions::default()
    };
    let f = Fixture::start_cropping_with(Disk::default(), options).await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let bytes = png(10_000);
    let hash = ContentHash::of(&bytes);
    let uri = path::attachment(hash);

    let mut tls = tls_from(&f, LOCAL, Some(&creds), false).await.unwrap();
    let request = format!("PUT {uri} HTTP/1.1\r\nhost: localhost\r\ncontent-length: 10000\r\n\r\n");
    tls.write_all(request.as_bytes()).await.unwrap();
    tls.write_all(&bytes).await.unwrap();
    tls.flush().await.unwrap();
    tokio::time::timeout(SOON, entered.opened())
        .await
        .expect("the upload reaches the hook with its whole body read");
    drop(tls);

    let second = {
        let (uri, bytes) = (uri.clone(), bytes.clone());
        tokio::spawn(async move {
            device
                .send(Method::PUT, &uri, &[], Bytes::from(bytes))
                .await
        })
    };
    // A lock released early would let the second PUT reach the hook by now.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !second.is_finished(),
        "the second PUT waits for the upload lock"
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "only the first upload has reached the hook"
    );

    release.open();
    let reply = tokio::time::timeout(SOON, second)
        .await
        .expect("the second PUT is answered once the upload is staged")
        .unwrap();
    assert_eq!(
        reply.json::<UploadResult>(),
        UploadResult::Stored { size: 10_000 }
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the second PUT answers the staged image without writing its body again"
    );
    let staged = f.storage.dir.attachments_dir().join(hash.to_hex());
    assert_eq!(std::fs::read(&staged).unwrap(), bytes);

    let mut check = f.client(&creds).await;
    let head = check.send(Method::HEAD, &uri, &[], Bytes::new()).await;
    assert_eq!(head.status, StatusCode::NO_CONTENT);
}

/// An upload offset that is not a decimal number is a bad request, and
/// writes nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_malformed_upload_offset_is_a_bad_request() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let bytes = png(1000);
    let uri = path::attachment(ContentHash::of(&bytes));
    let reply = device
        .send(
            Method::PUT,
            &uri,
            &[(upload_offset_header(), "x")],
            Bytes::from(bytes.clone()),
        )
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert!(files_under(&f.storage.dir.attachments_dir()).is_empty());

    let reply = device
        .send(Method::PUT, &uri, &[], Bytes::from(bytes))
        .await;
    assert_eq!(
        reply.json::<UploadResult>(),
        UploadResult::Stored { size: 1000 },
        "control: the same upload without a malformed offset is stored"
    );
}

/// A resumed upload counts its offset against `upload_max`: over the cap it is
/// `TooLarge`, and its partial is dropped.
#[tokio::test(flavor = "multi_thread")]
async fn a_resumed_upload_over_the_cap_is_too_large_and_drops_its_partial() {
    const CAP: u64 = 1 << 20;
    let options = ServerOptions {
        git: isolated_git(),
        upload_max: CAP,
        ..ServerOptions::default()
    };
    let f = Fixture::start_configured(|log| log, options).await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let bytes = png(CAP as usize + 1);
    let hash = ContentHash::of(&bytes);
    let uri = path::attachment(hash);
    let half = (CAP / 2) as usize;
    std::fs::write(partial_of(&f, hash), &bytes[..half]).unwrap();

    let offset = half.to_string();
    let reply = device
        .send(
            Method::PUT,
            &uri,
            &[(upload_offset_header(), offset.as_str())],
            Bytes::from(bytes[half..].to_vec()),
        )
        .await;
    assert_eq!(reply.json::<UploadResult>(), UploadResult::TooLarge);
    assert!(
        files_under(&f.storage.dir.attachments_dir()).is_empty(),
        "nothing is left, the partial included"
    );
}

/// A partial left over `upload_max` (from before the cap was lowered) is not
/// staged by an empty `PUT` at its length: it is `TooLarge` and dropped. A
/// partial of exactly the cap is still stored.
#[tokio::test(flavor = "multi_thread")]
async fn an_upload_past_the_cap_with_an_empty_body_is_too_large() {
    const CAP: u64 = 1 << 20;
    let options = ServerOptions {
        git: isolated_git(),
        upload_max: CAP,
        ..ServerOptions::default()
    };
    let f = Fixture::start_configured(|log| log, options).await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let over = png(CAP as usize + 1);
    let hash = ContentHash::of(&over);
    let uri = path::attachment(hash);
    std::fs::write(partial_of(&f, hash), &over).unwrap();

    let offset = over.len().to_string();
    let reply = device
        .send(
            Method::PUT,
            &uri,
            &[(upload_offset_header(), offset.as_str())],
            Bytes::new(),
        )
        .await;
    assert_eq!(reply.json::<UploadResult>(), UploadResult::TooLarge);
    assert!(
        files_under(&f.storage.dir.attachments_dir()).is_empty(),
        "nothing is left, the partial included"
    );

    let exact = png(CAP as usize);
    let hash = ContentHash::of(&exact);
    let uri = path::attachment(hash);
    std::fs::write(partial_of(&f, hash), &exact).unwrap();
    let offset = exact.len().to_string();
    let reply = device
        .send(
            Method::PUT,
            &uri,
            &[(upload_offset_header(), offset.as_str())],
            Bytes::new(),
        )
        .await;
    assert_eq!(
        reply.json::<UploadResult>(),
        UploadResult::Stored { size: CAP },
        "control: a partial of exactly the cap is stored"
    );
}

/// A `PUT` whose `content-length` takes the file past `upload_max` is
/// `TooLarge` before any body byte arrives, and its partial is dropped.
#[tokio::test(flavor = "multi_thread")]
async fn a_declared_length_over_the_cap_is_too_large_before_the_body() {
    const CAP: u64 = 1 << 20;
    let options = ServerOptions {
        git: isolated_git(),
        upload_max: CAP,
        ..ServerOptions::default()
    };
    let f = Fixture::start_configured(|log| log, options).await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let bytes = png(CAP as usize + 1);
    let hash = ContentHash::of(&bytes);
    let uri = path::attachment(hash);

    // A fresh upload: the head declares the whole file, and no body follows.
    let mut tls = tls_from(&f, LOCAL, Some(&creds), false).await.unwrap();
    let head = format!(
        "PUT {uri} HTTP/1.1\r\nhost: localhost\r\ncontent-length: {}\r\n\r\n",
        CAP + 1
    );
    tls.write_all(head.as_bytes()).await.unwrap();
    tls.flush().await.unwrap();
    let response = tokio::time::timeout(SOON, read_full_response(&mut tls))
        .await
        .expect("answered before the body");
    assert!(
        response.contains("\"too_large\""),
        "a fresh declared length over the cap is TooLarge: {response}"
    );
    assert!(
        files_under(&f.storage.dir.attachments_dir()).is_empty(),
        "nothing is left after a fresh refusal"
    );

    // A resumed upload: the partial is dropped, and the head again sends no body.
    let half = (CAP / 2) as usize;
    std::fs::write(partial_of(&f, hash), &bytes[..half]).unwrap();
    let mut tls = tls_from(&f, LOCAL, Some(&creds), false).await.unwrap();
    let head = format!(
        "PUT {uri} HTTP/1.1\r\nhost: localhost\r\n{}: {half}\r\ncontent-length: {}\r\n\r\n",
        upload_offset_header(),
        bytes.len() - half
    );
    tls.write_all(head.as_bytes()).await.unwrap();
    tls.flush().await.unwrap();
    let response = tokio::time::timeout(SOON, read_full_response(&mut tls))
        .await
        .expect("answered before the body");
    assert!(
        response.contains("\"too_large\""),
        "a resumed declared length over the cap is TooLarge: {response}"
    );
    assert!(
        files_under(&f.storage.dir.attachments_dir()).is_empty(),
        "the partial is dropped"
    );

    // Control: the same file, declared at the cap, is stored.
    let exact = png(CAP as usize);
    let exact_hash = ContentHash::of(&exact);
    let reply = device
        .send(
            Method::PUT,
            &path::attachment(exact_hash),
            &[],
            Bytes::from(exact),
        )
        .await;
    assert_eq!(
        reply.json::<UploadResult>(),
        UploadResult::Stored { size: CAP },
        "control: a file of exactly the cap is stored"
    );
}

/// Stages `bytes` as an image with `PUT`, as a device does before it sends
/// the message naming them; returns the hash.
async fn stage_image(device: &mut Client, bytes: &[u8]) -> ContentHash {
    let hash = ContentHash::of(bytes);
    let reply = device
        .send(
            Method::PUT,
            &path::attachment(hash),
            &[],
            Bytes::from(bytes.to_vec()),
        )
        .await;
    assert!(matches!(
        reply.json::<UploadResult>(),
        UploadResult::Stored { .. }
    ));
    hash
}

/// Whether an image with `hash` is staged, by `HEAD`.
async fn is_staged(device: &mut Client, hash: ContentHash) -> bool {
    let reply = device
        .send(Method::HEAD, &path::attachment(hash), &[], Bytes::new())
        .await;
    reply.status == StatusCode::NO_CONTENT
}

/// A `SendMessage` to `context` of `session` naming `attachments`.
fn message_command(
    session: SessionId,
    context: ContextId,
    text: &str,
    attachments: Vec<ContentHash>,
) -> Command {
    Command {
        id: CommandId::new(),
        kind: CommandKind::SendMessage {
            session_id: session,
            context,
            text: text.into(),
            attachments,
        },
    }
}

/// The attachments of the `UserMessage` logged at `seq`.
async fn attachments_at(device: &mut Client, seq: Seq) -> Vec<Attachment> {
    let batch = device.events(Seq(seq.0 - 1), 0).await;
    let (_, event) = batch.iter().next().unwrap();
    let EventKind::UserMessage { attachments, .. } = &event.kind else {
        panic!("not a message: {:?}", event.kind);
    };
    attachments.clone()
}

/// How many blobs the `blobs` table records.
fn blob_rows(f: &Fixture) -> i64 {
    f.storage
        .db
        .conn()
        .query_row("SELECT COUNT(*) FROM blobs", [], |r| r.get(0))
        .unwrap()
}

/// The file of blob `id` in the blob store.
fn blob_file(f: &Fixture, id: BlobId) -> PathBuf {
    f.storage.dir.blobs_dir().join(id.0.to_string())
}

/// A message naming a staged image takes it as a new blob of its own: the
/// event carries the blob, the blob is an `attachment` in `event_blobs`,
/// and the staged file goes. A resend is answered with the same result.
#[tokio::test(flavor = "multi_thread")]
async fn a_message_takes_its_staged_images_as_new_blobs() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let session = new_session(&f, &mut device).await;
    let context = ContextId::first_of(session);
    let bytes = png(1000);
    let hash = stage_image(&mut device, &bytes).await;
    let send = message_command(session, context, "look", vec![hash]);

    let seq = accepted_seq(&device.command(&send).await);
    let attachments = attachments_at(&mut device, seq).await;
    assert_eq!(attachments.len(), 1);
    let blob = &attachments[0].blob;
    assert_eq!(blob.sha256, hash);
    assert_eq!(blob.size, 1000);
    assert_eq!(attachments[0].media_type, ImageType::Png);
    assert_eq!(std::fs::read(blob_file(&f, blob.id)).unwrap(), bytes);
    let holds: String = f
        .storage
        .db
        .conn()
        .query_row(
            "SELECT holds FROM event_blobs WHERE blob_id = ?1",
            [blob.id.0.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(holds, "attachment");
    assert!(
        !is_staged(&mut device, hash).await,
        "the staged file is gone"
    );

    assert_eq!(
        device.command(&send).await,
        CommandResult::Accepted { seq },
        "a resend is answered from the log"
    );
    assert_eq!(blob_rows(&f), 1, "nothing was linked twice");
}

/// The same image named twice is two blobs, not one shared (D44). Once the
/// message took it, staging it again makes a third blob for a later message.
#[tokio::test(flavor = "multi_thread")]
async fn the_same_image_twice_is_two_blobs() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let session = new_session(&f, &mut device).await;
    let context = ContextId::first_of(session);
    let bytes = png(1000);
    let hash = stage_image(&mut device, &bytes).await;

    let twice = message_command(session, context, "twice", vec![hash, hash]);
    let seq = accepted_seq(&device.command(&twice).await);
    let first = attachments_at(&mut device, seq).await;
    assert_eq!(first.len(), 2);
    assert_ne!(first[0].blob.id, first[1].blob.id, "two blobs, not one");
    assert!(
        !is_staged(&mut device, hash).await,
        "consumed by the message"
    );

    assert_eq!(stage_image(&mut device, &bytes).await, hash);
    let again = message_command(session, context, "again", vec![hash]);
    let seq = accepted_seq(&device.command(&again).await);
    let second = attachments_at(&mut device, seq).await;
    assert_eq!(second.len(), 1);
    assert_ne!(second[0].blob.id, first[0].blob.id);
    assert_ne!(second[0].blob.id, first[1].blob.id);
    assert_eq!(blob_rows(&f), 3);
}

/// A message naming an image that is not staged is `UnknownAttachment`, and
/// logs and links nothing; a resend gets the same answer. Once the image is
/// staged, a message with it goes through.
#[tokio::test(flavor = "multi_thread")]
async fn a_message_naming_an_unstaged_image_is_refused_and_logs_nothing() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let session = new_session(&f, &mut device).await;
    let context = ContextId::first_of(session);
    let bytes = png(1000);
    let hash = ContentHash::of(&bytes);
    let head = f.log.head();
    let before = blob_files(&f);

    let send = message_command(session, context, "look", vec![hash]);
    assert_eq!(
        device.command(&send).await,
        rejected(RejectReason::UnknownAttachment)
    );
    assert_eq!(
        device.command(&send).await,
        rejected(RejectReason::UnknownAttachment),
        "stored"
    );
    assert_eq!(f.log.head(), head, "nothing logged");
    assert_eq!(blob_files(&f), before, "nothing linked");

    // Control: once staged, the same text with the image goes in.
    stage_image(&mut device, &bytes).await;
    let staged = message_command(session, context, "look", vec![hash]);
    let seq = accepted_seq(&device.command(&staged).await);
    assert_eq!(f.log.head(), seq);
}

/// A message naming a staged file that is not an image is `UnknownAttachment`,
/// and logs and links nothing; the file stays staged for a resend. A staged
/// image named the same way goes through.
#[tokio::test(flavor = "multi_thread")]
async fn a_message_naming_a_staged_non_image_is_refused() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let session = new_session(&f, &mut device).await;
    let context = ContextId::first_of(session);
    let pdf = b"%PDF-1.4 not an image".to_vec();
    let hash = ContentHash::of(&pdf);
    let reply = device
        .send(Method::PUT, &path::attachment(hash), &[], Bytes::from(pdf))
        .await;
    assert!(matches!(
        reply.json::<UploadResult>(),
        UploadResult::Stored { .. }
    ));
    let head = f.log.head();
    let before = blob_files(&f);

    let send = message_command(session, context, "look", vec![hash]);
    assert_eq!(
        device.command(&send).await,
        rejected(RejectReason::UnknownAttachment)
    );
    assert_eq!(
        device.command(&send).await,
        rejected(RejectReason::UnknownAttachment),
        "stored"
    );
    assert_eq!(f.log.head(), head, "nothing logged");
    assert_eq!(blob_files(&f), before, "nothing linked");
    assert!(is_staged(&mut device, hash).await, "the pdf stays staged");

    // Control: a staged image named the same way goes in.
    let bytes = png(1000);
    let image = stage_image(&mut device, &bytes).await;
    let staged = message_command(session, context, "look", vec![image]);
    let seq = accepted_seq(&device.command(&staged).await);
    assert_eq!(f.log.head(), seq);
}

/// A message refused for its context keeps its staged images: no blob is
/// linked or recorded, and the image stays staged for the resend.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_message_keeps_its_staged_images() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let session = new_session(&f, &mut device).await;
    let context = ContextId::first_of(session);
    let bytes = png(1000);
    let hash = stage_image(&mut device, &bytes).await;
    let head = f.log.head();
    let before = blob_files(&f);
    let rows = blob_rows(&f);

    let stale = message_command(session, ContextId::new(), "look", vec![hash]);
    assert_eq!(
        device.command(&stale).await,
        rejected(RejectReason::ContextChanged)
    );
    assert_eq!(f.log.head(), head, "nothing logged");
    assert_eq!(blob_files(&f), before, "no blob file left");
    assert_eq!(blob_rows(&f), rows, "no blob row added");
    assert!(is_staged(&mut device, hash).await, "still staged");

    // Control: the same image goes with the current context.
    let current = message_command(session, context, "look", vec![hash]);
    accepted_seq(&device.command(&current).await);
}

/// Too many names (more than `ATTACHMENTS_MAX`), an image over
/// `ATTACHMENT_MAX_BYTES`, and images over the total
/// (`ATTACHMENTS_MAX_TOTAL_BYTES`) are refused. Nothing is linked, and the
/// staged images stay staged. An over-large image still stages: uploads have
/// their own cap, `[files] upload_max`.
#[tokio::test(flavor = "multi_thread")]
async fn too_many_or_too_large_images_are_refused() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let session = new_session(&f, &mut device).await;
    let context = ContextId::first_of(session);

    let nine: Vec<ContentHash> = (0..=limits::ATTACHMENTS_MAX as u8)
        .map(|i| ContentHash::of(&[i]))
        .collect();
    let many = message_command(session, context, "many", nine);
    assert_eq!(
        device.command(&many).await,
        rejected(RejectReason::TooManyAttachments)
    );

    // Three images of 3.6 MB each: 10.8 MB, over the 10 MB total.
    let mut big = Vec::new();
    for tag in 0..3u8 {
        let mut bytes = png(3_600_000);
        bytes[8] = tag;
        big.push(stage_image(&mut device, &bytes).await);
    }
    let head = f.log.head();
    let before = blob_files(&f);
    let heavy = message_command(session, context, "heavy", big.clone());
    assert_eq!(
        device.command(&heavy).await,
        rejected(RejectReason::AttachmentsTooLarge)
    );
    assert_eq!(f.log.head(), head, "nothing logged");
    assert_eq!(blob_files(&f), before, "no blob added");
    for hash in &big {
        assert!(is_staged(&mut device, *hash).await, "still staged");
    }

    // One image over the per-image limit: it stages, but a message naming it
    // is refused, and nothing is logged.
    let over = stage_image(&mut device, &png(limits::ATTACHMENT_MAX_BYTES as usize + 1)).await;
    let head = f.log.head();
    let before = blob_files(&f);
    let large = message_command(session, context, "large", vec![over]);
    assert_eq!(
        device.command(&large).await,
        rejected(RejectReason::AttachmentsTooLarge)
    );
    assert_eq!(f.log.head(), head, "nothing logged");
    assert_eq!(blob_files(&f), before, "no blob added");
    assert!(is_staged(&mut device, over).await, "still staged");

    // Control: two of them, 7.2 MB, go in.
    let two = message_command(session, context, "two", big[..2].to_vec());
    accepted_seq(&device.command(&two).await);
}

/// On a low disk a message naming images is `DiskLow`, not
/// `UnknownAttachment` for the image it does not have, and links nothing.
/// With room again the message goes through.
#[tokio::test(flavor = "multi_thread")]
async fn an_image_message_on_a_low_disk_is_disk_low() {
    let free = Arc::new(Free::default());
    free.0.store(10, std::sync::atomic::Ordering::SeqCst);
    let disk = Disk::new(free.clone(), 10);
    let f = Fixture::start_cropping(disk.clone(), None).await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let session = new_session(&f, &mut device).await;
    let context = ContextId::first_of(session);
    let hash = stage_image(&mut device, &png(1000)).await;
    let unstaged = ContentHash::of(b"never staged");
    let before = blob_files(&f);

    free.0.store(9, std::sync::atomic::Ordering::SeqCst);
    assert!(disk.check());
    let low = message_command(session, context, "look", vec![hash, unstaged]);
    assert_eq!(device.command(&low).await, rejected(RejectReason::DiskLow));
    assert_eq!(blob_files(&f), before, "no blob linked");
    assert!(is_staged(&mut device, hash).await, "still staged");

    // Control: with room again, the same message goes through.
    free.0.store(10, std::sync::atomic::Ordering::SeqCst);
    assert!(!disk.check());
    let fine = message_command(session, context, "look", vec![hash]);
    accepted_seq(&device.command(&fine).await);
}

/// A message's image blob is an event blob that is not croppable, and it can
/// be deleted like any other.
#[tokio::test(flavor = "multi_thread")]
async fn an_image_blob_can_be_deleted_not_cropped() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let session = new_session(&f, &mut device).await;
    let context = ContextId::first_of(session);
    let hash = stage_image(&mut device, &png(1000)).await;
    let send = message_command(session, context, "look", vec![hash]);
    let seq = accepted_seq(&device.command(&send).await);
    let blob = attachments_at(&mut device, seq).await[0].blob.id;

    assert_eq!(
        device.command(&crop_command(blob, &[span(0, 1)])).await,
        rejected(RejectReason::NotCroppable)
    );
    let delete = Command {
        id: CommandId::new(),
        kind: CommandKind::DeleteBlob { blob },
    };
    accepted_seq(&device.command(&delete).await);
    assert!(!blob_file(&f, blob).exists(), "the blob file went");
}

/// D45: a request that accepts zstd gets an event batch and a blob as one
/// zstd frame, a blob range counted in decoded bytes, and a body under
/// `COMPRESS_MIN_BYTES` as it is.
#[tokio::test(flavor = "multi_thread")]
async fn batches_and_blobs_come_compressed_when_asked() {
    use skywave_proto::encoding::{COMPRESS_MIN_BYTES, DECODED_LENGTH_HEADER, ZSTD};
    let zstd_header = [(header::ACCEPT_ENCODING, ZSTD)];
    let mut f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let session = new_session(&f, &mut device).await;
    for i in 0..200 {
        f.append(session, &format!("message {i} of a long conversation"));
    }

    // A batch: the frame decodes to the very body sent to a request that
    // does not ask, and is smaller.
    let uri = events_uri(Seq::ZERO, 0);
    let plain = device.get(&uri).await;
    assert!(plain.headers.get(header::CONTENT_ENCODING).is_none());
    let packed = device
        .send(Method::GET, &uri, &zstd_header, Bytes::new())
        .await;
    assert_eq!(packed.status, StatusCode::OK);
    assert_eq!(packed.header(header::CONTENT_ENCODING), ZSTD);
    assert_eq!(packed.header(header::CONTENT_TYPE), "application/json");
    assert!(
        packed.body.len() * 4 < plain.body.len(),
        "{}",
        packed.body.len()
    );
    assert_eq!(
        zstd::stream::decode_all(&packed.body[..]).unwrap(),
        plain.body
    );
    // An empty long-poll answer is tiny: sent as it is.
    let head = f.log.head();
    let empty = device
        .send(
            Method::GET,
            &events_uri(head, 0),
            &zstd_header,
            Bytes::new(),
        )
        .await;
    assert!((empty.body.len() as u64) < COMPRESS_MIN_BYTES);
    assert!(empty.headers.get(header::CONTENT_ENCODING).is_none());
    assert!(empty.json::<EventBatch>().events.is_empty());

    // A blob, whole and from an offset.
    let content: Vec<u8> = (0..30_000u32)
        .flat_map(|i| format!("output line {i}\n").into_bytes())
        .collect();
    let size = content.len();
    let store = BlobStore::new(f.storage.dir.blobs_dir());
    let blob = store.put(f.storage.db.conn(), &content[..]).unwrap();
    let uri = path::blob(blob.id);
    let decoded_length = header::HeaderName::from_static(DECODED_LENGTH_HEADER);

    let whole = device
        .send(Method::GET, &uri, &zstd_header, Bytes::new())
        .await;
    assert_eq!(whole.status, StatusCode::OK);
    assert_eq!(whole.header(header::CONTENT_ENCODING), ZSTD);
    assert_eq!(whole.header(decoded_length.clone()), size.to_string());
    assert!(whole.headers.get(header::CONTENT_LENGTH).is_none());
    assert!(whole.body.len() * 4 < size, "{}", whole.body.len());
    assert_eq!(zstd::stream::decode_all(&whole.body[..]).unwrap(), content);

    let have = 123_457;
    let rest = device
        .send(
            Method::GET,
            &uri,
            &[
                zstd_header[0].clone(),
                (header::RANGE, &format!("bytes={have}-")),
            ],
            Bytes::new(),
        )
        .await;
    assert_eq!(rest.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(rest.header(header::CONTENT_ENCODING), ZSTD);
    assert_eq!(
        rest.header(header::CONTENT_RANGE),
        format!("bytes {have}-{}/{size}", size - 1),
        "the range counts decoded bytes"
    );
    assert_eq!(rest.header(decoded_length), (size - have).to_string());
    assert_eq!(
        zstd::stream::decode_all(&rest.body[..]).unwrap(),
        &content[have..]
    );

    // A tiny blob, and a range under the threshold, go as they are.
    let tiny = store.put(f.storage.db.conn(), &b"tiny"[..]).unwrap();
    let reply = device
        .send(
            Method::GET,
            &path::blob(tiny.id),
            &zstd_header,
            Bytes::new(),
        )
        .await;
    assert!(reply.headers.get(header::CONTENT_ENCODING).is_none());
    assert_eq!(reply.header(header::CONTENT_LENGTH), "4");
    assert_eq!(&reply.body[..], b"tiny");
    let reply = device
        .send(
            Method::GET,
            &uri,
            &[zstd_header[0].clone(), (header::RANGE, "bytes=-100")],
            Bytes::new(),
        )
        .await;
    assert_eq!(reply.status, StatusCode::PARTIAL_CONTENT);
    assert!(reply.headers.get(header::CONTENT_ENCODING).is_none());
    assert_eq!(&reply.body[..], &content[size - 100..]);
}

#[tokio::test(flavor = "multi_thread")]
async fn dirs_answers_every_outcome_with_200() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let root = std::fs::canonicalize(&f.root).unwrap();
    let roots = vec![root.display().to_string()];
    let dirs = |dir: &std::path::Path| format!("{}?dir={}", path::DIRS, dir.display());

    let cases = [
        (
            path::DIRS.to_owned(),
            DirsResult::Roots {
                roots: roots.clone(),
            },
        ),
        (
            format!("{}?dir=", path::DIRS),
            DirsResult::Roots {
                roots: roots.clone(),
            },
        ),
        (
            dirs(&root),
            DirsResult::Listed {
                dir: root.display().to_string(),
                names: vec!["project".into()],
                truncated: false,
            },
        ),
        (
            dirs(&root.join("project/../..")),
            DirsResult::NotAllowed {
                roots: roots.clone(),
            },
        ),
        (
            format!("{}?dir=relative", path::DIRS),
            DirsResult::NotAllowed {
                roots: roots.clone(),
            },
        ),
        (dirs(&root.join("missing")), DirsResult::NotFound),
    ];
    for (uri, want) in cases {
        let reply = device.get(&uri).await;
        assert_eq!(reply.status, StatusCode::OK, "{uri}");
        assert_eq!(reply.json::<DirsResult>(), want, "{uri}");
    }

    // Like every authenticated route, it needs a certificate.
    let mut anon = f.anonymous().await;
    assert_eq!(anon.get(path::DIRS).await.status, StatusCode::UNAUTHORIZED);
}

/// The default server's agent keeps no conversations it can read.
#[tokio::test(flavor = "multi_thread")]
async fn conversations_are_unavailable_without_a_source() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let reply = device.get(path::CONVERSATIONS).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.json::<ConversationsResult>(),
        ConversationsResult::Unavailable
    );
}

/// A login whose expiry tests change between hellos.
struct SetLogin(std::sync::Mutex<Option<DateTime<Utc>>>);

impl AgentLogin for SetLogin {
    fn expires(&self) -> Option<DateTime<Utc>> {
        *self.0.lock().unwrap()
    }

    fn offered(&self) -> bool {
        true
    }

    fn begin(
        &self,
        _deadline: Instant,
    ) -> futures_util::future::BoxFuture<'_, Result<Box<dyn LoginAttempt>, LoginError>> {
        Box::pin(async { Err(LoginError::Unavailable) })
    }
}

/// A Claude login stored in `credentials`, whose CLI is never started.
fn claude_login(credentials: PathBuf, other_login: bool) -> ClaudeLogin {
    let adapter = ClaudeCodeAdapter::new(AgentConfig {
        command: PathBuf::from("claude"),
        args: Vec::new(),
        allow_bypass: false,
    });
    ClaudeLogin::new(
        credentials,
        other_login,
        adapter,
        PathBuf::from("/nonexistent"),
    )
}

/// `agent_login_offered` follows the agent's login; `agent_logged_out`
/// follows the supervisor's cell, read on every hello, under any login.
#[tokio::test(flavor = "multi_thread")]
async fn hello_says_whether_a_device_may_log_the_agent_in_and_whether_it_is_logged_out() {
    let tmp = tempfile::tempdir().unwrap();
    let credentials = tmp.path().join(".credentials.json");
    let logins: [(&str, Arc<dyn AgentLogin>, bool); 3] = [
        ("no agent login", Arc::new(NoAgentLogin), false),
        (
            "environment login",
            Arc::new(claude_login(credentials.clone(), true)),
            false,
        ),
        (
            "stored login",
            Arc::new(claude_login(credentials, false)),
            true,
        ),
    ];
    for (name, login, offered) in logins {
        let state = LoginState::new();
        let options = ServerOptions {
            login,
            login_state: state.clone(),
            ..ServerOptions::default()
        };
        let f = Fixture::start_configured(|log| log, options).await;
        let creds = f.pair().await;
        let mut device = f.client(&creds).await;
        let hello: HelloResponse = device.get(path::HELLO).await.json();
        assert_eq!(hello.agent_login_offered, offered, "{name}");
        assert!(!hello.agent_logged_out, "{name}: not known yet");

        assert!(state.apply(false, LoginSource::Start, &LoginSeen::started(0)));
        let hello: HelloResponse = device.get(path::HELLO).await.json();
        assert!(hello.agent_logged_out, "{name}");
        assert_eq!(hello.agent_login_offered, offered, "{name}");

        state.login_succeeded();
        let hello: HelloResponse = device.get(path::HELLO).await.json();
        assert!(!hello.agent_logged_out, "{name}: logged in");
    }
}

/// Each `hello` reads the agent's login anew, so a login renewed on the
/// server shows at the next one.
#[tokio::test(flavor = "multi_thread")]
async fn hello_says_when_the_agent_login_expires_as_of_each_hello() {
    let expires = Utc::now() + chrono::TimeDelta::days(2);
    let login = Arc::new(SetLogin(std::sync::Mutex::new(Some(expires))));
    let options = ServerOptions {
        login: login.clone(),
        ..ServerOptions::default()
    };
    let f = Fixture::start_configured(|log| log, options).await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let hello: HelloResponse = device.get(path::HELLO).await.json();
    assert_eq!(hello.login_expires, Some(expires));

    let renewed = expires + chrono::TimeDelta::days(30);
    *login.0.lock().unwrap() = Some(renewed);
    let hello: HelloResponse = device.get(path::HELLO).await.json();
    assert_eq!(hello.login_expires, Some(renewed));
}

/// Answers each usage limits read with the next of `answers`.
struct ScriptedLimits(std::sync::Mutex<Vec<Result<Vec<UsageLimit>, LimitsError>>>);

impl UsageLimitsSource for ScriptedLimits {
    fn read(
        &self,
        _deadline: Instant,
    ) -> futures_util::future::BoxFuture<'_, Result<Vec<UsageLimit>, LimitsError>> {
        let answer = self.0.lock().unwrap().remove(0);
        Box::pin(async move { answer })
    }
}

#[tokio::test]
async fn usage_limits_answer_what_the_agent_reports() {
    let unavailable = Fixture::start().await;
    let creds = unavailable.pair().await;
    let mut device = unavailable.client(&creds).await;
    let reply = device.get(path::USAGE_LIMITS).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.json::<UsageLimitsResult>(),
        UsageLimitsResult::Unavailable
    );

    let limit = |scope: Option<&str>| UsageLimit {
        window: LimitWindow::Weekly,
        scope: scope.map(str::to_owned),
        percent: Some(22),
        severity: Some(LimitSeverity::Normal),
        resets_at: None,
    };
    let long = format!("{}\nsecond line", "m".repeat(200));
    let source = ScriptedLimits(std::sync::Mutex::new(vec![
        Ok(vec![limit(None), limit(Some(&long))]),
        Err(LimitsError::Failed(format!("{}\nmore", "x".repeat(300)))),
    ]));
    let options = ServerOptions {
        usage_limits: Arc::new(source),
        ..ServerOptions::default()
    };
    let f = Fixture::start_configured(|log| log, options).await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let before = Utc::now();
    let UsageLimitsResult::Read { at, limits } = device.get(path::USAGE_LIMITS).await.json() else {
        panic!("expected the limits");
    };
    assert!(at >= before);
    let cut = format!("{}…", "m".repeat(limits::TITLE_MAX_CHARS - 1));
    assert_eq!(limits, [limit(None), limit(Some(&cut))]);
    let UsageLimitsResult::Failed { reason } = device.get(path::USAGE_LIMITS).await.json() else {
        panic!("expected a failure");
    };
    assert_eq!(reason.chars().count(), limits::REFUSAL_MAX_CHARS);
    assert!(!reason.contains("more"));
}

#[tokio::test]
async fn storage_reports_sizes_by_session_blob_and_shell_run() {
    let mut f = Fixture::start_with_shell(true).await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let create = f.create_session_command();
    let result = device.command(&create).await;
    let big = session_of(&result, &device.events(Seq::ZERO, 0).await);
    let create = f.create_session_command();
    let result = device.command(&create).await;
    let batch = device.events(Seq::ZERO, 0).await;
    let small = session_of(&result, &batch);

    // A tool result whose whole output is a blob, in the big session.
    let blobs = BlobStore::new(f.storage.dir.blobs_dir());
    let output = blobs.put(f.storage.db.conn(), &[7u8; 50_000][..]).unwrap();
    let blob = output.blob_ref();
    let seq = f
        .log
        .append(
            &mut f.storage.db,
            big,
            ContextId::first_of(big),
            EventKind::ToolResult(ToolResult {
                call: None,
                is_error: false,
                status: None,
                preview: "x".into(),
                lines: 1,
                output: Some(skywave_proto::Full::Blob(blob.clone())),
            }),
        )
        .unwrap();
    f.append(small, "hi");

    // A running shell run (logged by hand: nothing runs it here) with two
    // progress events and its output so far.
    let run = Command {
        id: CommandId::new(),
        kind: CommandKind::RunShell {
            session_id: small,
            command: "cargo test\n--release".into(),
            when: Default::default(),
        },
    };
    assert!(matches!(
        device.command(&run).await,
        CommandResult::Accepted { .. }
    ));
    let progress = EventKind::ShellOutput {
        run: run.id,
        tail: "ok".into(),
        lines: 1,
        bytes: 300,
    };
    let progress_bytes = serde_json::to_string(&progress).unwrap().len() as u64;
    for kind in [
        EventKind::ShellStarted {
            run: run.id,
            cwd: "/".into(),
        },
        progress.clone(),
        progress,
    ] {
        f.log.shell_event(&mut f.storage.db, kind).unwrap();
    }
    std::fs::write(
        f.storage.dir.shell_dir().join(format!("{}.out", run.id.0)),
        [1u8; 300],
    )
    .unwrap();

    let reply = device.get(path::STORAGE).await;
    assert_eq!(reply.status, StatusCode::OK);
    let report: StorageReport = reply.json();
    assert!(report.database > 0);
    assert_eq!(report.event_blobs, 50_000);
    assert_eq!(report.workdir_blobs, 0);
    assert_eq!(report.shell_running, 300);
    assert_eq!(report.log_file, None);
    // The fixture's log has no disk to read.
    assert_eq!((report.free, report.min_free), (None, 0));

    let sessions: Vec<_> = report.sessions.iter().map(|s| s.session).collect();
    assert_eq!(sessions, vec![big, small], "largest first");
    let first = &report.sessions[0];
    assert_eq!(
        (first.events, first.blobs, first.blob_bytes),
        (2, 1, 50_000)
    );
    assert!(first.event_bytes > 0);
    assert_eq!(report.sessions[1].blobs, 0);

    assert_eq!(
        report.blobs,
        vec![BlobStorage {
            id: blob.id,
            size: 50_000,
            session: big,
            seq,
            holds: BlobUse::ToolOutput,
        }]
    );
    assert_eq!(
        report.shell_runs,
        vec![ShellRunStorage {
            run: run.id,
            session: small,
            command_line: "cargo test…".into(),
            event_bytes: 2 * progress_bytes,
            output: 300,
            output_blob: None,
        }]
    );
}

/// A device deletes a finished shell run's output blob: the blob is no
/// longer served or stored, the report stops counting it, and the log has a
/// `BlobDeleted` stamped like the event that referenced it. A repeat is
/// `BlobGone`; a blob no event referenced is `UnknownBlob`.
#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_shell_output_blob_frees_it_and_logs_it() {
    let mut f = Fixture::start_with_shell(true).await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let created = device.command(&f.create_session_command()).await;
    let session = session_of(&created, &device.events(Seq::ZERO, 0).await);

    // A finished run whose whole output is a blob (logged by hand).
    let run = Command {
        id: CommandId::new(),
        kind: CommandKind::RunShell {
            session_id: session,
            command: "cargo build".into(),
            when: Default::default(),
        },
    };
    assert!(matches!(
        device.command(&run).await,
        CommandResult::Accepted { .. }
    ));
    let blobs = BlobStore::new(f.storage.dir.blobs_dir());
    let output = blobs
        .put(f.storage.db.conn(), &[3u8; 40_000][..])
        .unwrap()
        .blob_ref();
    let file = f.storage.dir.blobs_dir().join(output.id.0.to_string());
    for kind in [
        EventKind::ShellStarted {
            run: run.id,
            cwd: "/".into(),
        },
        EventKind::ShellFinished {
            run: run.id,
            by: creds.id,
            started_at: None,
            command_line: "cargo build".into(),
            end: ShellEnd::Exited { code: 0 },
            cwd: None,
            preview: "Finished".into(),
            lines: 900,
            output: Some(output.clone()),
            truncated: true,
        },
    ] {
        f.log.shell_event(&mut f.storage.db, kind).unwrap();
    }
    let batch = device.events(Seq::ZERO, 0).await;
    let (finished_seq, finished) = batch.iter().last().unwrap();
    assert!(matches!(finished.kind, EventKind::ShellFinished { .. }));
    let finished_context = finished.context;
    // A new current context: the delete is stamped with the run's, not it.
    let clear = Command {
        id: CommandId::new(),
        kind: CommandKind::ClearContext {
            session_id: session,
            context: ContextId::new(),
            name: "next".into(),
        },
    };
    assert!(matches!(
        device.command(&clear).await,
        CommandResult::Accepted { .. }
    ));

    let report: StorageReport = device.get(path::STORAGE).await.json();
    assert_eq!(report.event_blobs, 40_000);
    assert_eq!(report.sessions[0].blob_bytes, 40_000);
    assert_eq!(
        (
            report.shell_runs[0].output,
            report.shell_runs[0].output_blob
        ),
        (40_000, Some(output.id)),
        "before any delete the run offers its blob"
    );
    assert_eq!(
        device.get(&path::blob(output.id)).await.status,
        StatusCode::OK
    );

    let delete = Command {
        id: CommandId::new(),
        kind: CommandKind::DeleteBlob { blob: output.id },
    };
    let CommandResult::Accepted { seq } = device.command(&delete).await else {
        panic!("delete not accepted");
    };
    assert_eq!(
        device.get(&path::blob(output.id)).await.status,
        StatusCode::NOT_FOUND
    );
    assert!(!file.exists(), "the file is gone");

    let batch = device.events(Seq(seq.0 - 1), 0).await;
    let (logged_seq, event) = batch.iter().next().unwrap();
    assert_eq!(logged_seq, seq);
    assert!(logged_seq > finished_seq);
    assert_eq!(
        event.kind,
        EventKind::BlobDeleted {
            blob: output.id,
            holds: BlobUse::ShellOutput,
            by: creds.id,
        }
    );
    assert_eq!(event.command_id, Some(delete.id));
    assert_eq!(
        (event.session_id, event.context),
        (session, finished_context)
    );

    let report: StorageReport = device.get(path::STORAGE).await.json();
    assert_eq!(report.event_blobs, 0);
    assert_eq!(report.sessions[0].blob_bytes, 0);
    assert_eq!(
        (
            report.shell_runs[0].output,
            report.shell_runs[0].output_blob
        ),
        (0, None)
    );
    assert!(report.blobs.is_empty());

    let again = Command {
        id: CommandId::new(),
        kind: CommandKind::DeleteBlob { blob: output.id },
    };
    assert_eq!(
        device.command(&again).await,
        CommandResult::Rejected {
            reason: RejectReason::BlobGone
        }
    );

    // A workdir blob has a row but no event; a random id has neither.
    std::fs::write(f.root.join("project").join("notes.txt"), "hello\n").unwrap();
    let reply = device
        .get(&workdir_uri(path::WORKDIR_FILE, session, "notes.txt"))
        .await;
    let FileResult::File {
        content: FileBody::Blob(workdir_blob),
        ..
    } = reply.json::<FileResult>()
    else {
        panic!("expected a file: {:?}", reply.body);
    };
    for blob in [workdir_blob.id, BlobId::new()] {
        let delete = Command {
            id: CommandId::new(),
            kind: CommandKind::DeleteBlob { blob },
        };
        assert_eq!(
            device.command(&delete).await,
            CommandResult::Rejected {
                reason: RejectReason::UnknownBlob
            }
        );
    }
    assert_eq!(
        device.get(&path::blob(workdir_blob.id)).await.status,
        StatusCode::OK,
        "a refused delete leaves the blob"
    );
}

/// `seq first last`'s output.
fn numbers(lines: std::ops::RangeInclusive<u32>) -> Vec<u8> {
    lines.flat_map(|n| format!("{n}\n").into_bytes()).collect()
}

fn span(first: u32, count: u32) -> LineSpan {
    LineSpan { first, count }
}

fn crop_command(blob: BlobId, keep: &[LineSpan]) -> Command {
    Command {
        id: CommandId::new(),
        kind: CommandKind::CropBlob {
            blob,
            keep: keep.to_vec(),
        },
    }
}

fn accepted_seq(result: &CommandResult) -> Seq {
    match result {
        CommandResult::Accepted { seq } => *seq,
        other => panic!("not accepted: {other:?}"),
    }
}

fn rejected(reason: RejectReason) -> CommandResult {
    CommandResult::Rejected { reason }
}

/// The names of the files in the blob store, temp files included.
fn blob_files(f: &Fixture) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(f.storage.dir.blobs_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// A new session of `device`'s.
async fn new_session(f: &Fixture, device: &mut Client) -> SessionId {
    let created = device.command(&f.create_session_command()).await;
    session_of(&created, &device.events(Seq::ZERO, 0).await)
}

/// A finished shell run of `session`, requested by `by`, whose whole
/// output `content` is a blob (logged by hand: nothing runs it here);
/// returns the run, its blob and its `ShellFinished`'s seq and context.
async fn finished_run(
    f: &mut Fixture,
    device: &mut Client,
    by: DeviceId,
    session: SessionId,
    content: &[u8],
) -> (CommandId, BlobRef, Seq, ContextId) {
    let run = Command {
        id: CommandId::new(),
        kind: CommandKind::RunShell {
            session_id: session,
            command: "seq 1 3000".into(),
            when: Default::default(),
        },
    };
    accepted_seq(&device.command(&run).await);
    let output = f.storage.put_blob(content).unwrap();
    for kind in [
        EventKind::ShellStarted {
            run: run.id,
            cwd: "/".into(),
        },
        EventKind::ShellFinished {
            run: run.id,
            by,
            started_at: None,
            command_line: "seq 1 3000".into(),
            end: ShellEnd::Exited { code: 0 },
            cwd: None,
            preview: "3000".into(),
            lines: 3000,
            output: Some(output.clone()),
            truncated: true,
        },
    ] {
        f.log.shell_event(&mut f.storage.db, kind).unwrap();
    }
    let batch = device.events(Seq::ZERO, 0).await;
    let (seq, finished) = batch.iter().last().unwrap();
    assert!(matches!(finished.kind, EventKind::ShellFinished { .. }));
    (run.id, output, seq, finished.context)
}

/// A blob holding `content` for an agent event of `session`: a tool
/// result's output, or a tool call's input.
fn tool_blob(f: &mut Fixture, session: SessionId, holds: BlobUse, content: &[u8]) -> BlobRef {
    let blob = f.storage.put_blob(content).unwrap();
    let full = Some(skywave_proto::Full::Blob(blob.clone()));
    let kind = match holds {
        BlobUse::ToolOutput => EventKind::ToolResult(ToolResult {
            call: None,
            is_error: false,
            status: None,
            preview: "x".into(),
            lines: 1,
            output: full,
        }),
        BlobUse::ToolInput => EventKind::ToolCall(skywave_proto::ToolCall {
            call: None,
            parent: None,
            name: "Bash".into(),
            display: skywave_proto::ToolDisplay {
                kind: skywave_proto::ToolKind::Shell,
                title: "$ ls".into(),
                detail: None,
                body: None,
            },
            input: full,
        }),
        other => panic!("no tool blob holds {other:?}"),
    };
    f.log
        .append(
            &mut f.storage.db,
            session,
            ContextId::first_of(session),
            kind,
        )
        .unwrap();
    blob
}

/// A device crops a 3000-line shell output to its first and last ten
/// lines: the new blob holds exactly those bytes, the old one is gone,
/// and the log, the storage report and the run point at the new one, as
/// the event that referenced the old one. A crop of the crop works the
/// same; a crop or delete of a replaced blob is `BlobGone`.
#[tokio::test(flavor = "multi_thread")]
async fn cropping_a_shell_output_keeps_only_the_chosen_lines() {
    let mut f = Fixture::start_with_shell(true).await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let session = new_session(&f, &mut device).await;
    let content = numbers(1..=3000);
    let (run, output, finished_seq, finished_context) =
        finished_run(&mut f, &mut device, creds.id, session, &content).await;
    // A new current context: the crop is stamped with the run's.
    let clear = Command {
        id: CommandId::new(),
        kind: CommandKind::ClearContext {
            session_id: session,
            context: ContextId::new(),
            name: "next".into(),
        },
    };
    accepted_seq(&device.command(&clear).await);

    let keep = [span(0, 10), span(2990, 10)];
    let crop = crop_command(output.id, &keep);
    let seq = accepted_seq(&device.command(&crop).await);
    let batch = device.events(Seq(seq.0 - 1), 0).await;
    let (logged_seq, event) = batch.iter().next().unwrap();
    assert_eq!(logged_seq, seq);
    let EventKind::BlobCropped {
        blob,
        holds,
        into,
        kept,
        lines,
        by,
    } = &event.kind
    else {
        panic!("not a crop: {:?}", event.kind);
    };
    assert_eq!(
        (*blob, *holds, kept.as_slice(), *lines, *by),
        (output.id, BlobUse::ShellOutput, &keep[..], 3000, creds.id)
    );
    assert_eq!(event.command_id, Some(crop.id));
    assert_eq!(
        (event.session_id, event.context),
        (session, finished_context)
    );

    let mut expected = numbers(1..=10);
    expected.extend(numbers(2991..=3000));
    let reply = device.get(&path::blob(into.id)).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(&reply.body[..], &expected[..]);
    assert_eq!(into.size, expected.len() as u64);
    assert_eq!(into.sha256, skywave_proto::ContentHash::of(&expected));
    assert_eq!(
        device.get(&path::blob(output.id)).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        blob_files(&f),
        vec![into.id.0.to_string()],
        "the old file went"
    );

    let report: StorageReport = device.get(path::STORAGE).await.json();
    assert_eq!(report.event_blobs, into.size);
    assert_eq!(report.sessions[0].blob_bytes, into.size);
    assert_eq!(
        report.blobs,
        vec![BlobStorage {
            id: into.id,
            size: into.size,
            session,
            seq: finished_seq,
            holds: BlobUse::ShellOutput,
        }],
        "the new blob stands where the old one did"
    );
    assert_eq!(report.shell_runs.len(), 1, "the run is listed once");
    assert_eq!(
        (
            report.shell_runs[0].run,
            report.shell_runs[0].output,
            report.shell_runs[0].output_blob
        ),
        (run, into.size, Some(into.id))
    );

    // A crop of the crop: its first five lines.
    let again = crop_command(into.id, &[span(0, 5)]);
    let seq = accepted_seq(&device.command(&again).await);
    let batch = device.events(Seq(seq.0 - 1), 0).await;
    let (_, event) = batch.iter().next().unwrap();
    let EventKind::BlobCropped {
        blob: second_blob,
        into: second,
        lines,
        ..
    } = &event.kind
    else {
        panic!("not a crop: {:?}", event.kind);
    };
    assert_eq!((*second_blob, *lines), (into.id, 20));
    assert_eq!(
        (event.session_id, event.context),
        (session, finished_context)
    );
    let reply = device.get(&path::blob(second.id)).await;
    assert_eq!(&reply.body[..], &numbers(1..=5)[..]);
    let report: StorageReport = device.get(path::STORAGE).await.json();
    assert_eq!(report.shell_runs.len(), 1, "still listed once");
    assert_eq!(report.shell_runs[0].output_blob, Some(second.id));
    assert_eq!(report.blobs[0].seq, finished_seq);

    // Replaced blobs are gone; the newest one is deleted like any.
    for blob in [output.id, into.id] {
        assert_eq!(
            device.command(&crop_command(blob, &[span(0, 1)])).await,
            rejected(RejectReason::BlobGone)
        );
    }
    let delete = Command {
        id: CommandId::new(),
        kind: CommandKind::DeleteBlob { blob: second.id },
    };
    accepted_seq(&device.command(&delete).await);
    assert_eq!(
        device
            .command(&crop_command(second.id, &[span(0, 1)]))
            .await,
        rejected(RejectReason::BlobGone),
        "a crop after a delete"
    );
    assert!(blob_files(&f).is_empty());
}

/// A crop that does not fit is refused, and leaves every blob as it was:
/// an unknown blob, a tool input, spans past the end, keeping every line,
/// spans of the wrong shape. A refusal is stored like any. The same blob
/// crops when the spans fit.
#[tokio::test(flavor = "multi_thread")]
async fn a_crop_that_does_not_fit_is_refused_and_writes_nothing() {
    let mut f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let session = new_session(&f, &mut device).await;
    let output = tool_blob(&mut f, session, BlobUse::ToolOutput, &numbers(1..=100));
    let input = tool_blob(&mut f, session, BlobUse::ToolInput, &numbers(1..=100));
    let before = blob_files(&f);
    let head = f.log.head();

    let refusals = [
        (BlobId::new(), vec![span(0, 1)], RejectReason::UnknownBlob),
        (input.id, vec![span(0, 1)], RejectReason::NotCroppable),
        (
            output.id,
            vec![span(0, 1), span(99, 2)],
            RejectReason::BadCrop,
        ),
        (output.id, vec![span(100, 1)], RejectReason::BadCrop),
        (
            output.id,
            vec![span(0, 50), span(50, 50)],
            RejectReason::BadCrop,
        ),
        (
            output.id,
            vec![span(5, 1), span(0, 1)],
            RejectReason::BadCrop,
        ),
        (output.id, vec![], RejectReason::BadCrop),
    ];
    for (blob, keep, reason) in refusals {
        let crop = crop_command(blob, &keep);
        assert_eq!(
            device.command(&crop).await,
            rejected(reason),
            "{blob:?} {keep:?}"
        );
        assert_eq!(device.command(&crop).await, rejected(reason), "stored");
        assert_eq!(blob_files(&f), before, "no file left by {keep:?}");
    }
    assert_eq!(f.log.head(), head, "nothing logged");
    for blob in [output.id, input.id] {
        assert_eq!(device.get(&path::blob(blob)).await.status, StatusCode::OK);
    }

    // Control: the tool output crops with spans that fit.
    let seq = accepted_seq(
        &device
            .command(&crop_command(output.id, &[span(99, 1)]))
            .await,
    );
    let batch = device.events(Seq(seq.0 - 1), 0).await;
    let (_, event) = batch.iter().next().unwrap();
    assert!(matches!(
        &event.kind,
        EventKind::BlobCropped { holds: BlobUse::ToolOutput, into, lines: 100, .. }
            if into.size == 4
    ));
}

/// A [`CropHook`] that holds every crop until `release` opens, then fails
/// its write with `fail` if set. Counts the crops that reached it, and the
/// temp files the blob store had then; `ended` opens once a crop's thread
/// ends.
#[derive(Default)]
struct HeldCrops {
    entered: Gate,
    release: Gate,
    ended: Gate,
    fail: Option<i32>,
    crops: std::sync::atomic::AtomicUsize,
    dir: std::sync::OnceLock<PathBuf>,
    temp_files: std::sync::atomic::AtomicUsize,
}

impl CropHook for HeldCrops {
    fn writing(&self) -> std::io::Result<()> {
        use std::sync::atomic::Ordering;
        self.crops.fetch_add(1, Ordering::SeqCst);
        if let Some(dir) = self.dir.get() {
            let temp = std::fs::read_dir(dir)
                .unwrap()
                .filter(|e| {
                    e.as_ref()
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".tmp-")
                })
                .count();
            self.temp_files.store(temp, Ordering::SeqCst);
        }
        self.entered.open();
        self.release.wait_blocking();
        match self.fail {
            Some(code) => Err(std::io::Error::from_raw_os_error(code)),
            None => Ok(()),
        }
    }

    fn ended(&self) {
        self.ended.open();
    }
}

impl HeldCrops {
    fn crops(&self) -> usize {
        self.crops.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Opens `gate` when dropped, so a failing test frees a held crop.
struct OpenOnDrop(Gate);

impl Drop for OpenOnDrop {
    fn drop(&mut self) {
        self.0.open();
    }
}

/// A free space a test sets.
#[derive(Default)]
struct Free(std::sync::atomic::AtomicU64);

impl FreeSpace for Free {
    fn free(&self) -> std::io::Result<u64> {
        Ok(self.0.load(std::sync::atomic::Ordering::SeqCst))
    }
}

/// A resend that arrives while the crop is written joins it: one event,
/// one new blob, one crop. A resend after it is answered from the log.
#[tokio::test(flavor = "multi_thread")]
async fn a_crop_resent_while_it_is_written_joins_the_first() {
    let hook = Arc::new(HeldCrops::default());
    let _open = OpenOnDrop(hook.release.clone());
    let joined = Gate::new();
    let options = ServerOptions {
        crop_hook: Some(hook.clone()),
        joined_hook: Some(Arc::new({
            let joined = joined.clone();
            move || joined.open()
        })),
        ..ServerOptions::default()
    };
    let mut f = Fixture::start_cropping_with(Disk::default(), options).await;
    let creds = f.pair().await;
    let mut first = f.client(&creds).await;
    let mut second = f.client(&creds).await;
    let session = new_session(&f, &mut first).await;
    let output = tool_blob(&mut f, session, BlobUse::ToolOutput, &numbers(1..=30));
    let crop = crop_command(output.id, &[span(0, 1)]);

    let sent = crop.clone();
    let first_reply = tokio::spawn(async move { first.command(&sent).await });
    tokio::time::timeout(SOON, hook.entered.opened())
        .await
        .expect("the first request crops");
    let sent = crop.clone();
    let second_reply = tokio::spawn(async move { second.command(&sent).await });
    tokio::time::timeout(SOON, joined.opened())
        .await
        .expect("the resend joins the first");
    assert!(
        !second_reply.is_finished(),
        "it waits for the first's reply"
    );
    assert_eq!(hook.crops(), 1);
    hook.release.open();

    let first = tokio::time::timeout(SOON, first_reply)
        .await
        .unwrap()
        .unwrap();
    let second = tokio::time::timeout(SOON, second_reply)
        .await
        .unwrap()
        .unwrap();
    let seq = accepted_seq(&first);
    assert_eq!(second, first);
    assert_eq!(f.log.head(), seq, "logged once");
    assert_eq!(blob_files(&f).len(), 1, "one new blob, the old one gone");

    let mut device = f.client(&creds).await;
    assert_eq!(device.command(&crop).await, first, "a resend after");
    assert_eq!(hook.crops(), 1, "answered from the log");
    assert_eq!(blob_files(&f).len(), 1);
}

/// A delete taken while a crop of the same blob is written wins: the crop
/// is `BlobGone`, and the blob it wrote is removed.
#[tokio::test(flavor = "multi_thread")]
async fn a_delete_while_a_crop_is_written_makes_it_blob_gone() {
    let hook = Arc::new(HeldCrops::default());
    let _open = OpenOnDrop(hook.release.clone());
    let mut f = Fixture::start_cropping(Disk::default(), Some(hook.clone())).await;
    let creds = f.pair().await;
    let mut cropping = f.client(&creds).await;
    let mut deleting = f.client(&creds).await;
    let session = new_session(&f, &mut cropping).await;
    let output = tool_blob(&mut f, session, BlobUse::ToolOutput, &numbers(1..=3000));
    let crop = crop_command(output.id, &[span(0, 1)]);

    let sent = crop.clone();
    let crop_reply = tokio::spawn(async move { cropping.command(&sent).await });
    tokio::time::timeout(SOON, hook.entered.opened())
        .await
        .expect("the crop is being written");
    let delete = Command {
        id: CommandId::new(),
        kind: CommandKind::DeleteBlob { blob: output.id },
    };
    let deleted = accepted_seq(&deleting.command(&delete).await);
    let files = blob_files(&f);
    assert!(
        files.len() == 1 && files[0].starts_with(".tmp-"),
        "only the crop's file is left: {files:?}"
    );
    hook.release.open();

    let result = tokio::time::timeout(SOON, crop_reply)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result, rejected(RejectReason::BlobGone));
    assert_eq!(f.log.head(), deleted, "the crop logged nothing");
    assert!(blob_files(&f).is_empty(), "the crop's blob was removed");
}

/// A crop is how a low disk gets space back, and lets a stopping server
/// end its work: it is taken in both, unlike new work.
#[tokio::test(flavor = "multi_thread")]
async fn a_crop_is_taken_while_the_disk_is_low_or_the_server_stops() {
    let free = Arc::new(Free::default());
    free.0.store(10, std::sync::atomic::Ordering::SeqCst);
    let disk = Disk::new(free.clone(), 10);
    let mut f = Fixture::start_cropping(disk.clone(), None).await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let session = new_session(&f, &mut device).await;
    let output = tool_blob(&mut f, session, BlobUse::ToolOutput, &numbers(1..=30));
    let message = |text: &str| Command {
        id: CommandId::new(),
        kind: CommandKind::SendMessage {
            session_id: session,
            context: ContextId::first_of(session),
            text: text.into(),
            attachments: Vec::new(),
        },
    };

    free.0.store(9, std::sync::atomic::Ordering::SeqCst);
    assert!(disk.check());
    assert_eq!(
        device.command(&message("work")).await,
        rejected(RejectReason::DiskLow),
        "control: new work is refused"
    );
    let first = accepted_seq(
        &device
            .command(&crop_command(output.id, &[span(0, 20)]))
            .await,
    );
    let batch = device.events(Seq(first.0 - 1), 0).await;
    let (_, event) = batch.iter().next().unwrap();
    let EventKind::BlobCropped { into, .. } = &event.kind else {
        panic!("not a crop: {:?}", event.kind);
    };

    free.0.store(10, std::sync::atomic::Ordering::SeqCst);
    assert!(!disk.check());
    f.log.set_stopping(true);
    assert_eq!(
        device.command(&message("more")).await,
        rejected(RejectReason::ServerStopping),
        "control: new work is refused"
    );
    accepted_seq(&device.command(&crop_command(into.id, &[span(0, 10)])).await);
}

/// A crop whose new blob finds no room on the disk is `DiskLow`, stored
/// like any refusal, and its partial file is removed; the blob it would
/// crop stays.
#[tokio::test(flavor = "multi_thread")]
async fn a_crop_with_no_room_for_its_blob_is_disk_low_and_leaves_no_file() {
    let hook = Arc::new(HeldCrops {
        fail: Some(libc::ENOSPC),
        ..HeldCrops::default()
    });
    hook.release.open();
    let mut f = Fixture::start_cropping(Disk::default(), Some(hook.clone())).await;
    hook.dir.set(f.storage.dir.blobs_dir()).unwrap();
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let session = new_session(&f, &mut device).await;
    let output = tool_blob(&mut f, session, BlobUse::ToolOutput, &numbers(1..=30));
    let head = f.log.head();

    let crop = crop_command(output.id, &[span(0, 1)]);
    assert_eq!(device.command(&crop).await, rejected(RejectReason::DiskLow));
    assert_eq!(
        hook.temp_files.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the crop had a file to remove"
    );
    assert_eq!(blob_files(&f), vec![output.id.0.to_string()]);
    assert_eq!(f.log.head(), head);
    assert_eq!(device.command(&crop).await, rejected(RejectReason::DiskLow));
    assert_eq!(hook.crops(), 1, "the refusal is stored");
    assert_eq!(
        device.get(&path::blob(output.id)).await.status,
        StatusCode::OK
    );
}

/// Another failure to write a crop's blob is a 500 that stores nothing:
/// the device resends it.
#[tokio::test(flavor = "multi_thread")]
async fn a_crop_that_fails_otherwise_is_a_500_and_stores_nothing() {
    let hook = Arc::new(HeldCrops {
        fail: Some(libc::EIO),
        ..HeldCrops::default()
    });
    hook.release.open();
    let mut f = Fixture::start_cropping(Disk::default(), Some(hook.clone())).await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let session = new_session(&f, &mut device).await;
    let output = tool_blob(&mut f, session, BlobUse::ToolOutput, &numbers(1..=30));

    let crop = crop_command(output.id, &[span(0, 1)]);
    for crops in [1, 2] {
        let reply = device
            .send(Method::POST, path::COMMANDS, &[], json(&crop))
            .await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(hook.crops(), crops, "nothing stored: cropped again");
    }
    assert_eq!(blob_files(&f), vec![output.id.0.to_string()]);
}

/// A crop past its deadline is a 503 and nothing is handled; the crop
/// left running removes its file when it ends, and a resend crops anew.
#[tokio::test(flavor = "multi_thread")]
async fn a_crop_past_its_deadline_is_a_503_and_leaves_no_file() {
    let hook = Arc::new(HeldCrops::default());
    let _open = OpenOnDrop(hook.release.clone());
    let options = ServerOptions {
        crop_hook: Some(hook.clone()),
        crop_deadline: Duration::from_millis(200),
        ..ServerOptions::default()
    };
    let mut f = Fixture::start_cropping_with(Disk::default(), options).await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let session = new_session(&f, &mut device).await;
    let output = tool_blob(&mut f, session, BlobUse::ToolOutput, &numbers(1..=30));
    let head = f.log.head();

    let crop = crop_command(output.id, &[span(0, 1)]);
    // Held until the reply is in: past any deadline.
    let reply = device
        .send(Method::POST, path::COMMANDS, &[], json(&crop))
        .await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(f.log.head(), head, "nothing handled");
    assert!(!hook.ended.is_open(), "the crop still runs");
    hook.release.open();
    tokio::time::timeout(SOON, hook.ended.opened())
        .await
        .expect("the abandoned crop ends");
    assert_eq!(
        blob_files(&f),
        vec![output.id.0.to_string()],
        "its file went with it"
    );

    let seq = accepted_seq(&device.command(&crop).await);
    assert_eq!(hook.crops(), 2, "the resend cropped anew");
    let batch = device.events(Seq(seq.0 - 1), 0).await;
    let (_, event) = batch.iter().next().unwrap();
    let EventKind::BlobCropped { into, .. } = &event.kind else {
        panic!("not a crop: {:?}", event.kind);
    };
    assert_eq!(blob_files(&f), vec![into.id.0.to_string()]);
}

/// `route?session=..&path=..`, with `path` percent-escaped.
fn workdir_uri(route: &str, session: SessionId, path: &str) -> String {
    let mut escaped = String::new();
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            escaped.push(char::from(byte));
        } else {
            escaped.push_str(&format!("%{byte:02X}"));
        }
    }
    format!("{route}?session={}&path={escaped}", session.0)
}

#[tokio::test(flavor = "multi_thread")]
async fn workdir_list_and_file_answer_every_outcome_with_200() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let workdir = f.root.join("project");
    std::fs::create_dir(workdir.join("src")).unwrap();
    std::fs::write(workdir.join("my notes & more.txt"), "hello\n").unwrap();
    let created = device.command(&f.create_session_command()).await;
    let session = session_of(&created, &device.events(Seq::ZERO, 0).await);
    let list = |path: &str| workdir_uri(path::WORKDIR_LIST, session, path);
    let file = |path: &str| workdir_uri(path::WORKDIR_FILE, session, path);

    let reply = device.get(&list("")).await;
    assert_eq!(reply.status, StatusCode::OK);
    let ListResult::Listed { path, entries, .. } = reply.json::<ListResult>() else {
        panic!("expected a listing: {:?}", reply.body);
    };
    assert_eq!(path, "");
    assert_eq!(
        entries,
        vec![
            DirEntry {
                name: "my notes & more.txt".into(),
                kind: EntryKind::File,
                size: Some(6),
            },
            DirEntry {
                name: "src".into(),
                kind: EntryKind::Dir,
                size: None,
            },
        ]
    );

    let reply = device.get(&file("src/../my notes & more.txt")).await;
    assert_eq!(reply.status, StatusCode::OK);
    let FileResult::File {
        path,
        size,
        content: FileBody::Blob(blob),
        ..
    } = reply.json::<FileResult>()
    else {
        panic!("expected a file: {:?}", reply.body);
    };
    assert_eq!((path.as_str(), size), ("my notes & more.txt", 6));
    let content = device.get(&path::blob(blob.id)).await;
    assert_eq!(content.status, StatusCode::OK);
    assert_eq!(&content.body[..], b"hello\n");

    let unknown = SessionId::new();
    let list_cases = [
        (list("../.."), ListResult::NotAllowed),
        (list("missing"), ListResult::NotFound),
        (list("my notes & more.txt"), ListResult::NotADirectory),
        (
            workdir_uri(path::WORKDIR_LIST, unknown, ""),
            ListResult::UnknownSession,
        ),
    ];
    for (uri, want) in list_cases {
        let reply = device.get(&uri).await;
        assert_eq!(reply.status, StatusCode::OK, "{uri}");
        assert_eq!(reply.json::<ListResult>(), want, "{uri}");
    }
    let file_cases = [
        (file("/etc/hostname"), FileResult::NotAllowed),
        (file("missing"), FileResult::NotFound),
        (file("src"), FileResult::IsADirectory),
        (
            workdir_uri(path::WORKDIR_FILE, unknown, "x"),
            FileResult::UnknownSession,
        ),
    ];
    for (uri, want) in file_cases {
        let reply = device.get(&uri).await;
        assert_eq!(reply.status, StatusCode::OK, "{uri}");
        assert_eq!(reply.json::<FileResult>(), want, "{uri}");
    }

    std::fs::remove_dir_all(&workdir).unwrap();
    let reply = device.get(&list("")).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json::<ListResult>(), ListResult::WorkdirGone);
    let reply = device.get(&file("x")).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json::<FileResult>(), FileResult::WorkdirGone);

    // A malformed query is a 400; without a certificate, a 401.
    let bad = format!("{}?session=nope", path::WORKDIR_LIST);
    assert_eq!(device.get(&bad).await.status, StatusCode::BAD_REQUEST);
    let mut anon = f.anonymous().await;
    for route in [path::WORKDIR_LIST, path::WORKDIR_FILE] {
        assert_eq!(
            anon.get(&workdir_uri(route, session, "")).await.status,
            StatusCode::UNAUTHORIZED,
            "{route}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn workdir_download_serves_a_binary_file_as_a_blob() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let workdir = f.root.join("project");
    let bytes = b"\x89PNG\0\x01\x02";
    std::fs::write(workdir.join("logo.bin"), bytes).unwrap();
    let created = device.command(&f.create_session_command()).await;
    let session = session_of(&created, &device.events(Seq::ZERO, 0).await);

    let reply = device
        .get(&workdir_uri(path::WORKDIR_DOWNLOAD, session, "logo.bin"))
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    let FileResult::File {
        size: 7,
        content: FileBody::Blob(blob),
        ..
    } = reply.json::<FileResult>()
    else {
        panic!("expected a binary blob: {:?}", reply.body);
    };
    let content = device.get(&path::blob(blob.id)).await;
    assert_eq!(content.status, StatusCode::OK);
    assert_eq!(&content.body[..], bytes);

    // The same path through the file route is still refused as binary.
    let reply = device
        .get(&workdir_uri(path::WORKDIR_FILE, session, "logo.bin"))
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    let FileResult::File {
        content: FileBody::Binary,
        ..
    } = reply.json::<FileResult>()
    else {
        panic!("expected binary, got {:?}", reply.body);
    };
}

/// Stages `bytes` with `PUT` as any file, not only an image; returns the hash.
async fn stage_file(device: &mut Client, bytes: &[u8]) -> ContentHash {
    let hash = ContentHash::of(bytes);
    let reply = device
        .send(
            Method::PUT,
            &path::attachment(hash),
            &[],
            Bytes::from(bytes.to_vec()),
        )
        .await;
    assert!(matches!(
        reply.json::<UploadResult>(),
        UploadResult::Stored { .. }
    ));
    hash
}

/// Posts `request` to the write route and returns its answer, which is a 200.
async fn write_file(device: &mut Client, request: &WriteFileRequest) -> WriteFileResult {
    let reply = device
        .send(Method::POST, path::WORKDIR_WRITE, &[], json(request))
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    reply.json()
}

/// A staged file is written where its path names and logged as `FileWritten`
/// by the calling device, with the answer naming that event's seq. The same
/// request again is `Same` and logs nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_staged_file_is_written_into_the_workdir_and_logged() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let workdir = f.root.join("project");
    std::fs::create_dir(workdir.join("notes")).unwrap();
    let created = device.command(&f.create_session_command()).await;
    let session = session_of(&created, &device.events(Seq::ZERO, 0).await);
    let content = b"hello from the phone\n";
    let hash = stage_file(&mut device, content).await;
    let request = WriteFileRequest {
        session,
        path: "notes/a.txt".into(),
        content: hash,
        replace: None,
    };

    let before = f.log.head();
    let WriteFileResult::Written {
        path: written,
        size,
        seq,
    } = write_file(&mut device, &request).await
    else {
        panic!("expected Written");
    };
    assert_eq!(written, "notes/a.txt");
    assert_eq!(size, content.len() as u64);
    assert!(seq > before);
    assert_eq!(seq, f.log.head(), "the answer names the event just logged");
    assert_eq!(std::fs::read(workdir.join("notes/a.txt")).unwrap(), content);

    let batch = device.events(Seq(seq.0 - 1), 0).await;
    let logged = batch
        .iter()
        .find(|(s, _)| *s == seq)
        .map(|(_, e)| e.kind.clone());
    assert_eq!(
        logged,
        Some(EventKind::FileWritten {
            path: "notes/a.txt".into(),
            sha256: hash,
            size,
            replaced: None,
            by: creds.id,
        })
    );

    let head = f.log.head();
    assert_eq!(
        write_file(&mut device, &request).await,
        WriteFileResult::Same {
            path: "notes/a.txt".into(),
            size,
        }
    );
    assert_eq!(f.log.head(), head, "a same write logs nothing");
}

/// A file already at the path is replaced only when the request names its
/// hash. Otherwise the answer is `Exists` with that hash and nothing changes;
/// naming it writes the file, and the event records what it replaced.
#[tokio::test(flavor = "multi_thread")]
async fn a_write_replaces_an_existing_file_only_with_its_hash() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let workdir = f.root.join("project");
    let old = b"the old a\n";
    std::fs::write(workdir.join("a.txt"), old).unwrap();
    let created = device.command(&f.create_session_command()).await;
    let session = session_of(&created, &device.events(Seq::ZERO, 0).await);
    let new = b"the new a, longer than the old\n";
    let new_hash = stage_file(&mut device, new).await;
    let old_hash = ContentHash::of(old);
    let mut request = WriteFileRequest {
        session,
        path: "a.txt".into(),
        content: new_hash,
        replace: None,
    };

    assert_eq!(
        write_file(&mut device, &request).await,
        WriteFileResult::Exists {
            sha256: old_hash,
            size: old.len() as u64,
        }
    );
    request.replace = Some(ContentHash::of(b"some other file\n"));
    assert_eq!(
        write_file(&mut device, &request).await,
        WriteFileResult::Exists {
            sha256: old_hash,
            size: old.len() as u64,
        },
        "a hash that is not the file's does not replace it"
    );
    assert_eq!(std::fs::read(workdir.join("a.txt")).unwrap(), old);

    request.replace = Some(old_hash);
    let WriteFileResult::Written { size, seq, .. } = write_file(&mut device, &request).await else {
        panic!("expected Written");
    };
    assert_eq!(size, new.len() as u64);
    assert_eq!(std::fs::read(workdir.join("a.txt")).unwrap(), new);

    let batch = device.events(Seq(seq.0 - 1), 0).await;
    let logged = batch
        .iter()
        .find(|(s, _)| *s == seq)
        .map(|(_, e)| e.kind.clone());
    assert_eq!(
        logged,
        Some(EventKind::FileWritten {
            path: "a.txt".into(),
            sha256: new_hash,
            size,
            replaced: Some(old_hash),
            by: creds.id,
        })
    );
}

/// A write the server refuses is answered in the body with a 200 and changes
/// neither the workdir nor the log. A body that is not a request is a 400.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_write_answers_200_and_writes_nothing() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let workdir = f.root.join("project");
    let created = device.command(&f.create_session_command()).await;
    let session = session_of(&created, &device.events(Seq::ZERO, 0).await);
    let hash = stage_file(&mut device, b"staged\n").await;
    let write = |path: &str, content: ContentHash| WriteFileRequest {
        session,
        path: path.into(),
        content,
        replace: None,
    };
    let head = f.log.head();

    let refused = [
        (write("../x", hash), WriteFileResult::NotAllowed),
        (write(".git/config", hash), WriteFileResult::NotAllowed),
        (
            write("ghost.txt", ContentHash::of(b"never staged\n")),
            WriteFileResult::NotStaged,
        ),
        (
            WriteFileRequest {
                session: SessionId::new(),
                path: "x.txt".into(),
                content: hash,
                replace: None,
            },
            WriteFileResult::UnknownSession,
        ),
    ];
    for (request, answer) in refused {
        assert_eq!(
            write_file(&mut device, &request).await,
            answer,
            "{request:?}"
        );
    }
    let not_json = device
        .send(
            Method::POST,
            path::WORKDIR_WRITE,
            &[],
            Bytes::from_static(b"{"),
        )
        .await;
    assert_eq!(not_json.status, StatusCode::BAD_REQUEST);

    assert_eq!(f.log.head(), head, "refused writes log nothing");
    assert!(!f.root.join("x").exists(), "../x is not written");
    assert!(!workdir.join("ghost.txt").exists());
    assert!(!workdir.join(".git").exists());

    // Control: the same route writes what is allowed.
    assert!(matches!(
        write_file(&mut device, &write("ok.txt", hash)).await,
        WriteFileResult::Written { .. }
    ));
    assert_eq!(std::fs::read(workdir.join("ok.txt")).unwrap(), b"staged\n");
}

/// On a low disk a write is `DiskLow` and writes nothing; with room again the
/// same write goes through.
#[tokio::test(flavor = "multi_thread")]
async fn a_write_on_a_low_disk_is_disk_low() {
    let free = Arc::new(Free::default());
    free.0.store(10, std::sync::atomic::Ordering::SeqCst);
    let disk = Disk::new(free.clone(), 10);
    let f = Fixture::start_cropping(disk.clone(), None).await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let workdir = f.root.join("project");
    let created = device.command(&f.create_session_command()).await;
    let session = session_of(&created, &device.events(Seq::ZERO, 0).await);
    let content = b"low disk\n";
    let hash = stage_file(&mut device, content).await;
    let request = WriteFileRequest {
        session,
        path: "low.txt".into(),
        content: hash,
        replace: None,
    };
    let head = f.log.head();

    free.0.store(9, std::sync::atomic::Ordering::SeqCst);
    assert!(disk.check());
    assert_eq!(
        write_file(&mut device, &request).await,
        WriteFileResult::DiskLow
    );
    assert!(!workdir.join("low.txt").exists());
    assert_eq!(f.log.head(), head, "a refused write logs nothing");

    free.0.store(10, std::sync::atomic::Ordering::SeqCst);
    assert!(!disk.check());
    assert!(
        matches!(
            write_file(&mut device, &request).await,
            WriteFileResult::Written { .. }
        ),
        "control: with room again the write lands"
    );
    assert_eq!(std::fs::read(workdir.join("low.txt")).unwrap(), content);
}

/// The content blob of the answer to reading file `name`.
async fn file_blob(device: &mut Client, session: SessionId, name: &str) -> BlobRef {
    let reply = device
        .get(&workdir_uri(path::WORKDIR_FILE, session, name))
        .await;
    match reply.json::<FileResult>() {
        FileResult::File {
            content: FileBody::Blob(blob),
            ..
        } => blob,
        other => panic!("expected a file blob, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_blob_only_a_replaced_answer_named_is_a_404() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let workdir = f.root.join("project");
    std::fs::write(workdir.join("notes"), "first\n").unwrap();
    let created = device.command(&f.create_session_command()).await;
    let session = session_of(&created, &device.events(Seq::ZERO, 0).await);
    let first = file_blob(&mut device, session, "notes").await;
    assert_eq!(
        device.get(&path::blob(first.id)).await.status,
        StatusCode::OK
    );
    std::fs::write(workdir.join("notes"), "second\n").unwrap();
    let second = file_blob(&mut device, session, "notes").await;
    assert_ne!(second.id, first.id);
    assert_eq!(
        device.get(&path::blob(first.id)).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        device.get(&path::blob(second.id)).await.status,
        StatusCode::OK
    );
}

/// The daemon's git, immune to the user's config.
fn isolated_git() -> Git {
    Git::default()
        .with_env("GIT_CONFIG_GLOBAL", "/dev/null")
        .with_env("GIT_CONFIG_NOSYSTEM", "1")
}

/// Runs the real git for test setup in `dir`, immune to the user's config.
fn setup_git(dir: &std::path::Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap_or_else(|e| panic!("this test needs `git` on PATH: {e}"));
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn workdir_changes_answer_with_200_and_a_diff_blob() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let workdir = f.root.join("project");
    setup_git(&workdir, &["init", "-q"]);
    std::fs::write(workdir.join("a.txt"), "one\n").unwrap();
    setup_git(&workdir, &["add", "a.txt"]);
    setup_git(&workdir, &["commit", "-q", "-m", "first"]);
    std::fs::write(workdir.join("a.txt"), "two\n").unwrap();
    let created = device.command(&f.create_session_command()).await;
    let session = session_of(&created, &device.events(Seq::ZERO, 0).await);
    let changes = |session: SessionId| format!("{}?session={}", path::WORKDIR_CHANGES, session.0);

    let reply = device.get(&changes(session)).await;
    assert_eq!(reply.status, StatusCode::OK);
    let ChangesResult::Changes(set) = reply.json::<ChangesResult>() else {
        panic!("expected changes: {:?}", reply.body);
    };
    assert_eq!((set.branch.as_deref(), set.total), (Some("main"), 1));
    let file = &set.files[0];
    assert_eq!(
        (file.path.as_str(), file.change),
        ("a.txt", FileChange::Modified)
    );
    let DiffBody::Blob(blob) = &file.diff else {
        panic!("expected a diff blob: {file:?}");
    };
    let diff = device.get(&path::blob(blob.id)).await;
    assert_eq!(diff.status, StatusCode::OK);
    let diff = String::from_utf8(diff.body.to_vec()).unwrap();
    assert!(diff.contains("-one\n+two\n"), "{diff}");

    let reply = device.get(&changes(SessionId::new())).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json::<ChangesResult>(), ChangesResult::UnknownSession);
    let mut anon = f.anonymous().await;
    assert_eq!(
        anon.get(&changes(session)).await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn workdir_log_answers_with_200_and_the_commits() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    let workdir = f.root.join("project");
    setup_git(&workdir, &["init", "-q"]);
    std::fs::write(workdir.join("a.txt"), "one\n").unwrap();
    setup_git(&workdir, &["add", "a.txt"]);
    setup_git(&workdir, &["commit", "-q", "-m", "first"]);
    let created = device.command(&f.create_session_command()).await;
    let session = session_of(&created, &device.events(Seq::ZERO, 0).await);
    let log = |session: SessionId| format!("{}?session={}", path::WORKDIR_LOG, session.0);

    let reply = device.get(&log(session)).await;
    assert_eq!(reply.status, StatusCode::OK);
    let LogResult::Log(answer) = reply.json::<LogResult>() else {
        panic!("expected a log: {:?}", reply.body);
    };
    assert_eq!(answer.branch.as_deref(), Some("main"));
    assert_eq!(answer.commits.len(), 1);
    assert_eq!(answer.commits[0].subject, "first");
    assert!(!answer.more);

    let reply = device.get(&log(SessionId::new())).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json::<LogResult>(), LogResult::UnknownSession);
    let mut anon = f.anonymous().await;
    assert_eq!(
        anon.get(&log(session)).await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn worktrees_lists_a_sessions_worktree_with_its_status() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    make_repo(&f.root.join("project"));
    let accepted = device.command(&worktree_command(&f, "feat/x")).await;
    assert_eq!(accepted, CommandResult::Accepted { seq: Seq(1) });
    let batch = device.events(Seq::ZERO, 0).await;
    let EventKind::SessionCreated {
        worktree: Some(worktree),
        ..
    } = &batch.events[0].kind
    else {
        panic!("{batch:?}");
    };

    let reply = device.get(path::WORKTREES).await;
    assert_eq!(reply.status, StatusCode::OK);
    let list = reply.json::<WorktreeList>();
    assert_eq!(list.repos.len(), 1, "{list:?}");
    assert_eq!(list.repos[0].repo, worktree.repo);
    let RepoListing::Worktrees { worktrees, more } = &list.repos[0].listing else {
        panic!("{:?}", list.repos[0]);
    };
    assert!(!more);
    let [main, linked] = worktrees.as_slice() else {
        panic!("{worktrees:?}");
    };
    assert!(
        main.main && main.merged && main.dirty == Some(false),
        "{main:?}"
    );
    assert!(
        !linked.main && linked.merged && linked.dirty == Some(false),
        "{linked:?}"
    );
    assert_eq!(linked.branch.as_deref(), Some("feat/x"));
    assert!(!linked.missing, "{linked:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn worktrees_answers_gone_for_a_deleted_repo() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    make_repo(&f.root.join("project"));
    let accepted = device.command(&worktree_command(&f, "feat/x")).await;
    assert_eq!(accepted, CommandResult::Accepted { seq: Seq(1) });

    // Positive control: listed while the repository is there.
    let reply = device.get(path::WORKTREES).await;
    assert_eq!(reply.status, StatusCode::OK);
    let list = reply.json::<WorktreeList>();
    assert!(
        matches!(list.repos[0].listing, RepoListing::Worktrees { .. }),
        "{list:?}"
    );

    std::fs::remove_dir_all(f.root.join("project.worktrees/feat-x")).unwrap();
    std::fs::remove_dir_all(f.root.join("project")).unwrap();
    let reply = device.get(path::WORKTREES).await;
    assert_eq!(reply.status, StatusCode::OK);
    let list = reply.json::<WorktreeList>();
    assert_eq!(list.repos.len(), 1, "{list:?}");
    assert_eq!(list.repos[0].listing, RepoListing::Gone, "{list:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn worktrees_is_empty_without_worktree_sessions() {
    let f = Fixture::start().await;
    let creds = f.pair().await;
    let mut device = f.client(&creds).await;
    make_repo(&f.root.join("project"));
    device.command(&f.create_session_command()).await;

    let reply = device.get(path::WORKTREES).await;
    assert_eq!(reply.status, StatusCode::OK);
    let list = reply.json::<WorktreeList>();
    assert!(list.repos.is_empty(), "{list:?}");

    // Positive control: a worktree session makes its repository listed.
    device.command(&worktree_command(&f, "feat/x")).await;
    let list = device.get(path::WORKTREES).await.json::<WorktreeList>();
    assert_eq!(list.repos.len(), 1, "{list:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_long_workdir_changes_does_not_block_hello() {
    let fake = tempfile::tempdir().unwrap();
    let script = fake.path().join("slow-git");
    let count = fake.path().join("slow-git.count");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho started >> '{}'\nsleep 5\n",
            count.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let f = Fixture::start_with_git(Git::new(&script)).await;
    let creds = f.pair().await;
    let mut slow = f.client(&creds).await;
    let created = slow.command(&f.create_session_command()).await;
    let session = session_of(&created, &slow.events(Seq::ZERO, 0).await);
    let uri = format!("{}?session={}", path::WORKDIR_CHANGES, session.0);
    let pending = tokio::spawn(async move { slow.get(&uri).await });

    // Once its git is running, another connection still gets answered.
    let started = Instant::now();
    while !count.exists() {
        assert!(started.elapsed() < SOON, "the fake git never started");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mut device = f.client(&creds).await;
    let hello = tokio::time::timeout(SOON, device.get(path::HELLO))
        .await
        .expect("hello answers while changes run");
    assert_eq!(hello.status, StatusCode::OK);
    assert!(!pending.is_finished(), "the changes request ended early");

    let reply = tokio::time::timeout(SOON, pending).await.unwrap().unwrap();
    assert_eq!(reply.status, StatusCode::OK);
    assert!(
        matches!(reply.json::<ChangesResult>(), ChangesResult::Failed { .. }),
        "{:?}",
        reply.body
    );
}

/// A TLS connection to `f` from the loopback address `local`, with `creds`
/// if given, and a small receive buffer when `small_window`.
async fn tls_from(
    f: &Fixture,
    local: Ipv4Addr,
    creds: Option<&Credentials>,
    small_window: bool,
) -> std::io::Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let socket = TcpSocket::new_v4()?;
    socket.bind(SocketAddr::from((local, 0)))?;
    if small_window {
        socket.set_recv_buffer_size(4096)?;
    }
    let tcp = socket.connect(f.addr).await?;
    let config = client_config(f.identity.ca_cert_der(), creds);
    TlsConnector::from(Arc::new(config))
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await
}

/// Whether a TLS connection from `local` gets through the handshake, decided
/// within [`SOON`]. A connection over a cap is closed before TLS, so its
/// handshake fails.
async fn admitted(f: &Fixture, local: Ipv4Addr) -> bool {
    tokio::time::timeout(SOON, tls_from(f, local, None, false))
        .await
        .expect("the handshake ends promptly either way")
        .is_ok()
}

const LOCAL: Ipv4Addr = Ipv4Addr::LOCALHOST;

#[tokio::test(flavor = "multi_thread")]
async fn connections_past_the_cap_are_closed_until_one_ends() {
    let f = Fixture::start_with_limits(ConnectionLimits {
        max_connections: 2,
        ..ConnectionLimits::DEFAULT
    })
    .await;
    // One connection that never starts TLS, one served: both under the cap.
    let idle = TcpStream::connect(f.addr).await.unwrap();
    let mut served = f.anonymous().await;
    assert_eq!(
        served.get(path::HELLO).await.status,
        StatusCode::UNAUTHORIZED
    );

    assert!(!admitted(&f, LOCAL).await, "a third connection is closed");
    assert_eq!(
        served.get(path::HELLO).await.status,
        StatusCode::UNAUTHORIZED,
        "the admitted ones are still served"
    );

    // Closing one gives its place back once the server sees the close,
    // which this side cannot observe: retry until then.
    drop(idle);
    let started = Instant::now();
    while !admitted(&f, LOCAL).await {
        assert!(
            started.elapsed() < SOON,
            "the closed connection's place came back"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn one_address_past_its_cap_does_not_shut_out_another() {
    let f = Fixture::start_with_limits(ConnectionLimits {
        max_per_ip: 1,
        ..ConnectionLimits::DEFAULT
    })
    .await;
    let _held = tls_from(&f, LOCAL, None, false).await.unwrap();
    assert!(!admitted(&f, LOCAL).await, "a second from the same address");
    // Linux routes all of 127.0.0.0/8 to loopback.
    assert!(
        admitted(&f, Ipv4Addr::new(127, 0, 0, 2)).await,
        "another address"
    );
}

/// Starts `POST /v1/pair` on a new anonymous connection with a body sent
/// chunk by chunk through the returned sender; the request's status (or
/// error) arrives on the returned task.
async fn post_pair_streamed(
    f: &Fixture,
) -> (
    mpsc::UnboundedSender<Bytes>,
    JoinHandle<hyper::Result<StatusCode>>,
) {
    let tls = tls_from(f, LOCAL, None, false).await.unwrap();
    let (mut sender, conn) = http1::handshake(TokioIo::new(tls)).await.unwrap();
    tokio::spawn(conn);
    let (chunks, rx) = mpsc::unbounded_channel::<Bytes>();
    let frames = futures_util::stream::unfold(rx, |mut rx| async move {
        let chunk = rx.recv().await?;
        Some((
            Ok::<_, std::convert::Infallible>(hyper::body::Frame::data(chunk)),
            rx,
        ))
    });
    let req = Request::post(path::PAIR)
        .header(header::HOST, "localhost")
        .header(header::CONTENT_TYPE, "application/json")
        .body(http_body_util::StreamBody::new(frames))
        .unwrap();
    let reply = tokio::spawn(async move { Ok(sender.send_request(req).await?.status()) });
    (chunks, reply)
}

fn pair_request(f: &Fixture) -> Bytes {
    let issued =
        pairing::issue_code(f.storage.db.conn(), Utc::now(), pairing::DEFAULT_TTL).unwrap();
    let key = KeyPair::generate().unwrap();
    json(&PairRequest {
        code: issued.code.to_string(),
        csr_pem: CertificateParams::default()
            .serialize_request(&key)
            .unwrap()
            .pem()
            .unwrap(),
        device_name: "trickle".into(),
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_body_that_stops_arriving_fails() {
    let f = Fixture::start_with_limits(ConnectionLimits {
        body_stall: Duration::from_millis(200),
        ..ConnectionLimits::DEFAULT
    })
    .await;
    let body = pair_request(&f);
    let (chunks, reply) = post_pair_streamed(&f).await;
    chunks.send(body.slice(..10)).unwrap();
    // `chunks` stays open: the rest never comes.
    let status = tokio::time::timeout(SOON, reply)
        .await
        .expect("a stalled body ends the request")
        .unwrap();
    // 408, which a device retries, not the 400 the JSON extractor makes of
    // a failed body, which it takes as final.
    assert_eq!(status.unwrap(), StatusCode::REQUEST_TIMEOUT);
    drop(chunks);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_body_that_keeps_arriving_has_no_deadline() {
    let stall = Duration::from_secs(2);
    let f = Fixture::start_with_limits(ConnectionLimits {
        body_stall: stall,
        ..ConnectionLimits::DEFAULT
    })
    .await;
    let body = pair_request(&f);
    let (chunks, reply) = post_pair_streamed(&f).await;
    // Six chunks a quarter of the stall apart: longer than the stall in
    // total, never stalled.
    let started = Instant::now();
    let step = body.len().div_ceil(6);
    for start in (0..body.len()).step_by(step) {
        if start > 0 {
            tokio::time::sleep(stall / 4).await;
        }
        chunks
            .send(body.slice(start..(start + step).min(body.len())))
            .unwrap();
    }
    drop(chunks);
    let status = tokio::time::timeout(SOON, reply).await.unwrap().unwrap();
    assert_eq!(status.unwrap(), StatusCode::OK);
    assert!(
        started.elapsed() > stall,
        "the body took longer than the stall"
    );
}

/// Downloads `uri` with `creds` over a connection with a small receive
/// window, reading nothing for `pause` first; how many bytes arrive before
/// the connection ends.
async fn download_after(f: &Fixture, creds: &Credentials, uri: &str, pause: Duration) -> usize {
    let mut tls = tls_from(f, LOCAL, Some(creds), true).await.unwrap();
    let request = format!("GET {uri} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    tls.write_all(request.as_bytes()).await.unwrap();
    tokio::time::sleep(pause).await;
    let mut received = 0;
    let mut buf = vec![0; 64 * 1024];
    let read_all = async {
        // A closed connection may end in an error rather than a clean EOF.
        while let Ok(n @ 1..) = tls.read(&mut buf).await {
            received += n;
        }
    };
    tokio::time::timeout(SOON, read_all)
        .await
        .expect("the download ends");
    received
}

/// Stores `size` bytes as a blob; its content and download URI.
fn put_blob(f: &Fixture, size: usize) -> (Vec<u8>, String) {
    let content: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    let blob = BlobStore::new(f.storage.dir.blobs_dir())
        .put(f.storage.db.conn(), &content[..])
        .unwrap();
    (content, path::blob(blob.id))
}

/// `TCP_USER_TIMEOUT` is Linux's: elsewhere a peer that stops taking data
/// is only dropped by the OS's own retransmission limit.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn a_peer_that_stops_reading_loses_the_connection() {
    let unacked = Duration::from_millis(200);
    let f = Fixture::start_with_limits(ConnectionLimits {
        unacked_timeout: unacked,
        ..ConnectionLimits::DEFAULT
    })
    .await;
    let creds = f.pair().await;
    // Far more than the socket buffers on both sides hold.
    let size = 32 << 20;
    let (_, uri) = put_blob(&f, size);

    // Read at once, the whole blob arrives.
    let whole = download_after(&f, &creds, &uri, Duration::ZERO).await;
    assert!(whole > size, "{whole} bytes for a {size} byte blob");
    // Read nothing for long enough that the server's zero-window probes
    // (which back off from the 200 ms minimum retransmission timeout) pass
    // the limit: the server gives up.
    let cut = download_after(&f, &creds, &uri, Duration::from_secs(3)).await;
    assert!(cut < size, "{cut} bytes for a {size} byte blob");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slow_download_outlasts_the_idle_and_unacked_limits_on_a_kept_connection() {
    let idle = Duration::from_millis(300);
    let f = Fixture::start_with_limits(ConnectionLimits {
        idle_timeout: idle,
        unacked_timeout: Duration::from_millis(200),
        ..ConnectionLimits::DEFAULT
    })
    .await;
    let creds = f.pair().await;
    // More than hyper's write buffer holds, so the response is fully
    // encoded long before a slow reader has taken it.
    let size = 1 << 20;
    let (content, uri) = put_blob(&f, size);

    // A small receive window and small reads keep the rest waiting on the
    // server, not in a client-side buffer: once the last byte is read, the
    // server has just sent it. Raw HTTP, so the request keeps the
    // connection alive.
    let mut tls = tls_from(&f, LOCAL, Some(&creds), true).await.unwrap();
    let get = |uri: &str| format!("GET {uri} HTTP/1.1\r\nHost: localhost\r\n\r\n");
    let started = Instant::now();
    tls.write_all(get(&uri).as_bytes()).await.unwrap();
    let mut received = Vec::new();
    let mut buf = [0; 4096];
    let body_start = loop {
        let n = tls.read(&mut buf).await.unwrap();
        assert!(n > 0, "the connection ended inside the head");
        received.extend_from_slice(&buf[..n]);
        if let Some(end) = received.windows(4).position(|w| w == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let head = String::from_utf8(received[..body_start].to_vec()).unwrap();
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    while received.len() < body_start + size {
        let n = tls.read(&mut buf).await.unwrap();
        assert!(n > 0, "the connection ended after {} bytes", received.len());
        received.extend_from_slice(&buf[..n]);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        started.elapsed() > idle * 3,
        "the download was slow: {:?}",
        started.elapsed()
    );
    assert_eq!(received.len(), body_start + size);
    assert!(
        received[body_start..] == content[..],
        "the blob arrived intact"
    );

    // The same connection still serves the next request.
    tls.write_all(get(path::HELLO).as_bytes()).await.unwrap();
    let n = tls.read(&mut buf).await.unwrap();
    assert!(buf[..n].starts_with(b"HTTP/1.1 200"), "{:?}", &buf[..n]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_half_sent_request_head_is_closed() {
    let idle = Duration::from_millis(300);
    let f = Fixture::start_with_limits(ConnectionLimits {
        idle_timeout: idle,
        ..ConnectionLimits::DEFAULT
    })
    .await;
    // The clock starts with the connection: the handshake counts.
    let started = Instant::now();
    let mut tls = tls_from(&f, LOCAL, None, false).await.unwrap();
    // Half a request head, then nothing: closed after the idle limit.
    tls.write_all(b"GET /v1/hello HTTP/1.1\r\n").await.unwrap();
    let mut buf = [0; 1024];
    let read = tokio::time::timeout(SOON, async {
        while let Ok(1..) = tls.read(&mut buf).await {}
    })
    .await;
    assert!(read.is_ok(), "the connection was closed");
    assert!(started.elapsed() >= idle, "not before the limit");
}

/// Reads one whole response (`Content-Length` framed) from `tls`; its head and
/// body, as text.
async fn read_full_response(tls: &mut tokio_rustls::client::TlsStream<TcpStream>) -> String {
    let mut received = Vec::new();
    let mut buf = [0; 4096];
    let (head_end, length) = loop {
        let n = tls.read(&mut buf).await.unwrap();
        assert!(n > 0, "the connection ended inside the head");
        received.extend_from_slice(&buf[..n]);
        if let Some(end) = received.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&received[..end]).to_ascii_lowercase();
            let length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .map_or(0, |n| n.trim().parse::<usize>().unwrap());
            break (end + 4, length);
        }
    };
    while received.len() < head_end + length {
        let n = tls.read(&mut buf).await.unwrap();
        assert!(n > 0, "the connection ended inside the body");
        received.extend_from_slice(&buf[..n]);
    }
    String::from_utf8_lossy(&received[..head_end + length]).into_owned()
}

/// Reads one whole response (`Content-Length` framed) from `tls`; its status
/// line.
async fn read_response(tls: &mut tokio_rustls::client::TlsStream<TcpStream>) -> String {
    let full = read_full_response(tls).await;
    full.lines().next().unwrap().to_owned()
}

/// Sends `GET uri` on a keep-alive connection, reads the whole response,
/// then sends nothing more: asserts the server closes the connection after
/// `idle`, counted from the response.
async fn closed_when_idle_after(
    f: &Fixture,
    creds: Option<&Credentials>,
    uri: &str,
    idle: Duration,
    small_window: bool,
) {
    let mut tls = tls_from(f, LOCAL, creds, small_window).await.unwrap();
    let request = format!("GET {uri} HTTP/1.1\r\nHost: localhost\r\n\r\n");
    tls.write_all(request.as_bytes()).await.unwrap();
    let status = read_response(&mut tls).await;
    assert!(status.starts_with("HTTP/1.1 "), "{status}");
    let answered = Instant::now();
    let mut buf = [0; 1024];
    let closed = tokio::time::timeout(SOON, async {
        while let Ok(1..) = tls.read(&mut buf).await {}
    })
    .await;
    assert!(closed.is_ok(), "the idle connection was closed");
    assert!(answered.elapsed() >= idle / 2, "not long before the limit");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_kept_connection_idle_after_a_response_is_closed() {
    let idle = Duration::from_millis(300);
    let f = Fixture::start_with_limits(ConnectionLimits {
        idle_timeout: idle,
        ..ConnectionLimits::DEFAULT
    })
    .await;
    closed_when_idle_after(&f, None, path::HELLO, idle, false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_kept_connection_idle_after_a_blocked_download_is_closed() {
    let idle = Duration::from_millis(300);
    let f = Fixture::start_with_limits(ConnectionLimits {
        idle_timeout: idle,
        ..ConnectionLimits::DEFAULT
    })
    .await;
    let creds = f.pair().await;
    // Through a small window the server's last writes pend: the clock must
    // start once they go through.
    let (_, uri) = put_blob(&f, 1 << 20);
    closed_when_idle_after(&f, Some(&creds), &uri, idle, true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_kept_connection_idle_after_a_long_poll_is_closed() {
    let idle = Duration::from_millis(300);
    let f = Fixture::start_with_limits(ConnectionLimits {
        idle_timeout: idle,
        ..ConnectionLimits::DEFAULT
    })
    .await;
    let creds = f.pair().await;
    // Longer than the idle limit: the wait itself is not idleness.
    let uri = events_uri(f.log.head(), 1);
    closed_when_idle_after(&f, Some(&creds), &uri, idle, false).await;
}
