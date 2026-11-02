//! The sync engine (architecture.md "Client core", D7): the single consumer of the core's
//! [`Request`]s, and the loops that keep the replica in step with the
//! server.
//!
//! [`Core::open`](crate::Core::open) starts it on the core's own tokio
//! runtime. It answers requests as follows:
//!
//! - `Pair` runs [`pair_and_report`] with [`EngineConfig::device_name`];
//!   success asks for `Connect` itself.
//! - `Connect` (re)starts the link with the stored identity and address,
//!   also after `Rejected`.
//! - `OutboxChanged` wakes the outbox sender.
//! - `RetryNow` cuts short the link's backoff wait, if it is waiting, to
//!   [`Backoff::RETRY_NOW_MIN`] after the failure.
//! - `DownloadBlob` and `BlobPolicyChanged` go to the blob queue; the blob
//!   downloader (see [`crate::blobs`]) runs beside the link, on its own
//!   task and connections, so blobs never hold up the pull or the sender.
//! - `ListDirs` lists a server directory on its own task within
//!   [`DIRS_TIMEOUT`], on a side transport kept for listings and workdir
//!   reads, and answers with [`Seam::dirs_listed`]; unpaired, suspended or
//!   rejected, it fails at once. A failure never touches the link.
//! - `Workdir` reads a session's workdir the same way, within
//!   [`WORKDIR_TIMEOUT`], and answers with [`Seam::workdir_answered`].
//!
//! **The link** runs while the device is paired, not rejected and not
//! suspended. Each connection starts with `hello`, then runs two loops side
//! by side until either fails:
//! - the *pull loop* long-polls `GET /v1/events` from the store's cursor and
//!   applies every batch in one transaction ([`Seam::apply_events`]);
//! - the *outbox sender* sends the oldest pending command, marking it
//!   `Sending` first, and waits for its `CommandResult` before the next, so
//!   the server sees commands in the order they were made. A failed send is
//!   repeated with the same command id; the server dedups (D7).
//!
//! `Link` moves as follows: `Connecting` when the link (re)starts; after
//! `hello`, `CatchingUp { have, server }` while the cursor is behind the
//! head `hello` reported (or a later batch revealed), else
//! `Online { latency_ms }` with `hello`'s round trip — blob downloads play
//! no part: metadata synced counts as synced (D6). Any failure ends the
//! connection: a rejection (401/403, protocol mismatch) shows
//! `Rejected(..)` and stops the link until the next `Connect`; anything
//! else shows `Reconnecting { attempt, retry_at, offline_since }`, waits
//! [`Backoff::delay`] and connects again, still showing `Reconnecting`
//! until `hello` succeeds (`RetryNow` ends the wait early and moves
//! `retry_at` to match, without touching the bookkeeping). Both times are
//! wall clock ([`EngineConfig::wall_clock`]), read once per failure. The
//! wait ends once either clock says the delay has passed: the monotonic
//! clock stops while a device sleeps, so the link also checks the wall
//! clock every [`WALL_CHECK`] and retries soon after the `retry_at` it
//! showed, and a `RetryNow` after a long sleep retries at once. The
//! attempt count and `offline_since` reset once
//! a pull succeeds, so a server that answers `hello` and then fails keeps
//! backing off.
//!
//! [`Core::suspend`](crate::Core::suspend) stops the link and the
//! downloader (in-flight requests are dropped; an unanswered command is
//! resent later with the same id; an interrupted blob resumes from its
//! partial file) and [`Core::resume`](crate::Core::resume) starts them
//! again from `Connecting`. Pairing runs regardless.
//!
//! Pairing with another server resets the replica
//! ([`Seam::pairing_succeeded`]) while a link to the old one may still be
//! running; the link and downloader write through a [`Seam`] bound to the
//! server they started for, which refuses their writes from then on, until
//! the `Connect` that follows restarts them.

use std::collections::HashSet;
use std::convert::Infallible;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use skywave_proto::{EventsQuery, Seq};
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::JoinHandle;

use crate::backoff::Backoff;
use crate::blobs::{BlobCtx, BlobQueue, run_downloader};
use crate::client::CoreError;
use crate::https::{HttpsTransport, TransportConfig};
use crate::identity::Identity;
use crate::pairing::pair_and_report;
use crate::seam::{Request, Seam};
use crate::store::{OutboxEntry, StoreError};
use crate::transport::{Transport, TransportError};
use crate::view::{CommandId, Link, LinkRejection};
use crate::workdir::{WORKDIR_TIMEOUT, WorkdirAnswer, WorkdirAsk, not_connected};

/// Long-poll wait the engine asks for by default; the server caps it at 60.
pub const DEFAULT_WAIT_SECS: u32 = 30;
/// Longest a directory listing for completion may take, connecting included.
pub const DIRS_TIMEOUT: Duration = Duration::from_secs(5);
/// Longest the link sleeps in a reconnect wait before it looks at the wall
/// clock again. The monotonic clock stops while a device sleeps (Android's
/// deep sleep), so after waking the attempt comes at most this long after
/// the `retry_at` the view shows. Only waits longer than this tick more
/// than once, and only while the device is awake.
pub const WALL_CHECK: Duration = Duration::from_secs(5);

/// How the sync engine talks to the server; [`Default`] is what heads use.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Timeouts for pairing and the link.
    pub transport: TransportConfig,
    /// Seconds a pull waits for new events (`wait=`).
    pub wait_secs: u32,
    /// Events per pull (`limit=`); `None` leaves it to the server (500).
    pub limit: Option<u32>,
    pub backoff: Backoff,
    /// Name sent when pairing, shown by `skywaved devices list`; `None`
    /// uses [`default_device_name`].
    pub device_name: Option<String>,
    /// Builds the transport for the paired server.
    pub connector: Connector,
    /// Where the link reads wall time.
    pub wall_clock: WallClock,
}

impl Default for EngineConfig {
    fn default() -> Self {
        EngineConfig {
            transport: TransportConfig::default(),
            wait_secs: DEFAULT_WAIT_SECS,
            limit: None,
            backoff: Backoff::default(),
            device_name: None,
            connector: Connector::https(),
            wall_clock: WallClock::system(),
        }
    }
}

type Connect = dyn Fn(&str, &dyn Identity, &TransportConfig) -> Result<Arc<dyn Transport>, TransportError>
    + Send
    + Sync;

/// Builds the [`Transport`] to the server at an address (`host:port`) for
/// an identity. [`Connector::https`] by default; tests and the link harness
/// wrap or replace it.
#[derive(Clone)]
pub struct Connector(Arc<Connect>);

impl Connector {
    /// [`HttpsTransport`].
    pub fn https() -> Connector {
        Connector::new(|address, identity, config| {
            Ok(
                Arc::new(HttpsTransport::new(address, identity, config.clone())?)
                    as Arc<dyn Transport>,
            )
        })
    }

    pub fn new(
        connect: impl Fn(
            &str,
            &dyn Identity,
            &TransportConfig,
        ) -> Result<Arc<dyn Transport>, TransportError>
        + Send
        + Sync
        + 'static,
    ) -> Connector {
        Connector(Arc::new(connect))
    }

    pub(crate) fn connect(
        &self,
        address: &str,
        identity: &dyn Identity,
        config: &TransportConfig,
    ) -> Result<Arc<dyn Transport>, TransportError> {
        (self.0)(address, identity, config)
    }
}

impl fmt::Debug for Connector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Connector")
    }
}

/// The wall clock the link stamps `Link::Reconnecting` with and checks its
/// wait against. [`WallClock::system`] by default; tests inject one that
/// moves with tokio's paused clock, or jumps as if the device had slept.
#[derive(Clone)]
pub struct WallClock(Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>);

impl WallClock {
    /// [`Utc::now`].
    pub fn system() -> WallClock {
        WallClock::new(Utc::now)
    }

    pub fn new(now: impl Fn() -> DateTime<Utc> + Send + Sync + 'static) -> WallClock {
        WallClock(Arc::new(now))
    }

    pub(crate) fn now(&self) -> DateTime<Utc> {
        (self.0)()
    }
}

impl fmt::Debug for WallClock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WallClock")
    }
}

