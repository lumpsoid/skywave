//! [`LinkProxy`]: a TCP proxy that makes a bad link out of a good one.
//!
//! Every accepted connection is paired with a fresh upstream connection and
//! piped in both directions. Each direction reads chunks, stamps each with a
//! delivery time (`latency` plus a random share of `jitter`, never earlier
//! than the previous chunk's, so bytes keep their order), and writes them at
//! that time, paced by the bandwidth cap. The cap is a shared budget per
//! direction across all connections, like one physical link.
//!
//! Drops are drawn per connection when it is accepted, and drawn again for
//! every live connection whenever the profile changes: after a random number
//! of further bytes (both directions together; the last write is cut short,
//! so a drop lands mid-message) and/or after a random time from then. [`LinkProxy::cut`] drops every live connection on command,
//! [`LinkProxy::down`] refuses new ones (accept then close) and
//! [`LinkProxy::blackhole`] accepts and holds them silently, like a dead
//! route. [`LinkProxy::up`] restores service.
//!
//! All randomness comes from the seed passed to [`LinkProxy::start`]; the
//! decisions for connection `n` depend on the seed and `n` only.

use std::future::pending;
use std::io;
use std::net::SocketAddr;
use std::ops::RangeInclusive;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};
use tokio::task::{AbortHandle, JoinHandle};
use tokio::time::{Instant, sleep_until};

use crate::rng::Rng;

/// Bytes read per chunk.
const READ_BUF: usize = 16 * 1024;
/// Bytes written per paced write.
const SLICE: usize = 4 * 1024;
/// Chunks in flight per direction before the reader waits.
const IN_FLIGHT: usize = 16;

/// What the link does to traffic. [`LinkProfile::default`] is a clean link.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LinkProfile {
    /// Added to every chunk, each direction.
    pub latency: Duration,
    /// Up to this much more, uniformly random per chunk.
    pub jitter: Duration,
    /// Bytes per second per direction, shared by all connections.
    pub bandwidth: Option<u64>,
    /// Drop each connection after this many bytes (both directions).
    pub drop_after_bytes: Option<RangeInclusive<u64>>,
    /// Drop each connection after this long.
    pub drop_after: Option<RangeInclusive<Duration>>,
}

impl LinkProfile {
    /// No latency, no cap, no drops.
    pub fn clean() -> LinkProfile {
        LinkProfile::default()
    }

    pub fn latency(mut self, latency: Duration) -> LinkProfile {
        self.latency = latency;
        self
    }

    pub fn jitter(mut self, jitter: Duration) -> LinkProfile {
        self.jitter = jitter;
        self
    }

    pub fn bandwidth(mut self, bytes_per_sec: u64) -> LinkProfile {
        self.bandwidth = Some(bytes_per_sec.max(1));
        self
    }

    pub fn drop_after_bytes(mut self, bytes: RangeInclusive<u64>) -> LinkProfile {
        self.drop_after_bytes = Some(bytes);
        self
    }

    pub fn drop_after(mut self, time: RangeInclusive<Duration>) -> LinkProfile {
        self.drop_after = Some(time);
        self
    }

    /// A random bad link that still lets work through, for soak tests.
    pub fn random(rng: &mut Rng) -> LinkProfile {
        let ms = Duration::from_millis;
        let mut profile = LinkProfile::clean()
            .latency(ms(rng.in_range(0..=120)))
            .jitter(ms(rng.in_range(0..=80)));
        if rng.chance(0.6) {
            profile = profile.bandwidth(rng.in_range(32..=512) * 1024);
        }
        if rng.chance(0.6) {
            let low = rng.in_range(8..=64) * 1024;
            profile = profile.drop_after_bytes(low..=low * 4);
        }
        if rng.chance(0.5) {
            let low = ms(rng.in_range(500..=2000));
            profile = profile.drop_after(low..=low * 3);
        }
        profile
    }
}

