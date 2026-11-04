//! [`HttpsTransport`]: the v1 API over HTTP/1.1 + rustls (D15), plus the
//! small connection pool pairing shares.
//!
//! Each request takes an idle connection or opens one, so a long-poll and a
//! command run side by side on separate connections. A request that fails
//! on a reused connection before any response (the server closed it while
//! idle) is retried once on a fresh one; every v1 request is safe to repeat.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::client::conn::http1::{self, SendRequest};
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::{Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use rustls::ClientConfig;
use serde::de::DeserializeOwned;
use skywave_proto::{
    BlobId, ChangesResult, Command, CommandResult, ConversationsResult, DirsResult, EventBatch,
    EventsQuery, FileResult, HelloResponse, ListResult, PROTOCOL_VERSION, RUN_HEADER, RunId,
    WorkdirQuery, path,
};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::address::ServerAddress;
use crate::identity::Identity;
use crate::tls;
use crate::transport::{BlobBody, BlobDownload, BoxFuture, Transport, TransportError};
use crate::workdir::WORKDIR_TIMEOUT;

/// Timeouts and pool size of an [`HttpsTransport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportConfig {
    /// TCP connect plus TLS handshake.
    pub connect_timeout: Duration,
    /// A whole request; for blobs and event batches, the response head and
    /// then each body chunk, so a slow link that keeps delivering bytes
    /// never times out. An events long-poll's head gets its `wait` on top.
    pub request_timeout: Duration,
    /// Idle connections kept for reuse.
    pub max_idle: usize,
}

impl Default for TransportConfig {
    fn default() -> Self {
        TransportConfig {
            connect_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(30),
            max_idle: 4,
        }
    }
}

type Sender = SendRequest<Full<Bytes>>;

/// HTTPS to one server with one TLS config.
pub(crate) struct Http {
    address: ServerAddress,
    authority: HeaderValue,
    connector: TlsConnector,
    config: TransportConfig,
    idle: Mutex<Vec<Sender>>,
}

/// A response whose body has not been read yet.
struct Head {
    status: StatusCode,
    headers: HeaderMap,
    body: Incoming,
    sender: Sender,
}

/// A response read in full.
pub(crate) struct Reply {
    pub(crate) status: StatusCode,
    pub(crate) headers: HeaderMap,
    pub(crate) body: Bytes,
}

impl Reply {
    pub(crate) fn json<T: DeserializeOwned>(&self) -> Result<T, TransportError> {
        serde_json::from_slice(&self.body).map_err(|e| TransportError::Decode(e.to_string()))
    }
}

impl Http {
    pub(crate) fn new(
        address: ServerAddress,
        tls: Arc<ClientConfig>,
        config: TransportConfig,
    ) -> Http {
        let authority =
            HeaderValue::from_str(&address.to_string()).expect("an address is a valid header");
        Http {
            address,
            authority,
            connector: TlsConnector::from(tls),
            config,
            idle: Mutex::new(Vec::new()),
        }
    }

    async fn connect(&self) -> Result<Sender, TransportError> {
        let open = async {
            let tcp = TcpStream::connect((self.address.host(), self.address.port()))
                .await
                .map_err(|e| TransportError::Unreachable(e.to_string()))?;
            let _ = tcp.set_nodelay(true);
            let tls = self
                .connector
                .connect(self.address.server_name(), tcp)
                .await
                .map_err(|e| TransportError::Tls(e.to_string()))?;
            let (sender, conn) = http1::handshake(TokioIo::new(tls))
                .await
                .map_err(|e| TransportError::Io(e.to_string()))?;
            tokio::spawn(async move {
                // Ends when the server closes or every sender is dropped.
                let _ = conn.await;
            });
            Ok(sender)
        };
        tokio::time::timeout(self.config.connect_timeout, open)
            .await
            .map_err(|_| TransportError::Timeout)?
    }

    fn take_idle(&self) -> Option<Sender> {
        let mut idle = self.idle.lock().unwrap_or_else(|p| p.into_inner());
        while let Some(sender) = idle.pop() {
            if sender.is_ready() {
                return Some(sender);
            }
        }
        None
    }

