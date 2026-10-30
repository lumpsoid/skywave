//! The pairing client and `HttpsTransport` against a real `skywaved`
//! server, in-process on 127.0.0.1 over real TLS.

use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use chrono::Utc;
use skywave_core::{
    Action, BlobDownload, Core, FileIdentity, HttpsTransport, Identity, PairError, PairingStatus,
    Request, Screen, Transport, TransportConfig, TransportError, pair, pair_and_report,
};
use skywave_proto::{
    BlobId, Command, CommandId, CommandKind, CommandResult, DirsResult, EventKind, EventsQuery,
    HelloResponse, PROTOCOL_VERSION, Seq,
};
use skywaved::identity::ServerIdentity;
use skywaved::pairing::{self, IssuedCode};
use skywaved::server::Server;
use skywaved::{EventLog, Storage, devices};
use tempfile::TempDir;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// Generous bound for things that should happen at once.
const SOON: Duration = Duration::from_secs(10);

struct Fixture {
    tmp: TempDir,
    storage: Storage,
    identity: Arc<ServerIdentity>,
    /// `127.0.0.1:<port>`.
    addr: String,
    port: u16,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<skywaved::Result<()>>,
}

impl Fixture {
    async fn start() -> Fixture {
        Fixture::start_with(&["localhost", "127.0.0.1"]).await
    }

    /// A server whose certificate covers `names`.
    async fn start_with(names: &[&str]) -> Fixture {
        let names: Vec<String> = names.iter().map(|n| n.to_string()).collect();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("root/project")).unwrap();
        let storage = Storage::open(tmp.path().join("server")).unwrap();
        let identity = Arc::new(ServerIdentity::bootstrap(&storage.dir, &names).unwrap());
        let log = EventLog::open(&storage.db, vec![tmp.path().join("root")]).unwrap();
        let server = Server::bind(
            "127.0.0.1:0".parse().unwrap(),
            &storage.dir,
            identity.clone(),
            log,
        )
        .await
        .unwrap();
        let port = server.local_addr().port();
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(server.serve(async {
            let _ = stopped.await;
        }));
        Fixture {
            tmp,
            storage,
            identity,
            addr: format!("127.0.0.1:{port}"),
            port,
            stop: Some(stop),
            task,
        }
    }

    fn issue_code(&self) -> IssuedCode {
        pairing::issue_code(self.storage.db.conn(), Utc::now(), pairing::DEFAULT_TTL).unwrap()
    }

    /// The fingerprint as `skywaved pair` prints it.
    fn fingerprint(&self) -> String {
        self.identity.ca_fingerprint().to_string()
    }

    fn workdir(&self) -> String {
        self.tmp.path().join("root/project").display().to_string()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.task.abort();
    }
}

fn config() -> TransportConfig {
    TransportConfig {
        connect_timeout: SOON,
        request_timeout: SOON,
        ..TransportConfig::default()
    }
}

fn recv(requests: &Receiver<Request>) -> Request {
    requests.recv_timeout(SOON).expect("a request")
}

async fn read_all(mut download: BlobDownload) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(chunk) = download.body.next_chunk().await.unwrap() {
        out.extend_from_slice(&chunk);
    }
    out
}

