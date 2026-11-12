//! The authenticated part of the HTTPS API (architecture.md "Protocol"): hello, commands,
//! events and blobs. `POST /v1/pair` lives in [`crate::pairing`]; the
//! [`crate::server`] assembles both behind TLS.
//!
//! **Authentication** is per request, not per connection, so a revocation
//! applies to the next request on a live keep-alive connection. TLS has
//! already checked that a presented client certificate chains to the CA;
//! [`authenticate`] then looks its fingerprint up in the device registry:
//! - no client certificate → 401;
//! - a certificate whose device is unknown or revoked → 403;
//! - otherwise the device's `last_seen` is updated (at most once every
//!   [`TOUCH_INTERVAL`]) and the handler sees the caller's [`DeviceId`].
//!
//! **The expected run** is checked next, also per request ([`expected_run`]):
//! a [`RUN_HEADER`] naming another run than this one (the device synced
//! with a run this server is not, e.g. one rolled back under a connection
//! that never closed) is a 412 and reaches no handler.
//!
//! **Long-poll** (`GET /v1/events`, D15): returns at once when events after
//! `after` exist, else waits up to `wait` seconds, capped at [`MAX_WAIT`],
//! for the next append. A timed-out wait returns an empty batch
//! (`from = after + 1`). Server shutdown ends waits early.
//!
//! **Blobs** (`GET /v1/blobs/{id}`): one `Range: bytes=` range (`a-b`, `a-`
//! or `-n`) gets 206 with `Content-Range`; an unsatisfiable one 416. Several
//! ranges, other units or malformed ranges are ignored and the full blob is
//! sent (RFC 9110 lets a server do that). Content is streamed from the file.
//!
//! **Attachments** (`PUT` and `HEAD /v1/attachments/{sha256}`, v30): an image
//! a device uploads before the message naming it ([`crate::attachments`]).
//! `PUT` answers a 200 [`UploadResult`]; `HEAD` is 204 for a staged image,
//! else 404. Neither takes a file-system permit, like blobs.
//!
//! **Compression** (D45, [`crate::encoding`]): an event batch or blob body
//! of at least `COMPRESS_MIN_BYTES` goes as one zstd frame when the request
//! accepts zstd. A blob's range counts decoded bytes, so a resumed download
//! gets a fresh frame of the rest.
//!
//! **Dirs** (`GET /v1/dirs?dir=`, workdir completion): every outcome is a
//! 200 [`DirsResult`](skywave_proto::DirsResult), since 401/403 would read
//! as a revoked device. The listing ([`crate::dirs`]) runs on a blocking
//! thread outside the API's database connection; one that takes longer than
//! [`DIRS_TIMEOUT`] gets a 503, and the scan, bounded by its cap, finishes
//! unobserved.
//!
//! **Conversations** (`GET /v1/conversations`, D37): the agent's
//! conversations under the roots, a 200
//! [`ConversationsResult`](skywave_proto::ConversationsResult) (an agent
//! that keeps none Skywave can read answers `Unavailable`). One short read
//! on the API's connection names the conversations Skywave's contexts run
//! ([`OutsideConversations::own`]); the scan itself runs on a blocking
//! thread outside it, with a deadline a little shorter than
//! [`CONVERSATIONS_TIMEOUT`], past which the reply is a 503. Titles and
//! first prompts are cut to one line of the wire limits.
//!
//! **Usage limits** (`GET /v1/usage-limits`): the account's limits as the
//! agent reports them now, a 200
//! [`UsageLimitsResult`](skywave_proto::UsageLimitsResult) (`Unavailable`
//! for an agent that reports none, `Failed` with the agent's reason). The
//! source gets [`USAGE_LIMITS_DEADLINE`], past which it answers `Failed`;
//! [`USAGE_LIMITS_TIMEOUT`] is the outer bound, a 503. A scope is cut to
//! one line of the wire limit. Nothing is stored.
//!
//! **The agent's login** (`POST /v1/agent-login/start` and `/finish`,
//! D49): a device logs the agent in over request/response through
//! [`LoginFlows`] (one flow at a time; racing starts join, a finish is
//! idempotent per flow), every outcome a 200 but a code over
//! `LOGIN_CODE_MAX_BYTES` (a 400). The agent gets [`LOGIN_START_DEADLINE`]
//! to answer a start, past which it answers `Failed`;
//! [`LOGIN_START_TIMEOUT`] is the outer bound, a 503. A finish waits for
//! the agent at most the server's login finish timeout
//! ([`LOGIN_FINISH_TIMEOUT`] by default), then answers 503; the finish goes
//! on, and a retry naming the same flow gets its outcome. A reason is cut
//! to one line of `REFUSAL_MAX_CHARS`. Neither the URL nor the code is
//! logged or stored. Each `hello` drops a flow left open too long.
//!
//! **Workdir** (`GET /v1/workdir/changes`, `/v1/workdir/log`,
//! `/v1/workdir/list` and `/v1/workdir/file`, D36): reads of a session's
//! workdir, every outcome a 200 [`ChangesResult`](skywave_proto::ChangesResult),
//! [`LogResult`](skywave_proto::LogResult),
//! [`ListResult`](skywave_proto::ListResult) or
//! [`FileResult`](skywave_proto::FileResult). They run on a blocking thread
//! through [`crate::workdir::WorkdirReader`], on its own database connection
//! and one snapshot at a time. Each request takes its deadline
//! ([`workdir::SNAPSHOT_DEADLINE`]) when the handler starts; a snapshot that
//! gets the lock only after it does nothing and the reply is a 503. One that
//! reaches it while running stops: a 503 for list and file, and the 200
//! `Failed { "timed out" }` for changes, whose git children are killed then.
//! [`WORKDIR_TIMEOUT`] is the outer bound, also a 503: it drops only the
//! future, and the deadline is what stops the abandoned work.
//!
//! **Blocking file-system work** (dirs, the conversations scan, workdir
//! reads and storage reports) first takes one of [`FS_PERMITS`] permits,
//! waiting at most [`FS_PERMIT_WAIT`], else the reply is a 503. A timeout
//! drops only the future, never the blocking thread, so a root on a hung
//! mount would strand
//! one thread per call until tokio's blocking pool is full and
//! [`authenticate`], itself on a blocking thread, stops every request. The
//! permit moves into the blocking closure and is released only when the
//! work returns: stuck threads hold theirs, so at most [`FS_PERMITS`] can
//! pile up. Hello, commands, events, blobs and attachments never take one.
//!
//! **Prepared commands** (an adopting create and a crop) are prepared off
//! the API's connection, then handed to [`EventLog::handle_with`]. A
//! command already handled is answered from its stored result without
//! preparing anything; a resend that arrives while the first request is
//! still preparing waits for that request's reply.
//!
//! **Adopting creates** (`CreateSession` with a `conversation`, D37): the
//! conversation's history is read and bounded first, on a blocking thread
//! and off the API's connection. The
//! import gets the server's import deadline
//! ([`import::IMPORT_DEADLINE`]), after which the reply goes on with
//! `Failed("timed out")`. Canonicalising the workdir first shares that
//! deadline; past it the reply is a 503 and nothing is handled, so the
//! device resends.
//!
//! **Crops** (`CropBlob`, D42): whether the blob can be cropped is read
//! briefly on the API's connection, which opens it; the crop runs on a
//! blocking thread off it ([`crate::crop`]), within the crop deadline
//! ([`crop::CROP_DEADLINE`] by default). Past it the reply is a 503 and nothing
//! is handled; the abandoned crop's file goes before its thread ends. An
//! I/O failure other than a full disk is a 500, also unhandled.
//!
//! Errors are bare status codes: 400 malformed request, 404 unknown blob or
//! attachment not staged, 412 another run expected, 500 server fault.