/// This machine's host name, for pairing; "skywave device" when there is
/// none to be found.
pub fn default_device_name() -> String {
    let from_file = ["/proc/sys/kernel/hostname", "/etc/hostname"]
        .into_iter()
        .filter_map(|path| std::fs::read_to_string(path).ok());
    let from_env = ["HOSTNAME", "COMPUTERNAME"]
        .into_iter()
        .filter_map(|var| std::env::var(var).ok());
    from_file
        .chain(from_env)
        .map(|name| name.trim().to_owned())
        .find(|name| !name.is_empty())
        .unwrap_or_else(|| "skywave device".to_owned())
}

/// The engine's state between requests. Runs on the core's runtime.
pub(crate) struct Engine {
    seam: Seam,
    config: Arc<EngineConfig>,
    device_name: String,
    requests: mpsc::UnboundedReceiver<Request>,
    suspended: watch::Receiver<bool>,
    /// Wakes the outbox sender; outlives each link so no wake-up is lost.
    outbox: Arc<Notify>,
    /// Ends the link's backoff wait; outlives each link.
    retry_now: Arc<Notify>,
    link: Option<JoinHandle<()>>,
    /// The blob downloader, running while the link does.
    downloader: Option<JoinHandle<Infallible>>,
    /// The server refused the device; no link until the next `Connect`.
    rejected: watch::Sender<bool>,
    /// Blobs waiting for the downloader; outlives each downloader.
    blobs: Arc<BlobQueue>,
    /// The transport kept for listings and workdir reads, built on the
    /// first; dropped whenever the link restarts, as the identity may have
    /// changed.
    side: Option<Arc<dyn Transport>>,
}

impl Engine {
    pub(crate) fn new(
        seam: Seam,
        config: EngineConfig,
        requests: mpsc::UnboundedReceiver<Request>,
        suspended: watch::Receiver<bool>,
    ) -> Engine {
        let device_name = config
            .device_name
            .clone()
            .unwrap_or_else(default_device_name);
        Engine {
            seam: seam.clone(),
            config: Arc::new(config),
            device_name,
            requests,
            suspended,
            outbox: Arc::new(Notify::new()),
            retry_now: Arc::new(Notify::new()),
            link: None,
            downloader: None,
            rejected: watch::Sender::new(false),
            blobs: Arc::new(BlobQueue::restore(&seam)),
            side: None,
        }
    }

    /// Serves requests until the core goes away.
    pub(crate) async fn run(mut self) {
        self.start_link();
        loop {
            tokio::select! {
                request = self.requests.recv() => match request {
                    Some(request) => self.handle(request),
                    None => break,
                },
                changed = self.suspended.changed() => match changed {
                    Ok(()) if *self.suspended.borrow_and_update() => self.stop_link(),
                    Ok(()) => self.start_link(),
                    Err(_) => break,
                },
            }
        }
        self.stop_link();
    }

    fn handle(&mut self, request: Request) {
        match request {
            Request::Pair {
                address,
                code,
                fingerprint,
            } => {
                let seam = self.seam.clone();
                let config = self.config.clone();
                let name = self.device_name.clone();
                tokio::spawn(async move {
                    pair_and_report(
                        &seam,
                        &address,
                        &code,
                        &fingerprint,
                        &name,
                        &config.transport,
                    )
                    .await;
                });
            }
            Request::Connect => {
                self.rejected.send_replace(false);
                self.start_link();
            }
            Request::OutboxChanged => self.outbox.notify_one(),
            // Only a link already waiting is woken: no permit is stored, so
            // a press that lands mid-connect cannot skip the next wait.
            Request::RetryNow => self.retry_now.notify_waiters(),
            Request::DownloadBlob(id) => self.blobs.want(id),
            Request::BlobsPurged(ids) => self.blobs.purged(&ids),
            Request::BlobPolicyChanged(policy) => {
                let blobs = self.seam.blobs().unwrap_or_else(|error| {
                    tracing::warn!(%error, "cannot read blobs; queueing none");
                    Vec::new()
                });
                self.blobs.set_policy(policy, &blobs);
            }
            Request::ListDirs { req, dir } => {
                let seam = self.seam.clone();
                let Some(transport) = self.side_transport() else {
                    seam.dirs_listed(req, Err(not_connected()));
                    return;
                };
                tokio::spawn(async move {
                    let result = tokio::time::timeout(DIRS_TIMEOUT, transport.list_dirs(&dir))
                        .await
                        .unwrap_or(Err(TransportError::Timeout));
                    seam.dirs_listed(req, result);
                });
            }
            Request::Workdir { req, ask } => {
                let seam = self.seam.clone();
                let Some(transport) = self.side_transport() else {
                    answer_workdir(&seam, req, ask, Err(not_connected()));
                    return;
                };
                tokio::spawn(async move {
                    let result =
                        tokio::time::timeout(WORKDIR_TIMEOUT, read_workdir(&*transport, &ask))
                            .await
                            .unwrap_or(Err(TransportError::Timeout));
                    answer_workdir(&seam, req, ask, result);
                });
            }
        }
    }

    /// The side transport, for listings and workdir reads, if the link may
    /// run.
    fn side_transport(&mut self) -> Option<Arc<dyn Transport>> {
        if *self.suspended.borrow() || *self.rejected.borrow() {
            return None;
        }
        if let Some(transport) = &self.side {
            return Some(transport.clone());
        }
        let identity = self.seam.identity()?;
        let address = self.seam.settings().ok()?.server_address?;
        let transport = self
            .config
            .connector
            .connect(&address, identity.as_ref(), &self.config.transport)
            .inspect_err(|error| tracing::debug!(%error, "no side transport"))
            .ok()?;
        self.side = Some(transport.clone());
        Some(transport)
    }

    /// (Re)starts the link and the blob downloader if the device is
    /// paired, not rejected and not suspended.
    fn start_link(&mut self) {
        self.stop_link();
        self.side = None;
        if *self.suspended.borrow() || *self.rejected.borrow() {
            return;
        }
        // Bound: if the device pairs with another server, this link and
        // downloader can no longer write to the replica.
        let Some((seam, identity)) = self.seam.bind() else {
            return;
        };
        let address = match seam.settings() {
            Ok(settings) => settings.server_address,
            Err(error) => {
                tracing::warn!(%error, "cannot read the server address; not connecting");
                None
            }
        };
        let Some(address) = address else {
            return;
        };
        let downloader = tokio::spawn(run_downloader(BlobCtx {
            seam: seam.clone(),
            config: self.config.clone(),
            address: address.clone(),
            identity: identity.clone(),
            queue: self.blobs.clone(),
        }));
        let stop_downloader = downloader.abort_handle();
        self.downloader = Some(downloader);
        let ctx = LinkCtx {
            seam,
            config: self.config.clone(),
            outbox: self.outbox.clone(),
            retry_now: self.retry_now.clone(),
            blobs: self.blobs.clone(),
            address,
            identity,
        };
        let rejected = self.rejected.clone();
        self.link = Some(tokio::spawn(async move {
            let rejection = run_link(ctx).await;
            tracing::warn!(?rejection, "the server refused this device; sync stopped");
            stop_downloader.abort();
            rejected.send_replace(true);
        }));
    }

    fn stop_link(&mut self) {
        if let Some(link) = self.link.take() {
            link.abort();
        }
        if let Some(downloader) = self.downloader.take() {
            downloader.abort();
        }
    }
}

/// One workdir read, as `ask` says.
async fn read_workdir(
    transport: &dyn Transport,
    ask: &WorkdirAsk,
) -> Result<WorkdirAnswer, TransportError> {
    let query = ask.query();
    match ask {
        WorkdirAsk::Changes { .. } => transport
            .workdir_changes(&query)
            .await
            .map(WorkdirAnswer::Changes),
        WorkdirAsk::List { .. } => transport
            .workdir_list(&query)
            .await
            .map(WorkdirAnswer::List),
        WorkdirAsk::File { .. } => transport
            .workdir_file(&query)
            .await
            .map(WorkdirAnswer::File),
    }
}

fn answer_workdir(
    seam: &Seam,
    req: u64,
    ask: WorkdirAsk,
    result: Result<WorkdirAnswer, TransportError>,
) {
    if let Err(error) = seam.workdir_answered(req, ask, result) {
        tracing::warn!(%error, "cannot keep a workdir answer");
    }
}

/// What one link needs.
pub(crate) struct LinkCtx {
    pub(crate) seam: Seam,
    pub(crate) config: Arc<EngineConfig>,
    pub(crate) outbox: Arc<Notify>,
    /// Ends a backoff wait early.
    pub(crate) retry_now: Arc<Notify>,
    /// Where an `Eager` policy queues the blobs a batch registers.
    pub(crate) blobs: Arc<BlobQueue>,
    pub(crate) address: String,
    pub(crate) identity: Arc<dyn Identity>,
}