/// The whole happy path: the head's Pair action reaches the server through
/// the seam, the identity is stored, and a transport built from it works
/// for hello, commands, events (incl. a long-poll) and blobs — until the
/// device is revoked.
#[tokio::test(flavor = "multi_thread")]
async fn pair_then_talk_to_the_server_until_revoked() {
    let fx = Fixture::start().await;
    let data = tempfile::tempdir().unwrap();
    let core = Core::open_detached(data.path()).unwrap();
    let seam = core.seam();
    let requests = seam.take_requests().unwrap();

    // As typed: lowercase code with its dash, the printed fingerprint.
    let code = fx.issue_code().code.to_string().to_lowercase();
    core.dispatch(Action::Pair {
        address: fx.addr.clone(),
        code,
        fingerprint: fx.fingerprint(),
    })
    .unwrap();
    let Request::Pair {
        address,
        code,
        fingerprint,
    } = recv(&requests)
    else {
        panic!("expected a pair request");
    };
    pair_and_report(
        &seam,
        &address,
        &code,
        &fingerprint,
        "test laptop",
        &config(),
    )
    .await;
    assert_eq!(recv(&requests), Request::Connect);
    assert!(
        matches!(core.view().screen, Screen::SessionList(_)),
        "{:?}",
        core.view().screen
    );

    let stored = FileIdentity::load(data.path())
        .unwrap()
        .expect("identity saved");
    assert_eq!(stored.ca_cert_pem(), fx.identity.ca_cert_pem());
    let server_device = devices::get(fx.storage.db.conn(), stored.device_id())
        .unwrap()
        .expect("server knows the device");
    assert_eq!(server_device.name, "test laptop");
    let address = seam.settings().unwrap().server_address.unwrap();
    assert_eq!(address, fx.addr);

    let transport: Arc<dyn Transport> =
        Arc::new(HttpsTransport::new(&address, &stored, config()).unwrap());
    let hello = transport.hello().await.unwrap();
    assert_eq!(hello.device_id, stored.device_id());
    assert_eq!(hello.protocol_version, PROTOCOL_VERSION);
    assert_eq!(hello.server_id, fx.identity.server_id());
    assert_eq!(hello.head, Seq::ZERO);

    // A command round-trip; resending the id returns the same result.
    let create = Command {
        id: CommandId::new(),
        kind: CommandKind::CreateSession {
            workdir: fx.workdir(),
        },
    };
    let accepted = transport.send_command(&create).await.unwrap();
    let CommandResult::Accepted { seq } = accepted else {
        panic!("expected accepted, got {accepted:?}");
    };
    assert_eq!(transport.send_command(&create).await.unwrap(), accepted);
    let batch = transport
        .events(EventsQuery {
            after: Seq::ZERO,
            wait: None,
            limit: Some(10),
        })
        .await
        .unwrap();
    assert_eq!(batch.last(), Some(seq));
    let session_id = batch.events[0].session_id;
    assert!(matches!(
        batch.events[0].kind,
        EventKind::SessionCreated { .. }
    ));

    // A long-poll and a command run side by side on separate connections.
    let poll = {
        let transport = transport.clone();
        tokio::spawn(async move {
            transport
                .events(EventsQuery {
                    after: seq,
                    wait: Some(30),
                    limit: None,
                })
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    let send = Command {
        id: CommandId::new(),
        kind: CommandKind::SendMessage {
            session_id,
            text: "hi".into(),
        },
    };
    let sent = tokio::time::timeout(SOON, transport.send_command(&send))
        .await
        .expect("command not blocked by the long-poll")
        .unwrap();
    assert_eq!(sent, CommandResult::Accepted { seq: seq.next() });
    let woke = tokio::time::timeout(SOON, poll)
        .await
        .expect("long-poll wakes")
        .unwrap()
        .unwrap();
    assert_eq!(woke.from, seq.next());
    assert!(matches!(
        &woke.events[0].kind,
        EventKind::UserMessage { text, .. } if text == "hi"
    ));

    // Blobs: whole, resumed from an offset, past the end, unknown.
    let blob = fx.storage.put_blob(&b"hello world"[..]).unwrap();
    let whole = transport.blob(blob.id, 0).await.unwrap();
    assert_eq!((whole.offset, whole.total), (0, 11));
    assert_eq!(read_all(whole).await, b"hello world");
    let tail = transport.blob(blob.id, 6).await.unwrap();
    assert_eq!((tail.offset, tail.total), (6, 11));
    assert_eq!(read_all(tail).await, b"world");
    assert_eq!(
        transport.blob(blob.id, 11).await.unwrap_err(),
        TransportError::RangeNotSatisfiable { size: Some(11) }
    );
    assert_eq!(
        transport.blob(BlobId::new(), 0).await.unwrap_err(),
        TransportError::NotFound
    );

    // Directory listings, with a name that needs escaping in the query.
    let root = std::fs::canonicalize(fx.tmp.path().join("root")).unwrap();
    let project = root.join("project");
    std::fs::create_dir(project.join("my dir %&#")).unwrap();
    let roots = vec![root.display().to_string()];
    assert_eq!(
        transport.list_dirs("").await.unwrap(),
        DirsResult::Roots {
            roots: roots.clone()
        }
    );
    assert_eq!(
        transport
            .list_dirs(&format!("{}/", project.display()))
            .await
            .unwrap(),
        DirsResult::Listed {
            dir: project.display().to_string(),
            names: vec!["my dir %&#".into()],
            truncated: false,
        }
    );
    assert_eq!(
        transport
            .list_dirs(&project.join("my dir %&#").display().to_string())
            .await
            .unwrap(),
        DirsResult::Listed {
            dir: project.join("my dir %&#").display().to_string(),
            names: Vec::new(),
            truncated: false,
        }
    );
    assert_eq!(
        transport.list_dirs("/").await.unwrap(),
        DirsResult::NotAllowed { roots }
    );

    // Revocation applies to the next request, even on a pooled connection.
    devices::revoke(fx.storage.db.conn(), stored.device_id(), Utc::now()).unwrap();
    let err = transport.hello().await.unwrap_err();
    assert_eq!(err, TransportError::Revoked);
    assert!(!err.is_retryable());
    assert_eq!(err.rejection(), Some(skywave_core::LinkRejection::Revoked));
    assert_eq!(
        transport.list_dirs("").await.unwrap_err(),
        TransportError::Revoked
    );
}

/// A transport that cannot list directories, like an older server without
/// `GET /v1/dirs` (404), answers `NotFound`: completion falls back to
/// recent workdirs.
#[tokio::test]
async fn list_dirs_defaults_to_not_found() {
    struct Old;
    impl Transport for Old {
        fn hello(&self) -> skywave_core::BoxFuture<'_, Result<HelloResponse, TransportError>> {
            unimplemented!()
        }
        fn send_command<'a>(
            &'a self,
            _: &'a Command,
        ) -> skywave_core::BoxFuture<'a, Result<CommandResult, TransportError>> {
            unimplemented!()
        }
        fn events(
            &self,
            _: EventsQuery,
        ) -> skywave_core::BoxFuture<'_, Result<skywave_proto::EventBatch, TransportError>>
        {
            unimplemented!()
        }
        fn blob(
            &self,
            _: BlobId,
            _: u64,
        ) -> skywave_core::BoxFuture<'_, Result<BlobDownload, TransportError>> {
            unimplemented!()
        }
    }
    assert_eq!(Old.list_dirs("/").await, Err(TransportError::NotFound));
}

