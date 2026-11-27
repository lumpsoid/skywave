//! The pairing client and `HttpsTransport` against a real `skywaved`
//! server, in-process on 127.0.0.1 over real TLS.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use chrono::{TimeZone, Utc};
use skywave_core::{
    Action, BlobDownload, Core, ExpectedRun, HttpsTransport, Identity, PairError,
    PairedCredentials, Pairing, PairingStatus, Request, Screen, ServerAddress, StagedUpload,
    Transport, TransportConfig, TransportError, UploadFile, pair, pair_and_report,
};
use skywave_proto::{
    BlobId, ChangesResult, Command, CommandId, CommandKind, CommandResult, ContentHash, ContextId,
    ConversationsResult, DirEntry, DirsResult, EntryKind, EventKind, EventsQuery, FileBody,
    FileResult, HelloResponse, ListResult, LogResult, OutsideConversation, PROTOCOL_VERSION, Seq,
    SessionId, UploadResult, WorkdirQuery, WriteFileRequest, WriteFileResult,
};
use skywave_testkit::{LinkProfile, LinkProxy, Rng, seed};
use skywaved::agent::fake::FakeOutside;
use skywaved::agent::{self, Listed};
use skywaved::identity::ServerIdentity;
use skywaved::pairing::{self, IssuedCode};
use skywaved::server::{Server, ServerOptions};
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
    addr: ServerAddress,
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
        Fixture::start_with_options(names, ServerOptions::default()).await
    }

    /// A server whose agent lists its conversations from `outside`.
    async fn start_with_outside(outside: FakeOutside) -> Fixture {
        let options = ServerOptions {
            outside: Arc::new(outside),
            ..ServerOptions::default()
        };
        Fixture::start_with_options(&["localhost", "127.0.0.1"], options).await
    }

    async fn start_with_options(names: &[&str], options: ServerOptions) -> Fixture {
        let names: Vec<String> = names.iter().map(|n| n.to_string()).collect();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("root/project")).unwrap();
        let storage = Storage::open(tmp.path().join("server")).unwrap();
        let identity = Arc::new(ServerIdentity::bootstrap(&storage.dir, &names).unwrap());
        let log = EventLog::open(&storage.db, vec![tmp.path().join("root")]).unwrap();
        let server = Server::bind_with(
            "127.0.0.1:0".parse().unwrap(),
            &storage.dir,
            identity.clone(),
            log,
            options,
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
            addr: format!("127.0.0.1:{port}").parse().unwrap(),
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
        address: fx.addr.to_string(),
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

    let stored = Pairing::load(data.path()).unwrap().expect("pairing saved");
    assert_eq!(stored.ca_cert_pem(), fx.identity.ca_cert_pem());
    let server_device = devices::get(fx.storage.db.conn(), stored.device_id())
        .unwrap()
        .expect("server knows the device");
    assert_eq!(server_device.name, "test laptop");
    let address = stored.addresses.current().clone();
    assert_eq!(address, fx.addr);
    assert_eq!(seam.pairing().as_deref(), Some(&stored));

    let transport: Arc<dyn Transport> =
        Arc::new(HttpsTransport::new(&address, &stored, config(), ExpectedRun::default()).unwrap());
    let hello = transport.hello(None).await.unwrap();
    assert_eq!(hello.device_id, stored.device_id());
    assert_eq!(hello.protocol_version, PROTOCOL_VERSION);
    assert_eq!(hello.server_id, fx.identity.server_id());
    assert_eq!(hello.head, Seq::ZERO);

    // A command round-trip; resending the id returns the same result.
    let create = Command {
        id: CommandId::new(),
        kind: CommandKind::CreateSession {
            session_id: SessionId::new(),
            workdir: fx.workdir(),
            conversation: None,
            worktree: None,
            mode: None,
            safety_check: false,
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
            context: ContextId::first_of(session_id),
            text: "hi".into(),
            attachments: Vec::new(),
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
    let err = transport.hello(None).await.unwrap_err();
    assert_eq!(err, TransportError::Revoked);
    assert!(!err.is_retryable());
    assert_eq!(err.rejection(), Some(skywave_core::LinkRejection::Revoked));
    assert_eq!(
        transport.list_dirs("").await.unwrap_err(),
        TransportError::Revoked
    );
}

/// A transport implementing only the required calls, as one for an older
/// server would.
struct Old;

impl Transport for Old {
    fn hello(
        &self,
        _: Option<skywave_proto::RunId>,
    ) -> skywave_core::BoxFuture<'_, Result<HelloResponse, TransportError>> {
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
    ) -> skywave_core::BoxFuture<'_, Result<skywave_proto::EventBatch, TransportError>> {
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

/// A transport that cannot list directories, like an older server without
/// `GET /v1/dirs` (404), answers `NotFound`: completion falls back to
/// recent workdirs.
#[tokio::test]
async fn list_dirs_defaults_to_not_found() {
    assert_eq!(Old.list_dirs("/").await, Err(TransportError::NotFound));
}

/// A transport that cannot list conversations answers `NotFound`, as a
/// server without `GET /v1/conversations` would.
#[tokio::test]
async fn list_conversations_defaults_to_not_found() {
    assert_eq!(
        Old.list_conversations().await,
        Err(TransportError::NotFound)
    );
}

/// A transport that cannot stage attachments answers `NotFound`, as a
/// server without `/v1/attachments` would.
#[tokio::test]
async fn attachment_calls_default_to_not_found() {
    let sha = ContentHash::of(b"image");
    assert_eq!(
        Old.attachment_staged(sha).await,
        Err(TransportError::NotFound)
    );
    assert_eq!(
        Old.upload_attachment(
            sha,
            UploadFile {
                path: PathBuf::new(),
                len: 0,
            },
            0
        )
        .await,
        Err(TransportError::NotFound)
    );
}

/// `GET /v1/conversations` against a real server with a paired device: the
/// listing decodes field for field, and a server whose agent cannot list
/// answers `Unavailable`, not an error.
#[tokio::test(flavor = "multi_thread")]
async fn conversations_are_listed_by_a_real_server() {
    let conversation = OutsideConversation {
        id: "0d6b2c1e-1b1a-4c1e-9f0e-6a2b8c3d4e5f".into(),
        workdir: "/srv/my project".into(),
        title: Some("Fix the flaky test".into()),
        first_prompt: Some("why does ci fail".into()),
        last_active: Utc.with_ymd_and_hms(2026, 11, 2, 9, 30, 0).unwrap(),
        messages: 12,
        session: None,
    };
    let outside = FakeOutside::new();
    outside.listing(Ok(Listed {
        conversations: vec![agent::OutsideConversation {
            id: conversation.id.clone(),
            workdir: conversation.workdir.clone(),
            title: conversation.title.clone(),
            first_prompt: conversation.first_prompt.clone(),
            last_active: conversation.last_active,
            messages: conversation.messages,
            session: None,
        }],
        truncated: true,
    }));
    let fx = Fixture::start_with_outside(outside).await;
    let transport = paired_transport(&fx).await;
    assert_eq!(
        transport.list_conversations().await.unwrap(),
        ConversationsResult::Listed {
            conversations: vec![conversation],
            truncated: true,
        }
    );

    // The default server's agent reads no conversations.
    let fx = Fixture::start().await;
    let transport = paired_transport(&fx).await;
    assert_eq!(
        transport.list_conversations().await.unwrap(),
        ConversationsResult::Unavailable
    );
}

/// `HEAD` and `PUT /v1/attachments/{sha256}` against a real server: an
/// image is staged under its hash, a hash that does not match its bytes is
/// refused and stages nothing, and a non-image is staged like any file.
#[tokio::test(flavor = "multi_thread")]
async fn attachments_stage_on_a_real_server() {
    let fx = Fixture::start().await;
    let transport = paired_transport(&fx).await;
    let dir = tempfile::tempdir().unwrap();
    // The server checks only that a body hashes to its name, so a PNG header
    // and padding stage like any bytes.
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend_from_slice(&[0u8; 64]);
    let sha = ContentHash::of(&png);

    assert_eq!(
        transport.attachment_staged(sha).await.unwrap(),
        StagedUpload::Missing { held: 0 }
    );
    assert_eq!(
        transport
            .upload_attachment(sha, upload_file(dir.path(), "image.png", &png), 0)
            .await
            .unwrap(),
        UploadResult::Stored {
            size: png.len() as u64,
        }
    );
    assert_eq!(
        transport.attachment_staged(sha).await.unwrap(),
        StagedUpload::Staged
    );

    // The PNG's bytes, one byte changed, under a hash they do not match.
    let mut altered = png.clone();
    let last = altered.len() - 1;
    altered[last] ^= 0xff;
    let fresh = ContentHash::of(b"other");
    assert_eq!(
        transport
            .upload_attachment(fresh, upload_file(dir.path(), "altered.png", &altered), 0)
            .await
            .unwrap(),
        UploadResult::HashMismatch
    );
    assert_eq!(
        transport.attachment_staged(fresh).await.unwrap(),
        StagedUpload::Missing { held: 0 }
    );

    let text = b"just text";
    assert_eq!(
        transport
            .upload_attachment(
                ContentHash::of(text),
                upload_file(dir.path(), "text.txt", text),
                0
            )
            .await
            .unwrap(),
        UploadResult::Stored {
            size: text.len() as u64,
        }
    );

    // Many chunks long: the upload is streamed from the file, not read whole.
    let big: Vec<u8> = (0..1_000_000).map(|i| (i % 251) as u8).collect();
    assert_eq!(
        transport
            .upload_attachment(
                ContentHash::of(&big),
                upload_file(dir.path(), "big.bin", &big),
                0
            )
            .await
            .unwrap(),
        UploadResult::Stored { size: 1_000_000 }
    );
}

/// An interrupted upload resumes where the server says it stopped: `HEAD`
/// names the bytes the server holds, a `PUT` from that offset completes the
/// file, and an offset the server does not hold is refused with what it does.
#[tokio::test(flavor = "multi_thread")]
async fn an_interrupted_upload_resumes_on_a_real_server() {
    let fx = Fixture::start().await;
    let transport = paired_transport(&fx).await;
    let dir = tempfile::tempdir().unwrap();
    let staging = fx.storage.dir.attachments_dir();
    std::fs::create_dir_all(&staging).unwrap();

    let big: Vec<u8> = (0..1_000_000).map(|i| (i % 251) as u8).collect();
    let sha = ContentHash::of(&big);
    std::fs::write(
        staging.join(format!(".part-{}", sha.to_hex())),
        &big[..300_000],
    )
    .unwrap();
    assert_eq!(
        transport.attachment_staged(sha).await.unwrap(),
        StagedUpload::Missing { held: 300_000 }
    );
    assert_eq!(
        transport
            .upload_attachment(sha, upload_file(dir.path(), "big.bin", &big), 300_000)
            .await
            .unwrap(),
        UploadResult::Stored { size: 1_000_000 }
    );
    assert_eq!(
        transport.attachment_staged(sha).await.unwrap(),
        StagedUpload::Staged
    );

    // A partial holding 4 bytes: an offset of 6 is refused with what the
    // server holds, and an offset of 0 starts the file again.
    let small = b"a small file".to_vec();
    let small_sha = ContentHash::of(&small);
    std::fs::write(
        staging.join(format!(".part-{}", small_sha.to_hex())),
        &small[..4],
    )
    .unwrap();
    assert_eq!(
        transport
            .upload_attachment(small_sha, upload_file(dir.path(), "small.txt", &small), 6)
            .await
            .unwrap(),
        UploadResult::OffsetMismatch { have: 4 }
    );
    assert_eq!(
        transport
            .upload_attachment(small_sha, upload_file(dir.path(), "small.txt", &small), 0)
            .await
            .unwrap(),
        UploadResult::Stored {
            size: small.len() as u64,
        }
    );
}

/// An upload over the server's cap is refused, and one of exactly the cap is
/// stored: the `100 Continue` comes from a real server and the body follows
/// it. The server answers the refusal from the `content-length`, before it
/// asks for the body.
#[tokio::test(flavor = "multi_thread")]
async fn an_upload_over_the_cap_is_refused_on_a_real_server() {
    let options = ServerOptions {
        upload_max: 1 << 20,
        ..ServerOptions::default()
    };
    let fx = Fixture::start_with_options(&["localhost", "127.0.0.1"], options).await;
    let transport = paired_transport(&fx).await;
    let dir = tempfile::tempdir().unwrap();

    let over = vec![7u8; (1 << 20) + 1];
    assert_eq!(
        transport
            .upload_attachment(
                ContentHash::of(&over),
                upload_file(dir.path(), "big.bin", &over),
                0
            )
            .await
            .unwrap(),
        UploadResult::TooLarge
    );

    let exact = vec![7u8; 1 << 20];
    assert_eq!(
        transport
            .upload_attachment(
                ContentHash::of(&exact),
                upload_file(dir.path(), "exact.bin", &exact),
                0
            )
            .await
            .unwrap(),
        UploadResult::Stored { size: 1 << 20 }
    );
}

/// Writes `bytes` to `dir/name` and names that file as the upload of all of
/// its bytes.
fn upload_file(dir: &Path, name: &str, bytes: &[u8]) -> UploadFile {
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    UploadFile {
        path,
        len: bytes.len() as u64,
    }
}

/// A transport for a device freshly paired with `fx`.
async fn paired_transport(fx: &Fixture) -> HttpsTransport {
    let identity = paired_identity(fx).await;
    HttpsTransport::new(&fx.addr, &identity, config(), ExpectedRun::default()).unwrap()
}

/// The credentials of a device freshly paired with `fx`.
async fn paired_identity(fx: &Fixture) -> PairedCredentials {
    let code = fx.issue_code().code.to_string();
    let paired = pair(&fx.addr, &code, &fx.fingerprint(), "laptop", &config())
        .await
        .unwrap();
    assert_eq!(paired.address, fx.addr);
    paired.credentials
}

/// The request timeout of the slow-link tests: short, so a whole-exchange
/// deadline would be blown well within a test's run.
const SHORT: Duration = Duration::from_secs(1);

/// A TCP proxy that relays server-to-client bytes `PIECE` at a time with
/// `PAUSE` between pieces (40 KB/s: a 16 KB TLS record in 0.4 s, inside
/// [`SHORT`]), and stops relaying, holding the connection open, once it has
/// relayed `stall_after` bytes on a connection.
struct SlowLink {
    addr: ServerAddress,
    task: JoinHandle<()>,
}

impl SlowLink {
    const PIECE: usize = 4 * 1024;
    const PAUSE: Duration = Duration::from_millis(100);

    async fn start(upstream: u16, stall_after: Option<u64>) -> SlowLink {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: ServerAddress = format!("127.0.0.1:{}", listener.local_addr().unwrap().port())
            .parse()
            .unwrap();
        let task = tokio::spawn(async move {
            // Dropped with the task when the proxy is: every pipe ends too.
            let mut pipes = tokio::task::JoinSet::new();
            loop {
                let Ok((inbound, _)) = listener.accept().await else {
                    return;
                };
                pipes.spawn(async move {
                    let Ok(outbound) =
                        tokio::net::TcpStream::connect(("127.0.0.1", upstream)).await
                    else {
                        return;
                    };
                    let (mut client_rx, mut client_tx) = inbound.into_split();
                    let (mut server_rx, mut server_tx) = outbound.into_split();
                    let up = tokio::io::copy(&mut client_rx, &mut server_tx);
                    let down = async {
                        let mut relayed = 0u64;
                        let mut buf = vec![0u8; SlowLink::PIECE];
                        loop {
                            if stall_after.is_some_and(|limit| relayed >= limit) {
                                std::future::pending::<()>().await;
                            }
                            let n = match server_rx.read(&mut buf).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => n,
                            };
                            if client_tx.write_all(&buf[..n]).await.is_err() {
                                return;
                            }
                            relayed += n as u64;
                            tokio::time::sleep(SlowLink::PAUSE).await;
                        }
                    };
                    tokio::select! {
                        _ = up => {}
                        () = down => {}
                    }
                });
            }
        });
        SlowLink { addr, task }
    }
}

impl Drop for SlowLink {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// How long a [`LateLink`] holds a connection: longer than [`SHORT`], well
/// under [`SOON`].
const LATE: Duration = Duration::from_secs(3);

/// A relay that accepts at once but forwards nothing, either way, for
/// `delay`: a connect that takes long to complete, like a first rendezvous
/// with a Tor onion service.
struct LateLink {
    port: u16,
}

impl LateLink {
    async fn start(upstream: u16, delay: Duration) -> LateLink {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut inbound, _)) = listener.accept().await {
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    let Ok(mut outbound) =
                        tokio::net::TcpStream::connect(("127.0.0.1", upstream)).await
                    else {
                        return;
                    };
                    let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                });
            }
        });
        LateLink { port }
    }
}

