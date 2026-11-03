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
//! **Dirs** (`GET /v1/dirs?dir=`, workdir completion): every outcome is a
//! 200 [`DirsResult`](skywave_proto::DirsResult), since 401/403 would read
//! as a revoked device. The listing ([`crate::dirs`]) runs on a blocking
//! thread outside the API's database connection; one that takes longer than
//! [`DIRS_TIMEOUT`] gets a 503, and the scan, bounded by its cap, finishes
//! unobserved.
//!
//! **Workdir** (`GET /v1/workdir/changes`, `/v1/workdir/list` and
//! `/v1/workdir/file`, D36): reads of a session's workdir, every outcome a
//! 200 [`ChangesResult`](skywave_proto::ChangesResult),
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
//! **Adopting creates** (`CreateSession` with a `conversation`, D37): the
//! conversation's history is read and bounded first, on a blocking thread
//! and off the API's connection, then handed to
//! [`EventLog::handle_with`]. A command already handled is answered from its
//! stored result without reading anything; a resend that arrives while the
//! first request is still preparing waits for that request's reply. The
//! import gets the server's import deadline
//! ([`import::IMPORT_DEADLINE`]), after which the reply goes on with
//! `Failed("timed out")`.
//!
//! Errors are bare status codes: 400 malformed request, 404 unknown blob,
//! 500 server fault.

use std::collections::HashMap;
use std::io::SeekFrom;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use bytes::BytesMut;
use chrono::Utc;
use rustls_pki_types::CertificateDer;
use skywave_proto::{
    BlobId, Command, CommandId, CommandKind, CommandResult, DeviceId, DeviceInfo, DirsQuery,
    EventsQuery, Fingerprint, HelloResponse, ImportOutcome, PROTOCOL_VERSION, ServerId,
    WorkdirQuery, check_conversation_id, path,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::watch;

use crate::EventLog;
use crate::agent::OutsideConversations;
use crate::agent::import::{self, Prepared};
use crate::blobs::BlobStore;
use crate::db::Db;
use crate::devices;
use crate::dirs;
use crate::error::{Error, Result};
use crate::event_log::DEFAULT_BATCH_LIMIT;
use crate::server::ServerOptions;
use crate::workdir::{self, Snapshot, WorkdirReader};

/// Longest a `GET /v1/events` waits, whatever `wait` asks for.
pub const MAX_WAIT: Duration = Duration::from_secs(60);
/// `last_seen` is written at most this often per device.
pub const TOUCH_INTERVAL: chrono::Duration = chrono::Duration::seconds(60);
/// Longest a `GET /v1/dirs` listing may take before the reply gives up.
pub const DIRS_TIMEOUT: Duration = Duration::from_secs(2);
/// Longest a workdir request may take before the reply gives up with a 503.
/// Longer than [`workdir::SNAPSHOT_DEADLINE`], which stops the snapshot
/// itself; this only stops waiting for it.
pub const WORKDIR_TIMEOUT: Duration = Duration::from_secs(20);
/// Read size when streaming a blob.
const BLOB_CHUNK: usize = 64 * 1024;

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
    log: EventLog,
    blobs: BlobStore,
    server_id: ServerId,
    /// Becomes `true` when the server shuts down, ending long-poll waits.
    shutdown: watch::Receiver<bool>,
    /// What an adopting `CreateSession` imports from.
    outside: Arc<dyn OutsideConversations>,
    import_deadline: Duration,
    /// The adopting creates whose import is being prepared, by command id:
    /// each answers with the command's reply once it has one.
    importing: Mutex<HashMap<CommandId, watch::Receiver<Option<Reply>>>>,
}

/// A command's reply: its result, or the status of a request that failed.
type Reply = std::result::Result<CommandResult, StatusCode>;

