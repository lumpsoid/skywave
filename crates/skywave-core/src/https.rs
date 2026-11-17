//! [`HttpsTransport`]: the v1 API over HTTP/1.1 + rustls (D15), plus the
//! small connection pool pairing shares.
//!
//! Each request takes an idle connection or opens one, so a long-poll and a
//! command run side by side on separate connections. A request that fails
//! on a reused connection before any response (the server closed it while
//! idle) is retried once on a fresh one; every v1 request is safe to repeat.
//! A request that times out or whose connection breaks takes down its own
//! connection and every idle one ([`Http::ended`]): they share its path, so
//! a NAT that silently dropped one most likely dropped them all. On Linux
//! and Android the kernel closes a connection whose sent bytes go
//! unacknowledged for [`UNACKED_MAX`] ([`limit_unacked`]), so a request on
//! a dead path fails then rather than at its own timeout.
//!
//! A command is timed by its upload's progress: the writes its socket
//! takes ([`Metered`]). On Linux and Android the socket takes a write only
//! while little of what it holds is unsent ([`limit_unsent`]), so a taken
//! write means bytes left the host. Elsewhere the kernel's send buffer
//! still counts as sent, and the wait for the reply also covers draining
//! it.
//!
//! Every request accepts zstd, and a reply marked so is decoded here
//! ([`encoding`], D45): whole for JSON replies, chunk by chunk for blobs,
//! whose per-chunk timeout still applies to the compressed chunks.

use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Frame, Incoming, SizeHint};
use hyper::client::conn::http1::{self, SendRequest};
use hyper::header::{self, HeaderMap, HeaderName, HeaderValue};
use hyper::{Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use rustls::ClientConfig;
use serde::de::DeserializeOwned;
use skywave_proto::encoding::{DECODED_LENGTH_HEADER, ZSTD};
use skywave_proto::{
    BlobId, ChangesResult, Command, CommandResult, ContentHash, ConversationsResult, DirsResult,
    EventBatch, EventsQuery, FileResult, HelloResponse, ListResult, LogResult, LoginFinish,
    LoginFinishResult, LoginStartResult, PROTOCOL_VERSION, RUN_HEADER, RunId, StorageReport,
    UPLOAD_OFFSET_HEADER, UploadResult, UsageLimitsResult, WorkdirQuery, WorktreeList,
    WriteFileRequest, WriteFileResult, path,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::{oneshot, watch};
use tokio_rustls::TlsConnector;

use crate::address::ServerAddress;
use crate::encoding::{self, ZstdBody};
use crate::engine::{RECLAIM_TIMEOUT, WORKTREES_TIMEOUT};
use crate::identity::Identity;
use crate::tls;
use crate::transport::{
    BlobBody, BlobDownload, BoxFuture, ExpectedRun, StagedUpload, Transport, TransportError,
    UploadFile,
};
use crate::workdir::{WORKDIR_TIMEOUT, WORKDIR_WRITE_TIMEOUT};

/// Timeouts and pool size of an [`HttpsTransport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportConfig {
    /// TCP connect plus TLS handshake.
    pub connect_timeout: Duration,
    /// A whole request; for blobs and event batches, the response head and
    /// then each body chunk, so a slow link that keeps delivering bytes
    /// never times out. An events long-poll's head gets its `wait` on top.
    /// A command is timed by its upload instead: it fails once the socket
    /// has taken none of it for this long, which, after the last byte,
    /// bounds the wait for the response head; its reply body is then read
    /// per chunk.
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

/// The body of a request on the wire.
type ReqBody = BoxBody<Bytes, io::Error>;

/// What a request sends: bytes in memory, or the bytes of a file from
/// `from` to its `len`, read from disk as the socket takes them
/// ([`FileBody`]).
enum RequestBody {
    Bytes(Bytes),
    File { file: UploadFile, from: u64 },
}

impl RequestBody {
    fn is_empty(&self) -> bool {
        match self {
            RequestBody::Bytes(bytes) => bytes.is_empty(),
            RequestBody::File { file, from } => *from >= file.len,
        }
    }

    /// A body for one send. A file is opened afresh, so a retry on a fresh
    /// connection sends it from `from` again.
    fn open(&self) -> Result<ReqBody, TransportError> {
        match self {
            RequestBody::Bytes(bytes) => Ok(Full::new(bytes.clone())
                .map_err(|never| match never {})
                .boxed()),
            RequestBody::File { file, from } => {
                if *from > file.len {
                    return Err(TransportError::Io(format!(
                        "upload offset {from} is past {} bytes of {}",
                        file.len,
                        file.path.display()
                    )));
                }
                let mut opened = std::fs::File::open(&file.path).map_err(|e| {
                    TransportError::Io(format!("opening upload {}: {e}", file.path.display()))
                })?;
                io::Seek::seek(&mut opened, io::SeekFrom::Start(*from)).map_err(|e| {
                    TransportError::Io(format!("seeking upload {}: {e}", file.path.display()))
                })?;
                Ok(FileBody {
                    file: tokio::fs::File::from_std(opened),
                    left: file.len - from,
                    buf: vec![0; FILE_CHUNK].into_boxed_slice(),
                }
                .boxed())
            }
        }
    }
}

/// What one read of a [`FileBody`] takes: one frame of the body.
const FILE_CHUNK: usize = 64 * 1024;

/// The first `left` bytes of a file as a request body, read a
/// [`FILE_CHUNK`] at a time, so an upload holds one chunk in memory, not
/// the file. Its exact size is known, so the request carries a
/// `content-length`. A file that ends early fails the body, and with it
/// the request.
struct FileBody {
    file: tokio::fs::File,
    left: u64,
    buf: Box<[u8]>,
}

impl Body for FileBody {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        let this = self.get_mut();
        if this.left == 0 {
            return Poll::Ready(None);
        }
        let want = usize::try_from(this.left)
            .unwrap_or(usize::MAX)
            .min(this.buf.len());
        let mut read = ReadBuf::new(&mut this.buf[..want]);
        if let Err(e) = std::task::ready!(Pin::new(&mut this.file).poll_read(cx, &mut read)) {
            return Poll::Ready(Some(Err(e)));
        }
        let n = read.filled().len();
        if n == 0 {
            return Poll::Ready(Some(Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "upload file ended early",
            ))));
        }
        this.left -= n as u64;
        Poll::Ready(Some(Ok(Frame::data(Bytes::copy_from_slice(read.filled())))))
    }

    fn is_end_stream(&self) -> bool {
        self.left == 0
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.left)
    }
}

/// A request body held until the server's `100 Continue`. The server answers
/// a refusal before it asks for the body, so the answer can come with no body
/// byte sent, and a connection reset then cannot drop bytes the server left
/// unread. A body answered without a `100` stays pending: ending or failing
/// it would fail the connection under the answer still being read.
struct ContinueGate {
    gate: Gate,
    inner: ReqBody,
}