/// A session with `messages` user messages of `len` random letters and
/// digits each: an events body of about `messages * len` bytes that
/// compresses (D45) only to some three quarters of that.
async fn big_log(transport: &HttpsTransport, fx: &Fixture, messages: usize, len: usize) -> Seq {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = Rng::new(messages as u64);
    let texts = (0..messages)
        .map(|_| {
            (0..len)
                .map(|_| char::from(ALPHABET[rng.below(ALPHABET.len() as u64) as usize]))
                .collect()
        })
        .collect();
    log_of(transport, fx, texts).await
}

/// A session with one user message of each of `texts`; the last seq.
async fn log_of(transport: &HttpsTransport, fx: &Fixture, texts: Vec<String>) -> Seq {
    let create = Command {
        id: CommandId::new(),
        kind: CommandKind::CreateSession {
            session_id: SessionId::new(),
            workdir: fx.workdir(),
            conversation: None,
            worktree: None,
            mode: None,
            safety_check: false,
        },
    };
    let CommandResult::Accepted { mut seq } = transport.send_command(&create).await.unwrap() else {
        panic!("session not created");
    };
    let batch = transport
        .events(EventsQuery {
            after: Seq::ZERO,
            wait: None,
            limit: None,
        })
        .await
        .unwrap();
    let session_id = batch.events[0].session_id;
    for text in texts {
        let send = Command {
            id: CommandId::new(),
            kind: CommandKind::SendMessage {
                session_id,
                context: ContextId::first_of(session_id),
                text,
                attachments: Vec::new(),
            },
        };
        let CommandResult::Accepted { seq: sent } = transport.send_command(&send).await.unwrap()
        else {
            panic!("message not accepted");
        };
        seq = sent;
    }
    seq
}