    fn put_idle(&self, sender: Sender) {
        if !sender.is_ready() {
            return;
        }
        let mut idle = self.idle.lock().unwrap_or_else(|p| p.into_inner());
        if idle.len() < self.config.max_idle {
            idle.push(sender);
        }
    }

    /// Sends a request and waits for the response head.
    async fn head(
        &self,
        method: Method,
        uri: &str,
        headers: &[(header::HeaderName, HeaderValue)],
        body: Bytes,
    ) -> Result<Head, TransportError> {
        let build = || {
            let mut req = Request::builder()
                .method(method.clone())
                .uri(uri)
                .header(header::HOST, self.authority.clone());
            if !body.is_empty() {
                req = req.header(header::CONTENT_TYPE, "application/json");
            }
            for (name, value) in headers {
                req = req.header(name, value.clone());
            }
            req.body(Full::new(body.clone()))
                .map_err(|e| TransportError::Decode(format!("building request: {e}")))
        };
        if let Some(mut sender) = self.take_idle() {
            match sender.send_request(build()?).await {
                Ok(resp) => return Ok(into_head(resp, sender)),
                Err(e) if e.is_canceled() || e.is_closed() || e.is_incomplete_message() => {
                    // Stale pooled connection: try once on a fresh one.
                }
                Err(e) => return Err(TransportError::Io(e.to_string())),
            }
        }
        let mut sender = self.connect().await?;
        let resp = sender
            .send_request(build()?)
            .await
            .map_err(|e| TransportError::Io(e.to_string()))?;
        Ok(into_head(resp, sender))
    }

    /// Sends a request and reads the whole response within `timeout`.
    pub(crate) async fn request(
        &self,
        method: Method,
        uri: &str,
        headers: &[(header::HeaderName, HeaderValue)],
        body: Bytes,
        timeout: Duration,
    ) -> Result<Reply, TransportError> {
        let exchange = async {
            let head = self.head(method, uri, headers, body).await?;
            let body = head
                .body
                .collect()
                .await
                .map_err(|e| TransportError::Io(e.to_string()))?
                .to_bytes();
            self.put_idle(head.sender);
            Ok(Reply {
                status: head.status,
                headers: head.headers,
                body,
            })
        };
        tokio::time::timeout(timeout, exchange)
            .await
            .map_err(|_| TransportError::Timeout)?
    }

    /// Sends a request, waits up to `head_timeout` for the response head,
    /// then reads the body with each chunk due within the request timeout.
    /// A slow link that keeps delivering bytes never times out; one that
    /// stalls for a request timeout does.
    async fn request_chunked(
        &self,
        method: Method,
        uri: &str,
        headers: &[(header::HeaderName, HeaderValue)],
        head_timeout: Duration,
    ) -> Result<Reply, TransportError> {
        let head =
            tokio::time::timeout(head_timeout, self.head(method, uri, headers, Bytes::new()))
                .await
                .map_err(|_| TransportError::Timeout)??;
        let mut body = TimedBody {
            body: Some(head.body),
            timeout: self.request_timeout(),
        };
        let mut read = Vec::new();
        while let Some(chunk) = body.next_chunk().await? {
            read.extend_from_slice(&chunk);
        }
        self.put_idle(head.sender);
        Ok(Reply {
            status: head.status,
            headers: head.headers,
            body: read.into(),
        })
    }

    pub(crate) fn request_timeout(&self) -> Duration {
        self.config.request_timeout
    }
}

fn into_head(resp: hyper::Response<Incoming>, sender: Sender) -> Head {
    let (parts, body) = resp.into_parts();
    Head {
        status: parts.status,
        headers: parts.headers,
        body,
        sender,
    }
}