/// A wrong fingerprint fails the TLS handshake, so the code never reaches
/// the server: it still works afterwards.
#[tokio::test(flavor = "multi_thread")]
async fn wrong_fingerprint_is_refused_before_the_code_is_sent() {
    let fx = Fixture::start().await;
    let code = fx.issue_code().code.to_string();
    let mut wrong = fx.identity.ca_fingerprint().to_hex();
    wrong.replace_range(0..2, if wrong.starts_with("00") { "11" } else { "00" });

    let err = pair(&fx.addr, &code, &wrong, "laptop", &config())
        .await
        .unwrap_err();
    assert_eq!(err, PairError::FingerprintMismatch);

    let paired = pair(&fx.addr, &code, &fx.fingerprint(), "laptop", &config())
        .await
        .unwrap();
    assert_eq!(paired.address, fx.addr);
}

/// The CA matches, but the address the user typed is not a name the
/// server certificate covers: refused, and the code stays unused.
#[tokio::test(flavor = "multi_thread")]
async fn server_certificate_must_cover_the_typed_address() {
    let fx = Fixture::start_with(&["localhost"]).await;
    let code = fx.issue_code().code.to_string();
    let err = pair(&fx.addr, &code, &fx.fingerprint(), "laptop", &config())
        .await
        .unwrap_err();
    assert!(matches!(err, PairError::BadServerCertificate(_)), "{err:?}");

    let by_name = format!("localhost:{}", fx.port);
    let paired = pair(&by_name, &code, &fx.fingerprint(), "laptop", &config())
        .await
        .unwrap();
    assert_eq!(paired.address, by_name);
}

/// Wrong codes are 401 → a readable reason, then the server's rate limit
/// (429 with Retry-After) is reported with the wait.
#[tokio::test(flavor = "multi_thread")]
async fn wrong_code_then_rate_limit() {
    let fx = Fixture::start().await;
    fx.issue_code();
    let addr = format!("localhost:{}", fx.port);
    let fp = fx.fingerprint().to_lowercase().replace(':', "");
    for _ in 0..5 {
        let err = pair(&addr, "ZZZZZ-ZZZZZ", &fp, "laptop", &config())
            .await
            .unwrap_err();
        assert_eq!(err, PairError::CodeRejected);
        assert!(err.to_string().contains("not accepted"), "{err}");
    }
    let err = pair(&addr, "ZZZZZ-ZZZZZ", &fp, "laptop", &config())
        .await
        .unwrap_err();
    let PairError::RateLimited {
        retry_after_secs: Some(secs),
    } = err
    else {
        panic!("expected rate limited, got {err:?}");
    };
    assert!((1..=60).contains(&secs), "{secs}");
}

/// Malformed input fails before any network access, and the pairing screen
/// shows why.
#[tokio::test(flavor = "multi_thread")]
async fn bad_input_is_reported_on_the_pairing_screen() {
    let data = tempfile::tempdir().unwrap();
    let core = Core::open_detached(data.path()).unwrap();
    let seam = core.seam();
    let requests = seam.take_requests().unwrap();
    // Port 9 (discard) is never contacted: the code is malformed.
    core.dispatch(Action::Pair {
        address: "127.0.0.1:9".into(),
        code: "ABC".into(),
        fingerprint: "00".repeat(32),
    })
    .unwrap();
    let Request::Pair {
        address,
        code,
        fingerprint,
    } = recv(&requests)
    else {
        panic!("expected a pair request");
    };
    pair_and_report(&seam, &address, &code, &fingerprint, "laptop", &config()).await;
    let Screen::Pairing(view) = core.view().screen else {
        panic!("expected the pairing screen");
    };
    assert_eq!(
        view.status,
        PairingStatus::Failed {
            address: "127.0.0.1:9".into(),
            reason: PairError::InvalidCode.to_string(),
        }
    );
    assert!(FileIdentity::load(data.path()).unwrap().is_none());
}