/// D45: an events batch comes compressed, a fraction of its size on the
/// wire, and decodes to the events the log holds.
#[tokio::test(flavor = "multi_thread")]
async fn an_events_batch_comes_compressed() {
    let fx = Fixture::start().await;
    let identity = paired_identity(&fx).await;
    let direct =
        HttpsTransport::new(&fx.addr, &identity, config(), ExpectedRun::default()).unwrap();
    let texts: Vec<String> = (0..5)
        .map(|i| format!("line {i} of a long paste\n").repeat(2000))
        .collect();
    let last = log_of(&direct, &fx, texts.clone()).await;

    let upstream = std::net::SocketAddr::from(([127, 0, 0, 1], fx.port));
    let link = LinkProxy::start(upstream, LinkProfile::clean(), seed()).await;
    let transport = HttpsTransport::new(
        &link.address().parse().unwrap(),
        &identity,
        config(),
        ExpectedRun::default(),
    )
    .unwrap();
    let batch = transport
        .events(EventsQuery {
            after: Seq::ZERO,
            wait: None,
            limit: None,
        })
        .await
        .unwrap();
    assert_eq!(batch.last(), Some(last));
    let sent: Vec<&str> = batch
        .events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::UserMessage { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(sent, texts);
    let raw: usize = texts.iter().map(String::len).sum();
    let down = link.stats().bytes_down;
    assert!(
        down < raw as u64 / 10,
        "{down} bytes on the wire for {raw} of messages"
    );
}

/// An events batch whose body takes longer to arrive than the request
/// timeout, but whose every chunk arrives within it, is read whole: a slow
/// but live link catches up instead of re-pulling the same batch forever.
#[tokio::test(flavor = "multi_thread")]
async fn a_slow_events_body_arrives_whole() {
    let fx = Fixture::start().await;
    let identity = paired_identity(&fx).await;
    let direct =
        HttpsTransport::new(&fx.addr, &identity, config(), ExpectedRun::default()).unwrap();
    let last = big_log(&direct, &fx, 5, 32 * 1024).await;

    let link = SlowLink::start(fx.port, None).await;
    let slow = TransportConfig {
        connect_timeout: SOON,
        request_timeout: SHORT,
        ..TransportConfig::default()
    };
    let transport =
        HttpsTransport::new(&link.addr, &identity, slow, ExpectedRun::default()).unwrap();
    let started = tokio::time::Instant::now();
    let batch = transport
        .events(EventsQuery {
            after: Seq::ZERO,
            wait: None,
            limit: None,
        })
        .await
        .unwrap();
    // Positive control: the exchange took longer than the old deadline over
    // the whole of it (the request timeout, with no wait).
    assert!(started.elapsed() > SHORT * 2, "{:?}", started.elapsed());
    assert_eq!(batch.last(), Some(last));
    assert_eq!(batch.events.len(), 6);
}

/// An events body that stops arriving times out one request timeout after
/// its last chunk, not after the long-poll's `wait`: a dead link is still
/// detected.
#[tokio::test(flavor = "multi_thread")]
async fn a_stalled_events_body_times_out() {
    let fx = Fixture::start().await;
    let identity = paired_identity(&fx).await;
    let direct =
        HttpsTransport::new(&fx.addr, &identity, config(), ExpectedRun::default()).unwrap();
    big_log(&direct, &fx, 5, 32 * 1024).await;

    // Past the handshake and the response head, well short of the body.
    let link = SlowLink::start(fx.port, Some(40 * 1024)).await;
    let slow = TransportConfig {
        connect_timeout: SOON,
        request_timeout: SHORT,
        ..TransportConfig::default()
    };
    let transport =
        HttpsTransport::new(&link.addr, &identity, slow, ExpectedRun::default()).unwrap();
    let started = tokio::time::Instant::now();
    let wait = 30;
    let result = transport
        .events(EventsQuery {
            after: Seq::ZERO,
            wait: Some(wait),
            limit: None,
        })
        .await;
    assert_eq!(result.unwrap_err(), TransportError::Timeout);
    assert!(
        started.elapsed() < Duration::from_secs(u64::from(wait)),
        "{:?}",
        started.elapsed()
    );
}

/// A connect slower than the request timeout but within the connect timeout
/// still gets the whole request timeout once it is open, for `hello` and for
/// an events long-poll.
#[tokio::test(flavor = "multi_thread")]
async fn a_slow_connect_leaves_the_request_its_whole_deadline() {
    let fx = Fixture::start().await;
    let identity = paired_identity(&fx).await;
    let late = LateLink::start(fx.port, LATE).await;
    let addr: ServerAddress = format!("127.0.0.1:{}", late.port).parse().unwrap();

    let slow = TransportConfig {
        connect_timeout: SOON,
        request_timeout: SHORT,
        ..TransportConfig::default()
    };
    let transport =
        HttpsTransport::new(&addr, &identity, slow.clone(), ExpectedRun::default()).unwrap();
    transport.hello(None).await.unwrap();

    // A new transport, so the events long-poll connects afresh.
    let transport = HttpsTransport::new(&addr, &identity, slow, ExpectedRun::default()).unwrap();
    transport
        .events(EventsQuery {
            after: Seq::ZERO,
            wait: Some(0),
            limit: None,
        })
        .await
        .unwrap();
}

/// The positive control: the same slow connect fails once it outlasts the
/// connect timeout.
#[tokio::test(flavor = "multi_thread")]
async fn a_connect_slower_than_the_connect_timeout_times_out() {
    let fx = Fixture::start().await;
    let identity = paired_identity(&fx).await;
    let late = LateLink::start(fx.port, LATE).await;
    let addr: ServerAddress = format!("127.0.0.1:{}", late.port).parse().unwrap();

    let impatient = TransportConfig {
        connect_timeout: SHORT,
        request_timeout: SOON,
        ..TransportConfig::default()
    };
    let transport =
        HttpsTransport::new(&addr, &identity, impatient, ExpectedRun::default()).unwrap();
    let started = std::time::Instant::now();
    let err = transport.hello(None).await.unwrap_err();
    assert_eq!(err, TransportError::Timeout);
    assert!(started.elapsed() < LATE, "{:?}", started.elapsed());
}

/// A transport that cannot read workdirs answers `NotFound`, as a server
/// without the endpoints would.
#[tokio::test]
async fn workdir_reads_default_to_not_found() {
    let query = WorkdirQuery {
        session: SessionId::new(),
        path: "src".into(),
    };
    assert_eq!(
        Old.workdir_changes(&query).await,
        Err(TransportError::NotFound)
    );
    assert_eq!(Old.workdir_log(&query).await, Err(TransportError::NotFound));
    assert_eq!(
        Old.workdir_list(&query).await,
        Err(TransportError::NotFound)
    );
    assert_eq!(
        Old.workdir_file(&query).await,
        Err(TransportError::NotFound)
    );
    assert_eq!(
        Old.workdir_download(&query).await,
        Err(TransportError::NotFound)
    );
    let write = WriteFileRequest {
        session: query.session,
        path: "notes.txt".into(),
        content: ContentHash::of(b"hello\n"),
        replace: None,
    };
    assert_eq!(
        Old.write_workdir_file(&write).await,
        Err(TransportError::NotFound)
    );
}

/// A transport that cannot read the worktree list answers `NotFound`, as a
/// server without `GET /v1/worktrees` would.
#[tokio::test]
async fn worktrees_defaults_to_not_found() {
    assert_eq!(Old.worktrees().await, Err(TransportError::NotFound));
}

/// The three workdir reads against a real server with a paired device,
/// paths that need escaping in the query included.
#[tokio::test(flavor = "multi_thread")]
async fn workdir_reads_reach_a_real_server() {
    let fx = Fixture::start().await;
    let code = fx.issue_code().code.to_string();
    let paired = pair(&fx.addr, &code, &fx.fingerprint(), "laptop", &config())
        .await
        .unwrap();
    let identity = paired.credentials;
    let transport =
        HttpsTransport::new(&paired.address, &identity, config(), ExpectedRun::default()).unwrap();

    let create = Command {
        id: CommandId::new(),
        kind: CommandKind::CreateSession {
            session_id: SessionId::new(),
            workdir: fx.workdir(),
            conversation: None,
            worktree: None,
            mode: None,
            safety_check: false,
        },
    };
    let accepted = transport.send_command(&create).await.unwrap();
    assert!(matches!(accepted, CommandResult::Accepted { .. }));
    let batch = transport
        .events(EventsQuery {
            after: Seq::ZERO,
            wait: None,
            limit: Some(1),
        })
        .await
        .unwrap();
    let session = batch.events[0].session_id;
    let dir = std::path::Path::new(&fx.workdir()).join("my dir %&#");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("a #1.txt"), "hello").unwrap();
    let query = |path: &str| WorkdirQuery {
        session,
        path: path.into(),
    };

    // `root/project` is in no git work tree.
    assert_eq!(
        transport.workdir_changes(&query("")).await.unwrap(),
        ChangesResult::NotGit
    );
    assert_eq!(
        transport.workdir_log(&query("")).await.unwrap(),
        LogResult::NotGit
    );
    let ListResult::Listed {
        path,
        entries,
        truncated,
        ..
    } = transport.workdir_list(&query("my dir %&#")).await.unwrap()
    else {
        panic!("expected a listing");
    };
    assert_eq!(path, "my dir %&#");
    assert!(!truncated);
    assert_eq!(
        entries,
        [DirEntry {
            name: "a #1.txt".into(),
            kind: EntryKind::File,
            size: Some(5),
        }]
    );
    let FileResult::File {
        path,
        size,
        content: FileBody::Blob(blob),
        ..
    } = transport
        .workdir_file(&query("my dir %&#/a #1.txt"))
        .await
        .unwrap()
    else {
        panic!("expected a file");
    };
    assert_eq!((path.as_str(), size), ("my dir %&#/a #1.txt", 5));
    let content = transport.blob(blob.id, 0).await.unwrap();
    assert_eq!(read_all(content).await, b"hello");

    // A session the server does not know is an answer, not an error.
    let unknown = WorkdirQuery {
        session: SessionId::new(),
        path: String::new(),
    };
    assert_eq!(
        transport.workdir_changes(&unknown).await.unwrap(),
        ChangesResult::UnknownSession
    );
    assert_eq!(
        transport.workdir_log(&unknown).await.unwrap(),
        LogResult::UnknownSession
    );
    assert_eq!(
        transport.workdir_list(&unknown).await.unwrap(),
        ListResult::UnknownSession
    );
    assert_eq!(
        transport.workdir_file(&unknown).await.unwrap(),
        FileResult::UnknownSession
    );
}

