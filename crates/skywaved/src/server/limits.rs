//! What one peer may hold of the server: how many connections, and how long
//! a connection may sit without progress.
//!
//! Every limit on time is about *no progress*, never total duration: a
//! long-poll waits up to [`crate::api::MAX_WAIT`] with nothing to send, and a
//! blob on a slow link takes as long as it takes while bytes keep moving.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use axum::body::Body as AxumBody;
use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use hyper::body::Incoming;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::time::Sleep;

/// How many connections the server holds and how long a stalled peer keeps
/// one. Tests shorten the times.
#[derive(Debug, Clone, Copy)]
pub struct ConnectionLimits {
    /// Connections open at once, from everyone. One past it is closed as
    /// soon as it is accepted.
    pub max_connections: usize,
    /// Connections open at once from one source: an IPv4 address, or an
    /// IPv6 /64 (one host usually owns the whole prefix). A device holds a
    /// few (long-poll, commands, blobs, listings); a NAT may put several
    /// devices behind one address.
    pub max_per_ip: usize,
    /// How long a connection may wait for a request's headers while it has
    /// no request in flight and nothing left to send: covers both a slow
    /// request head and a keep-alive connection idle between requests.
    pub idle_timeout: Duration,
    /// A request body that delivers nothing for this long fails with 408,
    /// and the connection closes after the reply.
    pub body_stall: Duration,
    /// Sent data that the peer acknowledges none of for this long drops the
    /// connection (`TCP_USER_TIMEOUT`, Linux and Android only).
    pub unacked_timeout: Duration,
}

impl ConnectionLimits {
    /// 512 connections, 32 per source, 30 s idle, 30 s body stall, 60 s
    /// unacknowledged (twice a device's own per-chunk timeout).
    pub const DEFAULT: ConnectionLimits = ConnectionLimits {
        max_connections: 512,
        max_per_ip: 32,
        idle_timeout: Duration::from_secs(30),
        body_stall: Duration::from_secs(30),
        unacked_timeout: Duration::from_secs(60),
    };
}

impl Default for ConnectionLimits {
    fn default() -> Self {
        ConnectionLimits::DEFAULT
    }
}