/// Counters since the proxy started.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LinkStats {
    /// Connections accepted and piped upstream.
    pub accepted: u64,
    /// Connections closed on accept while down.
    pub refused: u64,
    /// Connections held silently while blackholed.
    pub held: u64,
    /// Connections dropped by the profile's byte budget.
    pub dropped_bytes: u64,
    /// Connections dropped by the profile's timer.
    pub dropped_time: u64,
    /// Connections killed by `cut`, `down` or `blackhole`.
    pub cut: u64,
    /// Bytes delivered client → server.
    pub bytes_up: u64,
    /// Bytes delivered server → client.
    pub bytes_down: u64,
}

impl LinkStats {
    /// Connections the profile dropped.
    pub fn dropped(&self) -> u64 {
        self.dropped_bytes + self.dropped_time
    }
}

#[derive(Debug, Default)]
struct Counters {
    accepted: AtomicU64,
    refused: AtomicU64,
    held: AtomicU64,
    dropped_bytes: AtomicU64,
    dropped_time: AtomicU64,
    cut: AtomicU64,
    bytes: [AtomicU64; 2],
}

fn bump(counter: &AtomicU64, by: u64) {
    counter.fetch_add(by, Ordering::Relaxed);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Up,
    Refuse,
    Blackhole,
}

#[derive(Debug, Clone, Copy)]
enum Dir {
    Up = 0,
    Down = 1,
}

