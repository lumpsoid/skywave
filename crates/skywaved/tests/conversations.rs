//! Outside conversations over the API (D37): an adopting `CreateSession`
//! imports the conversation's history from the server's source, here the
//! scripted [`FakeOutside`], over real TLS on 127.0.0.1.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use chrono::Utc;
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
    Command, CommandId, CommandKind, CommandResult, ContextId, ConversationsResult, Event,
    EventBatch, EventKind, ImportOutcome, ImportedItem, OutsideConversation, PairRequest,
    PairResponse, RejectReason, Seq, SessionOrigin, limits, path,
};
use skywaved::agent::fake::{FakeAgent, FakeOutside, Gate};
use skywaved::agent::import::{DEADLINE_EVERY, TIMED_OUT};
use skywaved::agent::{
    self, AdapterEvent, AgentCallId, HistoryItem, HistoryRead, Listed, ResultDisplay, SourceError,
};
use skywaved::api::{CONVERSATIONS_TIMEOUT, FS_PERMITS};
use skywaved::identity::ServerIdentity;
use skywaved::server::{Server, ServerOptions};
use skywaved::{EventLog, Storage, pairing};
use tempfile::TempDir;
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_rustls::TlsConnector;

/// Generous bound for things that should happen at once.
const SOON: Duration = Duration::from_secs(10);
/// The device's command reply timeout (`skywave-core`'s `TransportConfig`).
const DEVICE_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const CONVERSATION: &str = "9b2c1e4a-5f3d-4c8e-a1b2-c3d4e5f6a7b8";

struct Fixture {
    _tmp: TempDir,
    storage: Storage,
    identity: Arc<ServerIdentity>,
    log: EventLog,
    /// The canonical workdir the fake keeps conversations for.
    workdir: String,
    outside: FakeOutside,
    /// Opens once a request joins another's preparation.
    joined: Gate,
    addr: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<skywaved::Result<()>>>,
}

impl Fixture {
    async fn start(outside: FakeOutside) -> Fixture {
        Fixture::start_with_deadline(outside, skywaved::agent::import::IMPORT_DEADLINE).await
    }

    async fn start_with_deadline(outside: FakeOutside, import_deadline: Duration) -> Fixture {
        Fixture::start_with(outside, import_deadline, CONVERSATIONS_TIMEOUT).await
    }

    async fn start_with_listing_timeout(outside: FakeOutside, timeout: Duration) -> Fixture {
        Fixture::start_with(outside, skywaved::agent::import::IMPORT_DEADLINE, timeout).await
    }

    async fn start_with(
        outside: FakeOutside,
        import_deadline: Duration,
        conversations_timeout: Duration,
    ) -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(root.join("project")).unwrap();
        let workdir = std::fs::canonicalize(root.join("project"))
            .unwrap()
            .display()
            .to_string();
        let storage = Storage::open(tmp.path().join("data")).unwrap();
        let identity =
            Arc::new(ServerIdentity::bootstrap(&storage.dir, &["localhost".into()]).unwrap());
        let log = EventLog::open(&storage.db, vec![root.clone()]).unwrap();
        let joined = Gate::new();
        let options = ServerOptions {
            outside: Arc::new(outside.clone()),
            import_deadline,
            conversations_timeout,
            joined_hook: Some(Arc::new({
                let joined = joined.clone();
                move || joined.open()
            })),
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
            workdir,
            outside,
            joined,
            addr,
            stop: Some(stop),
            task: Some(task),
        }
    }

    /// A new device, paired through `POST /v1/pair`, and a connection of it.
    async fn device(&self) -> Client {
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
            device_name: "laptop".into(),
        };
        let mut anonymous = Client::connect(self.addr, self.identity.ca_cert_der(), None).await;
        let reply = anonymous.send(Method::POST, path::PAIR, json(&req)).await;
        assert_eq!(reply.0, StatusCode::OK);
        let paired: PairResponse = serde_json::from_slice(&reply.1).unwrap();
        let creds = (paired.device_cert_pem, key.serialize_pem());
        Client::connect(self.addr, self.identity.ca_cert_der(), Some(&creds)).await
    }

    fn adopt(&self, conversation: &str) -> Command {
        self.create(&self.workdir, Some(conversation))
    }

    fn create(&self, workdir: &str, conversation: Option<&str>) -> Command {
        Command {
            id: CommandId::new(),
            kind: CommandKind::CreateSession {
                workdir: workdir.to_owned(),
                conversation: conversation.map(str::to_owned),
                worktree: None,
            },
        }
    }

    fn conversation(&self, read: HistoryRead) {
        self.outside
            .conversation(Path::new(&self.workdir), CONVERSATION, read);
    }

    /// Blob files in the data dir, whether or not a row records them.
    fn blob_files(&self) -> usize {
        std::fs::read_dir(self.storage.dir.blobs_dir())
            .unwrap()
            .filter(|entry| {
                !entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with('.')
            })
            .count()
    }

    fn blob_rows(&self) -> usize {
        self.storage
            .db
            .conn()
            .query_row("SELECT COUNT(*) FROM blobs", [], |r| r.get::<_, i64>(0))
            .unwrap() as usize
    }
}