/// Sets an accepted socket's TCP options.
///
/// - Keepalive, so a peer that vanished without a FIN or RST (a phone that
///   lost its network) is found dead while the connection is silent:
///   probes after 60 s, every 15 s, 4 times.
/// - `TCP_USER_TIMEOUT` of [`ConnectionLimits::unacked_timeout`], so a peer
///   that stops acknowledging what the server sent (gone, or advertising a
///   zero window) is dropped. It measures acknowledgements, not how fast
///   the server's writes complete: a slow link that keeps acknowledging is
///   never cut. Elsewhere only the OS's own retransmission limit applies.
pub(super) fn tune_socket(tcp: &TcpStream, peer: SocketAddr, limits: &ConnectionLimits) {
    let socket = socket2::SockRef::from(tcp);
    let keepalive = socket2::TcpKeepalive::new()
        .with_time(Duration::from_secs(60))
        .with_interval(Duration::from_secs(15))
        .with_retries(4);
    if let Err(e) = socket.set_tcp_keepalive(&keepalive) {
        tracing::debug!(%peer, error = %e, "cannot set TCP keepalive");
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    if let Err(e) = socket.set_tcp_user_timeout(Some(limits.unacked_timeout)) {
        tracing::debug!(%peer, error = %e, "cannot set TCP_USER_TIMEOUT");
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let _ = limits;
}

/// Counts open connections against [`ConnectionLimits`].
pub(super) struct Admissions {
    max_connections: usize,
    max_per_ip: usize,
    open: Arc<Mutex<Open>>,
}

#[derive(Default)]
struct Open {
    total: usize,
    per_source: HashMap<IpAddr, usize>,
}

/// One admitted connection's place in the counts, given back when dropped.
pub(super) struct Admission {
    source: IpAddr,
    open: Arc<Mutex<Open>>,
}

/// The source `ip` counts against: an IPv4 address (IPv4-mapped ones
/// included) as is, an IPv6 address cut to its /64.
fn source(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V6(v6) => IpAddr::V6((u128::from(v6) & !((1u128 << 64) - 1)).into()),
        v4 => v4,
    }
}

impl Admissions {
    pub(super) fn new(limits: &ConnectionLimits) -> Admissions {
        Admissions {
            max_connections: limits.max_connections,
            max_per_ip: limits.max_per_ip,
            open: Arc::default(),
        }
    }

    /// A place for a connection from `ip`, or `None` when either cap is
    /// reached.
    pub(super) fn admit(&self, ip: IpAddr) -> Option<Admission> {
        let source = source(ip);
        let mut open = crate::lock(&self.open);
        let from_source = open.per_source.get(&source).copied().unwrap_or(0);
        if open.total >= self.max_connections || from_source >= self.max_per_ip {
            return None;
        }
        open.total += 1;
        open.per_source.insert(source, from_source + 1);
        Some(Admission {
            source,
            open: self.open.clone(),
        })
    }
}

impl Drop for Admission {
    fn drop(&mut self) {
        let mut open = crate::lock(&self.open);
        open.total -= 1;
        if let Some(n) = open.per_source.get_mut(&self.source) {
            *n -= 1;
            if *n == 0 {
                open.per_source.remove(&self.source);
            }
        }
    }
}

/// What a connection's requests are doing, shared between the HTTP service
/// (which starts and ends requests) and [`IdleIo`] (which times the gaps).
#[derive(Default)]
pub(super) struct Activity {
    in_flight: AtomicUsize,
    /// Bumped whenever a request starts or ends, so an idle clock started
    /// before knows it is stale.
    epoch: AtomicU64,
    /// The task last left waiting on a read, woken when the connection may
    /// have become idle so the clock gets started.
    reader: Mutex<Option<Waker>>,
}

/// One request in flight, from the moment its head is parsed until its
/// response body is done (or dropped).
pub(super) struct InFlight(Arc<Activity>);

impl Activity {
    pub(super) fn start(self: &Arc<Self>) -> InFlight {
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        self.epoch.fetch_add(1, Ordering::SeqCst);
        InFlight(self.clone())
    }

    /// Wakes the reader, so its next read (hyper polls reads first) starts
    /// the idle clock if the connection is now idle. Without this, a
    /// response that ends while hyper already saw its read pend leaves the
    /// task waiting only on the socket, and a silent peer is never dropped.
    fn wake_reader(&self) {
        if let Some(waker) = crate::lock(&self.reader).take() {
            waker.wake();
        }
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
        self.0.epoch.fetch_add(1, Ordering::SeqCst);
        self.0.wake_reader();
    }
}

/// A socket that fails its reads once the connection has been idle for the
/// limit: no request in flight and nothing left to send.
///
/// Hyper's own header timeout is not used: it starts as soon as a response
/// body is fully *encoded*, while hundreds of KB may still wait in its
/// write buffer for a slow peer, and so ends a keep-alive connection (or
/// cuts the response) mid-download. Here the clock runs only while
/// nothing is left to send, at three levels:
/// - a request counts as in flight until hyper drops its response body;
/// - a write that returned `Pending` marks bytes as waiting above the
///   socket (every buffer above it only holds bytes back after a write to
///   it returned `Pending`);
/// - when the clock runs out, bytes still in the kernel's send queue
///   (accepted by `write` but not yet acknowledged, which can be MBs for a
///   slow peer) start it over. A peer that acknowledges nothing is
///   `TCP_USER_TIMEOUT`'s to drop.
///
/// Reads do not restart it, so trickling a request head does not help.
pub(super) struct IdleIo {
    inner: TcpStream,
    limit: Duration,
    activity: Arc<Activity>,
    write_blocked: bool,
    clock: Option<(u64, Pin<Box<Sleep>>)>,
}

/// Bytes in `tcp`'s send queue, not yet acknowledged by the peer.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn send_queue(tcp: &TcpStream) -> usize {
    use std::os::fd::AsRawFd;
    let mut queued: libc::c_int = 0;
    // SAFETY: TIOCOUTQ (SIOCOUTQ) writes one c_int to `queued`.
    let asked = unsafe { libc::ioctl(tcp.as_raw_fd(), libc::TIOCOUTQ, &mut queued) };
    if asked == -1 {
        return 0;
    }
    usize::try_from(queued).unwrap_or(0)
}