/// The error for a non-2xx status on the authenticated endpoints (architecture.md "Protocol").
fn status_error(status: StatusCode, headers: &HeaderMap) -> TransportError {
    match status {
        StatusCode::UNAUTHORIZED => TransportError::Unauthorized,
        StatusCode::FORBIDDEN => TransportError::Revoked,
        StatusCode::NOT_FOUND => TransportError::NotFound,
        StatusCode::PRECONDITION_FAILED => TransportError::RunChanged,
        StatusCode::RANGE_NOT_SATISFIABLE => TransportError::RangeNotSatisfiable {
            size: header_str(headers, header::CONTENT_RANGE)
                .and_then(|v| v.strip_prefix("bytes */"))
                .and_then(|n| n.trim().parse().ok()),
        },
        other => TransportError::BadStatus(other.as_u16()),
    }
}

fn header_str(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

fn ok_json<T: DeserializeOwned>(reply: Reply) -> Result<T, TransportError> {
    if reply.status != StatusCode::OK {
        return Err(status_error(reply.status, &reply.headers));
    }
    reply.json()
}

/// Just the version of a hello body: the one field every protocol
/// version's [`HelloResponse`] keeps, whatever else it adds or drops.
#[derive(serde::Deserialize)]
struct HelloVersion {
    protocol_version: u32,
}

/// The [`HelloResponse`] in `reply`, checking the protocol version before
/// decoding the rest: the shape of the rest is what versions change, so a
/// server of another version must be reported as a mismatch, not as an
/// unreadable body (a field this client requires and that server lacks).
fn hello_from(reply: Reply) -> Result<HelloResponse, TransportError> {
    if reply.status != StatusCode::OK {
        return Err(status_error(reply.status, &reply.headers));
    }
    let HelloVersion { protocol_version } = reply.json()?;
    if protocol_version != PROTOCOL_VERSION {
        return Err(TransportError::ProtocolMismatch {
            client: PROTOCOL_VERSION,
            server: protocol_version,
        });
    }
    reply.json()
}

/// The [`Transport`] of a paired device: mTLS with the device certificate,
/// trusting only the CA pinned at pairing. Cheap to share behind an `Arc`;
/// requests may run concurrently.
///
/// Once told the run of the server the replica is synced with
/// ([`Transport::expect_run`]), every request names it in
/// [`RUN_HEADER`], except a `hello` that asks about a run.
pub struct HttpsTransport {
    http: Http,
    /// The [`RUN_HEADER`] value, once expected.
    run: Mutex<Option<HeaderValue>>,
}

impl HttpsTransport {
    /// A transport to `address` (`host:port`, as stored at pairing) that
    /// authenticates with `identity`. Opens no connection yet.
    pub fn new(
        address: &str,
        identity: &dyn Identity,
        config: TransportConfig,
    ) -> Result<HttpsTransport, TransportError> {
        let address = ServerAddress::parse(address)
            .map_err(|e| TransportError::Unreachable(e.to_string()))?;
        let tls = tls::device_config(identity)?;
        Ok(HttpsTransport {
            http: Http::new(address, tls, config),
            run: Mutex::new(None),
        })
    }

    /// The headers every request but a `hello` asking about a run carries:
    /// the expected run, once there is one.
    fn run_header(&self) -> Vec<(header::HeaderName, HeaderValue)> {
        let run = self.run.lock().unwrap_or_else(|p| p.into_inner());
        run.iter()
            .map(|value| (header::HeaderName::from_static(RUN_HEADER), value.clone()))
            .collect()
    }

    async fn hello(&self, since: Option<RunId>) -> Result<HelloResponse, TransportError> {
        // Asking about a run is how the device learns the current one, so
        // that `hello` names none it expects.
        let (uri, headers) = match since {
            Some(run) => (format!("{}?since={}", path::HELLO, run.0), Vec::new()),
            None => (path::HELLO.to_owned(), self.run_header()),
        };
        let reply = self
            .http
            .request(
                Method::GET,
                &uri,
                &headers,
                Bytes::new(),
                self.http.request_timeout(),
            )
            .await?;
        hello_from(reply)
    }

    async fn send_command(&self, command: &Command) -> Result<CommandResult, TransportError> {
        let body = serde_json::to_vec(command)
            .map_err(|e| TransportError::Decode(format!("encoding command: {e}")))?;
        let reply = self
            .http
            .request(
                Method::POST,
                path::COMMANDS,
                &self.run_header(),
                body.into(),
                self.http.request_timeout(),
            )
            .await?;
        ok_json(reply)
    }

    async fn events(&self, query: EventsQuery) -> Result<EventBatch, TransportError> {
        let mut uri = format!("{}?after={}", path::EVENTS, query.after.0);
        if let Some(wait) = query.wait {
            uri.push_str(&format!("&wait={wait}"));
        }
        if let Some(limit) = query.limit {
            uri.push_str(&format!("&limit={limit}"));
        }
        // The server answers within `wait`; the body is bounded per chunk,
        // so a batch slower to download than any whole-exchange deadline
        // still lands instead of being re-pulled from the same cursor.
        let head_timeout =
            self.http.request_timeout() + Duration::from_secs(u64::from(query.wait.unwrap_or(0)));
        let reply = self
            .http
            .request_chunked(Method::GET, &uri, &self.run_header(), head_timeout)
            .await?;
        ok_json(reply)
    }

    async fn list_dirs(&self, dir: &str) -> Result<DirsResult, TransportError> {
        let uri = format!("{}?dir={}", path::DIRS, query_escape(dir));
        let reply = self
            .http
            .request(
                Method::GET,
                &uri,
                &self.run_header(),
                Bytes::new(),
                self.http.request_timeout(),
            )
            .await?;
        ok_json(reply)
    }

    /// `GET /v1/conversations`, which takes no query.
    async fn list_conversations(&self) -> Result<ConversationsResult, TransportError> {
        let reply = self
            .http
            .request(
                Method::GET,
                path::CONVERSATIONS,
                &self.run_header(),
                Bytes::new(),
                self.http.request_timeout(),
            )
            .await?;
        ok_json(reply)
    }

    /// `GET <endpoint>?session=<id>&path=<path>`, bounded by
    /// [`WORKDIR_TIMEOUT`] rather than the request timeout: the server may
    /// take its full 20 s.
    async fn workdir<T: DeserializeOwned>(
        &self,
        endpoint: &str,
        query: &WorkdirQuery,
    ) -> Result<T, TransportError> {
        let uri = format!(
            "{endpoint}?session={}&path={}",
            query.session.0,
            query_escape(&query.path)
        );
        let reply = self
            .http
            .request(
                Method::GET,
                &uri,
                &self.run_header(),
                Bytes::new(),
                WORKDIR_TIMEOUT,
            )
            .await?;
        ok_json(reply)
    }

    async fn blob(&self, id: BlobId, from: u64) -> Result<BlobDownload, TransportError> {
        let mut headers = self.run_header();
        if from > 0 {
            headers.push((
                header::RANGE,
                HeaderValue::from_str(&format!("bytes={from}-")).expect("a valid header"),
            ));
        }
        let head = tokio::time::timeout(
            self.http.request_timeout(),
            self.http
                .head(Method::GET, &path::blob(id), &headers, Bytes::new()),
        )
        .await
        .map_err(|_| TransportError::Timeout)??;
        let (offset, total) = match head.status {
            StatusCode::OK => {
                let total = header_str(&head.headers, header::CONTENT_LENGTH)
                    .and_then(|v| v.trim().parse().ok())
                    .ok_or_else(|| TransportError::Decode("blob without Content-Length".into()))?;
                (0, total)
            }
            StatusCode::PARTIAL_CONTENT => header_str(&head.headers, header::CONTENT_RANGE)
                .and_then(parse_content_range)
                .ok_or_else(|| TransportError::Decode("bad Content-Range".into()))?,
            status => return Err(status_error(status, &head.headers)),
        };
        Ok(BlobDownload {
            offset,
            total,
            body: Box::new(TimedBody {
                body: Some(head.body),
                timeout: self.http.request_timeout(),
            }),
        })
    }
}

/// Percent-encodes `value` for a query string: everything but RFC 3986
/// unreserved characters and `/`.
fn query_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(char::from(byte));
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// `bytes <first>-<last>/<size>` → (first, size).
fn parse_content_range(value: &str) -> Option<(u64, u64)> {
    let rest = value.trim().strip_prefix("bytes ")?;
    let (range, size) = rest.split_once('/')?;
    let (first, _last) = range.split_once('-')?;
    Some((first.trim().parse().ok()?, size.trim().parse().ok()?))
}

/// A response body read one chunk at a time, each within `timeout`: blobs
/// and event batches.
struct TimedBody {
    /// `None` once the body ended.
    body: Option<Incoming>,
    timeout: Duration,
}

impl BlobBody for TimedBody {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Bytes>, TransportError>> {
        Box::pin(async move {
            loop {
                let Some(body) = self.body.as_mut() else {
                    return Ok(None);
                };
                let frame = tokio::time::timeout(self.timeout, body.frame())
                    .await
                    .map_err(|_| TransportError::Timeout)?;
                match frame {
                    None => {
                        self.body = None;
                        return Ok(None);
                    }
                    Some(Err(e)) => return Err(TransportError::Io(e.to_string())),
                    Some(Ok(frame)) => {
                        // Trailers and empty frames carry no content.
                        if let Ok(data) = frame.into_data()
                            && !data.is_empty()
                        {
                            return Ok(Some(data));
                        }
                    }
                }
            }
        })
    }
}

impl Transport for HttpsTransport {
    fn expect_run(&self, run: RunId) {
        let value = HeaderValue::from_str(&run.0.to_string()).expect("a UUID is a valid header");
        *self.run.lock().unwrap_or_else(|p| p.into_inner()) = Some(value);
    }

    fn hello(&self, since: Option<RunId>) -> BoxFuture<'_, Result<HelloResponse, TransportError>> {
        Box::pin(HttpsTransport::hello(self, since))
    }

    fn send_command<'a>(
        &'a self,
        command: &'a Command,
    ) -> BoxFuture<'a, Result<CommandResult, TransportError>> {
        Box::pin(HttpsTransport::send_command(self, command))
    }

    fn events(&self, query: EventsQuery) -> BoxFuture<'_, Result<EventBatch, TransportError>> {
        Box::pin(HttpsTransport::events(self, query))
    }

    fn blob(&self, id: BlobId, from: u64) -> BoxFuture<'_, Result<BlobDownload, TransportError>> {
        Box::pin(HttpsTransport::blob(self, id, from))
    }

    fn list_dirs<'a>(&'a self, dir: &'a str) -> BoxFuture<'a, Result<DirsResult, TransportError>> {
        Box::pin(HttpsTransport::list_dirs(self, dir))
    }

    fn list_conversations(&self) -> BoxFuture<'_, Result<ConversationsResult, TransportError>> {
        Box::pin(HttpsTransport::list_conversations(self))
    }

    fn workdir_changes<'a>(
        &'a self,
        query: &'a WorkdirQuery,
    ) -> BoxFuture<'a, Result<ChangesResult, TransportError>> {
        Box::pin(self.workdir(path::WORKDIR_CHANGES, query))
    }

    fn workdir_list<'a>(
        &'a self,
        query: &'a WorkdirQuery,
    ) -> BoxFuture<'a, Result<ListResult, TransportError>> {
        Box::pin(self.workdir(path::WORKDIR_LIST, query))
    }

    fn workdir_file<'a>(
        &'a self,
        query: &'a WorkdirQuery,
    ) -> BoxFuture<'a, Result<FileResult, TransportError>> {
        Box::pin(self.workdir(path::WORKDIR_FILE, query))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range_parses() {
        assert_eq!(parse_content_range("bytes 5-9/10"), Some((5, 10)));
        assert_eq!(parse_content_range("bytes */10"), None);
        assert_eq!(parse_content_range("items 1-2/3"), None);
    }

    #[test]
    fn query_values_are_escaped() {
        assert_eq!(query_escape("/srv/a-b_c.d~"), "/srv/a-b_c.d~");
        assert_eq!(
            query_escape("/a b&c=d%#+é"),
            "/a%20b%26c%3Dd%25%23%2B%C3%A9"
        );
    }

    fn hello_json() -> serde_json::Value {
        serde_json::to_value(HelloResponse {
            protocol_version: PROTOCOL_VERSION,
            server_id: skywave_proto::ServerId::new(),
            device_id: skywave_proto::DeviceId::new(),
            head: skywave_proto::Seq::ZERO,
            log: skywave_proto::LogRun {
                log: skywave_proto::LogId::new(),
                run: skywave_proto::RunId::new(),
                since: None,
            },
            devices: Vec::new(),
            shell: false,
            bypass: false,
        })
        .unwrap()
    }

    fn reply(status: StatusCode, body: &serde_json::Value) -> Reply {
        Reply {
            status,
            headers: HeaderMap::new(),
            body: Bytes::from(serde_json::to_vec(body).unwrap()),
        }
    }

    /// A hello of this version decodes whole; one that lacks a field this
    /// client requires is unreadable, not a mismatch.
    #[test]
    fn a_hello_of_this_version_decodes() {
        let json = hello_json();
        let hello = hello_from(reply(StatusCode::OK, &json)).unwrap();
        assert_eq!(serde_json::to_value(hello).unwrap(), json);

        let mut missing = json.clone();
        missing.as_object_mut().unwrap().remove("bypass");
        let err = hello_from(reply(StatusCode::OK, &missing)).unwrap_err();
        assert!(matches!(err, TransportError::Decode(_)), "{err:?}");
        assert_eq!(err.rejection(), None);
    }

    /// An older server's hello lacks fields this client requires; a newer
    /// one's has fields it does not know (and may lack some it does). Both
    /// are a protocol mismatch, which rejects the link, rather than a
    /// decode error the link would retry forever.
    #[test]
    fn a_hello_of_another_version_is_a_mismatch_whatever_its_shape() {
        let mut older = hello_json();
        let fields = older.as_object_mut().unwrap();
        fields.insert("protocol_version".into(), (PROTOCOL_VERSION - 1).into());
        fields.remove("bypass");
        fields.remove("devices");
        let mut newer = hello_json();
        let fields = newer.as_object_mut().unwrap();
        fields.insert("protocol_version".into(), (PROTOCOL_VERSION + 1).into());
        fields.insert("something_new".into(), serde_json::json!({"a": [1]}));
        fields.remove("shell");

        for (body, server) in [(older, PROTOCOL_VERSION - 1), (newer, PROTOCOL_VERSION + 1)] {
            let err = hello_from(reply(StatusCode::OK, &body)).unwrap_err();
            assert_eq!(
                err,
                TransportError::ProtocolMismatch {
                    client: PROTOCOL_VERSION,
                    server,
                }
            );
            assert_eq!(
                err.rejection(),
                Some(crate::LinkRejection::ProtocolMismatch {
                    client: PROTOCOL_VERSION,
                    server,
                })
            );
        }
    }

    /// A status is reported before the body is read; a body without a
    /// version is unreadable.
    #[test]
    fn a_hello_without_a_version_is_not_a_mismatch() {
        let json = hello_json();
        assert_eq!(
            hello_from(reply(StatusCode::FORBIDDEN, &json)).unwrap_err(),
            TransportError::Revoked
        );
        let mut bare = json;
        bare.as_object_mut().unwrap().remove("protocol_version");
        assert!(matches!(
            hello_from(reply(StatusCode::OK, &bare)).unwrap_err(),
            TransportError::Decode(_)
        ));
    }

    #[test]
    fn statuses_map_to_typed_errors() {
        let none = HeaderMap::new();
        assert_eq!(
            status_error(StatusCode::FORBIDDEN, &none),
            TransportError::Revoked
        );
        assert_eq!(
            status_error(StatusCode::UNAUTHORIZED, &none),
            TransportError::Unauthorized
        );
        let mut range = HeaderMap::new();
        range.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_static("bytes */42"),
        );
        assert_eq!(
            status_error(StatusCode::RANGE_NOT_SATISFIABLE, &range),
            TransportError::RangeNotSatisfiable { size: Some(42) }
        );
        assert_eq!(
            status_error(StatusCode::INTERNAL_SERVER_ERROR, &none),
            TransportError::BadStatus(500)
        );
    }
}