/// Every test ends by checking that every row names a context of its own
/// session, and the server stops.
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
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

    async fn command(&mut self, command: &Command) -> CommandResult {
        let (status, body) = self.send(Method::POST, path::COMMANDS, json(command)).await;
        assert_eq!(status, StatusCode::OK);
        serde_json::from_slice(&body).unwrap()
    }

    async fn events(&mut self) -> Vec<Event> {
        let uri = format!("{}?after=0&limit=1000", path::EVENTS);
        let (status, body) = self.send(Method::GET, &uri, Bytes::new()).await;
        assert_eq!(status, StatusCode::OK);
        serde_json::from_slice::<EventBatch>(&body).unwrap().events
    }
}

/// Opens its gate when dropped, so a failing test never leaves a blocking
/// thread waiting on it (the runtime would wait for it forever).
struct OpenOnDrop(Gate);

impl Drop for OpenOnDrop {
    fn drop(&mut self) {
        self.0.open();
    }
}

fn json(value: &impl serde::Serialize) -> Bytes {
    serde_json::to_vec(value).unwrap().into()
}

/// A history with a user prompt, a call whose long output goes to a blob,
/// its result, and the agent's answer.
fn history() -> Vec<HistoryItem> {
    vec![
        HistoryItem::User {
            at: Some("2026-11-01T10:00:00Z".parse().unwrap()),
            text: "list the files".into(),
        },
        HistoryItem::Agent {
            at: Some("2026-11-01T10:00:01Z".parse().unwrap()),
            event: AdapterEvent::ToolCall {
                call: Some(AgentCallId::from("toolu_1")),
                parent: None,
                name: "Bash".into(),
                input: serde_json::json!({ "command": "ls" }),
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
                    output: "file\n".repeat(1000),
                },
            },
        },
        HistoryItem::Agent {
            at: None,
            event: AdapterEvent::Text {
                text: "There are many files.".into(),
                parent: None,
            },
        },
    ]
}