/// Where a [`ContinueGate`] stands.
enum Gate {
    /// Held until the `100` comes.
    Waiting(oneshot::Receiver<()>),
    /// The `100` came: the body goes.
    Open,
    /// The final answer came without a `100`: the body is never sent, and the
    /// server closes the connection after it. The receiver is not polled
    /// again, since a finished one panics.
    Refused,
}

impl Body for ContinueGate {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        let this = self.get_mut();
        if let Gate::Waiting(go) = &mut this.gate {
            this.gate = match std::task::ready!(Pin::new(go).poll(cx)) {
                Ok(()) => Gate::Open,
                Err(_) => Gate::Refused,
            };
        }
        match this.gate {
            Gate::Open => Pin::new(&mut this.inner).poll_frame(cx),
            Gate::Waiting(_) | Gate::Refused => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// A gate over `inner`, and the function that opens it when the server's
/// informational status comes: only a `100` opens it. Dropping the function
/// (hyper does, at the final response) leaves the gate held for good.
fn continue_gate(inner: ReqBody) -> (ContinueGate, impl Fn(StatusCode) + Send + Sync + 'static) {
    let (go, held) = oneshot::channel();
    // The function is `Fn`, so the sender is taken out of a lock once.
    let go = Mutex::new(Some(go));
    let open = move |status: StatusCode| {
        if status == StatusCode::CONTINUE
            && let Some(go) = go.lock().unwrap_or_else(|p| p.into_inner()).take()
        {
            let _ = go.send(());
        }
    };
    let gate = ContinueGate {
        gate: Gate::Waiting(held),
        inner,
    };
    (gate, open)
}

/// Asks the server for `100 Continue` before the body and holds the body
/// until it comes ([`ContinueGate`]). hyper does not wait for the `100` on
/// its own; the callback sees it as an informational response.
fn expect_continue(mut req: Request<ReqBody>) -> Request<ReqBody> {
    req.headers_mut()
        .insert(header::EXPECT, HeaderValue::from_static("100-continue"));
    let empty = Full::new(Bytes::new())
        .map_err(|never| match never {})
        .boxed();
    let inner = std::mem::replace(req.body_mut(), empty);
    let (gate, open) = continue_gate(inner);
    *req.body_mut() = gate.boxed();
    hyper::ext::on_informational(&mut req, move |res| open(res.status()));
    req
}

/// One HTTP/1.1 connection: its request handle and a signal of every write
/// its socket takes ([`Metered`]).
struct Conn {
    sender: SendRequest<ReqBody>,
    written: watch::Receiver<()>,
}

impl Conn {
    /// Runs the HTTP/1.1 handshake over `io` (whose writes `written`
    /// reports) and drives the connection on a task of its own.
    async fn open<T>(io: T, written: watch::Receiver<()>) -> Result<Conn, TransportError>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (sender, conn) = http1::handshake(TokioIo::new(io))
            .await
            .map_err(|e| TransportError::Io(e.to_string()))?;
        tokio::spawn(async move {
            // Ends when the server closes or every sender is dropped.
            let _ = conn.await;
        });
        Ok(Conn { sender, written })
    }

    /// Sends `req` and waits for the response head. With a `stall`, the
    /// wait fails with [`TransportError::Timeout`] once the socket has
    /// taken no byte for `stall`: an upload that keeps moving, however
    /// slowly, goes on, a stalled one fails, and once the last byte has
    /// left, the response head is due within `stall`. Without one, the
    /// caller bounds the wait. The outer error is the timeout, the inner
    /// one hyper's.
    async fn send(
        &mut self,
        req: Request<ReqBody>,
        stall: Option<Duration>,
    ) -> Result<hyper::Result<hyper::Response<Incoming>>, TransportError> {
        let Some(stall) = stall else {
            return Ok(self.sender.send_request(req).await);
        };
        let mut written = self.written.clone();
        written.borrow_and_update();
        let mut metered = true;
        let response = self.sender.send_request(req);
        tokio::pin!(response);
        let mut last = tokio::time::Instant::now();
        loop {
            tokio::select! {
                biased;
                response = &mut response => return Ok(response),
                changed = written.changed(), if metered => match changed {
                    Ok(()) => last = tokio::time::Instant::now(),
                    // The connection is gone; its request ends with it.
                    Err(_) => metered = false,
                },
                () = tokio::time::sleep_until(last + stall) => return Err(TransportError::Timeout),
            }
        }
    }
}

/// The unsent bytes a client socket may queue before it takes no more
/// writes (`TCP_NOTSENT_LOWAT`, [`limit_unsent`]).
const UNSENT_MAX: usize = 16 * 1024;

/// Makes the socket take a write only while fewer than [`UNSENT_MAX`] of
/// its bytes are still unsent, so a write it takes ([`Metered`]) follows
/// bytes leaving the host, not filling a send buffer that autotuning can
/// grow to megabytes: a whole command could otherwise vanish into it at
/// once and leave the head wait to cover its transmission. What is left
/// after the last write is then at most this plus a write and what is in
/// flight. Linux and Android only; elsewhere the send buffer still counts
/// as sent. Best effort: a socket that refuses keeps the default.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn limit_unsent(tcp: &TcpStream) {
    let value = libc::c_int::try_from(UNSENT_MAX).expect("UNSENT_MAX fits a c_int");
    set_tcp_option(tcp, libc::TCP_NOTSENT_LOWAT, value, "TCP_NOTSENT_LOWAT");
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn limit_unsent(_tcp: &TcpStream) {}

/// How long a client socket may hold sent bytes the server has not
/// acknowledged before the kernel closes it (`TCP_USER_TIMEOUT`,
/// [`limit_unacked`]).
const UNACKED_MAX: Duration = Duration::from_secs(20);

/// Makes the kernel close the socket once what it sent has gone
/// unacknowledged for [`UNACKED_MAX`], so a request on a connection whose
/// path died silently (a NAT that dropped its mapping, a network that is
/// gone) fails with [`TransportError::Io`] instead of waiting out its
/// timeout. It measures acknowledgements, not replies: a server holding a
/// long-poll has acknowledged the request, and a slow link that keeps
/// acknowledging is never cut. Linux and Android only; elsewhere the
/// request's own timeout finds the connection dead. Best effort.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn limit_unacked(tcp: &TcpStream) {
    let value = libc::c_int::try_from(UNACKED_MAX.as_millis()).expect("UNACKED_MAX fits a c_int");
    set_tcp_option(tcp, libc::TCP_USER_TIMEOUT, value, "TCP_USER_TIMEOUT");
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn limit_unacked(_tcp: &TcpStream) {}

/// Sets the `IPPROTO_TCP` option `option` (`name` in the log) to `value`,
/// logging a refusal.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn set_tcp_option(tcp: &TcpStream, option: libc::c_int, value: libc::c_int, name: &str) {
    use std::os::fd::AsRawFd;
    let len = libc::socklen_t::try_from(std::mem::size_of::<libc::c_int>())
        .expect("a c_int's size fits socklen_t");
    // SAFETY: both options read one c_int from `value`, `len` long.
    let set = unsafe {
        libc::setsockopt(
            tcp.as_raw_fd(),
            libc::IPPROTO_TCP,
            option,
            (&raw const value).cast(),
            len,
        )
    };
    if set == -1 {
        tracing::debug!(error = %io::Error::last_os_error(), "cannot set {name}");
    }
}