use std::collections::HashMap;
use std::io::{Seek, SeekFrom};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Extension, Json, Router};
use bytes::BytesMut;
use chrono::Utc;
use futures_util::StreamExt;
use rustls_pki_types::CertificateDer;
use skywave_proto::{
    BlobId, BlobUse, Command, CommandId, CommandKind, CommandResult, ContentHash,
    ConversationsResult, DeviceId, DeviceInfo, DirsQuery, EventsQuery, Fingerprint, HelloQuery,
    HelloResponse, ImportOutcome, LineSpan, LoginFinish, LoginFinishResult, LoginStartResult,
    PROTOCOL_VERSION, RUN_HEADER, RunId, ServerId, UploadResult, UsageLimit, UsageLimitsResult,
    WorkdirQuery, check_conversation_id, limits, path,
};
use tokio::io::AsyncReadExt;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot, watch};

use crate::agent::import::{self, Prepared};
use crate::agent::mapper::cut_line;
use crate::agent::{
    self, AgentLogin, LimitsError, LoginState, LoginStatus, OutsideConversations, SourceError,
    UsageLimitsSource,
};
use crate::attachments::{self, Staging};
use crate::blobs::BlobStore;
use crate::commands::Preparation;
use crate::crop::{self, CropHook};
use crate::data_dir::DataDir;
use crate::db::Db;
use crate::devices;
use crate::dirs;
use crate::encoding;
use crate::error::{Error, Result};
use crate::event_log::{self, DEFAULT_BATCH_LIMIT, MAX_SEQ, head_in};
use crate::login::LoginFlows;
use crate::server::{JoinedHook, ServerOptions};
use crate::storage::{self, StorageReader};
use crate::workdir::{self, Snapshot, WorkdirReader};
use crate::{EventLog, lock};

/// Longest a `GET /v1/events` waits, whatever `wait` asks for.
pub const MAX_WAIT: Duration = Duration::from_secs(60);
/// `last_seen` is written at most this often per device.
pub const TOUCH_INTERVAL: chrono::Duration = chrono::Duration::seconds(60);
/// Longest a `GET /v1/dirs` listing may take before the reply gives up.
pub const DIRS_TIMEOUT: Duration = Duration::from_secs(2);
/// Longest a `GET /v1/conversations` listing may take before the reply
/// gives up with a 503 (the default of `ServerOptions::conversations_timeout`).
/// The source's own deadline is [`listing_deadline`] of it, so a slow scan
/// normally answers `truncated` rather than a 503.
pub const CONVERSATIONS_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest a workdir request may take before the reply gives up with a 503.
/// Longer than [`workdir::SNAPSHOT_DEADLINE`], which stops the snapshot
/// itself; this only stops waiting for it.
pub const WORKDIR_TIMEOUT: Duration = Duration::from_secs(20);
/// Longest a storage report may take before the reply gives up with a 503.
/// Longer than [`storage::REPORT_DEADLINE`], which stops the report itself.
pub const STORAGE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the agent gets to report its usage limits before the answer is
/// `Failed`.
pub const USAGE_LIMITS_DEADLINE: Duration = Duration::from_secs(15);
/// Longest a usage limits read may take before the reply gives up with a
/// 503; longer than [`USAGE_LIMITS_DEADLINE`], which stops the read itself.
pub const USAGE_LIMITS_TIMEOUT: Duration = Duration::from_secs(20);
/// How long the agent gets to answer a login start with a URL before the
/// answer is `Failed`.
pub const LOGIN_START_DEADLINE: Duration = Duration::from_secs(15);
/// Longest a login start may take before the reply gives up with a 503;
/// longer than [`LOGIN_START_DEADLINE`], which stops the start itself.
pub const LOGIN_START_TIMEOUT: Duration = Duration::from_secs(20);
/// Longest a login finish waits for the agent's answer before the reply is
/// a 503 (the default of `ServerOptions::login_finish_timeout`). The finish
/// itself has no deadline and goes on: a retry collects its outcome.
pub const LOGIN_FINISH_TIMEOUT: Duration = Duration::from_secs(20);
/// Blocking file-system work (dirs, conversations, workdir reads and
/// storage reports) that may run at once; see the module docs, "Blocking
/// file-system work".
pub const FS_PERMITS: usize = 8;
/// Longest such a request waits for a permit before the reply is a 503.
pub const FS_PERMIT_WAIT: Duration = Duration::from_millis(500);