/// Why a connection ended.
#[derive(Debug, thiserror::Error)]
enum LinkError {
    #[error(transparent)]
    Transport(#[from] TransportError),
    /// The replica could not be read or written; retried like a link
    /// failure.
    #[error(transparent)]
    Core(#[from] CoreError),
}

impl LinkError {
    fn rejection(&self) -> Option<LinkRejection> {
        match self {
            LinkError::Transport(e) => e.rejection(),
            LinkError::Core(_) => None,
        }
    }
}

/// Reports `Link` changes, skipping repeats.
struct LinkReporter {
    seam: Seam,
    last: Option<Link>,
}

impl LinkReporter {
    fn set(&mut self, link: Link) {
        if self.last.as_ref() != Some(&link) {
            self.seam.set_link(link.clone());
            self.last = Some(link);
        }
    }
}

/// Reconnect bookkeeping, reset by a successful pull.
#[derive(Default)]
struct Retry {
    attempt: u32,
    offline_since: Option<DateTime<Utc>>,
}

impl Retry {
    fn reset(&mut self) {
        *self = Retry::default();
    }
}

/// Keeps the link up until the server rejects the device, and returns why.
pub(crate) async fn run_link(ctx: LinkCtx) -> LinkRejection {
    let mut reporter = LinkReporter {
        seam: ctx.seam.clone(),
        last: None,
    };
    reporter.set(Link::Connecting);
    let mut retry = Retry::default();
    let mut transport: Option<Arc<dyn Transport>> = None;
    loop {
        let error = match &transport {
            Some(transport) => {
                connection(&ctx, transport.as_ref(), &mut reporter, &mut retry).await
            }
            None => {
                match ctx.config.connector.connect(
                    &ctx.address,
                    ctx.identity.as_ref(),
                    &ctx.config.transport,
                ) {
                    Ok(built) => {
                        transport = Some(built);
                        continue;
                    }
                    Err(error) => LinkError::Transport(error),
                }
            }
        };
        if let Some(rejection) = error.rejection() {
            reporter.set(Link::Rejected(rejection));
            return rejection;
        }
        retry.attempt = retry.attempt.saturating_add(1);
        // The view gets the failure and the deadline as wall time, read once
        // here so every snapshot of this wait carries the same values. The
        // wait ends when either clock says the delay has passed: the
        // monotonic one survives wall-clock changes, the wall one keeps
        // counting while the device sleeps.
        let failed_at = tokio::time::Instant::now();
        let failed_wall = ctx.config.wall_clock.now();
        let offline_since = *retry.offline_since.get_or_insert(failed_wall);
        let next_in = ctx.config.backoff.delay(retry.attempt);
        tracing::debug!(%error, attempt = retry.attempt, ?next_in, "link down");
        let mut wait = Wait {
            failed_at,
            failed_wall,
            delay: next_in,
        };
        reporter.set(Link::Reconnecting {
            attempt: retry.attempt,
            retry_at: wait.retry_at(),
            offline_since,
        });
        let retry_now = ctx.retry_now.notified();
        tokio::pin!(retry_now);
        let mut pressed = false;
        loop {
            let left = wait.left(ctx.config.wall_clock.now());
            if left.is_zero() {
                break;
            }
            tokio::select! {
                () = tokio::time::sleep(left.min(WALL_CHECK)) => {}
                // Further presses are ignored until the attempt fails.
                () = &mut retry_now, if !pressed => {
                    pressed = true;
                    wait.delay = wait.delay.min(Backoff::RETRY_NOW_MIN);
                    tracing::debug!(after = ?wait.delay, "retrying now");
                    reporter.set(Link::Reconnecting {
                        attempt: retry.attempt,
                        retry_at: wait.retry_at(),
                        offline_since,
                    });
                }
            }
        }
    }
}

/// A reconnect wait: `delay` after a failure seen at `failed_at` on the
/// monotonic clock and `failed_wall` on the wall clock.
struct Wait {
    failed_at: tokio::time::Instant,
    failed_wall: DateTime<Utc>,
    delay: Duration,
}

impl Wait {
    /// When the next attempt is due, on the wall clock.
    fn retry_at(&self) -> DateTime<Utc> {
        self.failed_wall + TimeDelta::from_std(self.delay).unwrap_or(TimeDelta::MAX)
    }

    /// How much longer to wait, given the wall time `now`: the delay less
    /// what the further ahead of the two clocks says has passed.
    fn left(&self, now: DateTime<Utc>) -> Duration {
        let monotonic = self.failed_at.elapsed();
        let wall = (now - self.failed_wall).to_std().unwrap_or_default();
        self.delay.saturating_sub(monotonic.max(wall))
    }
}

/// One connection: `hello`, then pull and send until either fails.
async fn connection(
    ctx: &LinkCtx,
    transport: &dyn Transport,
    reporter: &mut LinkReporter,
    retry: &mut Retry,
) -> LinkError {
    let started = tokio::time::Instant::now();
    let hello = match transport.hello().await {
        Ok(hello) => hello,
        Err(error) => return error.into(),
    };
    let latency_ms = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);
    if let Err(error) = ctx.seam.hello_received(&hello) {
        return error.into();
    }
    let ended = tokio::select! {
        ended = pull(ctx, transport, reporter, retry, hello.head, latency_ms) => ended,
        ended = send(ctx, transport) => ended,
    };
    let Err(error) = ended;
    error
}

/// The pull loop: long-polls from the store's cursor and applies batches.
async fn pull(
    ctx: &LinkCtx,
    transport: &dyn Transport,
    reporter: &mut LinkReporter,
    retry: &mut Retry,
    mut head: Seq,
    latency_ms: u32,
) -> Result<Infallible, LinkError> {
    let wait = ctx.config.wait_secs;
    // Devices asked the server about on this connection, so one it does
    // not list costs one more `hello`, not one per batch.
    let mut asked = HashSet::new();
    loop {
        let cursor = ctx.seam.cursor()?;
        reporter.set(if cursor < head {
            Link::CatchingUp {
                have: cursor,
                server: head,
            }
        } else {
            Link::Online { latency_ms }
        });
        let batch = transport
            .events(EventsQuery {
                after: cursor,
                wait: Some(wait),
                limit: ctx.config.limit,
            })
            .await?;
        retry.reset();
        if batch.events.is_empty() {
            continue;
        }
        match ctx.seam.apply_events(&batch) {
            Ok(applied) => {
                head = head.max(applied.cursor);
                ctx.blobs.registered(&applied.blobs);
                // A command the log settled, or the log applied through a
                // head, may be what the sender waits on ([`settled_by_log`]).
                ctx.outbox.notify_one();
                // A device paired since this connection's `hello`: ask
                // again, so its name shows without waiting for a reconnect.
                let unnamed = ctx.seam.unnamed_devices(&batch);
                if unnamed.into_iter().filter(|id| asked.insert(*id)).count() > 0 {
                    ctx.seam.hello_received(&transport.hello().await?)?;
                }
            }
            Err(CoreError::Store(StoreError::Gap { cursor, from })) => {
                // The log is contiguous, so the server misbehaved; the next
                // connection pulls again from the store's cursor.
                tracing::warn!(?cursor, ?from, "pulled batch leaves a gap; re-pulling");
                return Err(CoreError::Store(StoreError::Gap { cursor, from }).into());
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// The outbox sender: one command at a time, oldest first, each until it
/// has a result.
///
/// Every command is claimed ([`Seam::command_sending`]) and the claim is
/// on disk before the request leaves, so the server never sees a command
/// with `tries == 0`. Any future batched send must claim every row of the
/// batch before the request leaves.
async fn send(ctx: &LinkCtx, transport: &dyn Transport) -> Result<Infallible, LinkError> {
    loop {
        let Some(entry) = ctx.seam.pending_outbox()?.into_iter().next() else {
            ctx.outbox.notified().await;
            continue;
        };
        send_entry(ctx, transport, &entry).await?;
    }
}

/// Claims and sends one outbox entry read earlier. An entry that can no
/// longer be claimed (gone from the outbox, or answered meanwhile) is
/// skipped. One the server refuses for good fails and the link stays up:
/// resending it would only hold back every later command.
///
/// An unreadable reply ([`TransportError::Decode`]) keeps the link up too:
/// the server answered, so the command's event, if it logged one, is on its
/// way to the pull on this connection, and settles it
/// ([`Seam::apply_events`]); if it logged none, the log shows that too
/// ([`settled_by_log`]). The entry waits for either, holding back the later
/// ones (D7); resending at once would most likely bring back the same
/// unreadable reply before the pull can apply the event.
async fn send_entry(
    ctx: &LinkCtx,
    transport: &dyn Transport,
    entry: &OutboxEntry,
) -> Result<(), LinkError> {
    let id = entry.command.id;
    if !ctx.seam.command_sending(id)? {
        return Ok(());
    }
    match transport.send_command(&entry.command).await {
        Ok(result) => ctx.seam.command_result(id, &result)?,
        Err(error) if error.refuses_command() => {
            tracing::warn!(%error, ?id, "server refused the command; giving up on it");
            ctx.seam.command_undeliverable(id)?;
        }
        Err(TransportError::Decode(error)) => {
            tracing::warn!(%error, ?id, "unreadable reply; waiting for the log to settle it");
            settled_by_log(ctx, transport, id).await?;
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

/// Waits until the log settles the outbox command `id`, whose reply was
/// unreadable: its event settles it as accepted, or, once the pull has
/// applied the log through the head read here without one, it fails as
/// never applied ([`Seam::command_unlogged`]). The head is read after the
/// reply came back, so it covers any event the command logged.
async fn settled_by_log(
    ctx: &LinkCtx,
    transport: &dyn Transport,
    id: CommandId,
) -> Result<(), LinkError> {
    let head = transport.hello().await?.head;
    loop {
        // Asked for before the look, so a batch in between still wakes it.
        let changed = ctx.outbox.notified();
        if ctx.seam.command_unlogged(id, head)? {
            return Ok(());
        }
        changed.await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use skywave_proto::{
        BlobId as ProtoBlobId, Command, CommandId, CommandKind, CommandResult, EventBatch,
        HelloResponse, PROTOCOL_VERSION, ServerId, SessionId,
    };

    use super::*;
    use crate::Core;
    use crate::backoff::Jitter;
    use crate::identity::PairedCredentials;
    use crate::store::OutboxStatus;
    use crate::transport::{BlobDownload, BoxFuture};
    use crate::view::{DeviceId, FailReason, Outgoing, Screen};

    /// A server that is down until `up` is set, then answers `hello` and
    /// long-polls that never end.
    #[derive(Default)]
    struct FlakyServer {
        up: AtomicBool,
        hellos: AtomicUsize,
    }

    impl Transport for FlakyServer {
        fn hello(&self) -> BoxFuture<'_, Result<HelloResponse, TransportError>> {
            Box::pin(async move {
                self.hellos.fetch_add(1, Ordering::SeqCst);
                if !self.up.load(Ordering::SeqCst) {
                    return Err(TransportError::Unreachable("down".into()));
                }
                Ok(HelloResponse {
                    protocol_version: PROTOCOL_VERSION,
                    server_id: ServerId::new(),
                    device_id: DeviceId::new(),
                    head: Seq::ZERO,
                    devices: Vec::new(),
                    shell: false,
                })
            })
        }

        fn send_command<'a>(
            &'a self,
            _command: &'a Command,
        ) -> BoxFuture<'a, Result<CommandResult, TransportError>> {
            Box::pin(async { Err(TransportError::Io("unused".into())) })
        }

        fn events(&self, query: EventsQuery) -> BoxFuture<'_, Result<EventBatch, TransportError>> {
            Box::pin(async move {
                if !self.up.load(Ordering::SeqCst) {
                    return Err(TransportError::Io("dropped".into()));
                }
                tokio::time::sleep(Duration::from_secs(u64::from(query.wait.unwrap_or(0)))).await;
                Ok(EventBatch {
                    from: Seq(query.after.0 + 1),
                    events: Vec::new(),
                })
            })
        }

        fn blob(
            &self,
            _id: ProtoBlobId,
            _from: u64,
        ) -> BoxFuture<'_, Result<BlobDownload, TransportError>> {
            Box::pin(async { Err(TransportError::NotFound) })
        }
    }

    /// A wall clock that moves with tokio's paused clock, plus a handle to
    /// push it ahead on its own, as a sleeping device's wall clock does
    /// while its monotonic clock stands still.
    fn paused_wall() -> (WallClock, Arc<Mutex<TimeDelta>>) {
        let start_wall = Utc::now();
        let start = tokio::time::Instant::now();
        let slept = Arc::new(Mutex::new(TimeDelta::zero()));
        let jump = slept.clone();
        let clock = WallClock::new(move || {
            start_wall + TimeDelta::from_std(start.elapsed()).unwrap() + *jump.lock().unwrap()
        });
        (clock, slept)
    }

    /// The `attempt` of every `Reconnecting` published so far.
    fn attempts(links: &[Link]) -> Vec<u32> {
        links
            .iter()
            .filter_map(|l| match l {
                Link::Reconnecting { attempt, .. } => Some(*attempt),
                _ => None,
            })
            .collect()
    }

    /// Waits (on the paused clock) until `server` has seen `n` hellos.
    async fn hellos(server: &FlakyServer, n: usize) {
        while server.hellos.load(Ordering::SeqCst) < n {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn paired_seam(dir: &std::path::Path) -> Seam {
        let core = Core::open_detached(dir).unwrap();
        let seam = core.seam();
        seam.pairing_succeeded(
            "srv:7443",
            PairedCredentials {
                device_id: DeviceId::new(),
                device_key_pem: "KEY".into(),
                device_cert_pem: "CERT".into(),
                ca_cert_pem: "CA".into(),
            },
        )
        .unwrap();
        seam
    }

    /// A link still running for the old server cannot write once the
    /// device pairs with another one.
    #[test]
    fn a_link_bound_to_the_old_server_cannot_write_after_pairing_elsewhere() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let (bound, _) = seam.bind().unwrap();
        let session_id = SessionId::new();
        let batch = |from| EventBatch {
            from: Seq(from),
            events: vec![skywave_proto::Event {
                session_id,
                at: chrono::Utc::now(),
                command_id: Some(CommandId::new()),
                context: skywave_proto::ContextId::first_of(session_id),
                kind: skywave_proto::EventKind::SessionCreated {
                    workdir: "/srv".into(),
                    context_name: "start".into(),
                    from: None,
                },
            }],
        };
        bound.apply_events(&batch(1)).unwrap();

        seam.pairing_succeeded(
            "other:7443",
            PairedCredentials {
                device_id: DeviceId::new(),
                device_key_pem: "KEY".into(),
                device_cert_pem: "CERT".into(),
                ca_cert_pem: "ANOTHER CA".into(),
            },
        )
        .unwrap();
        assert!(matches!(
            bound.apply_events(&batch(1)),
            Err(CoreError::Superseded)
        ));
        bound.set_link(Link::Online { latency_ms: 1 });
        assert_eq!(seam.cursor().unwrap(), Seq::ZERO);
        assert_eq!(seam.core.view().link, Link::Connecting);

        let (fresh, _) = seam.bind().unwrap();
        fresh.apply_events(&batch(1)).unwrap();
        assert_eq!(seam.cursor().unwrap(), Seq(1));
    }

    /// Reconnect delays double per attempt up to the cap, with the injected
    /// jitter, on tokio's paused clock; a successful pull resets them. The
    /// view's `retry_at` is the wall-clock failure plus that delay and
    /// `offline_since` the first failure, fixed for the whole outage. Waits
    /// longer than `WALL_CHECK` still end on time.
    #[tokio::test(start_paused = true)]
    async fn backoff_progresses_and_resets_on_the_paused_clock() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let links = Arc::new(Mutex::new(Vec::new()));
        let sink = links.clone();
        seam.core
            .subscribe(move |view| sink.lock().unwrap().push(view.link));

        let server = Arc::new(FlakyServer::default());
        let fake = server.clone();
        let (wall_clock, _) = paused_wall();
        let config = EngineConfig {
            backoff: Backoff {
                base: Duration::from_secs(1),
                cap: Duration::from_secs(8),
                jitter: Jitter::fixed(1.0),
            },
            connector: Connector::new(move |_, _, _| Ok(fake.clone() as Arc<dyn Transport>)),
            wait_secs: 5,
            wall_clock,
            ..EngineConfig::default()
        };
        let ctx = LinkCtx {
            seam: seam.clone(),
            config: Arc::new(config),
            outbox: Arc::new(Notify::new()),
            retry_now: Arc::new(Notify::new()),
            blobs: Arc::new(BlobQueue::default()),
            address: "srv:7443".into(),
            identity: seam.identity().unwrap(),
        };
        let link = tokio::spawn(run_link(ctx));

        // The wall clock moves with the paused one and a failing `hello`
        // takes no time, so each failure lands on the previous `retry_at`:
        // `retry_at - offline_since` is the sum of the delays so far.
        let reconnects = |links: &[Link]| -> Vec<(u32, i64)> {
            links
                .iter()
                .filter_map(|l| match l {
                    Link::Reconnecting {
                        attempt,
                        retry_at,
                        offline_since,
                    } => Some((*attempt, (*retry_at - *offline_since).num_milliseconds())),
                    _ => None,
                })
                .collect()
        };
        let outages = |links: &[Link]| -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
            links
                .iter()
                .filter_map(|l| match l {
                    Link::Reconnecting {
                        retry_at,
                        offline_since,
                        ..
                    } => Some((*retry_at, *offline_since)),
                    _ => None,
                })
                .collect()
        };
        let wait_for = |n: usize| {
            let links = links.clone();
            async move {
                while reconnects(&links.lock().unwrap()).len() < n {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        };
        wait_for(6).await;
        assert_eq!(
            reconnects(&links.lock().unwrap())[..6],
            [
                (1, 1_000),
                (2, 3_000),
                (3, 7_000),
                (4, 15_000),
                (5, 23_000),
                (6, 31_000)
            ]
        );
        let first = outages(&links.lock().unwrap());
        assert!(
            first[..6].windows(2).all(|w| w[0].1 == w[1].1),
            "offline_since is kept"
        );
        assert!(
            first[..6].windows(2).all(|w| w[0].0 < w[1].0),
            "each failure moves retry_at"
        );
        assert_eq!(
            first[0].0 - first[0].1,
            TimeDelta::seconds(1),
            "the first retry is the failure plus its delay"
        );

        // Back up: online, and the next drop starts over at attempt 1.
        server.up.store(true, Ordering::SeqCst);
        while !matches!(links.lock().unwrap().last(), Some(Link::Online { .. })) {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // Let one pull succeed before the drop.
        tokio::time::sleep(Duration::from_secs(6)).await;
        let before = reconnects(&links.lock().unwrap()).len();
        server.up.store(false, Ordering::SeqCst);
        wait_for(before + 2).await;
        assert_eq!(
            reconnects(&links.lock().unwrap())[before..before + 2],
            [(1, 1_000), (2, 3_000)]
        );
        let second = outages(&links.lock().unwrap())[before].1;
        assert!(second > first[0].1, "offline_since starts over");
        link.abort();
    }

    /// `retry_now` cuts a backoff wait down to `RETRY_NOW_MIN` after the
    /// failure; the attempt count and `offline_since` carry on as if the
    /// wait had run out. Held down, it retries about once a second.
    #[tokio::test(start_paused = true)]
    async fn retry_now_skips_the_wait_and_keeps_the_bookkeeping() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let links = Arc::new(Mutex::new(Vec::new()));
        let sink = links.clone();
        seam.core
            .subscribe(move |view| sink.lock().unwrap().push(view.link));

        let server = Arc::new(FlakyServer::default());
        let fake = server.clone();
        let config = EngineConfig {
            backoff: Backoff {
                base: Duration::from_secs(60),
                cap: Duration::from_secs(60),
                jitter: Jitter::fixed(1.0),
            },
            connector: Connector::new(move |_, _, _| Ok(fake.clone() as Arc<dyn Transport>)),
            ..EngineConfig::default()
        };
        let retry_now = Arc::new(Notify::new());
        let ctx = LinkCtx {
            seam: seam.clone(),
            config: Arc::new(config),
            outbox: Arc::new(Notify::new()),
            retry_now: retry_now.clone(),
            blobs: Arc::new(BlobQueue::default()),
            address: "srv:7443".into(),
            identity: seam.identity().unwrap(),
        };
        let link = tokio::spawn(run_link(ctx));

        let reconnecting = |links: &[Link]| -> Vec<(u32, DateTime<Utc>)> {
            links
                .iter()
                .filter_map(|l| match l {
                    Link::Reconnecting {
                        attempt,
                        offline_since,
                        ..
                    } => Some((*attempt, *offline_since)),
                    _ => None,
                })
                .collect()
        };
        let retry_ats = |links: &[Link]| -> Vec<DateTime<Utc>> {
            links
                .iter()
                .filter_map(|l| match l {
                    Link::Reconnecting { retry_at, .. } => Some(*retry_at),
                    _ => None,
                })
                .collect()
        };
        let wait_for = |n: usize| {
            let links = links.clone();
            async move {
                while reconnecting(&links.lock().unwrap()).len() < n {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        };
        wait_for(1).await;
        // Another snapshot later in the same wait (here: a queued command)
        // carries the very same link value.
        let published = links.lock().unwrap().len();
        seam.core
            .dispatch(crate::Action::CreateSession {
                conversation: None,
                workdir: "/srv".into(),
            })
            .unwrap();
        while links.lock().unwrap().len() == published {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        {
            let mut links = links.lock().unwrap();
            assert_eq!(
                links[published - 1],
                links[published],
                "same wait, same link"
            );
            // One entry per link value from here on.
            links.truncate(published);
        }
        let failed = tokio::time::Instant::now();
        retry_now.notify_waiters();
        // The shortened deadline is reported, counted from the failure;
        // then the floor is kept.
        wait_for(2).await;
        let seen = reconnecting(&links.lock().unwrap());
        let failed_wall = seen[0].1;
        let full = Backoff {
            base: Duration::from_secs(60),
            cap: Duration::from_secs(60),
            jitter: Jitter::fixed(1.0),
        }
        .delay(1);
        assert_eq!(seen[1].0, 1, "still the first wait");
        assert_eq!(
            retry_ats(&links.lock().unwrap())[..2],
            [
                failed_wall + TimeDelta::from_std(full).unwrap(),
                failed_wall + TimeDelta::from_std(Backoff::RETRY_NOW_MIN).unwrap(),
            ]
        );
        wait_for(3).await;
        let waited = failed.elapsed();
        assert!(waited >= Backoff::RETRY_NOW_MIN, "floor kept: {waited:?}");
        assert!(waited < Duration::from_secs(2), "no 60s wait: {waited:?}");
        assert_eq!(server.hellos.load(Ordering::SeqCst), 2);
        let seen = reconnecting(&links.lock().unwrap());
        assert_eq!(seen[2].0, 2, "attempts keep counting");
        assert_eq!(seen[0].1, seen[2].1, "offline_since is kept");

        // Held down: a press every 30 ms for 5 s is about 5 attempts.
        let start = server.hellos.load(Ordering::SeqCst);
        for _ in 0..(5000 / 30) {
            retry_now.notify_waiters();
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        let held = server.hellos.load(Ordering::SeqCst) - start;
        assert!((4..=6).contains(&held), "{held} attempts in 5 s");
        link.abort();
    }

    /// A config whose link waits 30 s per attempt against a server that is
    /// down, on a wall clock that moves with the paused one; with the
    /// server and the handle that jumps the wall clock.
    fn long_waits() -> (Arc<FlakyServer>, Arc<Mutex<TimeDelta>>, EngineConfig) {
        let server = Arc::new(FlakyServer::default());
        let fake = server.clone();
        let (wall_clock, slept) = paused_wall();
        let config = EngineConfig {
            backoff: Backoff {
                base: Backoff::MAX,
                cap: Backoff::MAX,
                jitter: Jitter::fixed(1.0),
            },
            connector: Connector::new(move |_, _, _| Ok(fake.clone() as Arc<dyn Transport>)),
            wall_clock,
            ..EngineConfig::default()
        };
        (server, slept, config)
    }

    fn link_ctx(seam: &Seam, config: EngineConfig, retry_now: Arc<Notify>) -> LinkCtx {
        LinkCtx {
            seam: seam.clone(),
            config: Arc::new(config),
            outbox: Arc::new(Notify::new()),
            retry_now,
            blobs: Arc::new(BlobQueue::default()),
            address: "srv:7443".into(),
            identity: seam.identity().unwrap(),
        }
    }

    /// Without a sleep the full delay is waited out, `WALL_CHECK` ticks and
    /// all; after a sleep that stopped the monotonic clock but not the wall
    /// clock, the attempt comes within `WALL_CHECK` of waking, not after the
    /// monotonic remainder.
    #[tokio::test(start_paused = true)]
    async fn a_sleep_past_retry_at_retries_within_the_wall_check() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let (server, slept, config) = long_waits();
        // The first `hello` fails at once, on the paused clock.
        let failed = tokio::time::Instant::now();
        let link = tokio::spawn(run_link(link_ctx(&seam, config, Arc::new(Notify::new()))));

        tokio::time::sleep_until(failed + Backoff::MAX - Duration::from_millis(1)).await;
        assert_eq!(server.hellos.load(Ordering::SeqCst), 1, "full wait kept");
        tokio::time::sleep_until(failed + Backoff::MAX).await;
        tokio::task::yield_now().await;
        assert_eq!(server.hellos.load(Ordering::SeqCst), 2, "on time");

        // Asleep through the whole delay: only the wall clock moved.
        let woke = tokio::time::Instant::now();
        *slept.lock().unwrap() += TimeDelta::from_std(Backoff::MAX).unwrap();
        tokio::time::sleep_until(woke + WALL_CHECK).await;
        tokio::task::yield_now().await;
        assert_eq!(
            server.hellos.load(Ordering::SeqCst),
            3,
            "retried within WALL_CHECK of waking"
        );
        link.abort();
    }

    /// `RetryNow` after a sleep longer than its floor retries at once: the
    /// floor counts from the failure on the wall clock too.
    #[tokio::test(start_paused = true)]
    async fn retry_now_after_a_sleep_retries_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let (server, slept, config) = long_waits();
        let retry_now = Arc::new(Notify::new());
        let link = tokio::spawn(run_link(link_ctx(&seam, config, retry_now.clone())));

        hellos(&server, 1).await;
        // Let the link reach its wait before pressing.
        tokio::time::sleep(Duration::from_millis(10)).await;
        *slept.lock().unwrap() += TimeDelta::seconds(10);
        let woke = tokio::time::Instant::now();
        retry_now.notify_waiters();
        hellos(&server, 2).await;
        let waited = woke.elapsed();
        assert!(
            waited < Backoff::RETRY_NOW_MIN,
            "retried {waited:?} after the press"
        );
        link.abort();
    }

    /// `resume` restarts the link from `Connecting` at once, so a head that
    /// resumes after a sleep never sits on a passed `retry_at`.
    #[tokio::test(start_paused = true)]
    async fn resume_after_a_sleep_connects_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let links = Arc::new(Mutex::new(Vec::new()));
        let sink = links.clone();
        seam.core
            .subscribe(move |view| sink.lock().unwrap().push(view.link));
        let (server, slept, config) = long_waits();
        let (_requests, requests_rx) = mpsc::unbounded_channel();
        let (suspended, suspended_rx) = watch::channel(false);
        let engine =
            tokio::spawn(Engine::new(seam.clone(), config, requests_rx, suspended_rx).run());

        hellos(&server, 1).await;
        while attempts(&links.lock().unwrap()) != [1] {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        suspended.send_replace(true);
        *slept.lock().unwrap() += TimeDelta::from_std(Backoff::MAX).unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        links.lock().unwrap().clear();
        let woke = tokio::time::Instant::now();
        suspended.send_replace(false);
        hellos(&server, 2).await;
        let waited = woke.elapsed();
        assert!(
            waited < Backoff::RETRY_NOW_MIN,
            "connected {waited:?} after resume"
        );
        assert_eq!(links.lock().unwrap().first(), Some(&Link::Connecting));
        engine.abort();
    }

    /// Answers every command, first noting the `tries` its outbox row has
    /// on disk at that moment, read through a connection of its own.
    struct RecordingServer {
        db: std::path::PathBuf,
        seen: Mutex<Vec<(CommandId, u32)>>,
    }

    impl RecordingServer {
        fn new(dir: &std::path::Path) -> Self {
            RecordingServer {
                db: dir.join(crate::store::DB_FILE),
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    impl Transport for RecordingServer {
        fn hello(&self) -> BoxFuture<'_, Result<HelloResponse, TransportError>> {
            Box::pin(async { Err(TransportError::Io("unused".into())) })
        }

        fn send_command<'a>(
            &'a self,
            command: &'a Command,
        ) -> BoxFuture<'a, Result<CommandResult, TransportError>> {
            Box::pin(async move {
                let db = rusqlite::Connection::open(&self.db).unwrap();
                let tries: u32 = db
                    .query_row(
                        "SELECT tries FROM outbox WHERE command_id = ?1",
                        [command.id.0.to_string()],
                        |row| row.get(0),
                    )
                    .unwrap();
                let mut seen = self.seen.lock().unwrap();
                seen.push((command.id, tries));
                Ok(CommandResult::Accepted {
                    seq: Seq(seen.len() as u64),
                })
            })
        }

        fn events(&self, _query: EventsQuery) -> BoxFuture<'_, Result<EventBatch, TransportError>> {
            Box::pin(async { Err(TransportError::Io("unused".into())) })
        }

        fn blob(
            &self,
            _id: ProtoBlobId,
            _from: u64,
        ) -> BoxFuture<'_, Result<BlobDownload, TransportError>> {
            Box::pin(async { Err(TransportError::NotFound) })
        }
    }

    fn sender_ctx(seam: &Seam) -> LinkCtx {
        LinkCtx {
            seam: seam.clone(),
            config: Arc::new(EngineConfig::default()),
            outbox: Arc::new(Notify::new()),
            retry_now: Arc::new(Notify::new()),
            blobs: Arc::new(BlobQueue::default()),
            address: "srv:7443".into(),
            identity: seam.identity().unwrap(),
        }
    }

    fn enqueue(seam: &Seam, text: &str) -> Command {
        let command = Command {
            id: CommandId::new(),
            kind: CommandKind::SendMessage {
                session_id: SessionId::new(),
                context: skywave_proto::ContextId::first_of(SessionId::new()),
                text: text.into(),
            },
        };
        crate::client::lock(&seam.core.shared.state)
            .store
            .enqueue(&command)
            .unwrap();
        command
    }

    /// The claim is on disk before the request leaves: the server can
    /// never receive a command whose row still says `tries == 0`.
    #[tokio::test(start_paused = true)]
    async fn server_never_sees_an_unclaimed_command() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let commands = [enqueue(&seam, "one"), enqueue(&seam, "two")];
        let server = RecordingServer::new(dir.path());
        let ctx = sender_ctx(&seam);

        // The sender drains the outbox, then waits for more forever.
        let drained = tokio::time::timeout(Duration::from_secs(1), send(&ctx, &server)).await;
        assert!(drained.is_err(), "the sender only ends on an error");
        assert_eq!(
            *server.seen.lock().unwrap(),
            vec![(commands[0].id, 1), (commands[1].id, 1)]
        );
        assert!(seam.pending_outbox().unwrap().is_empty());
    }

    /// A row read as pending but gone (or answered) by the time of the
    /// claim is not sent.
    #[tokio::test]
    async fn send_skips_row_that_cannot_be_claimed() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let server = RecordingServer::new(dir.path());
        let ctx = sender_ctx(&seam);

        let deleted = enqueue(&seam, "deleted");
        let entry = seam.pending_outbox().unwrap().remove(0);
        rusqlite::Connection::open(&server.db)
            .unwrap()
            .execute(
                "DELETE FROM outbox WHERE command_id = ?1",
                [deleted.id.0.to_string()],
            )
            .unwrap();
        send_entry(&ctx, &server, &entry).await.unwrap();

        let answered = enqueue(&seam, "answered");
        let entry = seam.pending_outbox().unwrap().remove(0);
        seam.command_result(answered.id, &CommandResult::Accepted { seq: Seq(1) })
            .unwrap();
        send_entry(&ctx, &server, &entry).await.unwrap();

        assert!(server.seen.lock().unwrap().is_empty());
        let outbox = crate::client::lock(&seam.core.shared.state)
            .store
            .outbox()
            .unwrap();
        assert_eq!(outbox.len(), 1);
        assert_eq!(outbox[0].tries, 0, "a refused claim counts nothing");
    }

    /// Fails the command for `workdir` with `error` and accepts every
    /// other one, noting each command it is sent.
    struct RefusingServer {
        workdir: &'static str,
        error: TransportError,
        seen: Mutex<Vec<String>>,
    }

    impl Transport for RefusingServer {
        fn hello(&self) -> BoxFuture<'_, Result<HelloResponse, TransportError>> {
            Box::pin(async { Err(TransportError::Io("unused".into())) })
        }

        fn send_command<'a>(
            &'a self,
            command: &'a Command,
        ) -> BoxFuture<'a, Result<CommandResult, TransportError>> {
            Box::pin(async move {
                let CommandKind::CreateSession { workdir, .. } = &command.kind else {
                    panic!("only creations are sent here");
                };
                let mut seen = self.seen.lock().unwrap();
                seen.push(workdir.clone());
                if workdir == self.workdir {
                    return Err(self.error.clone());
                }
                Ok(CommandResult::Accepted {
                    seq: Seq(seen.len() as u64),
                })
            })
        }

        fn events(&self, _query: EventsQuery) -> BoxFuture<'_, Result<EventBatch, TransportError>> {
            Box::pin(async { Err(TransportError::Io("unused".into())) })
        }