/// A stream that signals every write it takes, so a request can be timed
/// by its upload's progress ([`Conn::send`]). It sits under TLS, so what it
/// sees is what the socket accepted, which [`limit_unsent`] keeps close to
/// what left the host.
struct Metered<T> {
    io: T,
    written: watch::Sender<()>,
}

impl<T> Metered<T> {
    fn new(io: T) -> (Metered<T>, watch::Receiver<()>) {
        let (written, seen) = watch::channel(());
        (Metered { io, written }, seen)
    }

    fn mark(&self, poll: &Poll<io::Result<usize>>) {
        if let Poll::Ready(Ok(n)) = poll
            && *n > 0
        {
            self.written.send_replace(());
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for Metered<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for Metered<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let poll = Pin::new(&mut this.io).poll_write(cx, buf);
        this.mark(&poll);
        poll
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let poll = Pin::new(&mut this.io).poll_write_vectored(cx, bufs);
        this.mark(&poll);
        poll
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_shutdown(cx)
    }
}

/// HTTPS to one server with one TLS config.
pub(crate) struct Http {
    address: ServerAddress,
    authority: HeaderValue,
    connector: TlsConnector,
    config: TransportConfig,
    idle: Mutex<Vec<Conn>>,
}

/// A response whose body has not been read yet.
struct Head {
    status: StatusCode,
    headers: HeaderMap,
    body: Incoming,
    conn: Conn,
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

    async fn connect(&self) -> Result<Conn, TransportError> {
        let open = async {
            let tcp = TcpStream::connect((self.address.host(), self.address.port()))
                .await
                .map_err(|e| TransportError::Unreachable(e.to_string()))?;
            let _ = tcp.set_nodelay(true);
            limit_unsent(&tcp);
            limit_unacked(&tcp);
            let (tcp, written) = Metered::new(tcp);
            let tls = self
                .connector
                .connect(self.address.server_name(), tcp)
                .await
                .map_err(|e| handshake_error(&e))?;
            Conn::open(tls, written).await
        };
        tokio::time::timeout(self.config.connect_timeout, open)
            .await
            .map_err(|_| TransportError::Timeout)?
    }

    fn take_idle(&self) -> Option<Conn> {
        let mut idle = self.idle.lock().unwrap_or_else(|p| p.into_inner());
        while let Some(conn) = idle.pop() {
            if conn.sender.is_ready() {
                return Some(conn);
            }
        }
        None
    }

    fn put_idle(&self, conn: Conn) {
        if !conn.sender.is_ready() {
            return;
        }
        let mut idle = self.idle.lock().unwrap_or_else(|p| p.into_inner());
        if idle.len() < self.config.max_idle {
            idle.push(conn);
        }
    }

    /// Notes how a request ended: one that timed out or whose connection
    /// broke drops every idle connection too. They share its path, so if it
    /// died silently they are dead the same way, and a request on one would
    /// wait out its own timeout before the next got a fresh connection.
    fn ended<T>(&self, result: Result<T, TransportError>) -> Result<T, TransportError> {
        if let Err(TransportError::Timeout | TransportError::Io(_)) = &result {
            self.idle.lock().unwrap_or_else(|p| p.into_inner()).clear();
        }
        result
    }

    /// Sends a request and waits for the response head, timed by the
    /// upload's progress when `stall` is set ([`Conn::send`]).
    async fn head(
        &self,
        method: Method,
        uri: &str,
        headers: &[(header::HeaderName, HeaderValue)],
        body: RequestBody,
        stall: Option<Duration>,
    ) -> Result<Head, TransportError> {
        // A caller's own Content-Type replaces the JSON default; a second
        // one would be sent as well.
        let caller_sets_type = headers
            .iter()
            .any(|(name, _)| *name == header::CONTENT_TYPE);
        // Each attempt opens the body afresh ([`RequestBody::open`]). A file
        // body waits for the server's `100` ([`expect_continue`]).
        let build = || -> Result<Request<ReqBody>, TransportError> {
            let mut req = Request::builder()
                .method(method.clone())
                .uri(uri)
                .header(header::HOST, self.authority.clone())
                // Event batches and blobs come compressed (D45); every
                // reply is decoded ([`encoding`]), so every request asks.
                .header(header::ACCEPT_ENCODING, ZSTD);
            if !body.is_empty() && !caller_sets_type {
                req = req.header(header::CONTENT_TYPE, "application/json");
            }
            for (name, value) in headers {
                req = req.header(name, value.clone());
            }
            let req = req
                .body(body.open()?)
                .map_err(|e| TransportError::Decode(format!("building request: {e}")))?;
            if matches!(body, RequestBody::File { .. }) && !body.is_empty() {
                return Ok(expect_continue(req));
            }
            Ok(req)
        };
        if let Some(mut conn) = self.take_idle() {
            match conn.send(build()?, stall).await? {
                Ok(resp) => return Ok(into_head(resp, conn)),
                Err(e) if e.is_canceled() || e.is_closed() || e.is_incomplete_message() => {
                    // Stale pooled connection: try once on a fresh one.
                }
                Err(e) => return Err(TransportError::Io(e.to_string())),
            }
        }
        let mut conn = self.connect().await?;
        let resp = conn
            .send(build()?, stall)
            .await?
            .map_err(|e| TransportError::Io(e.to_string()))?;
        Ok(into_head(resp, conn))
    }

