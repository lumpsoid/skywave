//! The mTLS HTTPS API (T06), over real TLS on 127.0.0.1.

use std::net::SocketAddr;
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
    BlobId, Command, CommandId, CommandKind, CommandResult, ContextId, DeviceId, DeviceInfo,
    DirsResult, EventBatch, EventKind, HelloResponse, PROTOCOL_VERSION, PairRequest, PairResponse,
    RejectReason, Seq, SessionId, path,
};
use skywaved::blobs::BlobStore;
use skywaved::identity::ServerIdentity;
use skywaved::server::Server;
use skywaved::{EventLog, Storage, devices, pairing};
use tempfile::TempDir;
use tokio::net::TcpStream;
use tokio::sync::oneshot;
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
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(root.join("project")).unwrap();
        let storage = Storage::open(tmp.path().join("data")).unwrap();
        let identity =
            Arc::new(ServerIdentity::bootstrap(&storage.dir, &["localhost".into()]).unwrap());
        let log = EventLog::open(&storage.db, vec![root.clone()]).unwrap();
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
            },
        }
    }

    /// Appends an agent message from outside the API, as the supervisor will.
    fn append(&mut self, session: SessionId, text: &str) -> Seq {
        self.log
            .append(
                &mut self.storage.db,
                session,
                EventKind::AgentMessage { text: text.into() },
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
            devices: vec![DeviceInfo {
                id: creds.id,
                name: "laptop".into(),
            }],
        }
    );
    let device = devices::get(f.storage.db.conn(), creds.id)
        .unwrap()
        .unwrap();
    assert!(device.last_seen.is_some());
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