struct Shared {
    upstream: Mutex<SocketAddr>,
    rng: Rng,
    profile: watch::Sender<LinkProfile>,
    mode: Mutex<Mode>,
    live: Mutex<Vec<AbortHandle>>,
    held: Mutex<Vec<TcpStream>>,
    /// Per direction: when the link is free to carry the next byte.
    free_at: [Mutex<Instant>; 2],
    counters: Counters,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Shared {
    fn profile(&self) -> LinkProfile {
        self.profile.borrow().clone()
    }

    /// Waits until the link has carried `len` more bytes in `dir`.
    async fn pace(&self, dir: Dir, len: usize) {
        let Some(rate) = self.profile.borrow().bandwidth else {
            return;
        };
        let cost = Duration::from_secs_f64(len as f64 / rate as f64);
        let done = {
            let mut free_at = lock(&self.free_at[dir as usize]);
            *free_at = (*free_at).max(Instant::now()) + cost;
            *free_at
        };
        sleep_until(done).await;
    }

    fn cut(&self) {
        let live: Vec<AbortHandle> = lock(&self.live).drain(..).collect();
        for conn in live {
            if !conn.is_finished() {
                bump(&self.counters.cut, 1);
            }
            conn.abort();
        }
        lock(&self.held).clear();
    }

    fn set_mode(&self, mode: Mode) {
        *lock(&self.mode) = mode;
    }
}

/// See the module docs. Dropping it stops the proxy and every connection.
pub struct LinkProxy {
    addr: SocketAddr,
    seed: u64,
    shared: Arc<Shared>,
    task: JoinHandle<()>,
}

impl LinkProxy {
    /// Listens on `127.0.0.1:0` and forwards to `upstream` through `profile`.
    pub async fn start(upstream: SocketAddr, profile: LinkProfile, seed: u64) -> LinkProxy {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the link proxy");
        let addr = listener.local_addr().expect("proxy address");
        let shared = Arc::new(Shared {
            upstream: Mutex::new(upstream),
            rng: Rng::new(seed),
            profile: watch::Sender::new(profile),
            mode: Mutex::new(Mode::Up),
            live: Mutex::new(Vec::new()),
            held: Mutex::new(Vec::new()),
            free_at: [Mutex::new(Instant::now()), Mutex::new(Instant::now())],
            counters: Counters::default(),
        });
        let task = tokio::spawn(accept(listener, shared.clone()));
        LinkProxy {
            addr,
            seed,
            shared,
            task,
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// `127.0.0.1:<port>`, as a client is told to connect.
    pub fn address(&self) -> String {
        self.addr.to_string()
    }

    pub fn seed(&self) -> u64 {
        self.seed
    }

    pub fn profile(&self) -> LinkProfile {
        self.shared.profile()
    }

    /// Takes effect at once: delays and the cap apply to the next chunk,
    /// and every live connection draws its drops again from `profile`.
    pub fn set_profile(&self, profile: LinkProfile) {
        self.shared.profile.send_replace(profile);
    }

    pub fn stats(&self) -> LinkStats {
        let c = &self.shared.counters;
        let get = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        LinkStats {
            accepted: get(&c.accepted),
            refused: get(&c.refused),
            held: get(&c.held),
            dropped_bytes: get(&c.dropped_bytes),
            dropped_time: get(&c.dropped_time),
            cut: get(&c.cut),
            bytes_up: get(&c.bytes[Dir::Up as usize]),
            bytes_down: get(&c.bytes[Dir::Down as usize]),
        }
    }

    /// Forwards new connections to `upstream` from now on, e.g. a server
    /// restarted on another port; live ones stay where they are.
    pub fn retarget(&self, upstream: SocketAddr) {
        *lock(&self.shared.upstream) = upstream;
    }

    /// Kills every live (and held) connection now.
    pub fn cut(&self) {
        self.shared.cut();
    }

    /// Kills live connections and closes new ones right after accept, so
    /// connecting fails fast.
    pub fn down(&self) {
        self.shared.set_mode(Mode::Refuse);
        self.shared.cut();
    }

    /// Kills live connections and holds new ones open without forwarding a
    /// byte, so the client only notices through its own timeouts.
    pub fn blackhole(&self) {
        self.shared.set_mode(Mode::Blackhole);
        self.shared.cut();
    }

    /// Serves new connections again; held ones are closed.
    pub fn up(&self) {
        self.shared.set_mode(Mode::Up);
        lock(&self.shared.held).clear();
    }
}

impl Drop for LinkProxy {
    fn drop(&mut self) {
        self.task.abort();
        self.shared.cut();
    }
}

async fn accept(listener: TcpListener, shared: Arc<Shared>) {
    let mut n = 0u64;
    loop {
        let Ok((inbound, _)) = listener.accept().await else {
            continue;
        };
        let mode = *lock(&shared.mode);
        match mode {
            Mode::Refuse => {
                bump(&shared.counters.refused, 1);
                drop(inbound);
            }
            Mode::Blackhole => {
                bump(&shared.counters.held, 1);
                lock(&shared.held).push(inbound);
            }
            Mode::Up => {
                bump(&shared.counters.accepted, 1);
                let conn = tokio::spawn(connection(shared.clone(), inbound, n));
                n += 1;
                let mut live = lock(&shared.live);
                live.retain(|conn| !conn.is_finished());
                live.push(conn.abort_handle());
            }
        }
    }
}

/// Why a pipe stopped early.
enum Stop {
    Io,
    /// The byte budget ran out.
    Dropped,
}

impl From<io::Error> for Stop {
    fn from(_: io::Error) -> Stop {
        Stop::Io
    }
}

/// Bytes a connection may still carry; `u64::MAX` is unlimited.
struct Budget(AtomicU64);

impl Budget {
    fn set(&self, bytes: Option<u64>) {
        self.0.store(bytes.unwrap_or(u64::MAX), Ordering::SeqCst);
    }

    /// Takes up to `want` bytes, returning how many may be written.
    fn take(&self, want: usize) -> usize {
        let mut got = 0;
        let _ = self
            .0
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                got = left.min(want as u64);
                Some(if left == u64::MAX { left } else { left - got })
            });
        got as usize
    }
}

async fn connection(shared: Arc<Shared>, inbound: TcpStream, n: u64) {
    let upstream = *lock(&shared.upstream);
    let Ok(outbound) = TcpStream::connect(upstream).await else {
        return;
    };
    let _ = inbound.set_nodelay(true);
    let _ = outbound.set_nodelay(true);
    let mut rng = shared.rng.fork(n);
    let mut changes = shared.profile.subscribe();
    let budget = Budget(AtomicU64::new(u64::MAX));
    // Draws this connection's drops from the current profile.
    let arm = |rng: &mut Rng, changes: &mut watch::Receiver<LinkProfile>| {
        let profile = changes.borrow_and_update().clone();
        budget.set(profile.drop_after_bytes.map(|bytes| rng.in_range(bytes)));
        profile
            .drop_after
            .map(|time| Instant::now() + rng.duration_in(time))
    };
    let mut deadline = arm(&mut rng, &mut changes);

    let (in_read, in_write) = inbound.into_split();
    let (out_read, out_write) = outbound.into_split();
    let up = pipe(&shared, Dir::Up, in_read, out_write, rng.fork(0), &budget);
    let down = pipe(&shared, Dir::Down, out_read, in_write, rng.fork(1), &budget);
    let pipes = async { tokio::try_join!(up, down) };
    tokio::pin!(pipes);
    loop {
        let timer = async {
            match deadline {
                Some(at) => sleep_until(at).await,
                None => pending().await,
            }
        };
        tokio::select! {
            ended = &mut pipes => {
                if let Err(Stop::Dropped) = ended {
                    bump(&shared.counters.dropped_bytes, 1);
                }
                break;
            }
            () = timer => {
                bump(&shared.counters.dropped_time, 1);
                break;
            }
            Ok(()) = changes.changed() => deadline = arm(&mut rng, &mut changes),
        }
    }
    // Both halves of both sockets are dropped here: the connection closes.
}

/// One direction: a reader stamping chunks with their delivery time and a
/// writer delivering them in order, paced and within the byte budget.
async fn pipe(
    shared: &Shared,
    dir: Dir,
    mut from: OwnedReadHalf,
    mut to: OwnedWriteHalf,
    mut rng: Rng,
    budget: &Budget,
) -> Result<(), Stop> {
    // `None` is end of stream: half-close the other side.
    let (tx, mut rx) = mpsc::channel::<Option<(Instant, Vec<u8>)>>(IN_FLIGHT);
    let read = async move {
        let mut last = Instant::now();
        loop {
            let mut buf = vec![0u8; READ_BUF];
            let n = from.read(&mut buf).await?;
            if n == 0 {
                let _ = tx.send(None).await;
                return Ok::<(), Stop>(());
            }
            buf.truncate(n);
            let (latency, jitter) = {
                let profile = shared.profile.borrow();
                (profile.latency, profile.jitter)
            };
            let at = (Instant::now() + latency + rng.duration(jitter)).max(last);
            last = at;
            if tx.send(Some((at, buf))).await.is_err() {
                return Ok(());
            }
        }
    };
    let write = async move {
        while let Some(chunk) = rx.recv().await {
            let Some((at, bytes)) = chunk else {
                to.shutdown().await?;
                return Ok::<(), Stop>(());
            };
            sleep_until(at).await;
            for slice in bytes.chunks(SLICE) {
                shared.pace(dir, slice.len()).await;
                let allowed = budget.take(slice.len());
                to.write_all(&slice[..allowed]).await?;
                bump(&shared.counters.bytes[dir as usize], allowed as u64);
                if allowed < slice.len() {
                    return Err(Stop::Dropped);
                }
            }
        }
        Ok(())
    };
    tokio::try_join!(read, write).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOON: Duration = Duration::from_secs(10);

    /// An echo server; returns its address.
    async fn echo() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut conn, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let (mut r, mut w) = conn.split();
                    let _ = tokio::io::copy(&mut r, &mut w).await;
                });
            }
        });
        addr
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    /// Writes `data` and reads back up to `data.len()` bytes until EOF.
    async fn round_trip(proxy: &LinkProxy, data: &[u8]) -> Vec<u8> {
        let conn = TcpStream::connect(proxy.addr()).await.unwrap();
        let (mut r, mut w) = conn.into_split();
        let data = data.to_vec();
        let writer = tokio::spawn(async move {
            let _ = w.write_all(&data).await;
            let _ = w.shutdown().await;
        });
        let mut got = Vec::new();
        let _ = tokio::time::timeout(SOON, r.read_to_end(&mut got))
            .await
            .expect("round trip in time");
        let _ = writer.await;
        got
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn jitter_keeps_byte_order() {
        let profile = LinkProfile::clean()
            .latency(Duration::from_millis(2))
            .jitter(Duration::from_millis(20));
        let proxy = LinkProxy::start(echo().await, profile, 1).await;
        let data = pattern(512 * 1024);
        assert_eq!(round_trip(&proxy, &data).await, data);
        let stats = proxy.stats();
        assert_eq!(stats.bytes_up, data.len() as u64);
        assert_eq!(stats.bytes_down, data.len() as u64);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn latency_and_bandwidth_slow_the_link() {
        let proxy = LinkProxy::start(
            echo().await,
            LinkProfile::clean().latency(Duration::from_millis(100)),
            2,
        )
        .await;
        let started = Instant::now();
        assert_eq!(round_trip(&proxy, b"ping").await, b"ping");
        assert!(started.elapsed() >= Duration::from_millis(200), "both ways");

        // 64 KiB at 256 KiB/s each way, pipelined: about a quarter second.
        proxy.set_profile(LinkProfile::clean().bandwidth(256 * 1024));
        let data = pattern(64 * 1024);
        let started = Instant::now();
        assert_eq!(round_trip(&proxy, &data).await, data);
        assert!(started.elapsed() >= Duration::from_millis(230));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn byte_budget_cuts_mid_stream() {
        let proxy = LinkProxy::start(
            echo().await,
            LinkProfile::clean().drop_after_bytes(10_000..=10_000),
            3,
        )
        .await;
        let data = pattern(100_000);
        let got = round_trip(&proxy, &data).await;
        assert!(got.len() < 10_000, "got {}", got.len());
        assert_eq!(got, data[..got.len()], "a prefix, in order");
        let stats = proxy.stats();
        assert_eq!(stats.bytes_up + stats.bytes_down, 10_000);
        assert_eq!(stats.dropped_bytes, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn timer_drops_idle_connections() {
        let range = Duration::from_millis(50)..=Duration::from_millis(80);
        let proxy = LinkProxy::start(echo().await, LinkProfile::clean().drop_after(range), 4).await;
        let mut conn = TcpStream::connect(proxy.addr()).await.unwrap();
        let mut buf = [0u8; 1];
        let read = tokio::time::timeout(SOON, conn.read(&mut buf))
            .await
            .unwrap();
        assert!(matches!(read, Ok(0) | Err(_)), "closed: {read:?}");
        assert_eq!(proxy.stats().dropped_time, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn new_profile_rearms_live_connections() {
        let proxy = LinkProxy::start(echo().await, LinkProfile::clean(), 6).await;
        let mut conn = TcpStream::connect(proxy.addr()).await.unwrap();
        conn.write_all(b"x").await.unwrap();
        let mut buf = [0u8; 1];
        conn.read_exact(&mut buf).await.unwrap();

        // The connection was accepted clean; the new profile still drops it.
        proxy.set_profile(LinkProfile::clean().drop_after_bytes(10..=10));
        conn.write_all(&[7u8; 100]).await.unwrap();
        let mut got = Vec::new();
        let _ = tokio::time::timeout(SOON, conn.read_to_end(&mut got))
            .await
            .unwrap();
        assert!(got.len() <= 10, "got {}", got.len());
        assert_eq!(proxy.stats().dropped_bytes, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn down_blackhole_cut_and_up() {
        let proxy = LinkProxy::start(echo().await, LinkProfile::clean(), 5).await;
        let mut live = TcpStream::connect(proxy.addr()).await.unwrap();
        live.write_all(b"x").await.unwrap();
        let mut buf = [0u8; 1];
        live.read_exact(&mut buf).await.unwrap();

        proxy.cut();
        let read = tokio::time::timeout(SOON, live.read(&mut buf))
            .await
            .unwrap();
        assert!(matches!(read, Ok(0) | Err(_)), "cut: {read:?}");

        proxy.down();
        assert!(round_trip(&proxy, b"hello").await.is_empty(), "refused");

        proxy.blackhole();
        let mut held = TcpStream::connect(proxy.addr()).await.unwrap();
        held.write_all(b"hello").await.unwrap();
        let silent = tokio::time::timeout(Duration::from_millis(200), held.read(&mut buf)).await;
        assert!(silent.is_err(), "nothing comes back while blackholed");

        proxy.up();
        assert_eq!(round_trip(&proxy, b"hello").await, b"hello");
        let stats = proxy.stats();
        assert_eq!(stats.cut, 1);
        assert_eq!(stats.refused, 1);
        assert_eq!(stats.held, 1);
    }
}