/// The client certificate (leaf) the peer presented in the TLS handshake,
/// if any. The connection loop attaches it to every request.
#[derive(Debug, Clone)]
pub(crate) struct PeerCert(pub(crate) Option<Arc<CertificateDer<'static>>>);

/// The authenticated device making the request.
#[derive(Debug, Clone, Copy)]
struct Caller(DeviceId);

/// Shared state of the authenticated routes.
pub(crate) struct Api {
    /// The API's own connection. SQLite has one writer anyway; long-poll
    /// waits happen outside the lock.
    db: Mutex<Db>,
    /// Workdir reads, on a second connection of their own (behind its own
    /// mutex), so file I/O never holds `db`.
    workdir: WorkdirReader,
    /// Storage reports, on a third connection of their own.
    storage: StorageReader,
    log: EventLog,
    blobs: BlobStore,
    /// Images staged for a message (`/v1/attachments/{sha256}`).
    staging: Staging,
    server_id: ServerId,
    /// Becomes `true` when the server shuts down, ending long-poll waits.
    shutdown: watch::Receiver<bool>,
    /// What an adopting `CreateSession` imports from.
    outside: Arc<dyn OutsideConversations>,
    usage_limits: Arc<dyn UsageLimitsSource>,
    /// What a `hello` reads the agent's login expiry, and whether a device
    /// may log it in, from.
    login: Arc<dyn AgentLogin>,
    /// What a `hello` reads whether the agent is logged out from.
    login_state: LoginState,
    /// The device login flow the login endpoints drive.
    login_flows: LoginFlows,
    login_finish_timeout: Duration,
    import_deadline: Duration,
    conversations_timeout: Duration,
    crop_deadline: Duration,
    /// Run in every crop before it writes and when its thread ends (tests).
    crop_hook: Option<Arc<dyn CropHook>>,
    /// Run when a request waits for another's preparation (tests).
    joined_hook: Option<JoinedHook>,
    /// The commands being prepared (adopting creates and crops), by command
    /// id: each answers with the command's reply once it has one.
    preparing: Mutex<HashMap<CommandId, watch::Receiver<Option<Reply>>>>,
    /// [`FS_PERMITS`] permits; blocking file-system work holds one until it
    /// returns.
    fs: Arc<Semaphore>,
}

/// A command's reply: its result, or the status of a request that failed.
type Reply = std::result::Result<CommandResult, StatusCode>;

impl Api {
    /// Opens three connections to `dir`'s database: the API's own, the
    /// workdir reads' and the storage reports'.
    pub(crate) fn new(
        dir: &DataDir,
        log: EventLog,
        server_id: ServerId,
        shutdown: watch::Receiver<bool>,
        options: ServerOptions,
    ) -> Result<Api> {
        let blobs = BlobStore::new(dir.blobs_dir());
        let login_flows = LoginFlows::new(
            options.login.clone(),
            options.login_state.clone(),
            options.login_flow_max,
            options.joined_hook.clone(),
        );
        Ok(Api {
            db: Mutex::new(Db::open(&dir.db_path())?),
            workdir: WorkdirReader::new(
                Db::open(&dir.db_path())?,
                blobs.clone(),
                log.roots().to_vec(),
            )
            .with_git(options.git),
            storage: StorageReader::new(Db::open(&dir.db_path())?, dir.clone(), options.log_file),
            // A `DeleteBlob` removes the file from the store served here
            // (a `CropBlob` its own preparation's, the same store).
            log: log.with_blobs(blobs.clone()),
            blobs,
            staging: Staging::new(dir.attachments_dir()),
            server_id,
            shutdown,
            outside: options.outside,
            usage_limits: options.usage_limits,
            login: options.login,
            login_state: options.login_state,
            login_flows,
            login_finish_timeout: options.login_finish_timeout,
            import_deadline: options.import_deadline,
            conversations_timeout: options.conversations_timeout,
            crop_deadline: options.crop_deadline,
            crop_hook: options.crop_hook,
            joined_hook: options.joined_hook,
            preparing: Mutex::new(HashMap::new()),
            fs: Arc::new(Semaphore::new(FS_PERMITS)),
        })
    }

    /// A permit for blocking file-system work, waiting at most
    /// [`FS_PERMIT_WAIT`]; past it a 503. Move it into the blocking closure,
    /// so a thread stuck on a hung mount keeps it.
    async fn fs_permit(&self) -> std::result::Result<OwnedSemaphorePermit, StatusCode> {
        match tokio::time::timeout(FS_PERMIT_WAIT, self.fs.clone().acquire_owned()).await {
            Ok(Ok(permit)) => Ok(permit),
            // The semaphore is never closed; only the wait can end it.
            Ok(Err(_)) | Err(_) => {
                tracing::warn!("every file-system permit is held; answering 503");
                Err(StatusCode::SERVICE_UNAVAILABLE)
            }
        }
    }

    /// Runs `f` with the API's connection on a blocking thread; any error
    /// becomes a logged 500.
    async fn blocking<T: Send + 'static>(
        self: &Arc<Self>,
        f: impl FnOnce(&Api, &mut Db) -> Result<T> + Send + 'static,
    ) -> std::result::Result<T, StatusCode> {
        let api = self.clone();
        let joined = tokio::task::spawn_blocking(move || {
            let mut db = lock(&api.db);
            f(&api, &mut db)
        })
        .await;
        match joined {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(e)) => {
                tracing::error!(error = %e, "API request failed");
                Err(StatusCode::INTERNAL_SERVER_ERROR)
            }
            Err(e) => {
                tracing::error!(error = %e, "API task failed");
                Err(StatusCode::INTERNAL_SERVER_ERROR)
            }
        }
    }
}

/// Every route that needs a registered, non-revoked device certificate.
pub(crate) fn router(api: Arc<Api>) -> Router {
    Router::new()
        .route(path::HELLO, get(hello))
        .route(path::COMMANDS, post(commands))
        .route(path::EVENTS, get(events))
        .route(path::DIRS, get(list_dirs))
        .route(path::CONVERSATIONS, get(list_conversations))
        .route(path::STORAGE, get(storage_report))
        .route(path::USAGE_LIMITS, get(usage_limits))
        .route(path::AGENT_LOGIN_START, post(agent_login_start))
        .route(path::AGENT_LOGIN_FINISH, post(agent_login_finish))
        .route(path::WORKDIR_CHANGES, get(workdir_changes))
        .route(path::WORKDIR_LOG, get(workdir_log))
        .route(path::WORKDIR_LIST, get(workdir_list))
        .route(path::WORKDIR_FILE, get(workdir_file))
        .route(path::BLOB_ROUTE, get(blob))
        .route(
            path::ATTACHMENT_ROUTE,
            put(stage_attachment).head(attachment_staged),
        )
        // The last layer added runs first: authentication, then the run.
        .route_layer(middleware::from_fn_with_state(api.clone(), expected_run))
        .route_layer(middleware::from_fn_with_state(api.clone(), authenticate))
        .with_state(api)
}