        fn blob(
            &self,
            _id: ProtoBlobId,
            _from: u64,
        ) -> BoxFuture<'_, Result<BlobDownload, TransportError>> {
            Box::pin(async { Err(TransportError::NotFound) })
        }
    }

    fn create(seam: &Seam, workdir: &str) {
        seam.core
            .dispatch(crate::Action::CreateSession {
                conversation: None,
                workdir: workdir.into(),
            })
            .unwrap();
    }

    /// A command the server refuses with a 400 fails for good and is not
    /// sent again; the ones behind it still go out, on the same link.
    #[tokio::test(start_paused = true)]
    async fn a_refused_command_fails_and_the_next_still_goes_out() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        create(&seam, "/bad");
        create(&seam, "/good");
        let server = RefusingServer {
            workdir: "/bad",
            error: TransportError::BadStatus(400),
            seen: Mutex::new(Vec::new()),
        };
        let ctx = sender_ctx(&seam);

        let drained = tokio::time::timeout(Duration::from_secs(1), send(&ctx, &server)).await;
        assert!(drained.is_err(), "the refusal does not end the sender");
        assert_eq!(*server.seen.lock().unwrap(), ["/bad", "/good"]);
        assert!(seam.pending_outbox().unwrap().is_empty());

        let Screen::SessionList(list) = seam.core.view().screen else {
            panic!("not on the session list");
        };
        let creating: Vec<_> = list
            .creating
            .iter()
            .map(|p| (p.workdir.as_str(), p.outgoing))
            .collect();
        assert_eq!(
            creating,
            [
                ("/bad", Outgoing::Failed(FailReason::Undeliverable)),
                ("/good", Outgoing::Accepted(Seq(2))),
            ]
        );
        assert!(list.creating[0].discardable, "a refused creation can go");

        // Settled on disk too: a restart neither resends nor forgets it.
        drop(ctx);
        drop(seam);
        let reopened = Core::open_detached(dir.path()).unwrap();
        let outbox = crate::client::lock(&reopened.shared.state)
            .store
            .outbox()
            .unwrap();
        assert_eq!(outbox[0].status, OutboxStatus::Undeliverable);
        assert!(outbox[0].answered_at.is_some());
    }

    /// A 5xx is the server's trouble, not the command's: the sender ends
    /// so the link backs off and sends the same command again.
    #[tokio::test(start_paused = true)]
    async fn a_server_error_keeps_the_command_queued() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        create(&seam, "/busy");
        create(&seam, "/later");
        let server = RefusingServer {
            workdir: "/busy",
            error: TransportError::BadStatus(503),
            seen: Mutex::new(Vec::new()),
        };
        let ctx = sender_ctx(&seam);

        let ended = tokio::time::timeout(Duration::from_secs(1), send(&ctx, &server)).await;
        assert!(matches!(
            ended,
            Ok(Err(LinkError::Transport(TransportError::BadStatus(503))))
        ));
        assert_eq!(*server.seen.lock().unwrap(), ["/busy"]);
        assert_eq!(seam.pending_outbox().unwrap().len(), 2);
    }

    /// A server with a log of one session: a message logs its event once
    /// however often it is sent, and a long-poll wakes when the log grows.
    /// The reply to the message `lost` is never readable: it fails to decode
    /// at once, before any pull can bring the event. The message `refused`
    /// is rejected, so logs nothing, and its reply is never readable either.
    struct LogServer {
        session_id: SessionId,
        log: Mutex<Vec<skywave_proto::Event>>,
        grew: Notify,
        sent: Mutex<Vec<String>>,
        hellos: AtomicUsize,
        /// Who sends the messages.
        device: DeviceId,
    }

    impl LogServer {
        /// The log holds the session, created by another device.
        fn new() -> LogServer {
            let session_id = SessionId::new();
            LogServer {
                session_id,
                log: Mutex::new(vec![skywave_proto::Event {
                    session_id,
                    at: Utc::now(),
                    command_id: Some(CommandId::new()),
                    context: skywave_proto::ContextId::first_of(session_id),
                    kind: skywave_proto::EventKind::SessionCreated {
                        workdir: "/srv".into(),
                        context_name: "start".into(),
                        from: None,
                    },
                }]),
                grew: Notify::new(),
                sent: Mutex::new(Vec::new()),
                hellos: AtomicUsize::new(0),
                device: DeviceId::new(),
            }
        }

        fn batch(&self, after: Seq) -> EventBatch {
            let log = self.log.lock().unwrap();
            EventBatch {
                from: after.next(),
                events: log.iter().skip(after.0 as usize).cloned().collect(),
            }
        }
    }

    impl Transport for LogServer {
        fn hello(&self) -> BoxFuture<'_, Result<HelloResponse, TransportError>> {
            Box::pin(async move {
                self.hellos.fetch_add(1, Ordering::SeqCst);
                Ok(HelloResponse {
                    protocol_version: PROTOCOL_VERSION,
                    server_id: ServerId::new(),
                    device_id: DeviceId::new(),
                    head: Seq(self.log.lock().unwrap().len() as u64),
                    // Named, so no event asks `hello` again.
                    devices: vec![skywave_proto::DeviceInfo {
                        id: self.device,
                        name: "phone".into(),
                    }],
                    shell: false,
                })
            })
        }

        fn send_command<'a>(
            &'a self,
            command: &'a Command,
        ) -> BoxFuture<'a, Result<CommandResult, TransportError>> {
            Box::pin(async move {
                let CommandKind::SendMessage { text, .. } = &command.kind else {
                    panic!("only messages are sent here");
                };
                self.sent.lock().unwrap().push(text.clone());
                if text == "refused" {
                    return Err(TransportError::Decode("garbled".into()));
                }
                let seq = {
                    let mut log = self.log.lock().unwrap();
                    let logged = log.iter().position(|e| e.command_id == Some(command.id));
                    let at = logged.unwrap_or_else(|| {
                        log.push(skywave_proto::Event {
                            session_id: self.session_id,
                            at: Utc::now(),
                            command_id: Some(command.id),
                            context: skywave_proto::ContextId::first_of(self.session_id),
                            kind: skywave_proto::EventKind::UserMessage {
                                device_id: self.device,
                                text: text.clone(),
                            },
                        });
                        log.len() - 1
                    });
                    Seq(at as u64 + 1)
                };
                self.grew.notify_waiters();
                if text == "lost" {
                    return Err(TransportError::Decode("garbled".into()));
                }
                Ok(CommandResult::Accepted { seq })
            })
        }

        fn events(&self, query: EventsQuery) -> BoxFuture<'_, Result<EventBatch, TransportError>> {
            Box::pin(async move {
                // Registered before the look, so no append slips between.
                let grew = self.grew.notified();
                let batch = self.batch(query.after);
                if !batch.events.is_empty() {
                    return Ok(batch);
                }
                let wait = Duration::from_secs(u64::from(query.wait.unwrap_or(0)));
                let _ = tokio::time::timeout(wait, grew).await;
                Ok(self.batch(query.after))
            })
        }

        fn blob(
            &self,
            _id: ProtoBlobId,
            _from: u64,
        ) -> BoxFuture<'_, Result<BlobDownload, TransportError>> {
            Box::pin(async { Err(TransportError::NotFound) })
        }
    }

    fn link_to(seam: &Seam, server: &Arc<LogServer>) -> LinkCtx {
        let fake = server.clone();
        let (wall_clock, _) = paused_wall();
        LinkCtx {
            config: Arc::new(EngineConfig {
                backoff: Backoff {
                    base: Duration::from_secs(1),
                    cap: Duration::from_secs(8),
                    jitter: Jitter::fixed(1.0),
                },
                connector: Connector::new(move |_, _, _| Ok(fake.clone() as Arc<dyn Transport>)),
                wait_secs: 5,
                wall_clock,
                ..EngineConfig::default()
            }),
            ..sender_ctx(seam)
        }
    }

    fn send_message(seam: &Seam, session_id: SessionId, text: &str) -> CommandId {
        seam.core
            .dispatch(crate::Action::SendMessage {
                session_id,
                text: text.into(),
            })
            .unwrap();
        let outbox = seam.pending_outbox().unwrap();
        outbox.last().unwrap().command.id
    }

    /// The reply to a command is never readable and comes back before the
    /// pull brings its event, yet the event settles the command as accepted
    /// at its seq, as the reply would have: the link stays up, the command
    /// is sent once, and the next goes out after it.
    #[tokio::test(start_paused = true)]
    async fn an_unreadable_reply_settles_from_the_pulled_event() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let server = Arc::new(LogServer::new());
        let sid = server.session_id;
        seam.apply_events(&server.batch(Seq::ZERO)).unwrap();
        let lost = send_message(&seam, sid, "lost");
        let next = send_message(&seam, sid, "next");

        let link = tokio::spawn(run_link(link_to(&seam, &server)));
        let drained = tokio::time::timeout(Duration::from_secs(600), async {
            while !seam.pending_outbox().unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
        assert!(
            drained.await.is_ok(),
            "sent {:?}",
            server.sent.lock().unwrap()
        );
        // Long enough for any reconnect and resend to show.
        tokio::time::sleep(Duration::from_secs(60)).await;
        link.abort();

        assert_eq!(*server.sent.lock().unwrap(), ["lost", "next"]);
        // The connection's, and the head read after the unreadable reply.
        assert_eq!(server.hellos.load(Ordering::SeqCst), 2, "one connection");
        let outbox = crate::client::lock(&seam.core.shared.state)
            .store
            .outbox()
            .unwrap();
        let settled: Vec<_> = outbox.iter().map(|e| (e.command.id, &e.status)).collect();
        assert_eq!(
            settled,
            [
                (
                    lost,
                    &OutboxStatus::Done(CommandResult::Accepted { seq: Seq(2) })
                ),
                (
                    next,
                    &OutboxStatus::Done(CommandResult::Accepted { seq: Seq(3) })
                ),
            ]
        );
        assert!(outbox[0].answered_at.is_some());

        seam.core.dispatch(crate::Action::OpenSession(sid)).unwrap();
        let Screen::Session(view) = seam.core.view().screen else {
            panic!("not on the session");
        };
        let outgoing: Vec<_> = view
            .timeline
            .iter()
            .filter_map(|item| match item {
                crate::view::TimelineItem::Logged {
                    entry:
                        crate::view::Entry::UserMessage {
                            command_id,
                            outgoing,
                            ..
                        },
                    ..
                } => Some((*command_id, *outgoing)),
                _ => None,
            })
            .collect();
        assert_eq!(
            outgoing,
            [
                (lost, Some(Outgoing::Accepted(Seq(2)))),
                (next, Some(Outgoing::Accepted(Seq(3)))),
            ]
        );
    }

    /// The reply to a rejected command is unreadable, and the log, applied
    /// through the head read after it, holds no event for it: it fails as
    /// undeliverable on the same connection, sent once, and the next goes
    /// out after it.
    #[tokio::test(start_paused = true)]
    async fn an_unreadable_rejection_fails_once_the_log_shows_no_event() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let server = Arc::new(LogServer::new());
        let sid = server.session_id;
        seam.apply_events(&server.batch(Seq::ZERO)).unwrap();
        let refused = send_message(&seam, sid, "refused");
        let next = send_message(&seam, sid, "next");

        let link = tokio::spawn(run_link(link_to(&seam, &server)));
        let drained = tokio::time::timeout(Duration::from_secs(600), async {
            while !seam.pending_outbox().unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
        assert!(
            drained.await.is_ok(),
            "sent {:?}",
            server.sent.lock().unwrap()
        );
        // Long enough for any reconnect and resend to show.
        tokio::time::sleep(Duration::from_secs(60)).await;
        link.abort();

        assert_eq!(*server.sent.lock().unwrap(), ["refused", "next"]);
        // The connection's, and the head read after the unreadable reply.
        assert_eq!(server.hellos.load(Ordering::SeqCst), 2, "one connection");
        let outbox = crate::client::lock(&seam.core.shared.state)
            .store
            .outbox()
            .unwrap();
        let settled: Vec<_> = outbox.iter().map(|e| (e.command.id, &e.status)).collect();
        assert_eq!(
            settled,
            [
                (refused, &OutboxStatus::Undeliverable),
                (
                    next,
                    &OutboxStatus::Done(CommandResult::Accepted { seq: Seq(2) })
                ),
            ]
        );
    }

    /// An event the pull applies after the head was read, but before the
    /// replica reached it, still settles the command as accepted: it fails
    /// only once the cursor reaches the head without its event.
    #[test]
    fn an_event_behind_the_head_settles_an_unreadable_reply_as_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let server = LogServer::new();
        let sid = server.session_id;
        seam.apply_events(&server.batch(Seq::ZERO)).unwrap();
        let lost = send_message(&seam, sid, "lost");
        let entry = seam.pending_outbox().unwrap().remove(0);
        assert!(seam.command_sending(lost).unwrap());
        let reply = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(server.send_command(&entry.command));
        assert!(matches!(reply, Err(TransportError::Decode(_))));
        // The head read after the reply: the command's event is in it.
        let head = Seq(2);

        assert!(
            !seam.command_unlogged(lost, head).unwrap(),
            "behind the head"
        );
        seam.apply_events(&server.batch(Seq(1))).unwrap();
        assert!(seam.command_unlogged(lost, head).unwrap());
        let outbox = crate::client::lock(&seam.core.shared.state)
            .store
            .outbox()
            .unwrap();
        assert_eq!(
            outbox[0].status,
            OutboxStatus::Done(CommandResult::Accepted { seq: Seq(2) })
        );
    }

    /// The reply and the event settle a command alike, whichever comes
    /// first; the later one changes nothing, in the store or the view.
    #[test]
    fn reply_and_event_settle_once_in_either_order() {
        let settle = |reply_first: bool| {
            let dir = tempfile::tempdir().unwrap();
            let seam = paired_seam(dir.path());
            let server = LogServer::new();
            seam.apply_events(&server.batch(Seq::ZERO)).unwrap();
            let id = send_message(&seam, server.session_id, "hi");
            assert!(seam.command_sending(id).unwrap());
            let mut event = server.log.lock().unwrap()[0].clone();
            event.command_id = Some(id);
            event.kind = skywave_proto::EventKind::UserMessage {
                device_id: DeviceId::new(),
                text: "hi".into(),
            };
            let batch = EventBatch {
                from: Seq(2),
                events: vec![event],
            };
            let reply = CommandResult::Accepted { seq: Seq(2) };
            if reply_first {
                seam.command_result(id, &reply).unwrap();
                let applied = seam.apply_events(&batch).unwrap();
                assert!(applied.settled.is_empty(), "the reply came first");
            } else {
                let applied = seam.apply_events(&batch).unwrap();
                assert_eq!(applied.settled, [(id, Seq(2))]);
                seam.command_result(id, &reply).unwrap();
            }
            // A replay of the batch settles nothing either.
            assert!(seam.apply_events(&batch).unwrap().settled.is_empty());
            assert!(!seam.command_sending(id).unwrap(), "nothing left to send");

            let state = crate::client::lock(&seam.core.shared.state);
            let outbox = state.store.outbox().unwrap();
            let shown = state.model.view().queued;
            let stored: Vec<_> = outbox.iter().map(|e| (e.status.clone(), e.tries)).collect();
            (stored, shown)
        };
        let settled = (
            vec![(
                OutboxStatus::Done(CommandResult::Accepted { seq: Seq(2) }),
                1,
            )],
            0,
        );
        assert_eq!(settle(true), settled);
        assert_eq!(settle(false), settled);
    }

    #[test]
    fn device_name_is_never_empty() {
        assert!(!default_device_name().trim().is_empty());
    }
}
