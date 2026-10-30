//! The sync engine (spec §7, D7): the single consumer of the core's
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
//! else shows `Reconnecting { attempt, next_in, offline_since }`, waits
//! [`Backoff::delay`] and connects again, still showing `Reconnecting`
//! until `hello` succeeds (`RetryNow` ends the wait early, without
//! touching the bookkeeping). The attempt count and `offline_since` reset once
//! a pull succeeds, so a server that answers `hello` and then fails keeps
//! backing off.
//!
//! [`Core::suspend`](crate::Core::suspend) stops the link and the
//! downloader (in-flight requests are dropped; an unanswered command is
//! resent later with the same id; an interrupted blob resumes from its
//! partial file) and [`Core::resume`](crate::Core::resume) starts them
//! again from `Connecting`. Pairing runs regardless.

use std::convert::Infallible;
use std::fmt;
use std::sync::Arc;
use std::time::Instant;

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
use crate::store::StoreError;
use crate::transport::{Transport, TransportError};
use crate::view::{Link, LinkRejection};

/// Long-poll wait the engine asks for by default; the server caps it at 60.
pub const DEFAULT_WAIT_SECS: u32 = 30;

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
            Request::BlobPolicyChanged(policy) => {
                let blobs = self.seam.blobs().unwrap_or_else(|error| {
                    tracing::warn!(%error, "cannot read blobs; queueing none");
                    Vec::new()
                });
                self.blobs.set_policy(policy, &blobs);
            }
        }
    }

    /// (Re)starts the link and the blob downloader if the device is
    /// paired, not rejected and not suspended.
    fn start_link(&mut self) {
        self.stop_link();
        if *self.suspended.borrow() || *self.rejected.borrow() {
            return;
        }
        let Some(identity) = self.seam.identity() else {
            return;
        };
        let address = match self.seam.settings() {
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
            seam: self.seam.clone(),
            config: self.config.clone(),
            address: address.clone(),
            identity: identity.clone(),
            queue: self.blobs.clone(),
        }));
        let stop_downloader = downloader.abort_handle();
        self.downloader = Some(downloader);
        let ctx = LinkCtx {
            seam: self.seam.clone(),
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
    offline_since: Option<Instant>,
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
        let offline_since = *retry
            .offline_since
            .get_or_insert_with(|| tokio::time::Instant::now().into_std());
        let failed_at = tokio::time::Instant::now();
        let next_in = ctx.config.backoff.delay(retry.attempt);
        tracing::debug!(%error, attempt = retry.attempt, ?next_in, "link down");
        reporter.set(Link::Reconnecting {
            attempt: retry.attempt,
            next_in,
            offline_since,
        });
        let mut wake = failed_at + next_in;
        tokio::select! {
            () = tokio::time::sleep_until(wake) => {}
            () = ctx.retry_now.notified() => {
                wake = wake.min(failed_at + Backoff::RETRY_NOW_MIN);
                tracing::debug!(after = ?wake - failed_at, "retrying now");
                // The banner counts down from when this value arrives.
                reporter.set(Link::Reconnecting {
                    attempt: retry.attempt,
                    next_in: wake.saturating_duration_since(tokio::time::Instant::now()),
                    offline_since,
                });
                // Further presses are ignored until the attempt fails.
                tokio::time::sleep_until(wake).await;
            }
        }
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
async fn send(ctx: &LinkCtx, transport: &dyn Transport) -> Result<Infallible, LinkError> {
    loop {
        let Some(entry) = ctx.seam.pending_outbox()?.into_iter().next() else {
            ctx.outbox.notified().await;
            continue;
        };
        let id = entry.command.id;
        ctx.seam.command_sending(id)?;
        let result = transport.send_command(&entry.command).await?;
        ctx.seam.command_result(id, &result)?;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use skywave_proto::{
        BlobId as ProtoBlobId, Command, CommandResult, EventBatch, HelloResponse, PROTOCOL_VERSION,
        ServerId,
    };

    use super::*;
    use crate::Core;
    use crate::backoff::Jitter;
    use crate::identity::PairedCredentials;
    use crate::transport::{BlobDownload, BoxFuture};
    use crate::view::DeviceId;

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

    /// Reconnect delays double per attempt up to the cap, with the injected
    /// jitter, on tokio's paused clock; a successful pull resets them.
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
        let config = EngineConfig {
            backoff: Backoff {
                base: Duration::from_secs(1),
                cap: Duration::from_secs(8),
                jitter: Jitter::fixed(1.0),
            },
            connector: Connector::new(move |_, _, _| Ok(fake.clone() as Arc<dyn Transport>)),
            wait_secs: 5,
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

        let reconnects = |links: &[Link]| -> Vec<(u32, u64)> {
            links
                .iter()
                .filter_map(|l| match l {
                    Link::Reconnecting {
                        attempt, next_in, ..
                    } => Some((*attempt, next_in.as_secs())),
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
            [(1, 1), (2, 2), (3, 4), (4, 8), (5, 8), (6, 8)]
        );
        let offline: Vec<Instant> = links
            .lock()
            .unwrap()
            .iter()
            .filter_map(|l| match l {
                Link::Reconnecting { offline_since, .. } => Some(*offline_since),
                _ => None,
            })
            .collect();
        assert!(
            offline.windows(2).all(|w| w[0] == w[1]),
            "offline_since is kept"
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
            [(1, 1), (2, 2)]
        );
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

        let reconnecting = |links: &[Link]| -> Vec<(u32, Instant)> {
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
        let reconnecting_next_in = |links: &[Link]| -> Vec<(u32, Duration)> {
            links
                .iter()
                .filter_map(|l| match l {
                    Link::Reconnecting {
                        attempt, next_in, ..
                    } => Some((*attempt, *next_in)),
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
        let failed = tokio::time::Instant::now();
        retry_now.notify_waiters();
        // The shortened countdown is reported; then the floor is kept.
        wait_for(2).await;
        assert!(matches!(
            reconnecting_next_in(&links.lock().unwrap()).last(),
            Some(&(1, d)) if d <= Backoff::RETRY_NOW_MIN
        ));
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

    #[test]
    fn device_name_is_never_empty() {
        assert!(!default_device_name().trim().is_empty());
    }
}