/// Refuses a request whose [`RUN_HEADER`] names another run than this one
/// with 412, before its handler sees it; a malformed header is a 400, and
/// a request without one goes on.
async fn expected_run(State(api): State<Arc<Api>>, req: Request, next: Next) -> Response {
    if let Some(value) = req.headers().get(RUN_HEADER) {
        let Some(expected) = value
            .to_str()
            .ok()
            .and_then(|text| uuid::Uuid::parse_str(text).ok())
            .map(RunId)
        else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        let run = api.log.run_id();
        if expected != run {
            tracing::info!(?expected, ?run, "refused a request meant for another run");
            return StatusCode::PRECONDITION_FAILED.into_response();
        }
    }
    next.run(req).await
}

async fn authenticate(State(api): State<Arc<Api>>, mut req: Request, next: Next) -> Response {
    let cert = req.extensions().get::<PeerCert>().and_then(|p| p.0.clone());
    let Some(cert) = cert else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let fingerprint = Fingerprint::of_der(&cert);
    let now = Utc::now();
    let device = api
        .blocking(move |_, db| {
            let Some(device) = devices::by_fingerprint(db.conn(), fingerprint)? else {
                return Ok(Err("unknown"));
            };
            if device.is_revoked() {
                return Ok(Err("revoked"));
            }
            if device.last_seen.is_none_or(|t| now - t >= TOUCH_INTERVAL) {
                devices::touch(db.conn(), device.id, now)?;
            }
            Ok(Ok(device.id))
        })
        .await;
    match device {
        Ok(Ok(id)) => {
            req.extensions_mut().insert(Caller(id));
            next.run(req).await
        }
        Ok(Err(why)) => {
            tracing::info!(%fingerprint, why, "refused device certificate");
            StatusCode::FORBIDDEN.into_response()
        }
        Err(status) => status.into_response(),
    }
}

async fn hello(
    State(api): State<Arc<Api>>,
    Extension(Caller(device)): Extension<Caller>,
    query: std::result::Result<Query<HelloQuery>, QueryRejection>,
) -> Response {
    let Ok(Query(query)) = query else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let read = api
        .blocking(move |api, db| {
            let devices = devices::list(db.conn())?
                .into_iter()
                .map(|d| DeviceInfo {
                    id: d.id,
                    name: d.name,
                })
                .collect();
            // The committed head, not the watch: that is published after
            // the commit, so it may lag an event a pull already got.
            let head = head_in(db.conn())?;
            let log = api.log.run(db, query.since)?;
            // A device that started a login and never finished it: its
            // flow goes once it is old enough.
            api.login_flows.touch();
            // Read on every hello, so a login renewed on the server shows
            // at the device's next connection.
            Ok((devices, head, log, api.login.expires()))
        })
        .await;
    match read {
        Ok((devices, head, log, login_expires)) => Json(HelloResponse {
            protocol_version: PROTOCOL_VERSION,
            server_id: api.server_id,
            device_id: device,
            head,
            log,
            devices,
            shell: api.log.shell_enabled(),
            bypass: api.log.bypass_allowed(),
            login_expires,
            agent_login_offered: api.login.offered(),
            // Only a definite "not logged in": not knowing yet is not it.
            agent_logged_out: api.login_state.status() == LoginStatus::LoggedOut,
        })
        .into_response(),
        Err(status) => status.into_response(),
    }
}

async fn commands(
    State(api): State<Arc<Api>>,
    Extension(Caller(device)): Extension<Caller>,
    body: std::result::Result<Json<Command>, JsonRejection>,
) -> Response {
    let command = match body {
        Ok(Json(command)) => command,
        Err(rejection) if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            return StatusCode::PAYLOAD_TOO_LARGE.into_response();
        }
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    let prepares = matches!(
        command.kind,
        CommandKind::CreateSession {
            conversation: Some(_),
            ..
        } | CommandKind::CropBlob { .. }
    ) || matches!(
        &command.kind,
        CommandKind::SendMessage { attachments, .. } if !attachments.is_empty()
    );
    let reply = if prepares {
        prepared(&api, device, command).await
    } else {
        api.blocking(move |api, db| api.log.handle(db, device, &command))
            .await
    };
    // Accepted and Rejected are both 200: a rejection is a domain answer.
    match reply {
        Ok(result) => Json(result).into_response(),
        Err(status) => status.into_response(),
    }
}

/// Handles a command prepared before its transaction, outside the API's
/// connection: an adopting `CreateSession`, whose history is read and
/// bounded, a `CropBlob`, whose new blob is written, or a `SendMessage`
/// naming images, whose staged images are linked. Slow file work must not
/// hold up every other device's commands. A resend that arrives while the
/// command is being prepared waits for that one's reply, so a command is
/// never prepared twice.
async fn prepared(api: &Arc<Api>, device: DeviceId, command: Command) -> Reply {
    loop {
        let role = {
            let mut preparing = lock(&api.preparing);
            match preparing.get(&command.id) {
                Some(reply) => Err(reply.clone()),
                None => {
                    let (reply, waiting) = watch::channel(None);
                    preparing.insert(command.id, waiting);
                    Ok(Leading {
                        api,
                        id: command.id,
                        reply,
                    })
                }
            }
        };
        match role {
            Ok(leading) => {
                let reply = prepare_and_handle(api, device, command).await;
                leading.reply.send_replace(Some(reply.clone()));
                return reply;
            }
            Err(mut waiting) => {
                if let Some(joined) = &api.joined_hook {
                    joined();
                }
                if let Ok(reply) = waiting.wait_for(Option::is_some).await {
                    return reply.clone().expect("waited for a reply");
                }
                // The first request was dropped before it had a reply:
                // whatever it did is stored or undone, so start over.
            }
        }
    }
}

/// The request preparing a command; dropped, its entry goes.
struct Leading<'a> {
    api: &'a Api,
    id: CommandId,
    reply: watch::Sender<Option<Reply>>,
}

impl Drop for Leading<'_> {
    fn drop(&mut self) {
        lock(&self.api.preparing).remove(&self.id);
    }
}

async fn prepare_and_handle(api: &Arc<Api>, device: DeviceId, command: Command) -> Reply {
    // A command already handled is answered from the log: nothing is
    // prepared a second time.
    let id = command.id;
    if let Some(stored) = api
        .blocking(move |api, db| api.log.command_result(db, id))
        .await?
    {
        return Ok(stored);
    }
    let prepared = match &command.kind {
        CommandKind::CropBlob { blob, keep } => prepare_crop(api, id, *blob, keep.clone()).await?,
        CommandKind::SendMessage { attachments, .. } => {
            Some(prepare_attachments(api, attachments.clone()).await?)
        }
        _ => prepare_import(api, &command).await?.map(Preparation::from),
    };
    api.blocking(move |api, db| api.log.handle_with(db, device, &command, prepared))
        .await
}