/// `GET /v1/worktrees` against a real server with a paired device and no
/// worktree session: the list names no repository.
#[tokio::test(flavor = "multi_thread")]
async fn worktrees_reach_a_real_server() {
    let fx = Fixture::start().await;
    let code = fx.issue_code().code.to_string();
    let paired = pair(&fx.addr, &code, &fx.fingerprint(), "laptop", &config())
        .await
        .unwrap();
    let identity = paired.credentials;
    let transport =
        HttpsTransport::new(&paired.address, &identity, config(), ExpectedRun::default()).unwrap();

    let list = transport.worktrees().await.unwrap();
    assert!(list.repos.is_empty());
}

/// The download read against a real server: a binary file comes back as a
/// blob of its own bytes, where the file read refuses it as binary.
#[tokio::test(flavor = "multi_thread")]
async fn workdir_download_reaches_a_real_server() {
    let fx = Fixture::start().await;
    let code = fx.issue_code().code.to_string();
    let paired = pair(&fx.addr, &code, &fx.fingerprint(), "laptop", &config())
        .await
        .unwrap();
    let identity = paired.credentials;
    let transport =
        HttpsTransport::new(&paired.address, &identity, config(), ExpectedRun::default()).unwrap();

    let create = Command {
        id: CommandId::new(),
        kind: CommandKind::CreateSession {
            session_id: SessionId::new(),
            workdir: fx.workdir(),
            conversation: None,
            worktree: None,
            mode: None,
            safety_check: false,
        },
    };
    let accepted = transport.send_command(&create).await.unwrap();
    assert!(matches!(accepted, CommandResult::Accepted { .. }));
    let batch = transport
        .events(EventsQuery {
            after: Seq::ZERO,
            wait: None,
            limit: Some(1),
        })
        .await
        .unwrap();
    let session = batch.events[0].session_id;
    let bytes = b"\x89PNG\0\x01\x02";
    std::fs::write(std::path::Path::new(&fx.workdir()).join("logo.bin"), bytes).unwrap();
    let query = WorkdirQuery {
        session,
        path: "logo.bin".into(),
    };

    let FileResult::File {
        size,
        content: FileBody::Blob(blob),
        ..
    } = transport.workdir_download(&query).await.unwrap()
    else {
        panic!("expected the file as a blob");
    };
    assert_eq!(size, 7);
    let content = transport.blob(blob.id, 0).await.unwrap();
    assert_eq!(read_all(content).await, bytes);

    // The file read of the same path is the control: binary, no blob.
    assert!(matches!(
        transport.workdir_file(&query).await.unwrap(),
        FileResult::File {
            content: FileBody::Binary,
            ..
        }
    ));
}