    /// Sends a request whose body may take long to upload, timed by its
    /// progress rather than by one deadline ([`Conn::send`], with the
    /// request timeout as the stall bound), and reads the response with
    /// each chunk due within the request timeout. A slow uplink that keeps
    /// taking bytes never times out; one that stalls for a request timeout
    /// does.
    async fn upload(
        &self,
        method: Method,
        uri: &str,
        headers: &[(header::HeaderName, HeaderValue)],
        body: RequestBody,
    ) -> Result<Reply, TransportError> {
        let exchange = async {
            let head = self
                .head(method, uri, headers, body, Some(self.request_timeout()))
                .await?;
            self.read(head).await
        };
        self.ended(exchange.await)
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
            let head = self
                .head(method, uri, headers, RequestBody::Bytes(body), None)
                .await?;
            let body = head
                .body
                .collect()
                .await
                .map_err(|e| TransportError::Io(e.to_string()))?
                .to_bytes();
            self.put_idle(head.conn);
            Ok(Reply {
                body: encoding::decoded(&head.headers, body).await?,
                status: head.status,
                headers: head.headers,
            })
        };
        let ended = tokio::time::timeout(timeout, exchange)
            .await
            .map_err(|_| TransportError::Timeout)
            .flatten();
        self.ended(ended)
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
        let exchange = async {
            let head = tokio::time::timeout(
                head_timeout,
                self.head(method, uri, headers, RequestBody::Bytes(Bytes::new()), None),
            )
            .await
            .map_err(|_| TransportError::Timeout)??;
            self.read(head).await
        };
        self.ended(exchange.await)
    }

    /// Sends a request and waits up to the request timeout for the
    /// response head, leaving the body to the caller.
    async fn request_head(
        &self,
        method: Method,
        uri: &str,
        headers: &[(header::HeaderName, HeaderValue)],
    ) -> Result<Head, TransportError> {
        let head = tokio::time::timeout(
            self.request_timeout(),
            self.head(method, uri, headers, RequestBody::Bytes(Bytes::new()), None),
        )
        .await
        .map_err(|_| TransportError::Timeout)
        .flatten();
        self.ended(head)
    }

    /// Reads the body of `head` with each chunk due within the request
    /// timeout, and returns the connection to the pool.
    async fn read(&self, head: Head) -> Result<Reply, TransportError> {
        let mut body = TimedBody {
            body: Some(head.body),
            timeout: self.request_timeout(),
        };
        let mut read = Vec::new();
        while let Some(chunk) = body.next_chunk().await? {
            read.extend_from_slice(&chunk);
        }
        self.put_idle(head.conn);
        Ok(Reply {
            body: encoding::decoded(&head.headers, read.into()).await?,
            status: head.status,
            headers: head.headers,
        })
    }

    pub(crate) fn request_timeout(&self) -> Duration {
        self.config.request_timeout
    }
}

fn into_head(resp: hyper::Response<Incoming>, conn: Conn) -> Head {
    let (parts, body) = resp.into_parts();
    Head {
        status: parts.status,
        headers: parts.headers,
        body,
        conn,
    }
}