/// The crop of `blob` to `keep`, or `None` when `handle_with` refuses the
/// command without one: no event referenced the blob, it is gone, it holds
/// something not croppable, or the spans have the wrong shape. Those are
/// read briefly on the API's connection, which also opens the blob; the
/// crop itself runs on a blocking thread, off it, reading that open file,
/// so a delete in between unlinks nothing it reads. Past the crop deadline
/// ([`crop::CROP_DEADLINE`] by default) the reply is a 503 and nothing is
/// handled; the abandoned crop's new file is removed before its thread
/// ends (it drops its [`crop::Prepared`]). An I/O failure other than a
/// full disk is a 500.
async fn prepare_crop(
    api: &Arc<Api>,
    id: CommandId,
    blob: BlobId,
    keep: Vec<LineSpan>,
) -> std::result::Result<Option<Preparation>, StatusCode> {
    let deadline = Instant::now() + api.crop_deadline;
    let shape = skywave_proto::crop::check_spans(&keep);
    let file = api
        .blocking(move |api, db| {
            let croppable = event_log::event_blob_in(db.conn(), blob)?.is_some_and(|row| {
                row.stored && matches!(row.holds, BlobUse::ToolOutput | BlobUse::ShellOutput)
            });
            if !croppable || !shape {
                return Ok(None);
            }
            api.blobs.open(db.conn(), blob)
        })
        .await?;
    let Some(file) = file else {
        return Ok(None);
    };
    let (blobs, hook) = (api.blobs.clone(), api.crop_hook.clone());
    let (send, cropped) = oneshot::channel();
    // The thread hands its crop over, or, the request gone past its
    // deadline, drops it there, removing the file it wrote, before it ends.
    tokio::task::spawn_blocking(move || {
        let prepared = crop::prepare(file, blobs, blob, &keep, hook.as_deref());
        drop(send.send(prepared));
        if let Some(hook) = hook {
            hook.ended();
        }
    });
    match tokio::time::timeout_at(deadline.into(), cropped).await {
        Ok(Ok(Ok(prepared))) => Ok(Some(prepared.into())),
        Ok(Ok(Err(e))) => {
            tracing::error!(blob = %blob.0, error = %e, "cropping a blob failed");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
        Ok(Err(_)) => {
            tracing::error!("a crop's thread ended without a crop");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
        Err(_) => {
            tracing::warn!(command = %id.0, "cropping a blob timed out");
            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}

/// The staged images a `SendMessage` names, linked as new blobs on a blocking
/// thread ([`attachments::prepare`]), off the API's connection. The disk is
/// read here, as `prepare_import` reads it: while it is low nothing is
/// linked, and the command is refused `DiskLow`. A file-system failure is a
/// 500.
async fn prepare_attachments(
    api: &Arc<Api>,
    hashes: Vec<ContentHash>,
) -> std::result::Result<Preparation, StatusCode> {
    let disk_low = api.log.disk().refresh();
    let (staging, blobs) = (api.staging.clone(), api.blobs.clone());
    let linked = tokio::task::spawn_blocking(move || {
        attachments::prepare(&staging, &blobs, disk_low, &hashes)
    })
    .await;
    match linked {
        Ok(Ok(prepared)) => Ok(prepared.into()),
        Ok(Err(e)) => {
            tracing::error!(error = %e, "linking a message's images failed");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
        Err(e) => {
            tracing::error!(error = %e, "a message's image thread failed");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// The import of an adopting create, or `None` for a conversation id
/// `handle_with` refuses (a pure syntax check, so it refuses it too). The
/// history read
/// and its blob writes run on a blocking thread; past the deadline the
/// reply goes on without them as `Failed("timed out")`, and whatever the
/// abandoned thread still writes is removed when it ends. The workdir's
/// canonicalisation before it shares the deadline; past it the request is a
/// 503 and the command is not handled: `handle_with` would canonicalise
/// again, unbounded and holding the API's connection, so the device resends
/// it instead.
async fn prepare_import(
    api: &Arc<Api>,
    command: &Command,
) -> std::result::Result<Option<Prepared>, StatusCode> {
    let CommandKind::CreateSession {
        workdir,
        conversation: Some(conversation),
    } = &command.kind
    else {
        return Ok(None);
    };
    let Some(conversation) = check_conversation_id(conversation) else {
        return Ok(None);
    };
    // Refused `DiskLow` before it writes blobs. Should the disk have room
    // again by the transaction, the session is created with nothing
    // imported (`Unavailable`), as for any create with nothing prepared.
    if api.log.disk().refresh() {
        return Ok(None);
    }
    let conversation = conversation.to_owned();
    let deadline = Instant::now() + api.import_deadline;
    let (roots, workdir) = (api.log.roots().to_vec(), workdir.clone());
    // A refused workdir imports nothing: `handle_with` canonicalises again
    // and refuses the command, or, should it accept the workdir now, logs
    // `Failed("workdir changed")`.
    let canonical =
        tokio::task::spawn_blocking(move || dirs::canonical_under_roots(&roots, &workdir));
    let workdir = match tokio::time::timeout_at(deadline.into(), canonical).await {
        Ok(Ok(Ok(workdir))) => workdir,
        Ok(Ok(Err(_refused))) => return Ok(Some(Prepared::workdir_refused(api.blobs.clone()))),
        Ok(Err(e)) => {
            tracing::error!(error = %e, "canonicalising a workdir failed");
            return Ok(Some(Prepared::workdir_refused(api.blobs.clone())));
        }
        Err(_) => {
            tracing::warn!(command = %command.id.0, "canonicalising a workdir timed out");
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        }
    };
    let (source, blobs, read_for) = (api.outside.clone(), api.blobs.clone(), workdir.clone());
    let read = tokio::task::spawn_blocking(move || {
        import::prepare(&*source, blobs, &read_for, &conversation, deadline)
    });
    Ok(Some(
        match tokio::time::timeout_at(deadline.into(), read).await {
            Ok(Ok(prepared)) => prepared,
            Ok(Err(e)) => {
                tracing::error!(error = %e, "preparing an import failed");
                let outcome = ImportOutcome::Failed {
                    reason: "could not read the history".to_owned(),
                };
                Prepared::without_items(api.blobs.clone(), &workdir, outcome)
            }
            Err(_) => {
                tracing::warn!(command = %command.id.0, "importing a conversation timed out");
                Prepared::timed_out(api.blobs.clone(), &workdir)
            }
        },
    ))
}

async fn events(
    State(api): State<Arc<Api>>,
    query: std::result::Result<Query<EventsQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Response {
    let Ok(Query(query)) = query else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let compress = encoding::wants_zstd(&headers);
    let after = query.after;
    if after > MAX_SEQ {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let limit = query.limit.unwrap_or(DEFAULT_BATCH_LIMIT);
    let wait = Duration::from_secs(query.wait.unwrap_or(0).into()).min(MAX_WAIT);
    if api.log.head() <= after && !wait.is_zero() {
        let mut shutdown = api.shutdown.clone();
        tokio::select! {
            _ = api.log.wait_after(after, wait) => {}
            // Also fires if the server is gone, which is just as final.
            _ = shutdown.wait_for(|stop| *stop) => {}
        }
    }
    match api
        .blocking(move |api, db| api.log.read(db, after, limit))
        .await
    {
        // Encoded off the API's connection, which the read alone holds.
        Ok(batch) => encoding::json(batch, compress).await,
        Err(status) => status.into_response(),
    }
}

async fn list_dirs(
    State(api): State<Arc<Api>>,
    query: std::result::Result<Query<DirsQuery>, QueryRejection>,
) -> Response {
    let Ok(Query(query)) = query else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    // Not `Api::blocking`: a slow file system must not hold the database.
    let permit = match api.fs_permit().await {
        Ok(permit) => permit,
        Err(status) => return status.into_response(),
    };
    let roots = api.log.roots().to_vec();
    let listing = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        dirs::list_dirs(&roots, &query.dir)
    });
    match tokio::time::timeout(DIRS_TIMEOUT, listing).await {
        Ok(Ok(result)) => Json(result).into_response(),
        Ok(Err(e)) => {
            tracing::error!(error = %e, "directory listing failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
        Err(_) => {
            tracing::warn!("directory listing timed out");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

/// The source's own deadline for a listing given `timeout`, measured from
/// the request's start: four fifths of it, which leaves the rest for the
/// own-ids read and the reply.
fn listing_deadline(timeout: Duration) -> Duration {
    timeout * 4 / 5
}

async fn list_conversations(State(api): State<Arc<Api>>) -> Response {
    let timeout = api.conversations_timeout;
    let deadline = Instant::now() + listing_deadline(timeout);
    let listing = async {
        let permit = api.fs_permit().await?;
        let own = api.blocking(|api, db| api.outside.own(db)).await?;
        // Not `Api::blocking`: a slow file system must not hold the database.
        let outside = api.outside.clone();
        let roots = api.log.roots().to_vec();
        let listed = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            outside.list(&roots, &own, limits::CONVERSATIONS_MAX, deadline)
        })
        .await;
        match listed {
            Ok(Ok(listed)) => Ok(ConversationsResult::Listed {
                conversations: listed.conversations.into_iter().map(to_wire).collect(),
                truncated: listed.truncated,
            }),
            Ok(Err(SourceError::Unavailable)) => Ok(ConversationsResult::Unavailable),
            Ok(Err(e @ SourceError::Failed(_))) => {
                tracing::error!(error = %e, "conversations listing failed");
                Err(StatusCode::INTERNAL_SERVER_ERROR)
            }
            Err(e) => {
                tracing::error!(error = %e, "conversations listing task failed");
                Err(StatusCode::INTERNAL_SERVER_ERROR)
            }
        }
    };
    match tokio::time::timeout(timeout, listing).await {
        Ok(Ok(result)) => Json(result).into_response(),
        Ok(Err(status)) => status.into_response(),
        Err(_) => {
            tracing::warn!("conversations listing timed out");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

/// `conversation` as the wire carries it, title and first prompt cut to
/// one line of their limits.
fn to_wire(conversation: agent::OutsideConversation) -> skywave_proto::OutsideConversation {
    let cut = |text: Option<String>, max| text.map(|text| cut_line(&text, max).0);
    skywave_proto::OutsideConversation {
        id: conversation.id,
        workdir: conversation.workdir,
        title: cut(conversation.title, limits::TITLE_MAX_CHARS),
        first_prompt: cut(conversation.first_prompt, limits::FIRST_PROMPT_MAX_CHARS),
        last_active: conversation.last_active,
        messages: conversation.messages,
        session: conversation.session,
    }
}

async fn usage_limits(State(api): State<Arc<Api>>) -> Response {
    let read = api
        .usage_limits
        .read(Instant::now() + USAGE_LIMITS_DEADLINE);
    let result = match tokio::time::timeout(USAGE_LIMITS_TIMEOUT, read).await {
        Ok(Ok(limits)) => UsageLimitsResult::Read {
            at: Utc::now(),
            limits: limits
                .into_iter()
                .map(|limit| UsageLimit {
                    scope: limit
                        .scope
                        .map(|scope| cut_line(&scope, limits::TITLE_MAX_CHARS).0),
                    ..limit
                })
                .collect(),
        },
        Ok(Err(LimitsError::Unavailable)) => UsageLimitsResult::Unavailable,
        Ok(Err(LimitsError::Failed(reason))) => {
            tracing::warn!(%reason, "usage limits read failed");
            UsageLimitsResult::Failed {
                reason: cut_line(&reason, limits::REFUSAL_MAX_CHARS).0,
            }
        }
        Err(_) => {
            tracing::warn!("usage limits read timed out");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };
    Json(result).into_response()
}

/// Starts logging the agent in, or joins the login starting or open.
async fn agent_login_start(State(api): State<Arc<Api>>) -> Response {
    let start = api.login_flows.start(Instant::now() + LOGIN_START_DEADLINE);
    let result = match tokio::time::timeout(LOGIN_START_TIMEOUT, start).await {
        Ok(LoginStartResult::Failed { reason }) => {
            tracing::warn!(%reason, "login start failed");
            LoginStartResult::Failed {
                reason: cut_line(&reason, limits::REFUSAL_MAX_CHARS).0,
            }
        }
        Ok(result) => result,
        Err(_) => {
            tracing::warn!(reason = "timed out", "login start failed");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };
    Json(result).into_response()
}

/// Finishes the login a start opened with the pasted code, or answers what
/// its first finish came to.
async fn agent_login_finish(
    State(api): State<Arc<Api>>,
    body: std::result::Result<Json<LoginFinish>, JsonRejection>,
) -> Response {
    let finish = match body {
        Ok(Json(finish)) => finish,
        Err(rejection) if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            return StatusCode::PAYLOAD_TOO_LARGE.into_response();
        }
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    if finish.code.len() > limits::LOGIN_CODE_MAX_BYTES {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let finishing = api.login_flows.finish(finish.flow, finish.code);
    let result = match tokio::time::timeout(api.login_finish_timeout, finishing).await {
        Ok(LoginFinishResult::Failed { reason }) => {
            tracing::warn!(%reason, "login finish failed");
            LoginFinishResult::Failed {
                reason: cut_line(&reason, limits::REFUSAL_MAX_CHARS).0,
            }
        }
        Ok(result) => result,
        // The finish goes on; a retry naming the flow gets its outcome.
        Err(_) => {
            tracing::warn!(reason = "timed out", "login finish failed");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };
    Json(result).into_response()
}

/// The storage report (module `storage`) on a blocking thread: a 200, a
/// passed deadline, [`STORAGE_TIMEOUT`] or no file-system permit a 503, any
/// other error a 500.
async fn storage_report(State(api): State<Arc<Api>>) -> Response {
    let deadline = Instant::now() + storage::REPORT_DEADLINE;
    let permit = match api.fs_permit().await {
        Ok(permit) => permit,
        Err(status) => return status.into_response(),
    };
    let api = api.clone();
    let work = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        api.storage.report(api.log.disk(), deadline)
    });
    match tokio::time::timeout(STORAGE_TIMEOUT, work).await {
        Ok(Ok(Ok(report))) => Json(report).into_response(),
        Ok(Ok(Err(Error::StorageDeadline))) => {
            tracing::warn!("storage report passed its deadline");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
        Ok(Ok(Err(e))) => {
            tracing::error!(error = %e, "storage report failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
        Ok(Err(e)) => {
            tracing::error!(error = %e, "storage report task failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
        Err(_) => {
            tracing::warn!("storage report timed out");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

async fn workdir_changes(
    State(api): State<Arc<Api>>,
    query: std::result::Result<Query<WorkdirQuery>, QueryRejection>,
) -> Response {
    let deadline = Instant::now() + workdir::SNAPSHOT_DEADLINE;
    let Ok(Query(query)) = query else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    workdir_snapshot(&api, deadline, move |s| s.changes(&query)).await
}

async fn workdir_log(
    State(api): State<Arc<Api>>,
    query: std::result::Result<Query<WorkdirQuery>, QueryRejection>,
) -> Response {
    let deadline = Instant::now() + workdir::SNAPSHOT_DEADLINE;
    let Ok(Query(query)) = query else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    workdir_snapshot(&api, deadline, move |s| s.log(&query)).await
}

async fn workdir_list(
    State(api): State<Arc<Api>>,
    query: std::result::Result<Query<WorkdirQuery>, QueryRejection>,
) -> Response {
    let deadline = Instant::now() + workdir::SNAPSHOT_DEADLINE;
    let Ok(Query(query)) = query else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    workdir_snapshot(&api, deadline, move |s| s.list(&query)).await
}

async fn workdir_file(
    State(api): State<Arc<Api>>,
    query: std::result::Result<Query<WorkdirQuery>, QueryRejection>,
) -> Response {
    let deadline = Instant::now() + workdir::SNAPSHOT_DEADLINE;
    let Ok(Query(query)) = query else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    workdir_snapshot(&api, deadline, move |s| s.file(&query)).await
}

/// Runs `f` as one workdir snapshot on a blocking thread (module docs,
/// "Workdir"): its answer is a 200, a passed deadline, [`WORKDIR_TIMEOUT`]
/// or no file-system permit a 503, any other error a 500.
async fn workdir_snapshot<T: serde::Serialize + Send + 'static>(
    api: &Arc<Api>,
    deadline: Instant,
    f: impl FnOnce(&mut Snapshot<'_>) -> Result<T> + Send + 'static,
) -> Response {
    let permit = match api.fs_permit().await {
        Ok(permit) => permit,
        Err(status) => return status.into_response(),
    };
    let api = api.clone();
    let work = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        api.workdir.snapshot(deadline, f)
    });
    match tokio::time::timeout(WORKDIR_TIMEOUT, work).await {
        Ok(Ok(Ok(answer))) => Json(answer).into_response(),
        Ok(Ok(Err(Error::WorkdirDeadline))) => {
            tracing::warn!("workdir snapshot passed its deadline");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
        Ok(Ok(Err(e))) => {
            tracing::error!(error = %e, "workdir read failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
        Ok(Err(e)) => {
            tracing::error!(error = %e, "workdir task failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
        Err(_) => {
            tracing::warn!("workdir read timed out");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

/// The hash a `/v1/attachments/{sha256}` path names; a path that is not one
/// is a 400.
fn attachment_hash(
    sha256: std::result::Result<Path<String>, PathRejection>,
) -> std::result::Result<ContentHash, StatusCode> {
    sha256
        .ok()
        .and_then(|Path(text)| text.parse().ok())
        .ok_or(StatusCode::BAD_REQUEST)
}

/// `HEAD /v1/attachments/{sha256}`: 204 if that image is staged, else 404.
async fn attachment_staged(
    State(api): State<Arc<Api>>,
    sha256: std::result::Result<Path<String>, PathRejection>,
) -> Response {
    let hash = match attachment_hash(sha256) {
        Ok(hash) => hash,
        Err(status) => return status.into_response(),
    };
    let staging = api.staging.clone();
    match tokio::task::spawn_blocking(move || staging.contains(hash)).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!(error = %e, "attachment lookup task failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `PUT /v1/attachments/{sha256}`: stages the body's image and answers an
/// [`UploadResult`] with 200. The body is read in memory, and the answer is
/// `TooLarge` as soon as it passes [`limits::ATTACHMENT_MAX_BYTES`], without
/// reading the rest. A low disk is `DiskLow`, checked before anything is
/// written.
async fn stage_attachment(
    State(api): State<Arc<Api>>,
    sha256: std::result::Result<Path<String>, PathRejection>,
    body: Body,
) -> Response {
    let hash = match attachment_hash(sha256) {
        Ok(hash) => hash,
        Err(status) => return status.into_response(),
    };
    let mut bytes = Vec::new();
    let mut chunks = body.into_data_stream();
    while let Some(chunk) = chunks.next().await {
        let Ok(chunk) = chunk else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        if (bytes.len() + chunk.len()) as u64 > limits::ATTACHMENT_MAX_BYTES {
            return Json(UploadResult::TooLarge).into_response();
        }
        bytes.extend_from_slice(&chunk);
    }
    if api.log.disk().refresh() {
        return Json(UploadResult::DiskLow).into_response();
    }
    let staging = api.staging.clone();
    match tokio::task::spawn_blocking(move || staging.stage(hash, &bytes)).await {
        Ok(Ok(result)) => Json(result).into_response(),
        Ok(Err(e)) => {
            tracing::error!(error = %e, "staging an attachment failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "attachment staging task failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn blob(
    State(api): State<Arc<Api>>,
    id: std::result::Result<Path<BlobId>, PathRejection>,
    headers: HeaderMap,
) -> Response {
    let Ok(Path(id)) = id else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let file = match api
        .blocking(move |api, db| api.blobs.open(db.conn(), id))
        .await
    {
        Ok(Some(file)) => file,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(status) => return status.into_response(),
    };
    let size = file.meta.size;
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map_or(ByteRange::Full, |v| ByteRange::parse(v, size));
    let (status, start, len) = match range {
        ByteRange::Full => (StatusCode::OK, 0, size),
        ByteRange::Part { first, last } => (StatusCode::PARTIAL_CONTENT, first, last - first + 1),
        ByteRange::Unsatisfiable => {
            let mut resp = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
            let h = resp.headers_mut();
            h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
            h.insert(
                header::CONTENT_RANGE,
                header_value(format!("bytes */{size}")),
            );
            return resp;
        }
    };

    let mut file = file.into_file();
    // A seek moves the file offset and reads nothing, so it does not block.
    if let Err(e) = file.seek(SeekFrom::Start(start)) {
        tracing::error!(blob = %id.0, error = %e, "seeking blob");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let compress = encoding::wants_zstd(&headers) && encoding::worth_compressing(len);
    let body = if compress {
        encoding::blob_frame(file, len)
    } else {
        let file = tokio::fs::File::from_std(file);
        let chunks = futures_util::stream::try_unfold(file.take(len), |mut reader| async move {
            let mut buf = BytesMut::with_capacity(encoding::BLOB_CHUNK);
            let n = reader.read_buf(&mut buf).await?;
            Ok::<_, std::io::Error>((n > 0).then(|| (buf.freeze(), reader)))
        });
        Body::from_stream(chunks)
    };

    let mut resp = Response::new(body);
    *resp.status_mut() = status;
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    if compress {
        // The frame's length is known only once it is made: chunked.
        h.insert(
            header::CONTENT_ENCODING,
            HeaderValue::from_static(skywave_proto::encoding::ZSTD),
        );
        h.insert(
            header::HeaderName::from_static(skywave_proto::encoding::DECODED_LENGTH_HEADER),
            HeaderValue::from(len),
        );
    } else {
        h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    }
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if status == StatusCode::PARTIAL_CONTENT {
        let last = start + len - 1;
        h.insert(
            header::CONTENT_RANGE,
            header_value(format!("bytes {start}-{last}/{size}")),
        );
    }
    resp
}

fn header_value(text: String) -> HeaderValue {
    HeaderValue::try_from(text).expect("ASCII digits and punctuation")
}

/// What a `Range` header asks of a blob of known size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ByteRange {
    /// No usable range: send everything.
    Full,
    /// Bytes `first..=last`, both within the blob.
    Part { first: u64, last: u64 },
    /// A valid range that selects nothing (416).
    Unsatisfiable,
}

impl ByteRange {
    fn parse(header: &str, size: u64) -> ByteRange {
        let Some(spec) = header.trim().strip_prefix("bytes=") else {
            return ByteRange::Full;
        };
        if spec.contains(',') {
            return ByteRange::Full;
        }
        let Some((first, last)) = spec.split_once('-') else {
            return ByteRange::Full;
        };
        let (first, last) = (first.trim(), last.trim());
        if first.is_empty() {
            // `-n`: the last n bytes.
            return match number(last) {
                None => ByteRange::Full,
                Some(0) => ByteRange::Unsatisfiable,
                Some(_) if size == 0 => ByteRange::Unsatisfiable,
                Some(n) => ByteRange::Part {
                    first: size - n.min(size),
                    last: size - 1,
                },
            };
        }
        let Some(first) = number(first) else {
            return ByteRange::Full;
        };
        let last = if last.is_empty() {
            None
        } else {
            match number(last) {
                Some(last) if last >= first => Some(last),
                _ => return ByteRange::Full,
            }
        };
        if first >= size {
            return ByteRange::Unsatisfiable;
        }
        ByteRange::Part {
            first,
            last: last.map_or(size - 1, |last| last.min(size - 1)),
        }
    }
}

/// A non-empty run of ASCII digits.
fn number(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::ByteRange::{self, Full, Part, Unsatisfiable};

    #[test]
    fn ranges() {
        let cases: &[(&str, u64, ByteRange)] = &[
            ("bytes=0-9", 100, Part { first: 0, last: 9 }),
            (
                "bytes=90-",
                100,
                Part {
                    first: 90,
                    last: 99,
                },
            ),
            (
                "bytes=90-500",
                100,
                Part {
                    first: 90,
                    last: 99,
                },
            ),
            (
                "bytes=-10",
                100,
                Part {
                    first: 90,
                    last: 99,
                },
            ),
            ("bytes=-500", 100, Part { first: 0, last: 99 }),
            ("bytes=100-", 100, Unsatisfiable),
            ("bytes=-0", 100, Unsatisfiable),
            ("bytes=0-", 0, Unsatisfiable),
            ("bytes=-5", 0, Unsatisfiable),
            ("bytes=5-1", 100, Full),
            ("bytes=0-1,5-6", 100, Full),
            ("items=0-1", 100, Full),
            ("bytes=+1-2", 100, Full),
            ("bytes=x", 100, Full),
            ("bytes=-", 100, Full),
        ];
        for (header, size, want) in cases {
            assert_eq!(ByteRange::parse(header, *size), *want, "{header} of {size}");
        }
    }
}