/// The write against a real server: a staged upload is written into the
/// workdir; the same content again is `Same`; other content at the path is
/// `Exists` until its hash is named as `replace`.
#[tokio::test(flavor = "multi_thread")]
async fn workdir_write_reaches_a_real_server() {
    let fx = Fixture::start().await;
    let code = fx.issue_code().code.to_string();
    let paired = pair(&fx.addr, &code, &fx.fingerprint(), "laptop", &config())
        .await
        .unwrap();
    let identity = paired.credentials;
    let transport =
        HttpsTransport::new(&paired.address, &identity, config(), ExpectedRun::default()).unwrap();

    let create = Command {
        id: CommandId::new(),
        kind: CommandKind::CreateSession {
            session_id: SessionId::new(),
            workdir: fx.workdir(),
            conversation: None,
            worktree: None,
            mode: None,
            safety_check: false,
        },
    };
    let accepted = transport.send_command(&create).await.unwrap();
    assert!(matches!(accepted, CommandResult::Accepted { .. }));
    let batch = transport
        .events(EventsQuery {
            after: Seq::ZERO,
            wait: None,
            limit: Some(1),
        })
        .await
        .unwrap();
    let session = batch.events[0].session_id;
    let file = std::path::Path::new(&fx.workdir()).join("notes.txt");

    let uploads = tempfile::tempdir().unwrap();
    let hello = b"hello\n";
    let staged = ContentHash::of(hello);
    assert!(matches!(
        transport
            .upload_attachment(staged, upload_file(uploads.path(), "hello", hello), 0)
            .await
            .unwrap(),
        UploadResult::Stored { .. }
    ));
    let write = WriteFileRequest {
        session,
        path: "notes.txt".into(),
        content: staged,
        replace: None,
    };

    let WriteFileResult::Written { path, size: 6, .. } =
        transport.write_workdir_file(&write).await.unwrap()
    else {
        panic!("expected the file written");
    };
    assert_eq!(path, "notes.txt");
    assert_eq!(std::fs::read(&file).unwrap(), hello);

    assert!(matches!(
        transport.write_workdir_file(&write).await.unwrap(),
        WriteFileResult::Same { size: 6, .. }
    ));

    let other = b"other\n";
    let other_hash = ContentHash::of(other);
    assert!(matches!(
        transport
            .upload_attachment(other_hash, upload_file(uploads.path(), "other", other), 0)
            .await
            .unwrap(),
        UploadResult::Stored { .. }
    ));
    let other_write = WriteFileRequest {
        session,
        path: "notes.txt".into(),
        content: other_hash,
        replace: None,
    };
    let WriteFileResult::Exists { sha256, size: 6 } =
        transport.write_workdir_file(&other_write).await.unwrap()
    else {
        panic!("expected the other file to be refused");
    };
    assert_eq!(sha256, staged);
    assert_eq!(std::fs::read(&file).unwrap(), hello);

    let replacing = WriteFileRequest {
        replace: Some(staged),
        ..other_write
    };
    assert!(matches!(
        transport.write_workdir_file(&replacing).await.unwrap(),
        WriteFileResult::Written { size: 6, .. }
    ));
    assert_eq!(std::fs::read(&file).unwrap(), other);
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

    let by_name: ServerAddress = format!("localhost:{}", fx.port).parse().unwrap();
    let paired = pair(&by_name, &code, &fx.fingerprint(), "laptop", &config())
        .await
        .unwrap();
    assert_eq!(paired.address, by_name);
}