impl Api {
    /// `db` and `workdir_db` are two connections to the same database.
    pub(crate) fn new(
        db: Db,
        workdir_db: Db,
        log: EventLog,
        blobs: BlobStore,
        server_id: ServerId,
        shutdown: watch::Receiver<bool>,
        options: ServerOptions,
    ) -> Api {
        Api {
            db: Mutex::new(db),
            workdir: WorkdirReader::new(workdir_db, blobs.clone(), log.roots().to_vec())
                .with_git(options.git),
            log,
            blobs,
            server_id,
            shutdown,
            outside: options.outside,
            import_deadline: options.import_deadline,
            importing: Mutex::new(HashMap::new()),
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
            let mut db = api.db.lock().expect("api db lock poisoned");
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
        .route(path::WORKDIR_CHANGES, get(workdir_changes))
        .route(path::WORKDIR_LIST, get(workdir_list))
        .route(path::WORKDIR_FILE, get(workdir_file))
        .route(path::BLOB_ROUTE, get(blob))
        .route_layer(middleware::from_fn_with_state(api.clone(), authenticate))
        .with_state(api)
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
) -> Response {
    let devices = api
        .blocking(|_, db| {
            Ok(devices::list(db.conn())?
                .into_iter()
                .map(|d| DeviceInfo {
                    id: d.id,
                    name: d.name,
                })
                .collect())
        })
        .await;
    match devices {
        Ok(devices) => Json(HelloResponse {
            protocol_version: PROTOCOL_VERSION,
            server_id: api.server_id,
            device_id: device,
            head: api.log.head(),
            devices,
            shell: api.log.shell_enabled(),
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
    let adopting = matches!(
        command.kind,
        CommandKind::CreateSession {
            conversation: Some(_),
            ..
        }
    );
    let reply = if adopting {
        adopt(&api, device, command).await
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

/// Handles an adopting `CreateSession`, whose history is read and bounded
/// before its transaction, outside the API's connection: a slow read must
/// not hold up every other device's commands. A resend that arrives while
/// the import is being prepared waits for that one's reply, so a command
/// is never imported twice.
async fn adopt(api: &Arc<Api>, device: DeviceId, command: Command) -> Reply {
    loop {
        let role = {
            let mut importing = lock(&api.importing);
            match importing.get(&command.id) {
                Some(reply) => Err(reply.clone()),
                None => {
                    let (reply, waiting) = watch::channel(None);
                    importing.insert(command.id, waiting);
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
                if let Ok(reply) = waiting.wait_for(Option::is_some).await {
                    return reply.clone().expect("waited for a reply");
                }
                // The first request was dropped before it had a reply:
                // whatever it did is stored or undone, so start over.
            }
        }
    }
}

/// The request preparing a command's import; dropped, its entry goes.
struct Leading<'a> {
    api: &'a Api,
    id: CommandId,
    reply: watch::Sender<Option<Reply>>,
}

impl Drop for Leading<'_> {
    fn drop(&mut self) {
        lock(&self.api.importing).remove(&self.id);
    }
}

async fn prepare_and_handle(api: &Arc<Api>, device: DeviceId, command: Command) -> Reply {
    // A command already handled is answered from the log: no second import.
    let id = command.id;
    if let Some(stored) = api
        .blocking(move |api, db| api.log.command_result(db, id))
        .await?
    {
        return Ok(stored);
    }
    let prepared = prepare_import(api, &command).await;
    api.blocking(move |api, db| api.log.handle_with(db, device, &command, prepared))
        .await
}

/// The import of an adopting create, or `None` for a conversation id
/// `handle_with` refuses (a pure syntax check, so it refuses it too). The
/// history read
/// and its blob writes run on a blocking thread; past the deadline the
/// reply goes on without them as `Failed("timed out")`, and whatever the
/// abandoned thread still writes is removed when it ends.
async fn prepare_import(api: &Arc<Api>, command: &Command) -> Option<Prepared> {
    let CommandKind::CreateSession {
        workdir,
        conversation: Some(conversation),
    } = &command.kind
    else {
        return None;
    };
    let conversation = check_conversation_id(conversation)?.to_owned();
    let deadline = Instant::now() + api.import_deadline;
    let (roots, workdir) = (api.log.roots().to_vec(), workdir.clone());
    // A refused workdir imports nothing: `handle_with` canonicalises again
    // and refuses the command, or, should it accept the workdir now, logs
    // `Failed("workdir changed")`.
    let canonical =
        tokio::task::spawn_blocking(move || dirs::canonical_under_roots(&roots, &workdir));
    let workdir = match canonical.await {
        Ok(Ok(workdir)) => workdir,
        Ok(Err(_refused)) => return Some(Prepared::workdir_refused(api.blobs.clone())),
        Err(e) => {
            tracing::error!(error = %e, "canonicalising a workdir failed");
            return Some(Prepared::workdir_refused(api.blobs.clone()));
        }
    };
    let (source, blobs, read_for) = (api.outside.clone(), api.blobs.clone(), workdir.clone());
    let read = tokio::task::spawn_blocking(move || {
        import::prepare(&*source, blobs, &read_for, &conversation, deadline)
    });
    match tokio::time::timeout_at(deadline.into(), read).await {
        Ok(Ok(prepared)) => Some(prepared),
        Ok(Err(e)) => {
            tracing::error!(error = %e, "preparing an import failed");
            let outcome = ImportOutcome::Failed {
                reason: "could not read the history".to_owned(),
            };
            Some(Prepared::without_items(
                api.blobs.clone(),
                &workdir,
                outcome,
            ))
        }
        Err(_) => {
            tracing::warn!(command = %command.id.0, "importing a conversation timed out");
            Some(Prepared::timed_out(api.blobs.clone(), &workdir))
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().expect("api lock poisoned")
}

async fn events(
    State(api): State<Arc<Api>>,
    query: std::result::Result<Query<EventsQuery>, QueryRejection>,
) -> Response {
    let Ok(Query(query)) = query else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let after = query.after;
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
        Ok(batch) => Json(batch).into_response(),
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
    let roots = api.log.roots().to_vec();
    let listing = tokio::task::spawn_blocking(move || dirs::list_dirs(&roots, &query.dir));
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
/// "Workdir"): its answer is a 200, a passed deadline or [`WORKDIR_TIMEOUT`]
/// a 503, any other error a 500.
async fn workdir_snapshot<T: serde::Serialize + Send + 'static>(
    api: &Arc<Api>,
    deadline: Instant,
    f: impl FnOnce(&mut Snapshot<'_>) -> Result<T> + Send + 'static,
) -> Response {
    let api = api.clone();
    let work = tokio::task::spawn_blocking(move || api.workdir.snapshot(deadline, f));
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

    let mut file = tokio::fs::File::from_std(file.into_file());
    if let Err(e) = file.seek(SeekFrom::Start(start)).await {
        tracing::error!(blob = %id.0, error = %e, "seeking blob");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let chunks = futures_util::stream::try_unfold(file.take(len), |mut reader| async move {
        let mut buf = BytesMut::with_capacity(BLOB_CHUNK);
        let n = reader.read_buf(&mut buf).await?;
        Ok::<_, std::io::Error>((n > 0).then(|| (buf.freeze(), reader)))
    });

    let mut resp = Response::new(Body::from_stream(chunks));
    *resp.status_mut() = status;
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
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