fn accepted(result: &CommandResult) -> Seq {
    match result {
        CommandResult::Accepted { seq } => *seq,
        other => panic!("not accepted: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_adopting_create_logs_its_session_the_history_then_the_finish_naming_it() {
    let f = Fixture::start(FakeOutside::new()).await;
    f.conversation(HistoryRead::Read { items: history() });
    let mut device = f.device().await;
    let adopt = f.adopt(&format!(" {CONVERSATION} "));

    let result = device.command(&adopt).await;
    assert_eq!(result, CommandResult::Accepted { seq: Seq(6) });
    let events = device.events().await;
    assert_eq!(events.len(), 6);
    let session = events[0].session_id;
    assert!(
        events
            .iter()
            .all(|e| e.session_id == session && e.context == ContextId::first_of(session)),
        "every event is the new session's, stamped with its first context: {events:#?}"
    );
    let EventKind::SessionCreated { workdir, from, .. } = &events[0].kind else {
        panic!("{:?}", events[0]);
    };
    assert_eq!(workdir, &f.workdir);
    assert_eq!(
        from,
        &Some(SessionOrigin::Agent {
            conversation: CONVERSATION.into()
        })
    );
    assert_eq!(
        events[0].command_id, None,
        "the create names its last event"
    );

    let imported: Vec<_> = events[1..5]
        .iter()
        .map(|e| {
            assert_eq!(e.command_id, None);
            let EventKind::Imported { at, item } = &e.kind else {
                panic!("{e:?}");
            };
            (*at, item.clone())
        })
        .collect();
    assert_eq!(imported[0].0, Some("2026-11-01T10:00:00Z".parse().unwrap()));
    assert_eq!(
        imported[0].1,
        ImportedItem::UserMessage {
            text: "list the files".into(),
            full: None,
        }
    );
    let ImportedItem::ToolCall(call) = &imported[1].1 else {
        panic!("{:?}", imported[1]);
    };
    let ImportedItem::ToolResult(answer) = &imported[2].1 else {
        panic!("{:?}", imported[2]);
    };
    assert!(call.call.is_some());
    assert_eq!(
        answer.call, call.call,
        "the result links to the imported call"
    );
    let output = answer
        .output
        .as_ref()
        .and_then(skywave_proto::Full::blob)
        .cloned()
        .expect("the long output is a blob");
    assert_eq!(
        imported[3].1,
        ImportedItem::AgentMessage {
            text: "There are many files.".into(),
            full: None,
        }
    );

    let finished = &events[5];
    assert_eq!(finished.command_id, Some(adopt.id));
    assert_eq!(
        finished.kind,
        EventKind::ImportFinished {
            outcome: ImportOutcome::Imported { items: 4 }
        }
    );
    // Nothing the import logged brings an archived session back.
    assert!(events.iter().all(|e| e.kind.archived_effect().is_none()));
    // The blob is recorded and served.
    assert_eq!((f.blob_files(), f.blob_rows()), (1, 1));
    let (status, body) = device
        .send(Method::GET, &path::blob(output.id), Bytes::new())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.len(), "file\n".len() * 1000);
    assert_eq!(f.outside.history_calls(), 1);

    // A resend is answered from the log: no second read, no blob.
    assert_eq!(device.command(&adopt).await, result);
    assert_eq!(f.outside.history_calls(), 1);
    assert_eq!((f.blob_files(), f.blob_rows()), (1, 1));
    assert_eq!(f.log.head(), Seq(6));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resend_while_the_history_is_read_joins_the_first() {
    let outside = FakeOutside::new();
    let gate = Gate::new();
    outside.block_history(gate.clone());
    let _open = OpenOnDrop(gate.clone());
    let f = Fixture::start(outside).await;
    f.conversation(HistoryRead::Read { items: history() });
    let adopt = f.adopt(CONVERSATION);

    let mut first = f.device().await;
    let mut second = f.device().await;
    let sent = adopt.clone();
    let first_reply = tokio::spawn(async move { first.command(&sent).await });
    tokio::time::timeout(SOON, f.outside.history_entered().opened())
        .await
        .expect("the first request reads the history");
    let sent = adopt.clone();
    let second_reply = tokio::spawn(async move { second.command(&sent).await });
    tokio::time::timeout(SOON, f.joined.opened())
        .await
        .expect("the resend joins the first");
    assert!(
        !second_reply.is_finished(),
        "it waits for the first's reply"
    );
    assert_eq!(f.outside.history_calls(), 1);
    gate.open();

    let first = tokio::time::timeout(SOON, first_reply)
        .await
        .unwrap()
        .unwrap();
    let second = tokio::time::timeout(SOON, second_reply)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first, CommandResult::Accepted { seq: Seq(6) });
    assert_eq!(second, first);
    assert_eq!(f.outside.history_calls(), 1);
    assert_eq!(f.log.head(), Seq(6), "imported once");
    assert_eq!((f.blob_files(), f.blob_rows()), (1, 1), "no orphan blob");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_adopting_create_reads_nothing_and_logs_nothing() {
    let f = Fixture::start(FakeOutside::new()).await;
    f.conversation(HistoryRead::Read { items: history() });
    let mut device = f.device().await;

    let outside_roots = f.create("/", Some(CONVERSATION));
    assert_eq!(
        device.command(&outside_roots).await,
        CommandResult::Rejected {
            reason: RejectReason::WorkdirNotAllowed
        }
    );
    let bad_id = f.adopt("not an id");
    assert_eq!(
        device.command(&bad_id).await,
        CommandResult::Rejected {
            reason: RejectReason::BadConversation
        }
    );
    // A resend gets the stored refusal.
    assert_eq!(
        device.command(&bad_id).await,
        CommandResult::Rejected {
            reason: RejectReason::BadConversation
        }
    );
    assert_eq!(f.outside.history_calls(), 0);
    assert_eq!(f.log.head(), Seq::ZERO);
    assert_eq!((f.blob_files(), f.blob_rows()), (0, 0));
    // Positive control: the same conversation in the root imports.
    let good = f.adopt(CONVERSATION);
    accepted(&device.command(&good).await);
    assert_eq!((f.blob_files(), f.blob_rows()), (1, 1));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_import_that_brings_nothing_still_creates_the_session() {
    for (read, outcome) in [
        (None, ImportOutcome::NotFound),
        (
            Some(HistoryRead::Failed("unreadable line 3".into())),
            ImportOutcome::Failed {
                reason: "unreadable line 3".into(),
            },
        ),
        (Some(HistoryRead::Unavailable), ImportOutcome::Unavailable),
    ] {
        let f = Fixture::start(FakeOutside::new()).await;
        if let Some(read) = read {
            f.conversation(read);
        }
        let mut device = f.device().await;
        let adopt = f.adopt(CONVERSATION);
        assert_eq!(
            device.command(&adopt).await,
            CommandResult::Accepted { seq: Seq(2) }
        );
        let events = device.events().await;
        assert!(
            matches!(
                &events[0].kind,
                EventKind::SessionCreated {
                    from: Some(SessionOrigin::Agent { .. }),
                    ..
                }
            ),
            "{events:?}"
        );
        assert_eq!(events[0].command_id, None);
        assert_eq!(events[1].kind, EventKind::ImportFinished { outcome });
        assert_eq!(events[1].command_id, Some(adopt.id));
        assert_eq!(f.blob_files(), 0);
    }
}

/// Past the deadline the reply goes on without the import; the read the
/// API stopped waiting for still finishes, and bounds its few items (the
/// next deadline check is [`DEADLINE_EVERY`] items in), writing a blob. The
/// API dropped that import, so the blob's file is removed when it ends.
/// The runtime is the test's own: dropping it waits for that blocking
/// thread, so the count after it is final.
#[test]
fn an_import_past_its_deadline_fails_and_the_reply_comes_in_time() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let outside = FakeOutside::new();
    let gate = Gate::new();
    outside.block_history(gate.clone());
    let open = OpenOnDrop(gate.clone());
    let deadline = Duration::from_millis(300);
    assert!(
        history().len() <= DEADLINE_EVERY,
        "bounded before any check"
    );
    let f = runtime.block_on(async {
        let f = Fixture::start_with_deadline(outside, deadline).await;
        f.conversation(HistoryRead::Read { items: history() });
        let mut device = f.device().await;
        let adopt = f.adopt(CONVERSATION);

        let started = Instant::now();
        let result = tokio::time::timeout(DEVICE_REQUEST_TIMEOUT, device.command(&adopt))
            .await
            .expect("the reply comes before the device gives up");
        let took = started.elapsed();
        assert!(took >= deadline, "it waited for the read: {took:?}");
        assert!(took < deadline + SOON, "then went on: {took:?}");
        assert_eq!(result, CommandResult::Accepted { seq: Seq(2) });
        let events = device.events().await;
        assert_eq!(
            events[1].kind,
            EventKind::ImportFinished {
                outcome: ImportOutcome::Failed {
                    reason: TIMED_OUT.into()
                }
            }
        );
        assert_eq!(f.outside.history_calls(), 1);
        f
    });
    // Release the abandoned read, then wait for its thread to end.
    drop(open);
    drop(runtime);
    assert_eq!((f.blob_files(), f.blob_rows()), (0, 0), "no orphan blob");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fresh_create_reads_nothing() {
    let f = Fixture::start(FakeOutside::new()).await;
    let mut device = f.device().await;
    let create = f.create(&f.workdir, None);
    assert_eq!(
        device.command(&create).await,
        CommandResult::Accepted { seq: Seq(1) }
    );
    let events = device.events().await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].command_id, Some(create.id));
    assert!(matches!(
        events[0].kind,
        EventKind::SessionCreated { from: None, .. }
    ));
    assert_eq!(f.outside.history_calls(), 0);
}

/// An item as the fake source lists it.
fn listed_item(
    id: &str,
    workdir: &str,
    title: &str,
    first_prompt: &str,
) -> agent::OutsideConversation {
    agent::OutsideConversation {
        id: id.to_owned(),
        workdir: workdir.to_owned(),
        title: Some(title.to_owned()),
        first_prompt: Some(first_prompt.to_owned()),
        last_active: "2026-11-01T10:00:00Z".parse().unwrap(),
        messages: 4,
        session: None,
    }
}

async fn list(device: &mut Client) -> (StatusCode, Option<ConversationsResult>) {
    let (status, body) = device
        .send(Method::GET, path::CONVERSATIONS, Bytes::new())
        .await;
    let result = (status == StatusCode::OK).then(|| serde_json::from_slice(&body).unwrap());
    (status, result)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_listing_carries_the_owning_session_and_cuts_the_texts() {
    let f = Fixture::start(FakeOutside::new()).await;
    let mut device = f.device().await;
    device.command(&f.create(&f.workdir, None)).await;
    let session = device.events().await[0].session_id;
    // The context runs a conversation of its own once the agent reports
    // its handle; the fake names that conversation by the context's id.
    let context = ContextId::first_of(session);
    agent::store_resume_handle(&f.storage.db, context, &FakeAgent::resume_handle(context)).unwrap();
    let own_id = context.0.to_string();
    f.outside.listing(Ok(Listed {
        conversations: vec![
            listed_item(
                &own_id,
                &f.workdir,
                &"t".repeat(300),
                "fix the bug\nin detail",
            ),
            listed_item(CONVERSATION, &f.workdir, "short", "hello"),
        ],
        truncated: true,
    }));

    let (status, result) = list(&mut device).await;
    assert_eq!(status, StatusCode::OK);
    let wire = |id: &str, title: String, first_prompt: &str, session| OutsideConversation {
        id: id.to_owned(),
        workdir: f.workdir.clone(),
        title: Some(title),
        first_prompt: Some(first_prompt.to_owned()),
        last_active: "2026-11-01T10:00:00Z".parse().unwrap(),
        messages: 4,
        session,
    };
    assert_eq!(
        result,
        Some(ConversationsResult::Listed {
            conversations: vec![
                wire(
                    &own_id,
                    format!("{}…", "t".repeat(limits::TITLE_MAX_CHARS - 1)),
                    "fix the bug…",
                    Some(session),
                ),
                wire(CONVERSATION, "short".into(), "hello", None),
            ],
            truncated: true,
        })
    );
    assert_eq!(f.outside.lists(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_long_first_prompt_is_cut_to_its_limit() {
    let f = Fixture::start(FakeOutside::new()).await;
    let mut device = f.device().await;
    f.outside.listing(Ok(Listed {
        conversations: vec![listed_item(CONVERSATION, &f.workdir, "t", &"p".repeat(500))],
        truncated: false,
    }));
    let (_, result) = list(&mut device).await;
    let Some(ConversationsResult::Listed {
        conversations,
        truncated,
    }) = result
    else {
        panic!("{result:?}");
    };
    assert!(!truncated);
    let prompt = conversations[0].first_prompt.as_deref().unwrap();
    assert_eq!(prompt.chars().count(), limits::FIRST_PROMPT_MAX_CHARS);
    assert!(prompt.ends_with('…'), "{prompt}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unavailable_source_answers_unavailable_and_a_failed_one_500() {
    let f = Fixture::start(FakeOutside::new()).await;
    let mut device = f.device().await;
    f.outside.listing(Err(SourceError::Unavailable));
    assert_eq!(
        list(&mut device).await,
        (StatusCode::OK, Some(ConversationsResult::Unavailable))
    );
    f.outside
        .listing(Err(SourceError::Failed("disk on fire".into())));
    assert_eq!(
        list(&mut device).await,
        (StatusCode::INTERNAL_SERVER_ERROR, None)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_listing_slower_than_the_timeout_is_a_503() {
    let timeout = Duration::from_millis(300);
    let f = Fixture::start_with_listing_timeout(FakeOutside::new(), timeout).await;
    let gate = Gate::new();
    let _open = OpenOnDrop(gate.clone());
    f.outside.block_list(gate.clone());
    let mut device = f.device().await;

    let started = Instant::now();
    assert_eq!(
        list(&mut device).await,
        (StatusCode::SERVICE_UNAVAILABLE, None)
    );
    assert!(started.elapsed() >= timeout);
    assert!(started.elapsed() < SOON);
    assert_eq!(f.outside.lists(), 1, "the listing was asked and held");

    // Once the source answers again, so does the route.
    gate.open();
    assert_eq!(
        list(&mut device).await,
        (
            StatusCode::OK,
            Some(ConversationsResult::Listed {
                conversations: vec![],
                truncated: false,
            })
        )
    );
}

/// A source stuck on a hung mount strands a blocking thread per call; the
/// file-system permits cap those, while authentication, commands and events
/// (which need blocking threads too) go on.
#[tokio::test(flavor = "multi_thread")]
async fn stuck_file_system_reads_are_capped_and_never_stall_other_routes() {
    let timeout = Duration::from_millis(300);
    let f = Fixture::start_with_listing_timeout(FakeOutside::new(), timeout).await;
    let gate = Gate::new();
    let _open = OpenOnDrop(gate.clone());
    f.outside.block_list(gate.clone());
    let mut device = f.device().await;

    // Each listing gives up with a 503 and leaves its thread stuck, holding
    // its permit.
    for _ in 0..FS_PERMITS {
        assert_eq!(
            list(&mut device).await,
            (StatusCode::SERVICE_UNAVAILABLE, None)
        );
    }
    let roots = format!("{}?dir=", path::DIRS);
    let started = Instant::now();
    let (status, _) = device.send(Method::GET, &roots, Bytes::new()).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "no permit is left");
    assert!(started.elapsed() < SOON);
    assert_eq!(f.outside.lists(), FS_PERMITS, "only the stuck ones listed");

    // Authentication, hello, commands and events still answer.
    let (status, _) = device.send(Method::GET, path::HELLO, Bytes::new()).await;
    assert_eq!(status, StatusCode::OK);
    device.command(&f.create(&f.workdir, None)).await;
    let session = device.events().await[0].session_id;
    let workdir = format!("{}?session={}&path=", path::WORKDIR_LIST, session.0);
    let (status, _) = device.send(Method::GET, &workdir, Bytes::new()).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "workdir reads too");

    // Once the mount answers, the stuck threads return their permits.
    gate.open();
    let (status, _) = device.send(Method::GET, &roots, Bytes::new()).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = device.send(Method::GET, &workdir, Bytes::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list(&mut device).await.0, StatusCode::OK);
}