/// The error for a failed TLS handshake. rustls's verdict on the server's
/// certificate, which tokio-rustls wraps in the `io::Error`, tells the two
/// identity faults that no retry can fix from any other failure.
fn handshake_error(error: &std::io::Error) -> TransportError {
    use rustls::{CertificateError, Error};
    let reason = error.to_string();
    let verdict = error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<Error>());
    match verdict {
        Some(Error::InvalidCertificate(CertificateError::UnknownIssuer)) => {
            TransportError::UntrustedServer(reason)
        }
        Some(Error::InvalidCertificate(
            CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
        )) => TransportError::ServerNameMismatch(reason),
        _ => TransportError::Tls(reason),
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
/// Every request names the run `run` expects, read as it leaves, in
/// [`RUN_HEADER`], except a `hello` that asks about a run.
pub struct HttpsTransport {
    http: Http,
    run: ExpectedRun,
}

impl HttpsTransport {
    /// A transport to `address` that authenticates with `identity` and
    /// names the run `run` expects. Opens no connection yet.
    pub fn new(
        address: &ServerAddress,
        identity: &dyn Identity,
        config: TransportConfig,
        run: ExpectedRun,
    ) -> Result<HttpsTransport, TransportError> {
        let tls = tls::device_config(identity)?;
        Ok(HttpsTransport {
            http: Http::new(address.clone(), tls, config),
            run,
        })
    }

    /// The headers every request but a `hello` asking about a run carries:
    /// the expected run, once there is one.
    fn run_header(&self) -> Vec<(header::HeaderName, HeaderValue)> {
        self.run
            .get()
            .map(|run| {
                (
                    header::HeaderName::from_static(RUN_HEADER),
                    HeaderValue::from_str(&run.0.to_string()).expect("a UUID is a valid header"),
                )
            })
            .into_iter()
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
        // A message may be up to `MESSAGE_MAX_BYTES`: over a slow uplink its
        // upload alone can outlast any whole-exchange deadline, so it is
        // timed by its progress, and a resend would only time out again.
        let reply = self
            .http
            .upload(
                Method::POST,
                path::COMMANDS,
                &self.run_header(),
                RequestBody::Bytes(body.into()),
            )
            .await?;
        ok_json(reply)
    }

    /// `HEAD /v1/attachments/{sha256}`: `204` when the server holds the
    /// staged upload, `404` when it does not, with the bytes it holds of an
    /// interrupted upload in [`UPLOAD_OFFSET_HEADER`] (none when absent).
    async fn attachment_staged(&self, sha256: ContentHash) -> Result<StagedUpload, TransportError> {
        let reply = self
            .http
            .request(
                Method::HEAD,
                &path::attachment(sha256),
                &self.run_header(),
                Bytes::new(),
                self.http.request_timeout(),
            )
            .await?;
        match reply.status {
            StatusCode::NO_CONTENT => Ok(StagedUpload::Staged),
            StatusCode::NOT_FOUND => {
                let offset = header::HeaderName::from_static(UPLOAD_OFFSET_HEADER);
                let held = header_str(&reply.headers, offset)
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                Ok(StagedUpload::Missing { held })
            }
            status => Err(status_error(status, &reply.headers)),
        }
    }

    /// `PUT /v1/attachments/{sha256}` with the bytes of the file from `from`
    /// to its `len`, streamed from disk, never held whole. A `from` above 0
    /// names the offset in [`UPLOAD_OFFSET_HEADER`]. Every outcome the server
    /// decides is a 200 [`UploadResult`], refusals included.
    async fn upload_attachment(
        &self,
        sha256: ContentHash,
        file: UploadFile,
        from: u64,
    ) -> Result<UploadResult, TransportError> {
        let mut headers = self.run_header();
        headers.push((
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        ));
        if from > 0 {
            headers.push((
                header::HeaderName::from_static(UPLOAD_OFFSET_HEADER),
                HeaderValue::from(from),
            ));
        }
        let reply = self
            .http
            .upload(
                Method::PUT,
                &path::attachment(sha256),
                &headers,
                RequestBody::File { file, from },
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

    /// `GET /v1/storage`, which takes no query.
    async fn storage_report(&self) -> Result<StorageReport, TransportError> {
        let reply = self
            .http
            .request(
                Method::GET,
                path::STORAGE,
                &self.run_header(),
                Bytes::new(),
                self.http.request_timeout(),
            )
            .await?;
        ok_json(reply)
    }

    /// `POST /v1/storage/reclaim`, which takes no body. Bounded by
    /// [`RECLAIM_TIMEOUT`] rather than the request timeout: the server's
    /// `VACUUM` may take minutes.
    async fn storage_reclaim(&self) -> Result<StorageReport, TransportError> {
        let reply = self
            .http
            .request(
                Method::POST,
                path::STORAGE_RECLAIM,
                &self.run_header(),
                Bytes::new(),
                RECLAIM_TIMEOUT,
            )
            .await?;
        ok_json(reply)
    }

    /// `GET /v1/worktrees`, which takes no query. Bounded by
    /// [`WORKTREES_TIMEOUT`] rather than the request timeout: the server
    /// runs git in every worktree.
    async fn worktrees(&self) -> Result<WorktreeList, TransportError> {
        let reply = self
            .http
            .request(
                Method::GET,
                path::WORKTREES,
                &self.run_header(),
                Bytes::new(),
                WORKTREES_TIMEOUT,
            )
            .await?;
        ok_json(reply)
    }

    /// `GET /v1/usage-limits`, which takes no query.
    async fn usage_limits(&self) -> Result<UsageLimitsResult, TransportError> {
        let reply = self
            .http
            .request(
                Method::GET,
                path::USAGE_LIMITS,
                &self.run_header(),
                Bytes::new(),
                self.http.request_timeout(),
            )
            .await?;
        ok_json(reply)
    }

    /// `POST /v1/agent-login/start`, which takes no body.
    async fn login_start(&self) -> Result<LoginStartResult, TransportError> {
        let reply = self
            .http
            .request(
                Method::POST,
                path::AGENT_LOGIN_START,
                &self.run_header(),
                Bytes::new(),
                self.http.request_timeout(),
            )
            .await?;
        ok_json(reply)
    }

    /// `POST /v1/agent-login/finish` with `finish` as its JSON body.
    async fn login_finish(
        &self,
        finish: &LoginFinish,
    ) -> Result<LoginFinishResult, TransportError> {
        let body = serde_json::to_vec(finish)
            .map_err(|e| TransportError::Decode(format!("encoding a login finish: {e}")))?;
        let reply = self
            .http
            .request(
                Method::POST,
                path::AGENT_LOGIN_FINISH,
                &self.run_header(),
                body.into(),
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

    /// `POST /v1/workdir/write` with `request` as its JSON body, bounded by
    /// [`WORKDIR_WRITE_TIMEOUT`].
    async fn write_workdir_file(
        &self,
        request: &WriteFileRequest,
    ) -> Result<WriteFileResult, TransportError> {
        let body = serde_json::to_vec(request)
            .map_err(|e| TransportError::Decode(format!("encoding a workdir write: {e}")))?;
        let reply = self
            .http
            .request(
                Method::POST,
                path::WORKDIR_WRITE,
                &self.run_header(),
                body.into(),
                WORKDIR_WRITE_TIMEOUT,
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
        let head = self
            .http
            .request_head(Method::GET, &path::blob(id), &headers)
            .await?;
        let compressed = encoding::is_zstd(&head.headers)?;
        // A compressed body's length is the frame's, not the blob's: the
        // server names the decoded length instead. A range counts decoded
        // bytes either way.
        let length = if compressed {
            HeaderName::from_static(DECODED_LENGTH_HEADER)
        } else {
            header::CONTENT_LENGTH
        };
        let (offset, total) = match head.status {
            StatusCode::OK => {
                let total = header_str(&head.headers, length.clone())
                    .and_then(|v| v.trim().parse().ok())
                    .ok_or_else(|| TransportError::Decode(format!("blob without {length}")))?;
                (0, total)
            }
            StatusCode::PARTIAL_CONTENT => header_str(&head.headers, header::CONTENT_RANGE)
                .and_then(parse_content_range)
                .ok_or_else(|| TransportError::Decode("bad Content-Range".into()))?,
            status => return Err(status_error(status, &head.headers)),
        };
        let body = TimedBody {
            body: Some(head.body),
            timeout: self.http.request_timeout(),
        };
        let body: Box<dyn BlobBody> = if compressed {
            Box::new(ZstdBody::new(body)?)
        } else {
            Box::new(body)
        };
        Ok(BlobDownload {
            offset,
            total,
            body,
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

    fn storage_report(&self) -> BoxFuture<'_, Result<StorageReport, TransportError>> {
        Box::pin(HttpsTransport::storage_report(self))
    }

    fn storage_reclaim(&self) -> BoxFuture<'_, Result<StorageReport, TransportError>> {
        Box::pin(HttpsTransport::storage_reclaim(self))
    }

    fn worktrees(&self) -> BoxFuture<'_, Result<WorktreeList, TransportError>> {
        Box::pin(HttpsTransport::worktrees(self))
    }

    fn usage_limits(&self) -> BoxFuture<'_, Result<UsageLimitsResult, TransportError>> {
        Box::pin(HttpsTransport::usage_limits(self))
    }

    fn login_start(&self) -> BoxFuture<'_, Result<LoginStartResult, TransportError>> {
        Box::pin(HttpsTransport::login_start(self))
    }

    fn login_finish<'a>(
        &'a self,
        finish: &'a LoginFinish,
    ) -> BoxFuture<'a, Result<LoginFinishResult, TransportError>> {
        Box::pin(HttpsTransport::login_finish(self, finish))
    }

    fn workdir_changes<'a>(
        &'a self,
        query: &'a WorkdirQuery,
    ) -> BoxFuture<'a, Result<ChangesResult, TransportError>> {
        Box::pin(self.workdir(path::WORKDIR_CHANGES, query))
    }

    fn workdir_log<'a>(
        &'a self,
        query: &'a WorkdirQuery,
    ) -> BoxFuture<'a, Result<LogResult, TransportError>> {
        Box::pin(self.workdir(path::WORKDIR_LOG, query))
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

    fn workdir_download<'a>(
        &'a self,
        query: &'a WorkdirQuery,
    ) -> BoxFuture<'a, Result<FileResult, TransportError>> {
        Box::pin(self.workdir(path::WORKDIR_DOWNLOAD, query))
    }

    fn attachment_staged(
        &self,
        sha256: ContentHash,
    ) -> BoxFuture<'_, Result<StagedUpload, TransportError>> {
        Box::pin(HttpsTransport::attachment_staged(self, sha256))
    }

    fn upload_attachment(
        &self,
        sha256: ContentHash,
        file: UploadFile,
        from: u64,
    ) -> BoxFuture<'_, Result<UploadResult, TransportError>> {
        Box::pin(HttpsTransport::upload_attachment(self, sha256, file, from))
    }

    fn write_workdir_file<'a>(
        &'a self,
        request: &'a WriteFileRequest,
    ) -> BoxFuture<'a, Result<WriteFileResult, TransportError>> {
        Box::pin(HttpsTransport::write_workdir_file(self, request))
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
            login_expires: None,
            agent_login_offered: false,
            agent_logged_out: false,
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

    /// The stall bound of the upload tests: the default request timeout.
    const STALL: Duration = Duration::from_secs(30);
    /// What the in-memory uplink holds, and what the server reads at a time.
    const PIECE: usize = 1024;
    /// A command body the size of a large message.
    const BODY: usize = 100 * 1024;

    /// How the server behind the in-memory uplink reads a request.
    #[derive(Clone, Copy)]
    enum Uplink {
        /// One `PIECE` a second, then the reply.
        Steady,
        /// One `PIECE` a second until it has read this many bytes, then
        /// nothing, the connection held open.
        StallAfter(usize),
        /// The whole request at once, then no reply, the connection held.
        Mute,
    }

    /// Where `got` holds a whole request: its head and a `content-length`
    /// body.
    fn request_end(got: &[u8]) -> Option<usize> {
        let head = got.windows(4).position(|w| w == b"\r\n\r\n")? + 4;
        let text = std::str::from_utf8(&got[..head]).ok()?;
        let len: usize = text
            .lines()
            .find_map(|l| l.strip_prefix("content-length: "))?
            .trim()
            .parse()
            .ok()?;
        Some(head + len)
    }

    /// A plain HTTP/1.1 server on `io` that reads as `uplink` says.
    async fn server(mut io: tokio::io::DuplexStream, uplink: Uplink) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut got = Vec::new();
        let mut buf = vec![0u8; BODY * 2];
        while request_end(&got).is_none_or(|end| got.len() < end) {
            let want = match uplink {
                Uplink::Steady => PIECE,
                Uplink::StallAfter(limit) if got.len() >= limit => std::future::pending().await,
                Uplink::StallAfter(_) => PIECE,
                Uplink::Mute => buf.len(),
            };
            if !matches!(uplink, Uplink::Mute) {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            let n = io.read(&mut buf[..want]).await.unwrap();
            if n == 0 {
                return;
            }
            got.extend_from_slice(&buf[..n]);
        }
        if matches!(uplink, Uplink::Mute) {
            std::future::pending::<()>().await;
        }
        io.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}")
            .await
            .unwrap();
        // Held open, as a kept-alive connection is.
        std::future::pending::<()>().await;
    }

    /// Sends a `BODY`-byte POST over an uplink that holds one `PIECE`, to a
    /// server that reads it as `uplink` says, timed with [`STALL`].
    async fn post(uplink: Uplink) -> Result<StatusCode, TransportError> {
        let (client, far) = tokio::io::duplex(PIECE);
        tokio::spawn(server(far, uplink));
        let (client, written) = Metered::new(client);
        let mut conn = Conn::open(client, written).await.unwrap();
        let req = Request::post("/v1/commands")
            .header(header::HOST, "srv")
            .body(
                RequestBody::Bytes(Bytes::from(vec![b'x'; BODY]))
                    .open()
                    .unwrap(),
            )
            .unwrap();
        let resp = conn
            .send(req, Some(STALL))
            .await?
            .map_err(|e| TransportError::Io(e.to_string()))?;
        Ok(resp.status())
    }

    /// An upload far slower than the stall bound allows as a whole (about
    /// 100 s), but that never stops moving, gets its reply: the old
    /// whole-exchange deadline (the same 30 s) would have failed it every
    /// time.
    #[tokio::test(start_paused = true)]
    async fn a_slow_steady_upload_outlasts_the_stall_bound() {
        let started = tokio::time::Instant::now();
        assert_eq!(post(Uplink::Steady).await, Ok(StatusCode::OK));
        let took = started.elapsed();
        assert!(took > STALL * 3, "{took:?}");
    }

    /// An upload that stops moving fails one stall bound after the socket
    /// last took a byte, not when the whole body would have been sent.
    #[tokio::test(start_paused = true)]
    async fn a_stalled_upload_times_out_within_the_stall_bound() {
        let started = tokio::time::Instant::now();
        assert_eq!(
            post(Uplink::StallAfter(10 * PIECE)).await,
            Err(TransportError::Timeout)
        );
        // The last write fills the uplink the server's tenth read freed, at
        // 10 s.
        assert_eq!(started.elapsed(), Duration::from_secs(10) + STALL);
    }

    /// A server on `io` that reads one whole request, answers it `{}`, and
    /// hands back the raw request bytes it read.
    async fn echo_request(mut io: tokio::io::DuplexStream, got: oneshot::Sender<Vec<u8>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut bytes = Vec::new();
        let mut buf = vec![0u8; BODY * 2];
        while request_end(&bytes).is_none_or(|end| bytes.len() < end) {
            let n = io.read(&mut buf).await.unwrap();
            if n == 0 {
                return;
            }
            bytes.extend_from_slice(&buf[..n]);
        }
        io.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}")
            .await
            .unwrap();
        let _ = got.send(bytes);
        std::future::pending::<()>().await;
    }

    /// A file body is its first `len` bytes, read a chunk at a time under an
    /// exact `content-length`: the bytes after them are not sent.
    #[tokio::test]
    async fn a_file_body_streams_its_first_len_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("upload");
        let len = 3 * FILE_CHUNK + 17;
        let contents: Vec<u8> = (0..len + 100).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &contents).unwrap();
        let (sent, got) = oneshot::channel();
        let (client, far) = tokio::io::duplex(PIECE);
        tokio::spawn(echo_request(far, sent));
        let (client, written) = Metered::new(client);
        let mut conn = Conn::open(client, written).await.unwrap();
        let body = RequestBody::File {
            file: UploadFile {
                path,
                len: len as u64,
            },
            from: 0,
        }
        .open()
        .unwrap();
        let req = Request::put("/v1/attachments/x")
            .header(header::HOST, "srv")
            .body(body)
            .unwrap();
        let resp = conn.send(req, None).await.unwrap().unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let got = got.await.unwrap();
        let head_end = got.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let head = std::str::from_utf8(&got[..head_end]).unwrap();
        assert!(
            head.lines().any(|l| l == format!("content-length: {len}")),
            "{head}"
        );
        assert_eq!(got.len() - head_end, len);
        assert!(
            got[head_end..] == contents[..len],
            "the body is not the file's first {len} bytes"
        );
    }

    /// A file that ends before its `len` fails the request: the body errors
    /// rather than padding out the length.
    #[tokio::test]
    async fn a_file_shorter_than_its_len_fails_the_upload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("short");
        std::fs::write(&path, [7u8; 10]).unwrap();
        let (sent, _got) = oneshot::channel();
        let (client, far) = tokio::io::duplex(PIECE);
        tokio::spawn(echo_request(far, sent));
        let (client, written) = Metered::new(client);
        let mut conn = Conn::open(client, written).await.unwrap();
        let body = RequestBody::File {
            file: UploadFile { path, len: 20 },
            from: 0,
        }
        .open()
        .unwrap();
        let req = Request::put("/v1/attachments/x")
            .header(header::HOST, "srv")
            .body(body)
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(5), conn.send(req, None))
            .await
            .expect("the send hung rather than failing");
        assert!(
            matches!(result, Ok(Err(_))),
            "a short file must fail its request"
        );
    }

    /// A file that cannot be opened is an I/O error, not a body.
    #[test]
    fn a_missing_upload_file_is_an_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let file = UploadFile {
            path: dir.path().join("absent"),
            len: 1,
        };
        assert!(matches!(
            RequestBody::File { file, from: 0 }.open(),
            Err(TransportError::Io(_))
        ));
    }

    /// A file body from an offset is the rest of the file up to its `len`:
    /// the `content-length` counts from the offset, and the bytes before it
    /// are not sent.
    #[tokio::test]
    async fn a_file_body_from_an_offset_streams_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("upload");
        let len = 3 * FILE_CHUNK + 17;
        let from = FILE_CHUNK + 5;
        let contents: Vec<u8> = (0..len + 100).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &contents).unwrap();
        let (sent, got) = oneshot::channel();
        let (client, far) = tokio::io::duplex(PIECE);
        tokio::spawn(echo_request(far, sent));
        let (client, written) = Metered::new(client);
        let mut conn = Conn::open(client, written).await.unwrap();
        let body = RequestBody::File {
            file: UploadFile {
                path,
                len: len as u64,
            },
            from: from as u64,
        }
        .open()
        .unwrap();
        let req = Request::put("/v1/attachments/x")
            .header(header::HOST, "srv")
            .body(body)
            .unwrap();
        let resp = conn.send(req, None).await.unwrap().unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let got = got.await.unwrap();
        let head_end = got.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let head = std::str::from_utf8(&got[..head_end]).unwrap();
        let rest = len - from;
        assert!(
            head.lines().any(|l| l == format!("content-length: {rest}")),
            "{head}"
        );
        assert_eq!(got.len() - head_end, rest);
        assert!(
            got[head_end..] == contents[from..len],
            "the body is not the file's bytes from {from} to {len}"
        );
    }

    /// An offset past the file's `len` cannot be sent: opening the body is
    /// an I/O error, not a request that sends nothing.
    #[test]
    fn an_upload_offset_past_the_file_fails_to_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("upload");
        std::fs::write(&path, [7u8; 10]).unwrap();
        let result = RequestBody::File {
            file: UploadFile { path, len: 10 },
            from: 11,
        }
        .open()
        .map(|_| ());
        assert!(
            matches!(result, Err(TransportError::Io(_))),
            "an offset past the file must not open: {result:?}"
        );
    }

    /// A server on `io` for an upload sent with [`expect_continue`]. It reads
    /// the head, answers `100 Continue`, reads the whole body and answers
    /// `200`. It hands back the head and the bytes after it.
    async fn continue_server(
        mut io: tokio::io::DuplexStream,
        got: oneshot::Sender<(Vec<u8>, Vec<u8>)>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        const ANSWER: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}";
        let mut bytes = Vec::new();
        let mut buf = vec![0u8; BODY * 2];
        while !bytes.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = io.read(&mut buf).await.unwrap();
            if n == 0 {
                return;
            }
            bytes.extend_from_slice(&buf[..n]);
        }
        io.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
            .await
            .unwrap();
        while request_end(&bytes).is_none_or(|end| bytes.len() < end) {
            let n = io.read(&mut buf).await.unwrap();
            if n == 0 {
                return;
            }
            bytes.extend_from_slice(&buf[..n]);
        }
        io.write_all(ANSWER).await.unwrap();
        let head_end = bytes.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let after = bytes.split_off(head_end);
        let _ = got.send((bytes, after));
    }

    /// A file body sent with `Expect: 100-continue` waits for the server's
    /// `100`, then sends all of its bytes.
    #[tokio::test]
    async fn an_upload_sends_its_body_after_100_continue() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("upload");
        let contents: Vec<u8> = (0..200_000).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &contents).unwrap();
        let (sent, got) = oneshot::channel();
        let (client, far) = tokio::io::duplex(PIECE);
        tokio::spawn(continue_server(far, sent));
        let (client, written) = Metered::new(client);
        let mut conn = Conn::open(client, written).await.unwrap();
        let body = RequestBody::File {
            file: UploadFile {
                path,
                len: contents.len() as u64,
            },
            from: 0,
        }
        .open()
        .unwrap();
        let req = expect_continue(
            Request::put("/v1/attachments/x")
                .header(header::HOST, "srv")
                .body(body)
                .unwrap(),
        );
        let resp = conn.send(req, None).await.unwrap().unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let answer = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(answer, "{}");
        let (head, body) = got.await.unwrap();
        let head = String::from_utf8(head).unwrap().to_ascii_lowercase();
        assert!(head.contains("expect: 100-continue"), "{head}");
        assert_eq!(
            body.len(),
            contents.len(),
            "the server did not get the whole body"
        );
        assert!(body == contents, "the body is not the file");
    }

    /// The gate of a body of `abc`, polled with no waker: it stays held
    /// through an informational status that is not a `100`, and opens on the
    /// `100`, after which its bytes come as the body's own.
    #[test]
    fn a_continue_gate_holds_the_body_until_a_100() {
        use std::task::Waker;
        let inner = Full::new(Bytes::from_static(b"abc"))
            .map_err(|never| match never {})
            .boxed();
        let (mut gate, open) = continue_gate(inner);
        assert_eq!(gate.size_hint().exact(), Some(3));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(Pin::new(&mut gate).poll_frame(&mut cx).is_pending());
        open(StatusCode::PROCESSING);
        assert!(Pin::new(&mut gate).poll_frame(&mut cx).is_pending());
        open(StatusCode::CONTINUE);
        let Poll::Ready(Some(Ok(frame))) = Pin::new(&mut gate).poll_frame(&mut cx) else {
            panic!("the body did not come after the 100");
        };
        assert_eq!(frame.into_data().unwrap(), Bytes::from_static(b"abc"));
    }

    /// A final answer without a `100` drops the callback, so its open is
    /// gone: the body stays held, and polling it again stays held too.
    #[test]
    fn a_continue_gate_answered_without_a_100_stays_held() {
        use std::task::Waker;
        let inner = Full::new(Bytes::from_static(b"abc"))
            .map_err(|never| match never {})
            .boxed();
        let (mut gate, open) = continue_gate(inner);
        drop(open);
        let mut cx = Context::from_waker(Waker::noop());
        assert!(Pin::new(&mut gate).poll_frame(&mut cx).is_pending());
        assert!(Pin::new(&mut gate).poll_frame(&mut cx).is_pending());
    }

    /// How many bytes a socket to a peer that reads nothing and advertises
    /// a small window takes before it blocks, in 1 KiB writes, with a send
    /// buffer as large as the system allows, and with or without
    /// [`limit_unsent`].
    #[cfg(any(target_os = "linux", target_os = "android"))]
    async fn taken_by_a_stuck_socket(limited: bool) -> usize {
        let listener = tokio::net::TcpSocket::new_v4().unwrap();
        listener.set_recv_buffer_size(4096).unwrap();
        listener.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();
        let listener = listener.listen(1).unwrap();
        let client = tokio::net::TcpSocket::new_v4().unwrap();
        client.set_send_buffer_size(4 * 1024 * 1024).unwrap();
        let (tcp, accepted) = tokio::join!(client.connect(addr), listener.accept());
        let tcp = tcp.unwrap();
        // Held, never read.
        let _peer = accepted.unwrap();
        if limited {
            limit_unsent(&tcp);
        }
        let piece = [b'x'; 1024];
        let mut taken = 0;
        loop {
            match tcp.try_write(&piece) {
                Ok(n) => taken += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    // Blocked for good once the peer's window is full: no
                    // ACK can free anything.
                    let ready = tokio::time::timeout(Duration::from_millis(200), tcp.writable());
                    if ready.await.is_err() {
                        return taken;
                    }
                }
                Err(e) => panic!("{e}"),
            }
        }
    }

    /// A socket with [`limit_unsent`] blocks once about [`UNSENT_MAX`] is
    /// queued, not once its send buffer is full: a write it takes means
    /// bytes left. Without it, the same socket swallows far more.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[tokio::test]
    async fn a_client_socket_holds_little_unsent() {
        let limited = taken_by_a_stuck_socket(true).await;
        let unlimited = taken_by_a_stuck_socket(false).await;
        assert!(limited < 4 * UNSENT_MAX, "{limited}");
        assert!(unlimited > 8 * UNSENT_MAX, "{unlimited}");
    }

    /// Once the whole request has left, the reply's head is due within the
    /// stall bound.
    #[tokio::test(start_paused = true)]
    async fn a_reply_that_never_comes_times_out_after_the_upload() {
        let started = tokio::time::Instant::now();
        assert_eq!(post(Uplink::Mute).await, Err(TransportError::Timeout));
        assert_eq!(started.elapsed(), STALL);
    }

    /// A client socket gives up on unacknowledged bytes after
    /// [`UNACKED_MAX`].
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[tokio::test]
    async fn a_client_socket_gives_up_on_unacknowledged_bytes() {
        use std::os::fd::AsRawFd;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tcp, accepted) = tokio::join!(TcpStream::connect(addr), listener.accept());
        let (tcp, _peer) = (tcp.unwrap(), accepted.unwrap());
        limit_unacked(&tcp);
        let mut value: libc::c_int = 0;
        let mut len = libc::socklen_t::try_from(std::mem::size_of::<libc::c_int>()).unwrap();
        // SAFETY: TCP_USER_TIMEOUT writes one c_int to `value`, `len` long.
        let got = unsafe {
            libc::getsockopt(
                tcp.as_raw_fd(),
                libc::IPPROTO_TCP,
                libc::TCP_USER_TIMEOUT,
                (&raw mut value).cast(),
                &raw mut len,
            )
        };
        assert_eq!(got, 0, "{}", io::Error::last_os_error());
        assert_eq!(u128::try_from(value).unwrap(), UNACKED_MAX.as_millis());
    }

    /// A plain HTTP/1.1 server on `io` that answers each request with `{}`
    /// when `answers`, and otherwise reads it and says nothing, the
    /// connection held either way.
    async fn idle_server(mut io: tokio::io::DuplexStream, answers: bool) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut got = Vec::new();
        let mut buf = [0u8; PIECE];
        loop {
            let n = io.read(&mut buf).await.unwrap();
            if n == 0 {
                return;
            }
            got.extend_from_slice(&buf[..n]);
            if answers && let Some(end) = got.windows(4).position(|w| w == b"\r\n\r\n") {
                got.drain(..end + 4);
                io.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}")
                    .await
                    .unwrap();
            }
        }
    }

    /// An [`Http`] whose pool holds a connection to a server that answers
    /// and, taken first, one to a server that does not when `mute_first`,
    /// or one that does.
    async fn pooled(mute_first: bool) -> Http {
        let tls =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_root_certificates(rustls::RootCertStore::empty())
                .with_no_client_auth();
        let http = Http::new(
            ServerAddress::parse("srv:443").unwrap(),
            Arc::new(tls),
            TransportConfig::default(),
        );
        for answers in [true, !mute_first] {
            let (client, far) = tokio::io::duplex(PIECE);
            tokio::spawn(idle_server(far, answers));
            let (client, written) = Metered::new(client);
            let mut conn = Conn::open(client, written).await.unwrap();
            conn.sender.ready().await.unwrap();
            http.put_idle(conn);
        }
        http
    }

    fn idle_count(http: &Http) -> usize {
        http.idle.lock().unwrap().len()
    }

    /// A request that times out on a pooled connection drops the other
    /// idle one too: whatever silently killed its path most likely killed
    /// theirs, and the next request must open a fresh connection rather
    /// than wait out another timeout.
    #[tokio::test(start_paused = true)]
    async fn a_timed_out_request_drops_every_idle_connection() {
        let http = pooled(true).await;
        assert_eq!(idle_count(&http), 2);
        let got = http
            .request(Method::GET, "/v1/hello", &[], Bytes::new(), STALL)
            .await;
        assert_eq!(got.map(|r| r.status), Err(TransportError::Timeout));
        assert_eq!(idle_count(&http), 0);
    }

    /// A request that gets its answer leaves the pool as it was.
    #[tokio::test(start_paused = true)]
    async fn an_answered_request_keeps_the_idle_connections() {
        let http = pooled(false).await;
        let got = http
            .request(Method::GET, "/v1/hello", &[], Bytes::new(), STALL)
            .await;
        assert_eq!(got.map(|r| r.status), Ok(StatusCode::OK));
        assert_eq!(idle_count(&http), 2);
    }
}
