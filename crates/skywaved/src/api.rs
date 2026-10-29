//! The authenticated part of the HTTPS API (spec §4): hello, commands,
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
//! Errors are bare status codes: 400 malformed request, 404 unknown blob,
//! 500 server fault.

use std::io::SeekFrom;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
    BlobId, Command, DeviceId, EventsQuery, HelloResponse, PROTOCOL_VERSION, ServerId, path,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::watch;

use crate::EventLog;
use crate::blobs::BlobStore;
use crate::db::Db;
use crate::devices;
use crate::error::Result;
use crate::event_log::DEFAULT_BATCH_LIMIT;
use crate::identity::Fingerprint;

/// Longest a `GET /v1/events` waits, whatever `wait` asks for.
pub const MAX_WAIT: Duration = Duration::from_secs(60);
/// `last_seen` is written at most this often per device.
pub const TOUCH_INTERVAL: chrono::Duration = chrono::Duration::seconds(60);
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
    log: EventLog,
    blobs: BlobStore,
    server_id: ServerId,
    /// Becomes `true` when the server shuts down, ending long-poll waits.
    shutdown: watch::Receiver<bool>,
}

impl Api {
    pub(crate) fn new(
        db: Db,
        log: EventLog,
        blobs: BlobStore,
        server_id: ServerId,
        shutdown: watch::Receiver<bool>,
    ) -> Api {
        Api {
            db: Mutex::new(db),
            log,
            blobs,
            server_id,
            shutdown,
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
    Json(HelloResponse {
        protocol_version: PROTOCOL_VERSION,
        server_id: api.server_id,
        device_id: device,
        head: api.log.head(),
    })
    .into_response()
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
    // Accepted and Rejected are both 200: a rejection is a domain answer.
    match api
        .blocking(move |api, db| api.log.handle(db, device, &command))
        .await
    {
        Ok(result) => Json(result).into_response(),
        Err(status) => status.into_response(),
    }
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