/// Elsewhere the queue is not read: the clock can then run out while the
/// kernel still holds the tail of a response for a slow peer.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn send_queue(_: &TcpStream) -> usize {
    0
}

impl IdleIo {
    pub(super) fn new(inner: TcpStream, limit: Duration, activity: Arc<Activity>) -> IdleIo {
        IdleIo {
            inner,
            limit,
            activity,
            write_blocked: false,
            clock: None,
        }
    }

    /// Whether the idle clock has run out; when it is running and has not,
    /// `cx` is woken when it does.
    fn idle_expired(&mut self, cx: &mut Context<'_>) -> bool {
        if self.write_blocked || self.activity.in_flight.load(Ordering::SeqCst) > 0 {
            self.clock = None;
            return false;
        }
        let epoch = self.activity.epoch.load(Ordering::SeqCst);
        if self.clock.as_ref().is_none_or(|(since, _)| *since != epoch) {
            self.clock = Some((epoch, Box::pin(tokio::time::sleep(self.limit))));
        }
        let (_, sleep) = self.clock.as_mut().expect("just set");
        if sleep.as_mut().poll(cx).is_pending() {
            return false;
        }
        if send_queue(&self.inner) > 0 {
            sleep
                .as_mut()
                .reset(tokio::time::Instant::now() + self.limit);
            // Registers `cx` with the restarted clock.
            return sleep.as_mut().poll(cx).is_ready();
        }
        true
    }

    fn track<T>(&mut self, poll: Poll<io::Result<T>>) -> Poll<io::Result<T>> {
        let was_blocked = self.write_blocked;
        self.write_blocked = poll.is_pending();
        if self.write_blocked {
            self.clock = None;
        } else if was_blocked && self.activity.in_flight.load(Ordering::SeqCst) == 0 {
            // Unblocked: see `Activity::wake_reader`.
            self.activity.wake_reader();
        }
        poll
    }
}

impl AsyncRead for IdleIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let poll = Pin::new(&mut self.inner).poll_read(cx, buf);
        if poll.is_pending() {
            if self.idle_expired(cx) {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the connection was idle too long",
                )));
            }
            let mut reader = crate::lock(&self.activity.reader);
            if !reader.as_ref().is_some_and(|w| w.will_wake(cx.waker())) {
                *reader = Some(cx.waker().clone());
            }
        }
        poll
    }
}

impl AsyncWrite for IdleIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let poll = Pin::new(&mut self.inner).poll_write(cx, buf);
        self.track(poll)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let poll = Pin::new(&mut self.inner).poll_write_vectored(cx, bufs);
        self.track(poll)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let poll = Pin::new(&mut self.inner).poll_flush(cx);
        self.track(poll)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let poll = Pin::new(&mut self.inner).poll_shutdown(cx);
        self.track(poll)
    }
}

/// A response body that keeps its request counted as in flight until it is
/// done or dropped.
pub(super) struct Responding {
    inner: AxumBody,
    _request: InFlight,
}

impl Responding {
    pub(super) fn new(inner: AxumBody, request: InFlight) -> Responding {
        Responding {
            inner,
            _request: request,
        }
    }
}