/// A paired device tells the server's identity faults apart from a
/// failure that may pass: an address the server certificate does not
/// cover is `ServerNameMismatch`, a certificate from another CA is
/// `UntrustedServer`, each a rejection; the name it was paired with
/// still works.
#[tokio::test(flavor = "multi_thread")]
async fn a_paired_device_tells_identity_faults_from_transient_ones() {
    let fx = Fixture::start_with(&["localhost"]).await;
    let by_name: ServerAddress = format!("localhost:{}", fx.port).parse().unwrap();
    let code = fx.issue_code().code.to_string();
    let paired = pair(&by_name, &code, &fx.fingerprint(), "laptop", &config())
        .await
        .unwrap();
    let identity = paired.credentials;

    let named = HttpsTransport::new(&by_name, &identity, config(), ExpectedRun::default()).unwrap();
    named.hello(None).await.unwrap();

    let by_ip = HttpsTransport::new(&fx.addr, &identity, config(), ExpectedRun::default()).unwrap();
    let err = by_ip.hello(None).await.unwrap_err();
    assert!(
        matches!(err, TransportError::ServerNameMismatch(_)),
        "{err:?}"
    );
    assert_eq!(
        err.rejection(),
        Some(skywave_core::LinkRejection::NameMismatch)
    );

    let other = Fixture::start_with(&["localhost"]).await;
    let elsewhere: ServerAddress = format!("localhost:{}", other.port).parse().unwrap();
    let err = HttpsTransport::new(&elsewhere, &identity, config(), ExpectedRun::default())
        .unwrap()
        .hello(None)
        .await
        .unwrap_err();
    assert!(matches!(err, TransportError::UntrustedServer(_)), "{err:?}");
    assert_eq!(
        err.rejection(),
        Some(skywave_core::LinkRejection::ServerChanged)
    );
}

/// Wrong codes are 401 → a readable reason, then the server's rate limit
/// (429 with Retry-After) is reported with the wait.
#[tokio::test(flavor = "multi_thread")]
async fn wrong_code_then_rate_limit() {
    let fx = Fixture::start().await;
    fx.issue_code();
    let addr: ServerAddress = format!("localhost:{}", fx.port).parse().unwrap();
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
    assert!(Pairing::load(data.path()).unwrap().is_none());
}

/// The reply to an adopting `CreateSession` waits for the server's import;
/// a device treats a reply later than its request timeout as a dead link
/// (drop, reconnect, resend), so the server's import deadline leaves room
/// under it for the transaction and the network.
#[test]
fn the_import_deadline_leaves_the_reply_inside_the_request_timeout() {
    let margin = Duration::from_secs(5);
    assert!(
        skywaved::agent::import::IMPORT_DEADLINE + margin
            <= TransportConfig::default().request_timeout
    );
}
