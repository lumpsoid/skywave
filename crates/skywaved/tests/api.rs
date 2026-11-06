//! The mTLS HTTPS API (T06), over real TLS on 127.0.0.1.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use chrono::Utc;
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
    BlobId, BlobRef, BlobStorage, BlobUse, ChangesResult, Command, CommandId, CommandKind,
    CommandResult, ContextId, ConversationsResult, DeviceId, DeviceInfo, DiffBody, DirEntry,
    DirsResult, EntryKind, EventBatch, EventKind, FileBody, FileChange, FileResult, HelloResponse,
    ImportOutcome, ListResult, PROTOCOL_VERSION, PairRequest, PairResponse, RUN_HEADER,
    RejectReason, RunId, RunsSince, Seq, SessionId, SessionOrigin, ShellEnd, ShellRunStorage,
    StorageReport, ToolResult, path,
};
use skywaved::blobs::BlobStore;
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
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(root.join("project")).unwrap();
        let storage = Storage::open(tmp.path().join("data")).unwrap();
        let identity =
            Arc::new(ServerIdentity::bootstrap(&storage.dir, &["localhost".into()]).unwrap());
        let log = EventLog::open(&storage.db, vec![root.clone()])
            .unwrap()
            .with_shell(shell)
            .with_bypass(bypass);
        let options = ServerOptions {
            git,
            limits,
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
        }
    );
    let device = devices::get(f.storage.db.conn(), creds.id)
        .unwrap()
        .unwrap();
    assert!(device.last_seen.is_some());
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
        path::WORKDIR_LIST.to_owned(),
        path::WORKDIR_FILE.to_owned(),
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
            text: "news".into()
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

/// Reads one whole response (`Content-Length` framed) from `tls`; its
/// status line.
async fn read_response(tls: &mut tokio_rustls::client::TlsStream<TcpStream>) -> String {
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
    let head = String::from_utf8_lossy(&received[..head_end]);
    head.lines().next().unwrap().to_owned()
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