impl Body for Responding {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// A request body that fails once the peer has sent nothing of it for the
/// stall limit, and says so through a flag the server answers 408 on. The
/// clock runs only while a handler waits for the next frame.
pub(super) struct StalledBody {
    inner: Incoming,
    limit: Duration,
    clock: Option<Pin<Box<Sleep>>>,
    stalled: Arc<AtomicBool>,
}

impl StalledBody {
    pub(super) fn new(inner: Incoming, limit: Duration, stalled: Arc<AtomicBool>) -> StalledBody {
        StalledBody {
            inner,
            limit,
            clock: None,
            stalled,
        }
    }
}

impl Body for StalledBody {
    type Data = Bytes;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        let this = &mut *self;
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(frame) => {
                this.clock = None;
                Poll::Ready(frame.map(|frame| frame.map_err(Into::into)))
            }
            Poll::Pending => {
                let limit = this.limit;
                let clock = this
                    .clock
                    .get_or_insert_with(|| Box::pin(tokio::time::sleep(limit)));
                if clock.as_mut().poll(cx).is_pending() {
                    return Poll::Pending;
                }
                this.stalled.store(true, Ordering::SeqCst);
                Poll::Ready(Some(Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the request body stalled",
                )
                .into())))
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admissions_cap_the_total_and_each_source_and_give_places_back() {
        let admissions = Admissions::new(&ConnectionLimits {
            max_connections: 3,
            max_per_ip: 2,
            ..ConnectionLimits::DEFAULT
        });
        let a: IpAddr = "10.0.0.1".parse().unwrap();
        let b: IpAddr = "10.0.0.2".parse().unwrap();
        let a_mapped: IpAddr = "::ffff:10.0.0.1".parse().unwrap();
        let a1 = admissions.admit(a).unwrap();
        let _a2 = admissions.admit(a_mapped).unwrap();
        assert!(admissions.admit(a).is_none(), "a's cap");
        let _b1 = admissions.admit(b).unwrap();
        assert!(admissions.admit(b).is_none(), "the total cap");
        drop(a1);
        assert!(admissions.admit(b).is_some(), "a place given back");
    }

    #[test]
    fn ipv6_counts_by_its_64() {
        let admissions = Admissions::new(&ConnectionLimits {
            max_per_ip: 1,
            ..ConnectionLimits::DEFAULT
        });
        let _held = admissions
            .admit("2001:db8:1:2::1".parse().unwrap())
            .unwrap();
        assert!(
            admissions
                .admit("2001:db8:1:2:ffff::9".parse().unwrap())
                .is_none(),
            "the same /64"
        );
        assert!(
            admissions
                .admit("2001:db8:1:3::1".parse().unwrap())
                .is_some(),
            "the next /64"
        );
    }

    /// Bytes the kernel still holds for a peer that has not taken them keep
    /// the connection from counting as idle; once they are taken, it does.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[tokio::test]
    async fn the_idle_clock_waits_for_the_send_queue_to_drain() {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.set_recv_buffer_size(4096).unwrap();
        let mut client = socket
            .connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (server, _) = listener.accept().await.unwrap();
        // Written straight to the socket: nothing waits above it.
        while server.try_write(&[0; 64 * 1024]).is_ok() {}
        assert!(send_queue(&server) > 0);
        let limit = Duration::from_millis(50);
        let mut io = IdleIo::new(server, limit, Arc::default());
        let mut buf = [0; 16];

        let waited = tokio::time::timeout(limit * 10, io.read(&mut buf)).await;
        assert!(waited.is_err(), "still waiting: {waited:?}");

        let drain = tokio::spawn(async move {
            let mut buf = vec![0; 64 * 1024];
            while let Ok(1..) = client.read(&mut buf).await {}
        });
        let idle = tokio::time::timeout(Duration::from_secs(10), io.read(&mut buf))
            .await
            .expect("idle once the queue drained");
        assert_eq!(idle.unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert_eq!(send_queue(&io.inner), 0);
        drop(io);
        drain.await.unwrap();
    }

    #[tokio::test]
    async fn accepted_sockets_get_keepalive_and_a_user_timeout() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let _client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (accepted, peer) = listener.accept().await.unwrap();
        let socket = socket2::SockRef::from(&accepted);
        assert!(!socket.keepalive().unwrap(), "off before");
        let limits = ConnectionLimits {
            unacked_timeout: Duration::from_millis(1234),
            ..ConnectionLimits::DEFAULT
        };
        tune_socket(&accepted, peer, &limits);
        assert!(socket.keepalive().unwrap());
        assert_eq!(
            socket.tcp_keepalive_time().unwrap(),
            Duration::from_secs(60)
        );
        #[cfg(any(target_os = "linux", target_os = "android"))]
        assert_eq!(
            socket.tcp_user_timeout().unwrap(),
            Some(Duration::from_millis(1234))
        );
    }
}
