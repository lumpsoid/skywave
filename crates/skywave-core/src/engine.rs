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
//! - `ListConversations` lists the agent's conversations the same way,
//!   within [`CONVERSATIONS_TIMEOUT`], and answers with
//!   [`Seam::conversations_listed`].
//! - `Storage` asks for the server's storage report the same way, within
//!   [`STORAGE_TIMEOUT`], and answers with [`Seam::storage_reported`].
//! - `Worktrees` asks for the server's worktree list the same way, within
//!   [`WORKTREES_TIMEOUT`], and answers with [`Seam::worktrees_listed`].
//! - `UsageLimits` asks for the agent's usage limits the same way, within
//!   [`USAGE_LIMITS_TIMEOUT`], and answers with
//!   [`Seam::usage_limits_answered`].
//! - `LoginStart` and `LoginFinish` start and finish logging the server's
//!   agent in the same way, each within [`LOGIN_TIMEOUT`], and answer with
//!   [`Seam::login_started`] and [`Seam::login_finished`].
//! - `Workdir` reads a session's workdir the same way, within
//!   [`WORKDIR_TIMEOUT`], and answers with [`Seam::workdir_answered`].
//!
//! **The link** runs while the device is paired, not rejected and not
//! suspended. Each connection starts with `hello`, then runs three loops side
//! by side until one fails:
//! - the *pull loop* long-polls `GET /v1/events` from the store's cursor and
//!   applies every batch in one transaction ([`Seam::apply_events`]);
//! - the *outbox sender* sends the oldest pending command, marking it
//!   `Sending` first, and waits for its `CommandResult` before the next, so
//!   the server sees commands in the order they were made. A failed send is
//!   repeated with the same command id; the server dedups (D7). A refusal
//!   (4xx), an unreadable reply or a server error (5xx) is an answer about
//!   one command, so it keeps the connection and the pull going
//!   ([`send_entry`]); after [`MAX_SERVER_ERRORS`] server errors in a row
//!   the command is settled from the log instead of sent again. A failure
//!   of the command's request alone (a timeout, a broken connection, a
//!   408: [`TransportError::failed_request`]) is no answer, but it keeps
//!   the connection too, and the command is resent after a backoff: the
//!   request has a connection of its own, so the pull's tells whether the
//!   link is down. Taking a command back (`OutboxChanged`) ends the wait to
//!   resend it, and the next command goes.
//! - the *draft stager* uploads the images of a draft as soon as they are
//!   attached ([`Request::DraftImagesChanged`]), so a message finds them
//!   staged; a transport error makes it wait a backoff instead of ending the
//!   connection ([`stage_drafts`]).
//!
//! `Link` moves as follows: `Connecting` when the link (re)starts; after
//! `hello`, `CatchingUp { have, server }` while the cursor is behind the
//! head `hello` reported (or a later batch revealed), else
//! `Online { latency_ms }` with `hello`'s round trip — blob downloads play
//! no part: metadata synced counts as synced (D6). A `hello` whose log lost
//! events the replica holds ([`Seam::log_checked`]) shows
//! `Rejected(HistoryLost)` before anything is pulled or sent, and stops the
//! link until the user resets the replica. Every later request names the
//! run that `hello` reached ([`ExpectedRun`]), so a server in another run
//! (restarted, or rolled back under a connection that never closed)
//! refuses it with [`TransportError::RunChanged`]: that ends the
//! connection like any failure, and the next one's `hello` tells a clean
//! restart from a lost log. The blob downloader and the side transport
//! name the same run, read from the one [`ExpectedRun`] each link start
//! makes: a blob refused so is retried after a backoff, by when the link
//! has met the new run or stopped on a lost log; a listing or workdir read
//! fails with it. Any other failure ends
//! the connection, and its kind alone decides what follows
//! ([`TransportError::link_verdict`]), never a count or a timeout. A
//! rejection shows `Rejected(..)` and stops the link until the next
//! `Connect`: 401/403, a protocol mismatch, and the faults no retry can
//! fix, a server certificate from another CA (`ServerChanged`), one that
//! does not cover the address (`NameMismatch`) or an unusable stored
//! identity (`BadIdentity`). Anything else shows
//! `Reconnecting { attempt, retry_at, offline_since, reason }`, waits
//! [`Backoff::delay`] and connects again, still showing `Reconnecting`
//! until `hello` succeeds (`RetryNow` ends the wait early and moves
//! `retry_at` to match, without touching the bookkeeping). Both times are
//! wall clock ([`EngineConfig::wall_clock`]), read once per failure. The
//! wait ends once either clock says the delay has passed: the monotonic
//! clock stops while a device sleeps, so the link also checks the wall
//! clock every [`WALL_CHECK`] and retries soon after the `retry_at` it
//! showed, and a `RetryNow` after a long sleep retries at once.
//!
//! The bookkeeping resets on definitive signals only, when a connection
//! ends. The attempt count resets if the connection made *progress*: a
//! pull returned (with or without events) or the server settled a command
//! (a `CommandResult`, or a refusal for good). `hello` alone is not
//! progress, so a server that answers `hello` and then always fails keeps
//! backing off instead of being retried every half second. A connection
//! cut during every long-poll still delivers one command per connection at
//! the base delay. `offline_since` resets once `hello` succeeds, as the
//! link showed online from there: the next failure starts a new offline
//! stretch, whatever the attempt count.
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
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use skywave_proto::limits::ATTACHMENT_MAX_BYTES;
use skywave_proto::{
    Command, CommandKind, CommandResult, ContentHash, EventsQuery, Seq, SessionId, UploadResult,
    WriteFileRequest, WriteFileResult,
};
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::JoinHandle;

use crate::address::ServerAddress;
use crate::attachment::{AttachedFile, attachment_path};
use crate::backoff::Backoff;
use crate::blobs::{BlobCtx, BlobQueue, run_downloader};
use crate::client::CoreError;
use crate::https::{HttpsTransport, TransportConfig};
use crate::identity::Identity;
use crate::pairing::pair_and_report;
use crate::seam::{Request, Seam};
use crate::store::{AttachmentRefusal, LogCheck, OutboxEntry, StoreError};
use crate::transport::{ExpectedRun, StagedUpload, Transport, TransportError, UploadFile};
use crate::view::{CommandId, LinkFailure, LinkRejection, PairedLink};
use crate::workdir::{
    PUSH_MAX_BYTES, PushOutcome, WORKDIR_TIMEOUT, WorkdirAnswer, WorkdirAsk, not_connected,
};

/// Long-poll wait the engine asks for by default; the server caps it at 60.
pub const DEFAULT_WAIT_SECS: u32 = 30;
/// Longest a directory listing for completion may take, connecting included.
pub const DIRS_TIMEOUT: Duration = Duration::from_secs(5);
/// Longest a conversation listing for the picker may take, connecting
/// included: above the server's own 10 s for the scan, so its `503` (or
/// a truncated listing) arrives before this gives up.
pub const CONVERSATIONS_TIMEOUT: Duration = Duration::from_secs(12);
/// Longest a storage report may take, connecting included: above the
/// server's own 10 s, so its `503` arrives before this gives up.
pub const STORAGE_TIMEOUT: Duration = Duration::from_secs(12);
/// Longest a storage reclaim may take, connecting included: above the
/// server's own 600 s for the `VACUUM`, so its answer arrives before this
/// gives up.
pub const RECLAIM_TIMEOUT: Duration = Duration::from_secs(610);
/// How long the worktrees screen waits for the list: the server runs git in
/// every worktree.
pub const WORKTREES_TIMEOUT: Duration = Duration::from_secs(20);
/// Longest a usage limits read may take, connecting included: above the
/// server's own 20 s, so its `503` arrives before this gives up.
pub const USAGE_LIMITS_TIMEOUT: Duration = Duration::from_secs(25);
/// Longest a login start or finish may take, connecting included: above
/// the server's own 20 s, so its `503` arrives before this gives up. A
/// finish cut short keeps going on the server; resending it gets its
/// outcome.
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(25);
/// Longest the link sleeps in a reconnect wait before it looks at the wall
/// clock again. The monotonic clock stops while a device sleeps (Android's
/// deep sleep), so after waking the attempt comes at most this long after
/// the `retry_at` the view shows. Only waits longer than this tick more
/// than once, and only while the device is awake.
pub const WALL_CHECK: Duration = Duration::from_secs(5);
/// How many server errors (5xx answers,
/// [`TransportError::is_server_error`]) in a row the outbox takes for the
/// command at its head before it stops resending it and settles it from
/// the log: accepted if the log holds its event, else undeliverable. Until
/// then each resend waits [`Backoff::delay`] for the count so far, on the
/// same connection, so the pull goes on meanwhile.
pub const MAX_SERVER_ERRORS: u32 = 5;

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

type Connect = dyn Fn(
        &ServerAddress,
        &dyn Identity,
        &TransportConfig,
        &ExpectedRun,
    ) -> Result<Arc<dyn Transport>, TransportError>
    + Send
    + Sync;

/// Builds the [`Transport`] to the server at an address for
/// an identity, naming the run an [`ExpectedRun`] holds on every request.
/// [`Connector::https`] by default; tests and the link harness wrap or
/// replace it. A connector that builds an [`HttpsTransport`] must hand it
/// the [`ExpectedRun`] it was given, or the server's run goes unchecked.
#[derive(Clone)]
pub struct Connector(Arc<Connect>);

impl Connector {
    /// [`HttpsTransport`].
    pub fn https() -> Connector {
        Connector::new(|address, identity, config, run| {
            Ok(Arc::new(HttpsTransport::new(
                address,
                identity,
                config.clone(),
                run.clone(),
            )?) as Arc<dyn Transport>)
        })
    }

    pub fn new(
        connect: impl Fn(
            &ServerAddress,
            &dyn Identity,
            &TransportConfig,
            &ExpectedRun,
        ) -> Result<Arc<dyn Transport>, TransportError>
        + Send
        + Sync
        + 'static,
    ) -> Connector {
        Connector(Arc::new(connect))
    }

    pub(crate) fn connect(
        &self,
        address: &ServerAddress,
        identity: &dyn Identity,
        config: &TransportConfig,
        run: &ExpectedRun,
    ) -> Result<Arc<dyn Transport>, TransportError> {
        (self.0)(address, identity, config, run)
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
    /// Wakes the draft image stager; outlives each link so no wake-up is lost.
    drafts: Arc<Notify>,
    /// Ends the link's backoff wait; outlives each link.
    retry_now: Arc<Notify>,
    link: Option<JoinHandle<()>>,
    /// The blob downloader, running while the link does.
    downloader: Option<JoinHandle<()>>,
    /// The server refused the device; no link until the next `Connect`.
    rejected: watch::Sender<bool>,
    /// Blobs waiting for the downloader; outlives each downloader.
    blobs: Arc<BlobQueue>,
    /// The transport kept for listings and workdir reads, built on the
    /// first; dropped whenever the link restarts, as the identity may have
    /// changed.
    side: Option<Arc<dyn Transport>>,
    /// The run the current link expects, read by the link's, the
    /// downloader's and the side transport. A new one per link start, so a
    /// link to another server never inherits the last one's run.
    run: ExpectedRun,
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
            drafts: Arc::new(Notify::new()),
            retry_now: Arc::new(Notify::new()),
            link: None,
            downloader: None,
            rejected: watch::Sender::new(false),
            blobs: Arc::new(BlobQueue::restore(&seam)),
            side: None,
            run: ExpectedRun::default(),
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
                self.seam.spawn("pairing", async move {
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
            Request::DraftImagesChanged => self.drafts.notify_one(),
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
                self.seam.spawn("a directory listing", async move {
                    let result = tokio::time::timeout(DIRS_TIMEOUT, transport.list_dirs(&dir))
                        .await
                        .unwrap_or(Err(TransportError::Timeout));
                    seam.dirs_listed(req, result);
                });
            }
            Request::ListConversations { req } => {
                let seam = self.seam.clone();
                let Some(transport) = self.side_transport() else {
                    seam.conversations_listed(req, Err(not_connected()));
                    return;
                };
                self.seam.spawn("a conversation listing", async move {
                    let result =
                        tokio::time::timeout(CONVERSATIONS_TIMEOUT, transport.list_conversations())
                            .await
                            .unwrap_or(Err(TransportError::Timeout));
                    seam.conversations_listed(req, result);
                });
            }
            Request::Storage { req } => {
                let seam = self.seam.clone();
                let Some(transport) = self.side_transport() else {
                    seam.storage_reported(req, Err(not_connected()));
                    return;
                };
                self.seam.spawn("a storage report", async move {
                    let result = tokio::time::timeout(STORAGE_TIMEOUT, transport.storage_report())
                        .await
                        .unwrap_or(Err(TransportError::Timeout));
                    seam.storage_reported(req, result);
                });
            }
            Request::DeviceStorage { req } => {
                let seam = self.seam.clone();
                let data_dir = self.seam.data_dir();
                self.seam.spawn("a device storage report", async move {
                    // Off the async workers: the scan of the events can take seconds.
                    match tokio::task::spawn_blocking(move || {
                        crate::store::read_device_storage(&data_dir)
                    })
                    .await
                    {
                        Ok(result) => seam.device_storage_read(req, result),
                        Err(error) => tracing::warn!(%error, "cannot read this device's storage"),
                    }
                });
            }
            Request::ReclaimDeviceStorage { req } => {
                let seam = self.seam.clone();
                let data_dir = self.seam.data_dir();
                self.seam.spawn("a device storage reclaim", async move {
                    let reclaiming = seam.clone();
                    // Off the async workers: VACUUM holds the state lock for seconds.
                    match tokio::task::spawn_blocking(move || {
                        reclaiming.reclaim_device_space()?;
                        crate::store::read_device_storage(&data_dir)
                    })
                    .await
                    {
                        Ok(result) => seam.device_storage_read(req, result),
                        Err(error) => {
                            tracing::warn!(%error, "cannot reclaim this device's storage")
                        }
                    }
                });
            }
            Request::ReclaimStorage { req } => {
                let seam = self.seam.clone();
                let Some(transport) = self.side_transport() else {
                    seam.storage_reclaimed(req, Err(not_connected()));
                    return;
                };
                self.seam.spawn("a storage reclaim", async move {
                    let result = tokio::time::timeout(RECLAIM_TIMEOUT, transport.storage_reclaim())
                        .await
                        .unwrap_or(Err(TransportError::Timeout));
                    seam.storage_reclaimed(req, result);
                });
            }
            Request::Worktrees { req } => {
                let seam = self.seam.clone();
                let Some(transport) = self.side_transport() else {
                    seam.worktrees_listed(req, Err(not_connected()));
                    return;
                };
                self.seam.spawn("a worktree list", async move {
                    let result = tokio::time::timeout(WORKTREES_TIMEOUT, transport.worktrees())
                        .await
                        .unwrap_or(Err(TransportError::Timeout));
                    seam.worktrees_listed(req, result);
                });
            }
            Request::UsageLimits { req } => {
                let seam = self.seam.clone();
                let Some(transport) = self.side_transport() else {
                    answer_usage_limits(&seam, req, Err(not_connected()));
                    return;
                };
                self.seam.spawn("a usage limits read", async move {
                    let result =
                        tokio::time::timeout(USAGE_LIMITS_TIMEOUT, transport.usage_limits())
                            .await
                            .unwrap_or(Err(TransportError::Timeout));
                    answer_usage_limits(&seam, req, result);
                });
            }
            Request::LoginStart { req } => {
                let seam = self.seam.clone();
                let Some(transport) = self.side_transport() else {
                    seam.login_started(req, Err(not_connected()));
                    return;
                };
                self.seam.spawn("a login start", async move {
                    let result = tokio::time::timeout(LOGIN_TIMEOUT, transport.login_start())
                        .await
                        .unwrap_or(Err(TransportError::Timeout));
                    seam.login_started(req, result);
                });
            }
            Request::LoginFinish { req, finish } => {
                let seam = self.seam.clone();
                let Some(transport) = self.side_transport() else {
                    answer_login_finish(&seam, req, Err(not_connected()));
                    return;
                };
                self.seam.spawn("a login finish", async move {
                    let result =
                        tokio::time::timeout(LOGIN_TIMEOUT, transport.login_finish(&finish))
                            .await
                            .unwrap_or(Err(TransportError::Timeout));
                    answer_login_finish(&seam, req, result);
                });
            }
            Request::Workdir { req, ask } => {
                let seam = self.seam.clone();
                let Some(transport) = self.side_transport() else {
                    answer_workdir(&seam, req, ask, Err(not_connected()));
                    return;
                };
                self.seam.spawn("a workdir read", async move {
                    let result =
                        tokio::time::timeout(WORKDIR_TIMEOUT, read_workdir(&*transport, &ask))
                            .await
                            .unwrap_or(Err(TransportError::Timeout));
                    answer_workdir(&seam, req, ask, result);
                });
            }
            Request::PushWorkdirFile {
                req,
                session,
                source,
                path,
                replace,
            } => {
                let seam = self.seam.clone();
                let Some(transport) = self.side_transport() else {
                    answer_push(&seam, req, session, path, Err(not_connected()));
                    return;
                };
                self.seam.spawn("a workdir push", async move {
                    let result =
                        push_workdir_file(&*transport, &source, session, path.clone(), replace)
                            .await;
                    answer_push(&seam, req, session, path, result);
                });
            }
            Request::CountLines { req, path } => {
                let seam = self.seam.clone();
                self.seam.spawn("a line count", async move {
                    let result = tokio::task::spawn_blocking(move || {
                        skywave_proto::crop::count(std::fs::File::open(path)?)
                    })
                    .await
                    .unwrap_or_else(|error| Err(std::io::Error::other(error.to_string())));
                    seam.lines_counted(req, result);
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
        let pairing = self.seam.pairing()?;
        let address = pairing.addresses.current().clone();
        let transport = self
            .config
            .connector
            .connect(
                &address,
                pairing.as_ref(),
                &self.config.transport,
                &self.run,
            )
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
        self.run = ExpectedRun::default();
        if *self.suspended.borrow() || *self.rejected.borrow() {
            return;
        }
        // Bound: if the device pairs with another server, this link and
        // downloader can no longer write to the replica.
        let Some((seam, pairing)) = self.seam.bind() else {
            return;
        };
        let address = pairing.addresses.current().clone();
        let identity: Arc<dyn Identity> = pairing;
        let blobs = BlobCtx {
            seam: seam.clone(),
            config: self.config.clone(),
            address: address.clone(),
            identity: identity.clone(),
            run: self.run.clone(),
            queue: self.blobs.clone(),
        };
        let downloader = self.seam.spawn("the blob downloader", async move {
            match run_downloader(blobs).await {}
        });
        let stop_downloader = downloader.abort_handle();
        self.downloader = Some(downloader);
        let ctx = LinkCtx {
            seam,
            config: self.config.clone(),
            outbox: self.outbox.clone(),
            drafts: self.drafts.clone(),
            retry_now: self.retry_now.clone(),
            blobs: self.blobs.clone(),
            address,
            identity,
            run: self.run.clone(),
        };
        let rejected = self.rejected.clone();
        self.link = Some(self.seam.spawn("the link", async move {
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
        WorkdirAsk::Log { .. } => transport.workdir_log(&query).await.map(WorkdirAnswer::Log),
        WorkdirAsk::List { .. } => transport
            .workdir_list(&query)
            .await
            .map(WorkdirAnswer::List),
        WorkdirAsk::File { .. } => transport
            .workdir_file(&query)
            .await
            .map(WorkdirAnswer::File),
        WorkdirAsk::Download { .. } => transport
            .workdir_download(&query)
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

fn answer_push(
    seam: &Seam,
    req: u64,
    session: SessionId,
    path: String,
    result: Result<PushOutcome, TransportError>,
) {
    if let Err(error) = seam.workdir_pushed(req, session, path, result) {
        tracing::warn!(%error, "cannot keep a workdir push");
    }
}

fn answer_usage_limits(
    seam: &Seam,
    req: u64,
    result: Result<skywave_proto::UsageLimitsResult, TransportError>,
) {
    if let Err(error) = seam.usage_limits_answered(req, result) {
        tracing::warn!(%error, "cannot keep the usage limits");
    }
}

fn answer_login_finish(
    seam: &Seam,
    req: u64,
    result: Result<skywave_proto::LoginFinishResult, TransportError>,
) {
    if let Err(error) = seam.login_finished(req, result) {
        tracing::warn!(%error, "cannot keep the agent's login");
    }
}

/// What one link needs.
pub(crate) struct LinkCtx {
    pub(crate) seam: Seam,
    pub(crate) config: Arc<EngineConfig>,
    pub(crate) outbox: Arc<Notify>,
    /// Wakes the draft image stager.
    pub(crate) drafts: Arc<Notify>,
    /// Ends a backoff wait early.
    pub(crate) retry_now: Arc<Notify>,
    /// Where an `Eager` policy queues the blobs a batch registers.
    pub(crate) blobs: Arc<BlobQueue>,
    pub(crate) address: ServerAddress,
    pub(crate) identity: Arc<dyn Identity>,
    /// Set once a connection's `hello` passes the log check.
    pub(crate) run: ExpectedRun,
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
    /// `hello` said the log lost events the replica holds
    /// ([`Seam::log_checked`]).
    #[error("the server's log lost events this device holds (seen {have:?}, head {server:?})")]
    HistoryLost { have: Seq, server: Seq },
}

impl LinkError {
    /// Stop with a rejection, or retry and show why.
    fn verdict(&self) -> Result<LinkFailure, LinkRejection> {
        match self {
            LinkError::Transport(e) => e.link_verdict(),
            LinkError::Core(_) => Ok(LinkFailure::Storage),
            LinkError::HistoryLost { have, server } => Err(LinkRejection::HistoryLost {
                have: *have,
                server: *server,
            }),
        }
    }
}

/// Reports `Link` changes, skipping repeats.
struct LinkReporter {
    seam: Seam,
    last: Option<PairedLink>,
}

impl LinkReporter {
    fn set(&mut self, link: PairedLink) {
        if self.last.as_ref() != Some(&link) {
            self.seam.set_link(link.clone());
            self.last = Some(link);
        }
    }
}

/// Reconnect bookkeeping (see the module doc for when it resets).
#[derive(Default)]
struct Retry {
    attempt: u32,
    offline_since: Option<DateTime<Utc>>,
}

impl Retry {
    /// Books how far a connection that ended got: past `hello` starts a new
    /// offline stretch, and progress starts the attempt count over.
    fn connection_ended(&mut self, ended: &Ended) {
        if ended.up {
            self.offline_since = None;
        }
        if ended.progressed {
            self.attempt = 0;
        }
    }
}

/// A count of failed sends of the command at the head of the outbox, in
/// memory only, so a restart starts it over. The link keeps one for server
/// errors ([`MAX_SERVER_ERRORS`]) across connections, so a connection that
/// drops for another reason does not start that count over; each sender
/// keeps one for failed requests, for their backoff.
#[derive(Default)]
struct HeadCount {
    id: Option<CommandId>,
    count: u32,
}

impl HeadCount {
    /// Counts one more failure for `id` and returns the count. A
    /// different command starts over: only the head of the outbox is sent.
    fn note(&mut self, id: CommandId) -> u32 {
        if self.id != Some(id) {
            self.id = Some(id);
            self.count = 0;
        }
        self.count = self.count.saturating_add(1);
        self.count
    }
}

/// How a connection ended and how far it got.
struct Ended {
    error: LinkError,
    /// `hello` succeeded, so the link showed `Online` or `CatchingUp`.
    up: bool,
    /// A pull returned or the server settled a command.
    progressed: bool,
}

impl From<LinkError> for Ended {
    /// A connection that ended before `hello` succeeded.
    fn from(error: LinkError) -> Ended {
        Ended {
            error,
            up: false,
            progressed: false,
        }
    }
}

/// Keeps the link up until the server rejects the device, and returns why.
pub(crate) async fn run_link(ctx: LinkCtx) -> LinkRejection {
    let mut reporter = LinkReporter {
        seam: ctx.seam.clone(),
        last: None,
    };
    reporter.set(PairedLink::Connecting);
    let mut retry = Retry::default();
    let mut errors = HeadCount::default();
    let mut transport: Option<Arc<dyn Transport>> = None;
    loop {
        let error = match &transport {
            Some(transport) => {
                let ended = connection(&ctx, transport.as_ref(), &mut reporter, &mut errors).await;
                retry.connection_ended(&ended);
                ended.error
            }
            None => {
                match ctx.config.connector.connect(
                    &ctx.address,
                    ctx.identity.as_ref(),
                    &ctx.config.transport,
                    &ctx.run,
                ) {
                    Ok(built) => {
                        transport = Some(built);
                        continue;
                    }
                    Err(error) => LinkError::Transport(error),
                }
            }
        };
        let reason = match error.verdict() {
            Ok(reason) => reason,
            Err(rejection) => {
                tracing::warn!(%error, "link stopped");
                reporter.set(PairedLink::Rejected(rejection));
                return rejection;
            }
        };
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
        reporter.set(PairedLink::Reconnecting {
            attempt: retry.attempt,
            retry_at: wait.retry_at(),
            offline_since,
            reason,
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
                    reporter.set(PairedLink::Reconnecting {
                        attempt: retry.attempt,
                        retry_at: wait.retry_at(),
                        offline_since,
                        reason,
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

/// One connection: `hello`, then pull, send and stage drafts until one fails.
/// A server error for one command, or a failure of its request alone, is not
/// a failure of the sender ([`send_entry`]), so it never cuts the pull
/// short; the pull is what finds the link down. A transport error in the
/// draft stager waits instead ([`stage_drafts`]).
async fn connection(
    ctx: &LinkCtx,
    transport: &dyn Transport,
    reporter: &mut LinkReporter,
    errors: &mut HeadCount,
) -> Ended {
    let since = match ctx.seam.server_run() {
        Ok(since) => since,
        Err(error) => return LinkError::from(error).into(),
    };
    let started = tokio::time::Instant::now();
    let hello = match transport.hello(since).await {
        Ok(hello) => hello,
        Err(error) => return LinkError::from(error).into(),
    };
    let latency_ms = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);
    // Before anything is pulled or sent: a pull would wait past a head
    // below the cursor, or stack another history's events onto this one.
    match ctx.seam.log_checked(&hello) {
        Ok(LogCheck::Continues) => {}
        Ok(LogCheck::Lost { seen }) => {
            tracing::warn!(?seen, head = ?hello.head, log = ?hello.log, "the server's log lost events this device holds");
            return LinkError::HistoryLost {
                have: seen,
                server: hello.head,
            }
            .into();
        }
        Err(error) => return LinkError::from(error).into(),
    }
    // Every request from here on, on whatever connection the pool opens and
    // from whichever of this link's transports, reaches this run or is
    // refused: one rolled back under a connection that never closed cannot
    // take a command or serve a batch, a blob or a listing, and the next
    // connection's `hello` says what became of this run.
    ctx.run.expect(hello.log.run);
    if let Err(error) = ctx.seam.hello_received(&hello) {
        return LinkError::from(error).into();
    }
    let progress = AtomicBool::new(false);
    let ended = tokio::select! {
        ended = pull(ctx, transport, reporter, &progress, hello.head, latency_ms) => ended,
        ended = send(ctx, transport, &progress, errors) => ended,
        ended = stage_drafts(ctx, transport) => ended,
    };
    let Err(error) = ended;
    Ended {
        error,
        up: true,
        progressed: progress.load(Ordering::Relaxed),
    }
}

/// The pull loop: long-polls from the store's cursor and applies batches.
/// Sets `progress` once a pull returns.
async fn pull(
    ctx: &LinkCtx,
    transport: &dyn Transport,
    reporter: &mut LinkReporter,
    progress: &AtomicBool,
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
            PairedLink::CatchingUp {
                have: cursor,
                server: head,
            }
        } else {
            PairedLink::Online { latency_ms }
        });
        let batch = transport
            .events(EventsQuery {
                after: cursor,
                wait: Some(wait),
                limit: ctx.config.limit,
            })
            .await?;
        progress.store(true, Ordering::Relaxed);
        if batch.events.is_empty() {
            continue;
        }
        match ctx.seam.apply_events(&batch) {
            Ok(applied) => {
                head = head.max(applied.cursor);
                ctx.blobs.local_crops(&applied.local_crops);
                ctx.blobs.registered(&applied.blobs);
                // A command the log settled, or the log applied through a
                // head, may be what the sender waits on ([`settled_by_log`]).
                ctx.outbox.notify_one();
                // A device paired since this connection's `hello`: ask
                // again, so its name shows without waiting for a reconnect.
                let unnamed = ctx.seam.unnamed_devices(&batch);
                if unnamed.into_iter().filter(|id| asked.insert(*id)).count() > 0 {
                    ctx.seam.hello_received(&transport.hello(None).await?)?;
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
///
/// Sets `progress` once the server settles a command.
async fn send(
    ctx: &LinkCtx,
    transport: &dyn Transport,
    progress: &AtomicBool,
    errors: &mut HeadCount,
) -> Result<Infallible, LinkError> {
    let mut failures = HeadCount::default();
    loop {
        let Some(entry) = ctx.seam.pending_outbox()?.into_iter().next() else {
            ctx.outbox.notified().await;
            continue;
        };
        if send_entry(ctx, transport, &entry, errors, &mut failures).await? {
            progress.store(true, Ordering::Relaxed);
        }
    }
}

/// What staging a message's images and files came to.
enum Staged {
    /// Post the message: its images are staged, or a low disk stopped the
    /// uploads, and its files are in the workdir.
    Post,
    /// The message cannot be posted, and why.
    Refused(AttachmentRefusal),
    /// The user took the message back meanwhile: it is not posted.
    Withdrawn,
    /// The workdir holds other content at file `position`'s name, with the
    /// hash `existing`: the message cannot be posted.
    Exists {
        position: usize,
        existing: ContentHash,
    },
}

/// Uploads the images a `SendMessage` names, in order, so the server holds
/// them staged when the message is posted. [`Staged::Post`] means post now;
/// [`Staged::Refused`] means the message cannot be posted, and why.
///
/// Every hash is checked with a HEAD on every attempt, and the ones not
/// staged are PUT from the local file. Nothing is remembered between
/// attempts: the server unlinks the staged files a message used, so a
/// cached "staged" would post an image the server no longer holds. A local
/// file that is missing or unreadable, or over [`ATTACHMENT_MAX_BYTES`], is
/// refused without an upload, as is an upload the server refuses as too
/// large, not an image or a hash mismatch. A `DiskLow` answer stops the
/// uploads and posts the message anyway: the server then refuses it with a
/// stored `DiskLow` rejection like any message on a low disk. Waiting here
/// would hold up the one outbox, interrupts and approvals included. A
/// transport error is returned for [`send_entry`] to handle, as its
/// request's error would be.
///
/// A message the user takes back while its images upload is not posted:
/// it is checked to be still pending before each image's HEAD and once more
/// after the last upload, and [`Staged::Withdrawn`] stops everything. A PUT
/// already in flight may still finish; nothing after it is sent. A message
/// without images is not checked.
async fn stage_attachments(
    ctx: &LinkCtx,
    transport: &dyn Transport,
    command: &Command,
) -> Result<Staged, LinkError> {
    let CommandKind::SendMessage { attachments, .. } = &command.kind else {
        return Ok(Staged::Post);
    };
    if attachments.is_empty() {
        return Ok(Staged::Post);
    }
    let data_dir = ctx.seam.data_dir();
    for &sha in attachments {
        if !still_pending(ctx, command.id)? {
            return Ok(Staged::Withdrawn);
        }
        match stage_one(transport, &data_dir, sha).await? {
            ImageStaging::Staged => {}
            ImageStaging::DiskLow => break,
            ImageStaging::Refused(reason) => return Ok(Staged::Refused(reason)),
        }
    }
    if !still_pending(ctx, command.id)? {
        return Ok(Staged::Withdrawn);
    }
    Ok(Staged::Post)
}

/// What staging one image came to.
enum ImageStaging {
    /// The server holds the image.
    Staged,
    /// A low disk stopped the upload.
    DiskLow,
    /// The image cannot be staged, and why.
    Refused(AttachmentRefusal),
}

/// Stages one image: a HEAD, then, unless the server holds it, the local
/// file is PUT. A missing, unreadable or oversized file is refused without an
/// upload. A transport error is returned to the caller.
async fn stage_one(
    transport: &dyn Transport,
    data_dir: &Path,
    sha: ContentHash,
) -> Result<ImageStaging, TransportError> {
    let held = match transport.attachment_staged(sha).await? {
        StagedUpload::Staged => return Ok(ImageStaging::Staged),
        StagedUpload::Missing { held } => held,
    };
    let path = attachment_path(data_dir, sha);
    // Opening the copy is what finds an unreadable one; the upload then opens
    // it again, so the handle is dropped here.
    let opened = match tokio::fs::File::open(&path).await {
        Ok(file) => file.metadata().await,
        Err(error) => Err(error),
    };
    let size = match opened {
        Ok(meta) => meta.len(),
        Err(error) => {
            tracing::warn!(%error, sha256 = %sha.to_hex(), "attachment file cannot be read");
            return Ok(ImageStaging::Refused(AttachmentRefusal::Missing));
        }
    };
    if size > ATTACHMENT_MAX_BYTES {
        tracing::warn!(sha256 = %sha.to_hex(), size, "attachment is over the upload limit");
        return Ok(ImageStaging::Refused(AttachmentRefusal::TooLarge));
    }
    let answer = upload_resuming(transport, sha, UploadFile { path, len: size }, held).await?;
    Ok(match answer {
        UploadResult::Stored { .. } => ImageStaging::Staged,
        UploadResult::DiskLow => ImageStaging::DiskLow,
        UploadResult::TooLarge => ImageStaging::Refused(AttachmentRefusal::TooLarge),
        UploadResult::HashMismatch => ImageStaging::Refused(AttachmentRefusal::HashMismatch),
        UploadResult::OffsetMismatch { .. } => {
            return Err(TransportError::Io("upload offset kept moving".to_owned()));
        }
    })
}

/// Writes the files a message carries into the workdir root, in order, so
/// they are there when it is posted. [`Staged::Post`] means post now: every
/// file is written, or the message has none. A file the workdir already holds
/// with other content stops the message as [`Staged::Exists`], and a file
/// that cannot be written as [`Staged::Refused`]. The user taking the message
/// back is checked as [`stage_attachments`] checks it. A transport error is
/// returned for [`send_entry`] to handle.
async fn write_files(
    ctx: &LinkCtx,
    transport: &dyn Transport,
    entry: &OutboxEntry,
) -> Result<Staged, LinkError> {
    let CommandKind::SendMessage { session_id, .. } = &entry.command.kind else {
        return Ok(Staged::Post);
    };
    if entry.files.is_empty() {
        return Ok(Staged::Post);
    }
    let data_dir = ctx.seam.data_dir();
    for (position, file) in entry.files.iter().enumerate() {
        if !still_pending(ctx, entry.command.id)? {
            return Ok(Staged::Withdrawn);
        }
        match write_one(transport, &data_dir, *session_id, &file.file).await? {
            FileWrite::Written => {}
            FileWrite::Exists(existing) => return Ok(Staged::Exists { position, existing }),
            FileWrite::Refused(reason) => return Ok(Staged::Refused(reason)),
        }
    }
    if !still_pending(ctx, entry.command.id)? {
        return Ok(Staged::Withdrawn);
    }
    Ok(Staged::Post)
}

/// What writing one file of a message came to.
enum FileWrite {
    /// The workdir holds the file's content at its name.
    Written,
    /// The workdir holds other content at its name, with this hash.
    Exists(ContentHash),
    /// The file cannot be written, and why.
    Refused(AttachmentRefusal),
}

/// A file that counts the bytes read from it, so an upload can send exactly
/// the bytes it hashed.
struct CountingFile {
    file: std::fs::File,
    count: u64,
}

impl std::io::Read for CountingFile {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.file.read(buf)?;
        self.count += n as u64;
        Ok(n)
    }
}

/// The hash of the file at `path` and the number of bytes it hashed, by a
/// streaming read on a blocking thread: the file is never held whole. The
/// upload then sends exactly those bytes ([`UploadFile`]).
async fn hash_file(path: &Path) -> std::io::Result<(ContentHash, u64)> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || -> std::io::Result<(ContentHash, u64)> {
        let mut counted = CountingFile {
            file: std::fs::File::open(&path)?,
            count: 0,
        };
        let hash = ContentHash::read(&mut counted)?;
        Ok((hash, counted.count))
    })
    .await
    .map_err(std::io::Error::other)?
}

/// Most PUTs one [`upload_resuming`] call makes before it gives up.
const UPLOAD_ATTEMPTS: u32 = 3;

/// Uploads `file` as `sha256` from the `held` bytes the server keeps of an
/// interrupted upload (0 if more than the file). `OffsetMismatch { have }`
/// re-PUTs from `have`; a `HashMismatch` to a resumed PUT restarts from 0 once,
/// as the server dropped its partial. Still unsettled after
/// [`UPLOAD_ATTEMPTS`] PUTs is a retryable error: the caller starts from the HEAD.
async fn upload_resuming(
    transport: &dyn Transport,
    sha256: ContentHash,
    file: UploadFile,
    held: u64,
) -> Result<UploadResult, TransportError> {
    let mut from = if held <= file.len { held } else { 0 };
    for _ in 0..UPLOAD_ATTEMPTS {
        let answer = transport
            .upload_attachment(sha256, file.clone(), from)
            .await?;
        from = match answer {
            UploadResult::OffsetMismatch { have } if have <= file.len => have,
            UploadResult::OffsetMismatch { .. } => 0,
            UploadResult::HashMismatch if from > 0 => 0,
            other => return Ok(other),
        };
    }
    Err(TransportError::Io("upload offset kept moving".to_owned()))
}

/// Writes one file of a message into the workdir root as its name. The device
/// copy must still hash to the file's hash; it is staged the way an image is
/// (a HEAD, then a PUT unless the server holds it), then written with the
/// file's `replace`. A `NotStaged` answer uploads the file once more and
/// writes it again; a second one refuses it. A write that finds the same
/// content already there counts as written, so a resend after a lost answer
/// carries on. A transport error is returned to the caller.
async fn write_one(
    transport: &dyn Transport,
    data_dir: &Path,
    session: SessionId,
    file: &AttachedFile,
) -> Result<FileWrite, TransportError> {
    let path = attachment_path(data_dir, file.sha256);
    match tokio::fs::metadata(&path).await {
        Ok(meta) if meta.len() > PUSH_MAX_BYTES => {
            tracing::warn!(name = %file.name, "message file is over the upload limit");
            return Ok(FileWrite::Refused(AttachmentRefusal::TooLarge));
        }
        Ok(_) => {}
        Err(error) => {
            tracing::warn!(%error, name = %file.name, "message file cannot be read");
            return Ok(FileWrite::Refused(AttachmentRefusal::Missing));
        }
    }
    let (hash, len) = match hash_file(&path).await {
        Ok(hashed) => hashed,
        Err(error) => {
            tracing::warn!(%error, name = %file.name, "message file cannot be read");
            return Ok(FileWrite::Refused(AttachmentRefusal::Missing));
        }
    };
    if hash != file.sha256 {
        tracing::warn!(name = %file.name, "message file no longer holds its content");
        return Ok(FileWrite::Refused(AttachmentRefusal::Missing));
    }
    let sha = file.sha256;
    let mut round = 1;
    loop {
        // A second round follows a `NotStaged` write: no HEAD, and the upload
        // starts from 0.
        let state = if round > 1 {
            StagedUpload::Missing { held: 0 }
        } else {
            transport.attachment_staged(sha).await?
        };
        if let StagedUpload::Missing { held } = state {
            let file = UploadFile {
                path: path.clone(),
                len,
            };
            match upload_resuming(transport, sha, file, held).await? {
                UploadResult::Stored { .. } => {}
                UploadResult::TooLarge => {
                    return Ok(FileWrite::Refused(AttachmentRefusal::TooLarge));
                }
                UploadResult::HashMismatch => {
                    return Ok(FileWrite::Refused(AttachmentRefusal::HashMismatch));
                }
                UploadResult::DiskLow => {
                    return Ok(FileWrite::Refused(AttachmentRefusal::ServerDiskLow));
                }
                UploadResult::OffsetMismatch { .. } => {
                    return Err(TransportError::Io("upload offset kept moving".to_owned()));
                }
            }
        }
        let request = WriteFileRequest {
            session,
            path: file.name.clone(),
            content: sha,
            replace: file.replace,
        };
        let answer = transport.write_workdir_file(&request).await?;
        if matches!(answer, WriteFileResult::NotStaged) && round == 1 {
            tracing::debug!(
                name = %file.name,
                "workdir write found the file unstaged; uploading it again"
            );
            round += 1;
            continue;
        }
        return Ok(match answer {
            WriteFileResult::Written { .. } | WriteFileResult::Same { .. } => FileWrite::Written,
            WriteFileResult::Exists { sha256, .. } if sha256 == sha => FileWrite::Written,
            WriteFileResult::Exists { sha256, .. } => FileWrite::Exists(sha256),
            WriteFileResult::DiskLow => FileWrite::Refused(AttachmentRefusal::ServerDiskLow),
            _ => FileWrite::Refused(AttachmentRefusal::FileNotWritten),
        });
    }
}

/// Pushes one device file into a session's workdir. The file is read where
/// it is, not copied into the data dir, and hashed by a streaming read; the
/// upload sends the bytes hashed, so the content sent is the content named (a
/// file changed in between fails as the server's hash mismatch). A HEAD, then,
/// unless the server holds the bytes, a PUT, then the write at `path`. A file
/// that is missing, unreadable or over [`PUSH_MAX_BYTES`] is refused without an
/// upload. A transport error is returned to the caller.
pub(crate) async fn push_workdir_file(
    transport: &dyn Transport,
    source: &Path,
    session: SessionId,
    path: String,
    replace: Option<ContentHash>,
) -> Result<PushOutcome, TransportError> {
    match tokio::fs::metadata(source).await {
        Ok(meta) if meta.len() > PUSH_MAX_BYTES => {
            tracing::warn!("workdir push file is over the upload limit");
            return Ok(PushOutcome::UploadRefused(UploadResult::TooLarge));
        }
        Ok(_) => {}
        Err(error) => {
            tracing::warn!(%error, "workdir push file cannot be read");
            return Ok(PushOutcome::Unreadable {
                reason: error.to_string(),
            });
        }
    }
    let (sha, len) = match hash_file(source).await {
        Ok(hashed) => hashed,
        Err(error) => {
            tracing::warn!(%error, "workdir push file cannot be read");
            return Ok(PushOutcome::Unreadable {
                reason: error.to_string(),
            });
        }
    };
    if let StagedUpload::Missing { held } = transport.attachment_staged(sha).await? {
        let file = UploadFile {
            path: source.to_path_buf(),
            len,
        };
        let answer = upload_resuming(transport, sha, file, held).await?;
        if !matches!(answer, UploadResult::Stored { .. }) {
            return Ok(PushOutcome::UploadRefused(answer));
        }
    }
    let request = WriteFileRequest {
        session,
        path,
        content: sha,
        replace,
    };
    let answer = transport.write_workdir_file(&request).await?;
    Ok(PushOutcome::Written(answer))
}

/// The background stager for agent drafts: uploads the images a draft
/// gains as soon as it gains them, so on a bad link the bytes are usually on
/// the server before the message is sent. It runs beside the pull and the
/// outbox sender, and wakes when a draft gains images
/// ([`Request::DraftImagesChanged`]).
///
/// Each image is staged once per connection. `done` only stops this loop
/// from repeating itself; nothing else trusts it. The outbox still HEADs
/// every hash before it posts a message, since the server unlinks the staged
/// files a message uses. Abandoned drafts leave staged files behind, which
/// the server's own cap and age eviction remove. A refused or missing image
/// is marked done and left for the outbox to report when its message is
/// sent. A transport error waits a backoff and tries again: it never ends
/// the connection, as the pull is what notices a dead link.
async fn stage_drafts(ctx: &LinkCtx, transport: &dyn Transport) -> Result<Infallible, LinkError> {
    let data_dir = ctx.seam.data_dir();
    let mut done: HashSet<ContentHash> = HashSet::new();
    let mut failures = 0u32;
    loop {
        let wanted: Vec<ContentHash> = ctx
            .seam
            .draft_image_hashes()?
            .into_iter()
            .filter(|sha| !done.contains(sha))
            .collect();
        if wanted.is_empty() {
            ctx.drafts.notified().await;
            continue;
        }
        let mut failed = false;
        for sha in wanted {
            match stage_one(transport, &data_dir, sha).await {
                Ok(ImageStaging::Staged) => {}
                Ok(ImageStaging::DiskLow) => {
                    tracing::debug!(sha256 = %sha.to_hex(), "disk low; left for the outbox");
                }
                Ok(ImageStaging::Refused(reason)) => {
                    tracing::debug!(
                        ?reason,
                        sha256 = %sha.to_hex(),
                        "refused; left for the outbox"
                    );
                }
                Err(error) => {
                    tracing::debug!(%error, "staging a draft image failed; trying again later");
                    failed = true;
                    break;
                }
            }
            done.insert(sha);
        }
        if failed {
            failures = failures.saturating_add(1);
            let after = ctx.config.backoff.delay(failures);
            tokio::select! {
                () = tokio::time::sleep(after) => {}
                () = ctx.drafts.notified() => {}
            }
        } else {
            failures = 0;
        }
    }
}

/// Claims and sends one outbox entry read earlier, and says whether the
/// server settled it: answered it or refused it for good. An entry that can no
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
/// unreadable reply before the pull can apply the event. It does not count
/// as settled by the server here: the pull that settles it is progress of
/// its own.
///
/// A server error (5xx) is an answer too, but not about the command: the
/// link stays up and the entry is sent again after [`Backoff::delay`] for
/// the count so far, so the pull on this connection goes on meanwhile. The
/// [`MAX_SERVER_ERRORS`]th in a row for one command ends the resends: it is
/// settled from the log like an unreadable reply, so one command the server
/// keeps failing cannot hold back every later one forever, and one it did
/// take (D7 dedups the resends) is not shown failed.
///
/// A request that failed on its own ([`TransportError::failed_request`]: a
/// stalled upload, a reply that never came, a broken connection) is no
/// answer at all, so it settles nothing; but it is no sign the link is down
/// either, as the pull runs on a connection of its own. The link stays up
/// and the entry is sent again after [`Backoff::delay`] for the count of
/// such failures in a row (`failures`), as often as it takes: if the link
/// is down, the pull fails and ends the connection. The server may have
/// taken the command meanwhile; then the pull settles it and the resend
/// cannot claim it, or the server answers the resend from its dedup. The
/// user may take it back while it waits ([`resend_after`]); an answer to a
/// request still out then settles nothing.
///
/// Before the request, a message's images are staged ([`stage_attachments`]),
/// then its files are written into the workdir ([`write_files`]). An image or
/// file that cannot be staged or written settles the message as
/// `AttachmentRefused`, final and never posted, and the link stays up. A
/// transport error while staging is handled as the same error from the
/// request itself, and the staging runs again on the resend. A message the
/// user takes back while its images upload is not posted, and the sender
/// moves on to the next entry ([`stage_attachments`]).
async fn send_entry(
    ctx: &LinkCtx,
    transport: &dyn Transport,
    entry: &OutboxEntry,
    errors: &mut HeadCount,
    failures: &mut HeadCount,
) -> Result<bool, LinkError> {
    let id = entry.command.id;
    if !ctx.seam.command_sending(id)? {
        return Ok(false);
    }
    let staged = match stage_attachments(ctx, transport, &entry.command).await {
        Ok(Staged::Post) => write_files(ctx, transport, entry).await,
        other => other,
    };
    let sent: Result<Result<CommandResult, AttachmentRefusal>, LinkError> = match staged {
        Ok(Staged::Post) => transport
            .send_command(&entry.command)
            .await
            .map(Ok)
            .map_err(LinkError::Transport),
        Ok(Staged::Refused(reason)) => Ok(Err(reason)),
        Ok(Staged::Exists { position, existing }) => {
            tracing::warn!(
                ?id,
                position,
                "a message file is in the workdir with other content; giving up on the message"
            );
            ctx.seam.command_file_exists(id, position, existing)?;
            return Ok(true);
        }
        Ok(Staged::Withdrawn) => {
            tracing::debug!(
                ?id,
                "withdrawn while its attachments uploaded; not posting it"
            );
            return Ok(false);
        }
        Err(error) => Err(error),
    };
    match sent {
        Ok(Ok(result)) => {
            ctx.seam.command_result(id, &result)?;
            Ok(true)
        }
        Ok(Err(reason)) => {
            tracing::warn!(
                ?reason,
                ?id,
                "the outbox could not stage an attachment; giving up on the message"
            );
            ctx.seam.command_attachment_refused(id, reason)?;
            Ok(true)
        }
        Err(LinkError::Transport(error)) if error.refuses_command() => {
            tracing::warn!(%error, ?id, "server refused the command; giving up on it");
            ctx.seam.command_undeliverable(id)?;
            Ok(true)
        }
        Err(LinkError::Transport(TransportError::Decode(error))) => {
            tracing::warn!(%error, ?id, "unreadable reply; waiting for the log to settle it");
            settled_by_log(ctx, transport, id).await?;
            Ok(false)
        }
        Err(LinkError::Transport(error)) if error.is_server_error() => {
            let count = errors.note(id);
            if count < MAX_SERVER_ERRORS {
                let after = ctx.config.backoff.delay(count);
                tracing::warn!(%error, ?id, count, ?after, "server error; sending again");
                resend_after(ctx, id, after).await?;
            } else {
                tracing::warn!(%error, ?id, count, "server error again; settling from the log");
                settled_by_log(ctx, transport, id).await?;
            }
            Ok(false)
        }
        Err(LinkError::Transport(error)) if error.failed_request() => {
            let count = failures.note(id);
            let after = ctx.config.backoff.delay(count);
            tracing::debug!(%error, ?id, count, ?after, "command request failed; sending again");
            resend_after(ctx, id, after).await?;
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

/// Whether the outbox still holds command `id` unanswered: the user has not
/// taken it back (`Store::withdraw`), and the server has not answered it.
fn still_pending(ctx: &LinkCtx, id: CommandId) -> Result<bool, LinkError> {
    Ok(ctx
        .seam
        .pending_outbox()?
        .iter()
        .any(|e| e.command.id == id))
}

/// Waits `after` before the outbox command `id` is sent again, or less once
/// it is no longer pending: the user took it back ([`Store::withdraw`]), so
/// the next command goes at once, or the log settled it.
///
/// [`Store::withdraw`]: crate::Store::withdraw
async fn resend_after(ctx: &LinkCtx, id: CommandId, after: Duration) -> Result<(), LinkError> {
    let resend = tokio::time::sleep(after);
    tokio::pin!(resend);
    loop {
        // Asked for before the look, so a change in between still wakes it.
        let changed = ctx.outbox.notified();
        if !still_pending(ctx, id)? {
            return Ok(());
        }
        tokio::select! {
            () = &mut resend => return Ok(()),
            () = changed => {}
        }
    }
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
    let head = transport.hello(None).await?.head;
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
    use std::collections::{HashMap, VecDeque};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use skywave_proto::{
        Attachment, BlobId as ProtoBlobId, BlobRef, Command, CommandId, CommandKind, CommandResult,
        ContentHash, EventBatch, EventKind, HelloResponse, ImageType, PROTOCOL_VERSION, ServerId,
        SessionId, UploadResult, WriteFileRequest, WriteFileResult,
    };

    use super::*;
    use crate::Core;
    use crate::attachment::{AttachedImage, attachment_dir};
    use crate::backoff::Jitter;
    use crate::blobs::blob_dir;
    use crate::identity::PairedCredentials;
    use crate::store::{BlobStatus, OutboxStatus, Store};
    use crate::transport::{BlobDownload, BoxFuture};
    use crate::view::{DeviceId, FailReason, Link, Outgoing, Screen};

    /// The one log, in one run, every fake server here serves: whatever
    /// run it is asked about is the current one.
    fn one_run() -> skywave_proto::LogRun {
        skywave_proto::LogRun {
            log: skywave_proto::LogId(uuid::Uuid::from_u128(1)),
            run: skywave_proto::RunId(uuid::Uuid::from_u128(2)),
            since: Some(skywave_proto::RunsSince::Known { next_start: None }),
        }
    }

    /// A server that is down until `up` is set, then answers `hello` and
    /// long-polls that never end.
    #[derive(Default)]
    struct FlakyServer {
        up: AtomicBool,
        hellos: AtomicUsize,
    }

    impl Transport for FlakyServer {
        fn hello(
            &self,
            _since: Option<skywave_proto::RunId>,
        ) -> BoxFuture<'_, Result<HelloResponse, TransportError>> {
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
                    log: one_run(),
                    devices: Vec::new(),
                    shell: false,
                    screen: false,
                    bypass: false,
                    login_expires: None,
                    agent_login_offered: false,
                    agent_logged_out: false,
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
            &"srv:7443".parse().unwrap(),
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
                    worktree: None,
                },
            }],
        };
        bound.apply_events(&batch(1)).unwrap();

        seam.pairing_succeeded(
            &"other:7443".parse().unwrap(),
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
        bound.set_link(PairedLink::Online { latency_ms: 1 });
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
            connector: Connector::new(move |_, _, _, _| Ok(fake.clone() as Arc<dyn Transport>)),
            wait_secs: 5,
            wall_clock,
            ..EngineConfig::default()
        };
        let ctx = LinkCtx {
            seam: seam.clone(),
            config: Arc::new(config),
            outbox: Arc::new(Notify::new()),
            drafts: Arc::new(Notify::new()),
            retry_now: Arc::new(Notify::new()),
            blobs: Arc::new(BlobQueue::default()),
            address: "srv:7443".parse().unwrap(),
            identity: seam.pairing().unwrap(),
            run: ExpectedRun::default(),
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
                        ..
                    } => Some((*attempt, (*retry_at - *offline_since).num_milliseconds())),
                    _ => None,
                })
                .collect()
        };
        let outages = |links: &[Link]| -> Vec<(DateTime<Utc>, DateTime<Utc>, LinkFailure)> {
            links
                .iter()
                .filter_map(|l| match l {
                    Link::Reconnecting {
                        retry_at,
                        offline_since,
                        reason,
                        ..
                    } => Some((*retry_at, *offline_since, *reason)),
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
        assert!(
            first[..6].iter().all(|o| o.2 == LinkFailure::Unreachable),
            "each wait says why its attempt failed: {first:?}"
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
        let second = outages(&links.lock().unwrap());
        assert!(second[before].1 > first[0].1, "offline_since starts over");
        // The pull broke first, then `hello` found the server gone: the
        // reason follows the latest failure.
        assert_eq!(
            [second[before].2, second[before + 1].2],
            [LinkFailure::ConnectionLost, LinkFailure::Unreachable]
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
            connector: Connector::new(move |_, _, _, _| Ok(fake.clone() as Arc<dyn Transport>)),
            ..EngineConfig::default()
        };
        let retry_now = Arc::new(Notify::new());
        let ctx = LinkCtx {
            seam: seam.clone(),
            config: Arc::new(config),
            outbox: Arc::new(Notify::new()),
            drafts: Arc::new(Notify::new()),
            retry_now: retry_now.clone(),
            blobs: Arc::new(BlobQueue::default()),
            address: "srv:7443".parse().unwrap(),
            identity: seam.pairing().unwrap(),
            run: ExpectedRun::default(),
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
                worktree: None,
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

    /// A server behind a proxy that cuts every connection: `hello` works,
    /// the first command of a connection is answered and any later one is
    /// cut, and every long-poll is cut after `CUT` without returning.
    #[derive(Default)]
    struct CuttingServer {
        /// A command was answered since the last `hello`.
        answered: AtomicBool,
        seen: Mutex<Vec<CommandId>>,
    }

    impl CuttingServer {
        const CUT: Duration = Duration::from_secs(3);
    }

    impl Transport for CuttingServer {
        fn hello(
            &self,
            _since: Option<skywave_proto::RunId>,
        ) -> BoxFuture<'_, Result<HelloResponse, TransportError>> {
            Box::pin(async move {
                self.answered.store(false, Ordering::SeqCst);
                Ok(HelloResponse {
                    protocol_version: PROTOCOL_VERSION,
                    server_id: ServerId::new(),
                    device_id: DeviceId::new(),
                    // Its log holds what it accepted: below that, the
                    // device would see the log lost history.
                    head: Seq(self.seen.lock().unwrap().len() as u64),
                    log: one_run(),
                    devices: Vec::new(),
                    shell: false,
                    screen: false,
                    bypass: false,
                    login_expires: None,
                    agent_login_offered: false,
                    agent_logged_out: false,
                })
            })
        }

        fn send_command<'a>(
            &'a self,
            command: &'a Command,
        ) -> BoxFuture<'a, Result<CommandResult, TransportError>> {
            Box::pin(async move {
                if self.answered.swap(true, Ordering::SeqCst) {
                    return Err(TransportError::Io("cut".into()));
                }
                let mut seen = self.seen.lock().unwrap();
                seen.push(command.id);
                Ok(CommandResult::Accepted {
                    seq: Seq(seen.len() as u64),
                })
            })
        }

        fn events(&self, _query: EventsQuery) -> BoxFuture<'_, Result<EventBatch, TransportError>> {
            Box::pin(async {
                tokio::time::sleep(CuttingServer::CUT).await;
                Err(TransportError::Io("cut".into()))
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

    /// Connections that each get one command through before they are cut
    /// keep the attempt count at 1, so the queue drains at the base delay.
    /// Once the outbox is empty, a connection that only got through `hello`
    /// is no progress and the count climbs again. Every connection got past
    /// `hello`, so each failure starts its own offline stretch.
    #[tokio::test(start_paused = true)]
    async fn connections_that_deliver_a_command_keep_the_attempt_count_low() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let commands: Vec<_> = (0..4)
            .map(|n| enqueue(&seam, &format!("m{n}")).id)
            .collect();
        let links = Arc::new(Mutex::new(Vec::new()));
        let sink = links.clone();
        seam.core
            .subscribe(move |view| sink.lock().unwrap().push(view.link));

        let server = Arc::new(CuttingServer::default());
        let fake = server.clone();
        let (wall_clock, _) = paused_wall();
        let config = EngineConfig {
            backoff: Backoff {
                base: Duration::from_secs(1),
                cap: Duration::from_secs(8),
                jitter: Jitter::fixed(1.0),
            },
            connector: Connector::new(move |_, _, _, _| Ok(fake.clone() as Arc<dyn Transport>)),
            wall_clock,
            ..EngineConfig::default()
        };
        let link = tokio::spawn(run_link(link_ctx(&seam, config, Arc::new(Notify::new()))));

        // `retry_at - offline_since` is the delay when the stretch started
        // at this very failure. Snapshots for outbox changes repeat the
        // link, so one entry per wait.
        let reconnects = |links: &[Link]| -> Vec<(u32, i64)> {
            let mut links = links.to_vec();
            links.dedup();
            links
                .iter()
                .filter_map(|l| match l {
                    Link::Reconnecting {
                        attempt,
                        retry_at,
                        offline_since,
                        ..
                    } => Some((*attempt, (*retry_at - *offline_since).num_milliseconds())),
                    _ => None,
                })
                .collect()
        };
        while reconnects(&links.lock().unwrap()).len() < 6 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(*server.seen.lock().unwrap(), commands);
        assert!(seam.pending_outbox().unwrap().is_empty());
        assert_eq!(
            reconnects(&links.lock().unwrap())[..6],
            [
                (1, 1_000),
                (1, 1_000),
                (1, 1_000),
                (1, 1_000),
                (2, 2_000),
                (3, 4_000)
            ]
        );
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
            connector: Connector::new(move |_, _, _, _| Ok(fake.clone() as Arc<dyn Transport>)),
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
            drafts: Arc::new(Notify::new()),
            retry_now,
            blobs: Arc::new(BlobQueue::default()),
            address: "srv:7443".parse().unwrap(),
            identity: seam.pairing().unwrap(),
            run: ExpectedRun::default(),
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

    /// An identity whose stored server address does not parse (the
    /// address schema 29 dropped) is no pairing: the device shows the
    /// pairing screen, unpaired, rather than `Connecting` with nothing
    /// trying, and the engine has nothing to connect to.
    #[tokio::test(start_paused = true)]
    async fn an_identity_without_a_usable_address_is_unpaired() {
        let dir = tempfile::tempdir().unwrap();
        drop(crate::Store::open(dir.path()).unwrap());
        let db = rusqlite::Connection::open(dir.path().join(crate::store::DB_FILE)).unwrap();
        db.execute(
            "INSERT INTO settings (key, value) VALUES ('server_address', 'host:port')",
            [],
        )
        .unwrap();
        db.execute_batch(
            "DROP TABLE safety_rules;
             ALTER TABLE sessions DROP COLUMN safety_check;
             ALTER TABLE sessions DROP COLUMN worktree_removed;
             ALTER TABLE sessions DROP COLUMN worktree;",
        )
        .unwrap();
        db.pragma_update(None, "user_version", crate::store::SCHEMA_VERSION - 9)
            .unwrap();
        drop(db);
        crate::identity::write_legacy_identity(
            dir.path(),
            &PairedCredentials {
                device_id: DeviceId::new(),
                device_key_pem: "KEY".into(),
                device_cert_pem: "CERT".into(),
                ca_cert_pem: "CA".into(),
            },
        );
        let core = Core::open_detached(dir.path()).unwrap();
        let seam = core.seam();
        assert!(seam.pairing().is_none());
        let view = core.view();
        assert_eq!(view.link, Link::Unpaired);
        assert!(matches!(view.screen, Screen::Pairing(_)), "{view:?}");

        let connects = Arc::new(AtomicUsize::new(0));
        let counted = connects.clone();
        let config = EngineConfig {
            connector: Connector::new(move |_, _, _, _| {
                counted.fetch_add(1, Ordering::SeqCst);
                Err(TransportError::Unreachable("no server here".into()))
            }),
            ..EngineConfig::default()
        };
        let (requests, requests_rx) = mpsc::unbounded_channel();
        let (_suspended, suspended_rx) = watch::channel(false);
        let engine =
            tokio::spawn(Engine::new(seam.clone(), config, requests_rx, suspended_rx).run());
        requests.send(Request::Connect).unwrap();
        tokio::time::sleep(Duration::from_secs(600)).await;
        assert_eq!(connects.load(Ordering::SeqCst), 0, "never tried");
        engine.abort();
    }

    /// A server whose link stays up, that opens a login on `flow` and
    /// answers its finishes from `answers` in turn (`None` never answers),
    /// noting each finish it gets.
    struct LoginServer {
        link: FlakyServer,
        flow: skywave_proto::LoginFlowId,
        answers: Mutex<Vec<Option<Result<skywave_proto::LoginFinishResult, TransportError>>>>,
        finishes: Mutex<Vec<skywave_proto::LoginFinish>>,
    }

    impl Transport for LoginServer {
        fn hello(
            &self,
            since: Option<skywave_proto::RunId>,
        ) -> BoxFuture<'_, Result<HelloResponse, TransportError>> {
            self.link.hello(since)
        }

        fn send_command<'a>(
            &'a self,
            command: &'a Command,
        ) -> BoxFuture<'a, Result<CommandResult, TransportError>> {
            self.link.send_command(command)
        }

        fn events(&self, query: EventsQuery) -> BoxFuture<'_, Result<EventBatch, TransportError>> {
            self.link.events(query)
        }

        fn blob(
            &self,
            id: ProtoBlobId,
            from: u64,
        ) -> BoxFuture<'_, Result<BlobDownload, TransportError>> {
            self.link.blob(id, from)
        }

        fn login_start(
            &self,
        ) -> BoxFuture<'_, Result<skywave_proto::LoginStartResult, TransportError>> {
            Box::pin(async move {
                Ok(skywave_proto::LoginStartResult::Open {
                    flow: self.flow,
                    url: "https://login.example/authorize?state=s".into(),
                })
            })
        }

        fn login_finish<'a>(
            &'a self,
            finish: &'a skywave_proto::LoginFinish,
        ) -> BoxFuture<'a, Result<skywave_proto::LoginFinishResult, TransportError>> {
            Box::pin(async move {
                self.finishes.lock().unwrap().push(finish.clone());
                let answer = self.answers.lock().unwrap().remove(0);
                match answer {
                    Some(answer) => answer,
                    None => std::future::pending().await,
                }
            })
        }
    }

    /// Waits (on the paused clock) until the settings screen's login flow
    /// is as `want` says, and returns it.
    async fn login_flow(
        core: &Core,
        want: impl Fn(&crate::view::LoginFlowView) -> bool,
    ) -> crate::view::LoginFlowView {
        loop {
            let flow = match core.view().screen {
                Screen::Settings(settings) => settings.login.flow,
                other => panic!("expected settings, got {other:?}"),
            };
            if want(&flow) {
                return flow;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// A finish the server never answers gives up at `LOGIN_TIMEOUT`, one
    /// it answers with a `503` gives up at once; either keeps the flow and
    /// the code, and each retry sends exactly what the first finish sent.
    #[tokio::test(start_paused = true)]
    async fn a_login_finish_without_an_answer_is_resent_unchanged() {
        use crate::Action;
        use crate::view::{LoginFlowView, LoginNoAnswer, LoginOutcome};

        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let core = seam.core.clone();
        let asked = seam.take_requests().unwrap();
        let flow = skywave_proto::LoginFlowId::random();
        let server = Arc::new(LoginServer {
            link: FlakyServer::default(),
            flow,
            answers: Mutex::new(vec![
                None,
                Some(Err(TransportError::BadStatus(503))),
                Some(Ok(skywave_proto::LoginFinishResult::LoggedIn {
                    expires: None,
                })),
            ]),
            finishes: Mutex::new(Vec::new()),
        });
        server.link.up.store(true, Ordering::SeqCst);
        let fake = server.clone();
        let config = EngineConfig {
            connector: Connector::new(move |_, _, _, _| Ok(fake.clone() as Arc<dyn Transport>)),
            ..EngineConfig::default()
        };
        let (requests, requests_rx) = mpsc::unbounded_channel();
        let (_suspended, suspended_rx) = watch::channel(false);
        let engine =
            tokio::spawn(Engine::new(seam.clone(), config, requests_rx, suspended_rx).run());
        // What the core asks of the login goes to this engine.
        let dispatch = |action: Action| {
            core.dispatch(action).unwrap();
            for request in asked.try_iter() {
                if matches!(
                    request,
                    Request::LoginStart { .. } | Request::LoginFinish { .. }
                ) {
                    requests.send(request).unwrap();
                }
            }
        };

        dispatch(Action::Navigate(crate::Route::Settings));
        dispatch(Action::StartAgentLogin);
        login_flow(&core, |f| matches!(f, LoginFlowView::Open { .. })).await;

        let sent = tokio::time::Instant::now();
        dispatch(Action::FinishAgentLogin {
            code: crate::LoginCode::new("code#state"),
        });
        let stalled = login_flow(&core, |f| matches!(f, LoginFlowView::NoAnswer { .. })).await;
        assert!(sent.elapsed() >= LOGIN_TIMEOUT, "gave up at the bound");
        assert!(matches!(
            stalled,
            LoginFlowView::NoAnswer {
                why: LoginNoAnswer::Failed(_),
                ..
            }
        ));

        dispatch(Action::RetryAgentLogin);
        let refused = login_flow(&core, |f| matches!(f, LoginFlowView::NoAnswer { .. })).await;
        assert_eq!(
            refused,
            LoginFlowView::NoAnswer {
                url: "https://login.example/authorize?state=s".into(),
                why: LoginNoAnswer::Failed(TransportError::BadStatus(503).to_string()),
            }
        );

        dispatch(Action::RetryAgentLogin);
        login_flow(&core, |f| *f == LoginFlowView::Done(LoginOutcome::LoggedIn)).await;
        let finishes = server.finishes.lock().unwrap().clone();
        let first = skywave_proto::LoginFinish {
            flow,
            code: "code#state".into(),
        };
        assert_eq!(finishes, vec![first.clone(), first.clone(), first]);
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
        fn hello(
            &self,
            _since: Option<skywave_proto::RunId>,
        ) -> BoxFuture<'_, Result<HelloResponse, TransportError>> {
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
            drafts: Arc::new(Notify::new()),
            retry_now: Arc::new(Notify::new()),
            blobs: Arc::new(BlobQueue::default()),
            address: "srv:7443".parse().unwrap(),
            identity: seam.pairing().unwrap(),
            run: ExpectedRun::default(),
        }
    }

    fn enqueue(seam: &Seam, text: &str) -> Command {
        enqueue_with(seam, text, Vec::new())
    }

    fn enqueue_with(seam: &Seam, text: &str, attachments: Vec<ContentHash>) -> Command {
        enqueue_with_files(seam, text, attachments, Vec::new())
    }

    fn enqueue_with_files(
        seam: &Seam,
        text: &str,
        attachments: Vec<ContentHash>,
        files: Vec<AttachedFile>,
    ) -> Command {
        let command = Command {
            id: CommandId::new(),
            kind: CommandKind::SendMessage {
                session_id: SessionId::new(),
                context: skywave_proto::ContextId::first_of(SessionId::new()),
                text: text.into(),
                attachments,
            },
        };
        crate::client::lock(&seam.core.shared.state)
            .store
            .enqueue(&command, &files)
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
        let progress = AtomicBool::new(false);

        // The sender drains the outbox, then waits for more forever.
        let errors = &mut HeadCount::default();
        let drained = tokio::time::timeout(
            Duration::from_secs(1),
            send(&ctx, &server, &progress, errors),
        )
        .await;
        assert!(drained.is_err(), "the sender only ends on an error");
        assert!(progress.load(Ordering::Relaxed), "answers are progress");
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
        let errors = &mut HeadCount::default();
        let failures = &mut HeadCount::default();
        assert!(
            !send_entry(&ctx, &server, &entry, errors, failures)
                .await
                .unwrap()
        );

        let answered = enqueue(&seam, "answered");
        let entry = seam.pending_outbox().unwrap().remove(0);
        seam.command_result(answered.id, &CommandResult::Accepted { seq: Seq(1) })
            .unwrap();
        assert!(
            !send_entry(&ctx, &server, &entry, errors, failures)
                .await
                .unwrap(),
            "a skipped row is no progress"
        );

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
        fn hello(
            &self,
            _since: Option<skywave_proto::RunId>,
        ) -> BoxFuture<'_, Result<HelloResponse, TransportError>> {
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
                worktree: None,
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
        let progress = AtomicBool::new(false);

        let errors = &mut HeadCount::default();
        let drained = tokio::time::timeout(
            Duration::from_secs(1),
            send(&ctx, &server, &progress, errors),
        )
        .await;
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

    /// A 5xx is the server's answer, but not about the command: the sender
    /// does not end, and sends the same command again after a backoff, up
    /// to [`MAX_SERVER_ERRORS`] times in a row. Then it turns to the log
    /// (`hello` for the head), which this server cannot give, so only then
    /// does the sender end; the command behind never went out.
    #[tokio::test(start_paused = true)]
    async fn a_server_error_resends_the_command_up_to_the_limit() {
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
        let progress = AtomicBool::new(false);
        let errors = &mut HeadCount::default();

        let ended = tokio::time::timeout(
            Duration::from_secs(120),
            send(&ctx, &server, &progress, errors),
        )
        .await;
        assert!(
            matches!(
                ended,
                Ok(Err(LinkError::Transport(TransportError::Io(ref unused)))) if unused == "unused"
            ),
            "{ended:?}"
        );
        assert!(!progress.load(Ordering::Relaxed), "a 5xx is no progress");
        assert_eq!(
            *server.seen.lock().unwrap(),
            ["/busy"; MAX_SERVER_ERRORS as usize]
        );
        assert_eq!(seam.pending_outbox().unwrap().len(), 2);
    }

    /// A server on a link whose uplink stalls for the command to `/slow`:
    /// its first `stalls` sends time out after `STALL`; every other command
    /// is accepted. Long-polls return empty after their `wait`.
    struct StallingServer {
        stalls: AtomicUsize,
        hellos: AtomicUsize,
        pulled: AtomicUsize,
        seen: Mutex<Vec<(String, CommandId)>>,
    }

    impl StallingServer {
        const STALL: Duration = Duration::from_secs(30);
    }

    impl Transport for StallingServer {
        fn hello(
            &self,
            _since: Option<skywave_proto::RunId>,
        ) -> BoxFuture<'_, Result<HelloResponse, TransportError>> {
            Box::pin(async move {
                self.hellos.fetch_add(1, Ordering::SeqCst);
                Ok(HelloResponse {
                    protocol_version: PROTOCOL_VERSION,
                    server_id: ServerId::new(),
                    device_id: DeviceId::new(),
                    head: Seq::ZERO,
                    log: one_run(),
                    devices: Vec::new(),
                    shell: false,
                    screen: false,
                    bypass: false,
                    login_expires: None,
                    agent_login_offered: false,
                    agent_logged_out: false,
                })
            })
        }

        fn send_command<'a>(
            &'a self,
            command: &'a Command,
        ) -> BoxFuture<'a, Result<CommandResult, TransportError>> {
            Box::pin(async move {
                let CommandKind::CreateSession { workdir, .. } = &command.kind else {
                    panic!("only creations are sent here");
                };
                self.seen
                    .lock()
                    .unwrap()
                    .push((workdir.clone(), command.id));
                let stalled = workdir == "/slow"
                    && self
                        .stalls
                        .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                        .is_ok();
                if stalled {
                    tokio::time::sleep(StallingServer::STALL).await;
                    return Err(TransportError::Timeout);
                }
                let seen = self.seen.lock().unwrap().len() as u64;
                Ok(CommandResult::Accepted { seq: Seq(seen) })
            })
        }

        fn events(&self, query: EventsQuery) -> BoxFuture<'_, Result<EventBatch, TransportError>> {
            Box::pin(async move {
                tokio::time::sleep(Duration::from_secs(u64::from(query.wait.unwrap_or(0)))).await;
                self.pulled.fetch_add(1, Ordering::SeqCst);
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

    /// A command whose request times out is sent again, with the same id,
    /// after a backoff, on the same connection: the long-poll beside it
    /// goes on, the link never shows down, and no `hello` is repeated.
    /// Once it gets through, the command behind it goes out.
    #[tokio::test(start_paused = true)]
    async fn a_command_timeout_leaves_the_pull_running() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        create(&seam, "/slow");
        create(&seam, "/later");
        let links = Arc::new(Mutex::new(Vec::new()));
        let sink = links.clone();
        seam.core
            .subscribe(move |view| sink.lock().unwrap().push(view.link));

        let server = Arc::new(StallingServer {
            stalls: AtomicUsize::new(3),
            hellos: AtomicUsize::new(0),
            pulled: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
        });
        let fake = server.clone();
        let (wall_clock, _) = paused_wall();
        let config = EngineConfig {
            wait_secs: 10,
            backoff: Backoff {
                base: Duration::from_secs(1),
                cap: Duration::from_secs(8),
                jitter: Jitter::fixed(1.0),
            },
            connector: Connector::new(move |_, _, _, _| Ok(fake.clone() as Arc<dyn Transport>)),
            wall_clock,
            ..EngineConfig::default()
        };
        let link = tokio::spawn(run_link(link_ctx(&seam, config, Arc::new(Notify::new()))));

        while !seam.pending_outbox().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        link.abort();

        let seen = server.seen.lock().unwrap().clone();
        let workdirs: Vec<_> = seen.iter().map(|(w, _)| w.as_str()).collect();
        assert_eq!(workdirs, ["/slow", "/slow", "/slow", "/slow", "/later"]);
        assert!(
            seen[..4].iter().all(|(_, id)| *id == seen[0].1),
            "a resend keeps the command's id"
        );
        assert_eq!(server.hellos.load(Ordering::SeqCst), 1, "one connection");
        // Three stalls and their backoffs (1 + 2 + 4 s): 97 s of 10 s polls.
        assert!(server.pulled.load(Ordering::SeqCst) >= 9);
        let links = links.lock().unwrap();
        assert!(
            !links.iter().any(|l| matches!(l, Link::Reconnecting { .. })),
            "{links:?}"
        );
    }

    /// Stages uploads the way the server does, and records every request as
    /// `head`, `put` or `post` with the first eight hex digits of its hash.
    struct StagingServer {
        staged: Mutex<HashSet<ContentHash>>,
        calls: Mutex<Vec<String>>,
        seq: Mutex<u64>,
        /// When set, every PUT answers this and stages nothing.
        put_answer: Option<UploadResult>,
        /// The first this many PUTs fail with a timeout.
        put_failures: Mutex<u32>,
        /// Whether a post must find each of its images staged.
        check_staged: bool,
        /// Runs on every PUT, after it is recorded and before it answers.
        on_put: Option<Box<dyn Fn() + Send + Sync>>,
        /// Every workdir write, in the order it came.
        writes: Mutex<Vec<WriteFileRequest>>,
        /// When set, every write answers this; otherwise it writes.
        write_answer: Option<WriteFileResult>,
        /// The first this many writes answer `NotStaged` and unstage the
        /// file's content, as a server that lost it does.
        write_not_staged: Mutex<u32>,
        /// Every upload the server was sent, in the order it came.
        uploads: Mutex<Vec<UploadFile>>,
        /// The `from` of every PUT, in the order it came.
        put_froms: Mutex<Vec<u64>>,
        /// Bytes held of an interrupted upload, per hash: what a HEAD reports
        /// and a PUT resumes at.
        partials: Mutex<HashMap<ContentHash, u64>>,
        /// Scripted answers, one per PUT, used before `put_answer`. A scripted
        /// answer stages nothing; a scripted `OffsetMismatch` also sets the
        /// held count to its `have`, as the server would.
        answers: Mutex<VecDeque<UploadResult>>,
    }

    impl StagingServer {
        fn new() -> Self {
            StagingServer {
                staged: Mutex::new(HashSet::new()),
                calls: Mutex::new(Vec::new()),
                seq: Mutex::new(0),
                put_answer: None,
                put_failures: Mutex::new(0),
                check_staged: true,
                on_put: None,
                writes: Mutex::new(Vec::new()),
                write_answer: None,
                write_not_staged: Mutex::new(0),
                uploads: Mutex::new(Vec::new()),
                put_froms: Mutex::new(Vec::new()),
                partials: Mutex::new(HashMap::new()),
                answers: Mutex::new(VecDeque::new()),
            }
        }

        fn record(&self, call: String) {
            let mut calls = self.calls.lock().unwrap();
            // A stager that repeats itself never yields, so no timeout could
            // end it: fail here instead.
            assert!(calls.len() < 1000, "the sender repeats itself");
            calls.push(call);
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    fn head_call(sha256: ContentHash) -> String {
        format!("head {}", &sha256.to_hex()[..8])
    }

    fn put_call(sha256: ContentHash) -> String {
        format!("put {}", &sha256.to_hex()[..8])
    }

    fn write_call(sha256: ContentHash) -> String {
        format!("write {}", &sha256.to_hex()[..8])
    }

    fn post_call(text: &str) -> String {
        format!("post {text}")
    }

    impl Transport for StagingServer {
        fn hello(
            &self,
            _since: Option<skywave_proto::RunId>,
        ) -> BoxFuture<'_, Result<HelloResponse, TransportError>> {
            Box::pin(async { Err(TransportError::Io("unused".into())) })
        }

        fn send_command<'a>(
            &'a self,
            command: &'a Command,
        ) -> BoxFuture<'a, Result<CommandResult, TransportError>> {
            Box::pin(async move {
                if let CommandKind::SendMessage {
                    text, attachments, ..
                } = &command.kind
                {
                    {
                        let mut staged = self.staged.lock().unwrap();
                        for sha256 in attachments {
                            if self.check_staged {
                                assert!(
                                    staged.contains(sha256),
                                    "posted before its images were staged"
                                );
                            }
                            // The server unlinks the staged files a message used.
                            staged.remove(sha256);
                        }
                    }
                    self.record(post_call(text));
                }
                let mut seq = self.seq.lock().unwrap();
                *seq += 1;
                Ok(CommandResult::Accepted { seq: Seq(*seq) })
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

        fn attachment_staged(
            &self,
            sha256: ContentHash,
        ) -> BoxFuture<'_, Result<StagedUpload, TransportError>> {
            Box::pin(async move {
                self.record(head_call(sha256));
                if self.staged.lock().unwrap().contains(&sha256) {
                    Ok(StagedUpload::Staged)
                } else {
                    let held = self
                        .partials
                        .lock()
                        .unwrap()
                        .get(&sha256)
                        .copied()
                        .unwrap_or(0);
                    Ok(StagedUpload::Missing { held })
                }
            })
        }

        fn upload_attachment(
            &self,
            sha256: ContentHash,
            file: UploadFile,
            from: u64,
        ) -> BoxFuture<'_, Result<UploadResult, TransportError>> {
            Box::pin(async move {
                self.record(put_call(sha256));
                self.uploads.lock().unwrap().push(file.clone());
                self.put_froms.lock().unwrap().push(from);
                if let Some(on_put) = &self.on_put {
                    on_put();
                }
                {
                    let mut failures = self.put_failures.lock().unwrap();
                    if *failures > 0 {
                        *failures -= 1;
                        return Err(TransportError::Timeout);
                    }
                }
                let mut bytes = std::fs::read(&file.path).unwrap();
                bytes.truncate(file.len as usize);
                assert!(
                    ContentHash::of(&bytes) == sha256,
                    "the upload does not hash to its name"
                );
                let scripted = self.answers.lock().unwrap().pop_front();
                if let Some(answer) = scripted {
                    if let UploadResult::OffsetMismatch { have } = answer {
                        self.partials.lock().unwrap().insert(sha256, have);
                    }
                    return Ok(answer);
                }
                let held = self
                    .partials
                    .lock()
                    .unwrap()
                    .get(&sha256)
                    .copied()
                    .unwrap_or(0);
                if from != 0 && from != held {
                    return Ok(UploadResult::OffsetMismatch { have: held });
                }
                match &self.put_answer {
                    Some(answer) => Ok(answer.clone()),
                    None => {
                        self.staged.lock().unwrap().insert(sha256);
                        self.partials.lock().unwrap().remove(&sha256);
                        Ok(UploadResult::Stored {
                            size: bytes.len() as u64,
                        })
                    }
                }
            })
        }

        fn write_workdir_file<'a>(
            &'a self,
            request: &'a WriteFileRequest,
        ) -> BoxFuture<'a, Result<WriteFileResult, TransportError>> {
            Box::pin(async move {
                self.record(write_call(request.content));
                self.writes.lock().unwrap().push(request.clone());
                let unstaged = {
                    let mut left = self.write_not_staged.lock().unwrap();
                    if *left > 0 {
                        *left -= 1;
                        true
                    } else {
                        false
                    }
                };
                if unstaged {
                    self.staged.lock().unwrap().remove(&request.content);
                    return Ok(WriteFileResult::NotStaged);
                }
                Ok(match &self.write_answer {
                    Some(answer) => answer.clone(),
                    None => WriteFileResult::Written {
                        path: request.path.clone(),
                        size: 0,
                        seq: Seq(1),
                    },
                })
            })
        }
    }

    /// Writes `bytes` to `dir/device.bin`, the way a picked file sits on disk.
    fn source_file(dir: &std::path::Path, bytes: &[u8]) -> std::path::PathBuf {
        let path = dir.join("device.bin");
        std::fs::write(&path, bytes).unwrap();
        path
    }

    /// Writes `bytes` where the outbox looks for the image they hash to.
    fn put_file(dir: &std::path::Path, bytes: &[u8]) -> ContentHash {
        let sha256 = ContentHash::of(bytes);
        std::fs::create_dir_all(attachment_dir(dir)).unwrap();
        std::fs::write(attachment_path(dir, sha256), bytes).unwrap();
        sha256
    }

    /// A file of a message named `name` whose device copy holds `bytes`.
    fn outbox_file(dir: &std::path::Path, name: &str, bytes: &[u8]) -> AttachedFile {
        AttachedFile {
            sha256: put_file(dir, bytes),
            name: name.into(),
            size: bytes.len() as u64,
            replace: None,
        }
    }

    /// Runs the sender until the outbox is drained. The sender only ends on
    /// an error, so the timeout is what ends it.
    async fn drain(seam: &Seam, server: &StagingServer) {
        let ctx = sender_ctx(seam);
        let progress = AtomicBool::new(false);
        let errors = &mut HeadCount::default();
        let drained = tokio::time::timeout(
            Duration::from_secs(60),
            send(&ctx, server, &progress, errors),
        )
        .await;
        assert!(drained.is_err(), "the sender only ends on an error");
    }

    fn outbox(seam: &Seam) -> Vec<OutboxEntry> {
        crate::client::lock(&seam.core.shared.state)
            .store
            .outbox()
            .unwrap()
    }

    fn statuses(seam: &Seam) -> Vec<OutboxStatus> {
        outbox(seam).into_iter().map(|entry| entry.status).collect()
    }

    /// Every image is staged before its message is posted, and one the
    /// server already holds is not uploaded again.
    #[tokio::test(start_paused = true)]
    async fn images_are_uploaded_before_the_message() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let a = put_file(dir.path(), b"image a");
        let b = ContentHash::of(b"image b");
        let server = StagingServer::new();
        server.staged.lock().unwrap().insert(b);
        enqueue_with(&seam, "m", vec![a, b]);

        drain(&seam, &server).await;
        assert_eq!(
            server.calls(),
            [head_call(a), put_call(a), head_call(b), post_call("m")]
        );
        assert_eq!(
            statuses(&seam),
            [OutboxStatus::Done(CommandResult::Accepted { seq: Seq(1) })]
        );
    }

    /// The server unlinks the images a message used, so a later message
    /// naming the same image uploads it again.
    #[tokio::test(start_paused = true)]
    async fn a_reused_image_is_uploaded_again() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let a = put_file(dir.path(), b"image a");
        let server = StagingServer::new();
        enqueue_with(&seam, "one", vec![a]);
        enqueue_with(&seam, "two", vec![a]);

        drain(&seam, &server).await;
        assert_eq!(
            server.calls(),
            [
                head_call(a),
                put_call(a),
                post_call("one"),
                head_call(a),
                put_call(a),
                post_call("two"),
            ]
        );
        assert_eq!(
            statuses(&seam),
            [
                OutboxStatus::Done(CommandResult::Accepted { seq: Seq(1) }),
                OutboxStatus::Done(CommandResult::Accepted { seq: Seq(2) }),
            ]
        );
    }

    /// A failed upload is tried again from the head check, not from the
    /// upload alone, and the message goes out once the image is staged.
    #[tokio::test(start_paused = true)]
    async fn a_failed_upload_is_retried_from_the_head() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let a = put_file(dir.path(), b"image a");
        let server = StagingServer::new();
        *server.put_failures.lock().unwrap() = 1;
        enqueue_with(&seam, "m", vec![a]);

        drain(&seam, &server).await;
        assert_eq!(
            server.calls(),
            [
                head_call(a),
                put_call(a),
                head_call(a),
                put_call(a),
                post_call("m"),
            ]
        );
        assert_eq!(
            statuses(&seam),
            [OutboxStatus::Done(CommandResult::Accepted { seq: Seq(1) })]
        );
    }

    /// An interrupted upload resumes at the bytes the HEAD says the server
    /// holds, not at 0.
    #[tokio::test(start_paused = true)]
    async fn an_image_upload_resumes_from_the_held_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let a = put_file(dir.path(), b"image a");
        let server = StagingServer::new();
        server.partials.lock().unwrap().insert(a, 3);
        enqueue_with(&seam, "m", vec![a]);

        drain(&seam, &server).await;
        assert_eq!(server.calls(), [head_call(a), put_call(a), post_call("m")]);
        assert_eq!(*server.put_froms.lock().unwrap(), [3]);
        assert_eq!(
            statuses(&seam),
            [OutboxStatus::Done(CommandResult::Accepted { seq: Seq(1) })]
        );
    }

    /// An `OffsetMismatch` says where the server is, and the upload goes again
    /// from there.
    #[tokio::test(start_paused = true)]
    async fn an_offset_mismatch_uploads_again_from_what_the_server_has() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let a = put_file(dir.path(), b"image a");
        let server = StagingServer::new();
        server.partials.lock().unwrap().insert(a, 3);
        server
            .answers
            .lock()
            .unwrap()
            .push_back(UploadResult::OffsetMismatch { have: 2 });
        enqueue_with(&seam, "m", vec![a]);

        drain(&seam, &server).await;
        assert_eq!(
            server.calls(),
            [head_call(a), put_call(a), put_call(a), post_call("m")]
        );
        assert_eq!(*server.put_froms.lock().unwrap(), [3, 2]);
        assert_eq!(
            statuses(&seam),
            [OutboxStatus::Done(CommandResult::Accepted { seq: Seq(1) })]
        );
    }

    /// A held count past the file's length is no resume point: the upload
    /// starts from 0.
    #[tokio::test(start_paused = true)]
    async fn a_held_count_past_the_file_uploads_from_zero() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let a = put_file(dir.path(), b"image a");
        let server = StagingServer::new();
        server.partials.lock().unwrap().insert(a, 100);
        enqueue_with(&seam, "m", vec![a]);

        drain(&seam, &server).await;
        assert_eq!(server.calls(), [head_call(a), put_call(a), post_call("m")]);
        assert_eq!(*server.put_froms.lock().unwrap(), [0]);
        assert_eq!(
            statuses(&seam),
            [OutboxStatus::Done(CommandResult::Accepted { seq: Seq(1) })]
        );
    }

    /// A resumed upload the server answers `HashMismatch` restarts from 0
    /// once, as the server has dropped its partial.
    #[tokio::test(start_paused = true)]
    async fn a_hash_mismatch_on_a_resumed_upload_restarts_from_zero() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let a = put_file(dir.path(), b"image a");
        let server = StagingServer::new();
        server.partials.lock().unwrap().insert(a, 3);
        server
            .answers
            .lock()
            .unwrap()
            .push_back(UploadResult::HashMismatch);
        enqueue_with(&seam, "m", vec![a]);

        drain(&seam, &server).await;
        assert_eq!(
            server.calls(),
            [head_call(a), put_call(a), put_call(a), post_call("m")]
        );
        assert_eq!(*server.put_froms.lock().unwrap(), [3, 0]);
        assert_eq!(
            statuses(&seam),
            [OutboxStatus::Done(CommandResult::Accepted { seq: Seq(1) })]
        );
    }

    /// An offset that keeps moving is three PUTs and then a retryable error,
    /// and the next attempt starts again from the HEAD.
    #[tokio::test(start_paused = true)]
    async fn an_offset_that_keeps_moving_backs_off_to_the_head() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let a = put_file(dir.path(), b"image a");
        let server = StagingServer::new();
        server.partials.lock().unwrap().insert(a, 1);
        server.answers.lock().unwrap().extend([
            UploadResult::OffsetMismatch { have: 1 },
            UploadResult::OffsetMismatch { have: 2 },
            UploadResult::OffsetMismatch { have: 1 },
        ]);
        enqueue_with(&seam, "m", vec![a]);

        drain(&seam, &server).await;
        assert_eq!(
            server.calls(),
            [
                head_call(a),
                put_call(a),
                put_call(a),
                put_call(a),
                head_call(a),
                put_call(a),
                post_call("m"),
            ]
        );
        assert_eq!(*server.put_froms.lock().unwrap(), [1, 1, 2, 1]);
        assert_eq!(
            statuses(&seam),
            [OutboxStatus::Done(CommandResult::Accepted { seq: Seq(1) })]
        );
    }

    /// On a low disk the uploads stop and the message is posted anyway, so
    /// the server's own refusal shows it and the outbox keeps moving.
    #[tokio::test(start_paused = true)]
    async fn a_low_disk_posts_the_message() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let a = put_file(dir.path(), b"image a");
        let b = put_file(dir.path(), b"image b");
        let mut server = StagingServer::new();
        server.put_answer = Some(UploadResult::DiskLow);
        server.check_staged = false;
        enqueue_with(&seam, "m", vec![a, b]);

        drain(&seam, &server).await;
        assert_eq!(server.calls(), [head_call(a), put_call(a), post_call("m")]);
        assert_eq!(
            statuses(&seam),
            [OutboxStatus::Done(CommandResult::Accepted { seq: Seq(1) })]
        );
    }

    /// An image the server refuses fails its message for good, unposted and
    /// discardable, and the message behind it goes out.
    #[tokio::test(start_paused = true)]
    async fn a_refused_image_fails_the_message_and_the_next_goes_out() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let a = put_file(dir.path(), b"not really a picture");
        let mut server = StagingServer::new();
        server.put_answer = Some(UploadResult::HashMismatch);
        enqueue_with(&seam, "one", vec![a]);
        enqueue(&seam, "two");

        drain(&seam, &server).await;
        assert_eq!(
            server.calls(),
            [head_call(a), put_call(a), post_call("two")]
        );
        let entries = outbox(&seam);
        assert_eq!(
            entries.iter().map(|e| e.status.clone()).collect::<Vec<_>>(),
            [
                OutboxStatus::AttachmentRefused(AttachmentRefusal::HashMismatch),
                OutboxStatus::Done(CommandResult::Accepted { seq: Seq(1) }),
            ]
        );
        assert!(entries[0].discardable());
    }

    /// A local image that was never written is refused without an upload.
    #[tokio::test(start_paused = true)]
    async fn a_missing_image_fails_without_uploading() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let a = ContentHash::of(b"never written");
        let server = StagingServer::new();
        enqueue_with(&seam, "m", vec![a]);

        drain(&seam, &server).await;
        assert_eq!(server.calls(), [head_call(a)]);
        assert_eq!(
            statuses(&seam),
            [OutboxStatus::AttachmentRefused(AttachmentRefusal::Missing)]
        );
    }

    /// A local image that exists but cannot be read is refused without an
    /// upload: staging opens the copy, so an unreadable one is refused as
    /// missing rather than retried as a transport error.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn an_unreadable_image_fails_without_uploading() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let a = put_file(dir.path(), b"unreadable image");
        std::fs::set_permissions(
            attachment_path(dir.path(), a),
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();
        if std::fs::File::open(attachment_path(dir.path(), a)).is_ok() {
            // Running as root reads any file, so there is nothing to refuse.
            return;
        }
        let server = StagingServer::new();
        enqueue_with(&seam, "m", vec![a]);

        drain(&seam, &server).await;
        assert_eq!(server.calls(), [head_call(a)]);
        assert_eq!(
            statuses(&seam),
            [OutboxStatus::AttachmentRefused(AttachmentRefusal::Missing)]
        );
    }

    /// A local image over the upload limit is refused without an upload.
    #[tokio::test(start_paused = true)]
    async fn an_oversized_image_fails_without_uploading() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let big = vec![0u8; usize::try_from(ATTACHMENT_MAX_BYTES + 1).unwrap()];
        let a = put_file(dir.path(), &big);
        let server = StagingServer::new();
        enqueue_with(&seam, "m", vec![a]);

        drain(&seam, &server).await;
        assert_eq!(server.calls(), [head_call(a)]);
        assert_eq!(
            statuses(&seam),
            [OutboxStatus::AttachmentRefused(AttachmentRefusal::TooLarge)]
        );
    }

    /// A message without images asks about none.
    #[tokio::test(start_paused = true)]
    async fn a_message_without_images_never_asks_about_any() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        enqueue(&seam, "plain");
        let server = StagingServer::new();

        drain(&seam, &server).await;
        assert_eq!(server.calls(), [post_call("plain")]);
        assert_eq!(
            statuses(&seam),
            [OutboxStatus::Done(CommandResult::Accepted { seq: Seq(1) })]
        );
    }

    /// The user takes a message back while its first image uploads: the
    /// second image is not uploaded and the message is not posted, so the
    /// server never gets it twice (once from the outbox, once from the
    /// edited draft). The message behind it goes out.
    #[tokio::test(start_paused = true)]
    async fn a_message_withdrawn_while_its_images_upload_is_not_posted() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let a = put_file(dir.path(), b"image a");
        let b = put_file(dir.path(), b"image b");
        let one = enqueue_with(&seam, "one", vec![a, b]).id;
        enqueue(&seam, "two");
        let mut server = StagingServer::new();
        let taker = seam.clone();
        // Only the first PUT takes it back: a second withdraw would fail.
        let once = std::sync::Once::new();
        server.on_put = Some(Box::new(move || {
            once.call_once(|| {
                crate::client::lock(&taker.core.shared.state)
                    .store
                    .withdraw(one, None, None, None)
                    .unwrap();
            });
        }));

        drain(&seam, &server).await;
        assert_eq!(
            server.calls(),
            [head_call(a), put_call(a), post_call("two")]
        );
        assert_eq!(
            statuses(&seam),
            [OutboxStatus::Done(CommandResult::Accepted { seq: Seq(1) })]
        );
    }

    /// A message's image is staged, then its file is uploaded and written into
    /// the workdir root, and only then is the message posted.
    #[tokio::test(start_paused = true)]
    async fn files_are_written_before_the_message() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let img = put_file(dir.path(), b"image a");
        let file = outbox_file(dir.path(), "notes.txt", b"the notes");
        let server = StagingServer::new();
        enqueue_with_files(&seam, "m", vec![img], vec![file.clone()]);

        drain(&seam, &server).await;
        assert_eq!(
            server.calls(),
            [
                head_call(img),
                put_call(img),
                head_call(file.sha256),
                put_call(file.sha256),
                write_call(file.sha256),
                post_call("m"),
            ]
        );
        let writes = server.writes.lock().unwrap().clone();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].path, "notes.txt");
        assert_eq!(writes[0].content, file.sha256);
        assert_eq!(writes[0].replace, None);
        assert_eq!(
            statuses(&seam),
            [OutboxStatus::Done(CommandResult::Accepted { seq: Seq(1) })]
        );
    }

    /// A message file the server holds part of resumes its upload at those
    /// bytes, then is written and the message posted.
    #[tokio::test(start_paused = true)]
    async fn a_file_write_resumes_from_the_held_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let img = put_file(dir.path(), b"image a");
        let file = outbox_file(dir.path(), "notes.txt", b"the notes");
        let server = StagingServer::new();
        server.partials.lock().unwrap().insert(file.sha256, 3);
        enqueue_with_files(&seam, "m", vec![img], vec![file.clone()]);

        drain(&seam, &server).await;
        assert_eq!(
            server.calls(),
            [
                head_call(img),
                put_call(img),
                head_call(file.sha256),
                put_call(file.sha256),
                write_call(file.sha256),
                post_call("m"),
            ]
        );
        assert_eq!(*server.put_froms.lock().unwrap(), [0, 3]);
        assert_eq!(server.writes.lock().unwrap().len(), 1);
        assert_eq!(
            statuses(&seam),
            [OutboxStatus::Done(CommandResult::Accepted { seq: Seq(1) })]
        );
    }

    /// The file's `replace` goes with its write, so the server may overwrite
    /// the file it names.
    #[tokio::test(start_paused = true)]
    async fn a_file_replace_is_sent_with_its_write() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let old = ContentHash::of(b"the old notes");
        let mut file = outbox_file(dir.path(), "notes.txt", b"the notes");
        file.replace = Some(old);
        let server = StagingServer::new();
        enqueue_with_files(&seam, "m", Vec::new(), vec![file]);

        drain(&seam, &server).await;
        let writes = server.writes.lock().unwrap().clone();
        assert_eq!(writes[0].replace, Some(old));
        assert_eq!(
            statuses(&seam),
            [OutboxStatus::Done(CommandResult::Accepted { seq: Seq(1) })]
        );
    }

    /// The workdir holds other content at the file's name: the message fails
    /// unposted, keeping the hash found there on the file, and the message
    /// behind it goes out.
    #[tokio::test(start_paused = true)]
    async fn a_file_found_in_the_workdir_fails_the_message_with_its_hash() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let file = outbox_file(dir.path(), "notes.txt", b"the notes");
        let other = ContentHash::of(b"other notes");
        let mut server = StagingServer::new();
        server.write_answer = Some(WriteFileResult::Exists {
            sha256: other,
            size: 1,
        });
        enqueue_with_files(&seam, "one", Vec::new(), vec![file.clone()]);
        enqueue(&seam, "two");

        drain(&seam, &server).await;
        assert_eq!(
            server.calls(),
            [
                head_call(file.sha256),
                put_call(file.sha256),
                write_call(file.sha256),
                post_call("two"),
            ]
        );
        let entries = outbox(&seam);
        assert_eq!(
            entries.iter().map(|e| e.status.clone()).collect::<Vec<_>>(),
            [
                OutboxStatus::AttachmentRefused(AttachmentRefusal::FileExists),
                OutboxStatus::Done(CommandResult::Accepted { seq: Seq(1) }),
            ]
        );
        assert_eq!(entries[0].files[0].existing, Some(other));
        assert!(entries[0].discardable());
    }

    /// The user resends a message refused for a file the workdir holds, now
    /// replacing that file: the same command goes out, and its write carries
    /// the hash found.
    #[tokio::test(start_paused = true)]
    async fn a_resent_message_replaces_the_found_file() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let file = outbox_file(dir.path(), "notes.txt", b"the notes");
        let other = ContentHash::of(b"other notes");
        let mut server = StagingServer::new();
        server.write_answer = Some(WriteFileResult::Exists {
            sha256: other,
            size: 1,
        });
        let command = enqueue_with_files(&seam, "one", Vec::new(), vec![file.clone()]);
        drain(&seam, &server).await;
        assert_eq!(
            statuses(&seam),
            [OutboxStatus::AttachmentRefused(
                AttachmentRefusal::FileExists
            )]
        );

        // This entry went into the store past the model, so the model is
        // reloaded from the store, as opening the core would do.
        crate::client::lock(&seam.core.shared.state).resync();
        // The user confirms: the write from now on replaces the file.
        server.write_answer = None;
        seam.core
            .dispatch(crate::Action::ResendReplacing(command.id))
            .unwrap();
        drain(&seam, &server).await;

        let replaces: Vec<Option<ContentHash>> = server
            .writes
            .lock()
            .unwrap()
            .iter()
            .map(|write| write.replace)
            .collect();
        assert_eq!(replaces, [None, Some(other)]);
        assert_eq!(server.calls().last(), Some(&post_call("one")));
        assert_eq!(
            statuses(&seam),
            [OutboxStatus::Done(CommandResult::Accepted { seq: Seq(1) })]
        );
    }

    /// The workdir already holds this file's content, from a write whose
    /// answer was lost: the write counts as landed and the message is posted.
    #[tokio::test(start_paused = true)]
    async fn a_file_already_written_by_us_counts_as_written() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let file = outbox_file(dir.path(), "notes.txt", b"the notes");
        let mut server = StagingServer::new();
        server.write_answer = Some(WriteFileResult::Exists {
            sha256: file.sha256,
            size: file.size,
        });
        enqueue_with_files(&seam, "m", Vec::new(), vec![file.clone()]);

        drain(&seam, &server).await;
        assert_eq!(
            server.calls(),
            [
                head_call(file.sha256),
                put_call(file.sha256),
                write_call(file.sha256),
                post_call("m"),
            ]
        );
        assert_eq!(
            statuses(&seam),
            [OutboxStatus::Done(CommandResult::Accepted { seq: Seq(1) })]
        );
    }

    /// A write the workdir refuses fails the message: `NotAllowed` as
    /// `FileNotWritten`, a low server disk as `ServerDiskLow`. The message
    /// behind it still goes out.
    #[tokio::test(start_paused = true)]
    async fn a_refused_file_write_fails_the_message() {
        for (answer, reason) in [
            (
                WriteFileResult::NotAllowed,
                AttachmentRefusal::FileNotWritten,
            ),
            (WriteFileResult::DiskLow, AttachmentRefusal::ServerDiskLow),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let seam = paired_seam(dir.path());
            let file = outbox_file(dir.path(), "notes.txt", b"the notes");
            let mut server = StagingServer::new();
            server.write_answer = Some(answer);
            enqueue_with_files(&seam, "one", Vec::new(), vec![file.clone()]);
            enqueue(&seam, "two");

            drain(&seam, &server).await;
            assert_eq!(
                server.calls(),
                [
                    head_call(file.sha256),
                    put_call(file.sha256),
                    write_call(file.sha256),
                    post_call("two"),
                ]
            );
            assert_eq!(
                statuses(&seam),
                [
                    OutboxStatus::AttachmentRefused(reason),
                    OutboxStatus::Done(CommandResult::Accepted { seq: Seq(1) }),
                ]
            );
        }
    }

    /// A `NotStaged` write uploads the file again and writes it once more.
    #[tokio::test(start_paused = true)]
    async fn a_not_staged_write_uploads_again_once() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let file = outbox_file(dir.path(), "notes.txt", b"the notes");
        let server = StagingServer::new();
        *server.write_not_staged.lock().unwrap() = 1;
        enqueue_with_files(&seam, "m", Vec::new(), vec![file.clone()]);

        drain(&seam, &server).await;
        assert_eq!(
            server.calls(),
            [
                head_call(file.sha256),
                put_call(file.sha256),
                write_call(file.sha256),
                put_call(file.sha256),
                write_call(file.sha256),
                post_call("m"),
            ]
        );
        assert_eq!(server.writes.lock().unwrap().len(), 2);
        assert_eq!(
            statuses(&seam),
            [OutboxStatus::Done(CommandResult::Accepted { seq: Seq(1) })]
        );
    }

    /// A second `NotStaged` after the upload again fails the file, and the
    /// message behind it goes out.
    #[tokio::test(start_paused = true)]
    async fn a_second_not_staged_write_fails_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let file = outbox_file(dir.path(), "notes.txt", b"the notes");
        let server = StagingServer::new();
        *server.write_not_staged.lock().unwrap() = 2;
        enqueue_with_files(&seam, "one", Vec::new(), vec![file.clone()]);
        enqueue(&seam, "two");

        drain(&seam, &server).await;
        assert_eq!(
            server.calls(),
            [
                head_call(file.sha256),
                put_call(file.sha256),
                write_call(file.sha256),
                put_call(file.sha256),
                write_call(file.sha256),
                post_call("two"),
            ]
        );
        assert_eq!(
            statuses(&seam),
            [
                OutboxStatus::AttachmentRefused(AttachmentRefusal::FileNotWritten),
                OutboxStatus::Done(CommandResult::Accepted { seq: Seq(1) }),
            ]
        );
    }

    /// A file whose device copy was never written is refused without any
    /// request to the server.
    #[tokio::test(start_paused = true)]
    async fn a_missing_file_copy_fails_without_uploading() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let file = AttachedFile {
            sha256: ContentHash::of(b"never written"),
            name: "gone.txt".into(),
            size: 13,
            replace: None,
        };
        let server = StagingServer::new();
        enqueue_with_files(&seam, "m", Vec::new(), vec![file]);

        drain(&seam, &server).await;
        assert!(server.calls().is_empty(), "{:?}", server.calls());
        assert_eq!(
            statuses(&seam),
            [OutboxStatus::AttachmentRefused(AttachmentRefusal::Missing)]
        );
    }

    /// A device copy that no longer hashes to the file's hash is refused
    /// without any request to the server.
    #[tokio::test(start_paused = true)]
    async fn a_file_copy_that_changed_fails_without_uploading() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let file = outbox_file(dir.path(), "notes.txt", b"the notes");
        std::fs::write(attachment_path(dir.path(), file.sha256), b"edited").unwrap();
        let server = StagingServer::new();
        enqueue_with_files(&seam, "m", Vec::new(), vec![file]);

        drain(&seam, &server).await;
        assert!(server.calls().is_empty(), "{:?}", server.calls());
        assert_eq!(
            statuses(&seam),
            [OutboxStatus::AttachmentRefused(AttachmentRefusal::Missing)]
        );
    }

    /// Attaches the image `bytes` make to a draft of `session`, as the model
    /// does when the user attaches it.
    fn attach_draft_image(seam: &Seam, session: SessionId, bytes: &[u8]) {
        let image = AttachedImage {
            sha256: ContentHash::of(bytes),
            media_type: ImageType::Png,
            size: bytes.len() as u64,
            width: 4,
            height: 3,
        };
        crate::client::lock(&seam.core.shared.state)
            .store
            .attach_to_draft(session, &image, Utc::now())
            .unwrap();
    }

    /// A batch of a session's creation and one user message carrying an
    /// image per `(blob, bytes)`, each blob the hash of its bytes.
    fn message_with_images(session_id: SessionId, images: &[(ProtoBlobId, &[u8])]) -> EventBatch {
        let attachments = images
            .iter()
            .map(|(id, bytes)| Attachment {
                blob: BlobRef {
                    id: *id,
                    size: bytes.len() as u64,
                    sha256: ContentHash::of(bytes),
                },
                media_type: ImageType::Png,
            })
            .collect();
        let event = |kind| skywave_proto::Event {
            session_id,
            at: Utc::now(),
            command_id: None,
            context: skywave_proto::ContextId::first_of(session_id),
            kind,
        };
        EventBatch {
            from: Seq(1),
            events: vec![
                event(EventKind::SessionCreated {
                    workdir: "/srv".into(),
                    context_name: "start".into(),
                    from: None,
                    worktree: None,
                }),
                event(EventKind::UserMessage {
                    device_id: DeviceId::new(),
                    text: "look at these".into(),
                    attachments,
                }),
            ],
        }
    }

    /// A message's image this device attached is available from its local
    /// file as soon as the batch lands, and no download of it is queued. The
    /// message's other image, which this device never attached, still
    /// downloads.
    #[test]
    fn an_own_image_is_available_without_a_download() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let session = SessionId::new();
        let own = b"an image this device attached";
        let other = b"an image another device attached";
        put_file(dir.path(), own);
        attach_draft_image(&seam, session, own);
        let (own_blob, other_blob) = (ProtoBlobId::new(), ProtoBlobId::new());

        let applied = seam
            .apply_events(&message_with_images(
                session,
                &[(own_blob, own), (other_blob, other)],
            ))
            .unwrap();

        let own_path = blob_dir(dir.path()).join(own_blob.0.to_string());
        let own_record = seam.blob(own_blob).unwrap().unwrap();
        assert_eq!(
            own_record.status,
            BlobStatus::Available {
                path: own_path.clone()
            }
        );
        assert_eq!(std::fs::read(&own_path).unwrap(), own);
        let other_record = seam.blob(other_blob).unwrap().unwrap();
        assert_eq!(other_record.status, BlobStatus::NotDownloaded);
        assert_eq!(applied.blobs, [other_blob]);
        assert!(
            !blob_dir(dir.path())
                .join(format!("{}.own", own_blob.0))
                .exists()
        );
        assert!(
            !blob_dir(dir.path())
                .join(format!("{}.own", other_blob.0))
                .exists()
        );
    }

    /// A blob the replica already knows gets no local file from a batch that
    /// references it again: the planted file under its `.own` name is left
    /// alone, and the blob stays available.
    #[test]
    fn a_known_blob_gets_no_own_image_file() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let session = SessionId::new();
        let own = b"an image this device attached";
        put_file(dir.path(), own);
        attach_draft_image(&seam, session, own);
        let own_blob = ProtoBlobId::new();
        seam.apply_events(&message_with_images(session, &[(own_blob, own)]))
            .unwrap();
        assert!(matches!(
            seam.blob(own_blob).unwrap().unwrap().status,
            BlobStatus::Available { .. }
        ));

        let planted = blob_dir(dir.path()).join(format!("{}.own", own_blob.0));
        std::fs::write(&planted, b"planted").unwrap();
        seam.apply_events(&message_with_images(session, &[(own_blob, own)]))
            .unwrap();

        assert_eq!(std::fs::read(&planted).unwrap(), b"planted");
        assert!(matches!(
            seam.blob(own_blob).unwrap().unwrap().status,
            BlobStatus::Available { .. }
        ));
    }

    /// An image this device attached whose local file is gone is no image
    /// to use: its blob stays not downloaded and is queued as any other.
    #[test]
    fn an_own_image_without_its_file_still_downloads() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let session = SessionId::new();
        let gone = b"an image whose file is gone";
        put_file(dir.path(), gone);
        attach_draft_image(&seam, session, gone);
        std::fs::remove_file(attachment_path(dir.path(), ContentHash::of(gone))).unwrap();
        let blob = ProtoBlobId::new();

        let applied = seam
            .apply_events(&message_with_images(session, &[(blob, gone)]))
            .unwrap();

        assert_eq!(
            seam.blob(blob).unwrap().unwrap().status,
            BlobStatus::NotDownloaded
        );
        assert!(!blob_dir(dir.path()).join(blob.0.to_string()).exists());
        assert_eq!(applied.blobs, [blob]);
    }

    /// A batch leaves the file of an image detached from a draft alone: its
    /// row and file stay until the store opens again, so an attach of the
    /// same image that lands after the batch still finds both.
    #[test]
    fn an_applied_batch_keeps_a_detached_images_file() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let session = SessionId::new();
        let image = b"an image attached, then detached";
        let sha256 = put_file(dir.path(), image);
        attach_draft_image(&seam, session, image);
        crate::client::lock(&seam.core.shared.state)
            .store
            .detach_from_draft(session, sha256)
            .unwrap();
        let path = attachment_path(dir.path(), sha256);

        seam.apply_events(&message_with_images(session, &[]))
            .unwrap();

        assert!(path.exists(), "a batch removed the image's file");
        let rows = crate::client::lock(&seam.core.shared.state)
            .store
            .attachments()
            .unwrap();
        assert!(
            rows.contains_key(&sha256),
            "a batch removed the image's row"
        );

        drop(seam);
        let store = Store::open(dir.path()).unwrap();
        assert!(!path.exists(), "opening the store kept the image's file");
        assert!(!store.attachments().unwrap().contains_key(&sha256));
    }

    /// Runs the stager for a minute of paused time. It only ends on an
    /// error, so the timeout is what ends it.
    async fn stage_for_a_minute(ctx: &LinkCtx, server: &StagingServer) {
        let staged = tokio::time::timeout(Duration::from_secs(60), stage_drafts(ctx, server)).await;
        assert!(staged.is_err(), "the stager only ends on an error");
    }

    /// A device file the server holds part of resumes its upload at those
    /// bytes, then is written at its path.
    #[tokio::test(start_paused = true)]
    async fn push_resumes_from_the_held_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let source = source_file(dir.path(), b"device bytes");
        let server = StagingServer::new();
        let session = SessionId::new();
        let sha = ContentHash::of(b"device bytes");
        server.partials.lock().unwrap().insert(sha, 3);

        let outcome = push_workdir_file(&server, &source, session, "docs/a.txt".into(), None)
            .await
            .unwrap();

        assert_eq!(
            server.calls(),
            [head_call(sha), put_call(sha), write_call(sha)]
        );
        assert_eq!(*server.put_froms.lock().unwrap(), [3]);
        assert!(
            matches!(
                outcome,
                PushOutcome::Written(WriteFileResult::Written { .. })
            ),
            "{outcome:?}"
        );
    }

    /// A device file is staged, then written at its path: a HEAD, a PUT, then
    /// the write, which names the staged hash.
    #[tokio::test(start_paused = true)]
    async fn push_uploads_then_writes() {
        let dir = tempfile::tempdir().unwrap();
        let source = source_file(dir.path(), b"device bytes");
        let server = StagingServer::new();
        let session = SessionId::new();
        let sha = ContentHash::of(b"device bytes");

        let outcome = push_workdir_file(&server, &source, session, "docs/a.txt".into(), None)
            .await
            .unwrap();

        assert_eq!(
            server.calls(),
            [head_call(sha), put_call(sha), write_call(sha)]
        );
        assert_eq!(
            server.uploads.lock().unwrap().clone(),
            [UploadFile {
                path: source.clone(),
                len: b"device bytes".len() as u64,
            }]
        );
        assert!(
            matches!(
                outcome,
                PushOutcome::Written(WriteFileResult::Written { .. })
            ),
            "{outcome:?}"
        );
        let writes = server.writes.lock().unwrap().clone();
        assert_eq!(
            writes,
            [WriteFileRequest {
                session,
                path: "docs/a.txt".into(),
                content: sha,
                replace: None,
            }]
        );
    }

    /// A file is hashed whole by a streaming read, across more than one chunk.
    #[tokio::test]
    async fn hash_file_hashes_the_whole_file_by_a_streaming_read() {
        let dir = tempfile::tempdir().unwrap();
        let bytes: Vec<u8> = (0..3 * 64 * 1024 + 5).map(|i| (i % 251) as u8).collect();
        let path = source_file(dir.path(), &bytes);

        assert_eq!(
            hash_file(&path).await.unwrap(),
            (ContentHash::of(&bytes), bytes.len() as u64)
        );
        assert!(hash_file(&dir.path().join("absent")).await.is_err());
    }

    /// A device file larger than one upload chunk is uploaded from its file,
    /// the upload naming exactly the bytes hashed, and the write names their
    /// hash.
    #[tokio::test(start_paused = true)]
    async fn push_hashes_and_uploads_a_large_file() {
        let dir = tempfile::tempdir().unwrap();
        let bytes: Vec<u8> = (0..196_613).map(|i| (i % 251) as u8).collect();
        let source = source_file(dir.path(), &bytes);
        let server = StagingServer::new();
        let session = SessionId::new();

        push_workdir_file(&server, &source, session, "big.bin".into(), None)
            .await
            .unwrap();

        assert_eq!(
            server.uploads.lock().unwrap().clone(),
            [UploadFile {
                path: source.clone(),
                len: 196_613,
            }]
        );
        let writes = server.writes.lock().unwrap().clone();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].content, ContentHash::of(&bytes));
    }

    /// A file the server already holds is only written: no upload, and the
    /// `replace` the user chose goes along with the write.
    #[tokio::test(start_paused = true)]
    async fn push_of_a_staged_file_only_writes() {
        let dir = tempfile::tempdir().unwrap();
        let source = source_file(dir.path(), b"device bytes");
        let server = StagingServer::new();
        let sha = ContentHash::of(b"device bytes");
        server.staged.lock().unwrap().insert(sha);
        let session = SessionId::new();
        let old = ContentHash::of(b"old");

        push_workdir_file(&server, &source, session, "a.txt".into(), Some(old))
            .await
            .unwrap();

        assert_eq!(server.calls(), [head_call(sha), write_call(sha)]);
        let writes = server.writes.lock().unwrap().clone();
        assert_eq!(
            writes,
            [WriteFileRequest {
                session,
                path: "a.txt".into(),
                content: sha,
                replace: Some(old),
            }]
        );
    }

    /// A file that is not there is refused on the device: nothing is sent.
    #[tokio::test(start_paused = true)]
    async fn push_of_a_missing_file_sends_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let server = StagingServer::new();
        let missing = dir.path().join("gone.bin");

        let outcome = push_workdir_file(&server, &missing, SessionId::new(), "a".into(), None)
            .await
            .unwrap();

        assert!(
            matches!(outcome, PushOutcome::Unreadable { .. }),
            "{outcome:?}"
        );
        assert!(server.calls().is_empty(), "{:?}", server.calls());
    }

    /// A file over [`PUSH_MAX_BYTES`] is refused without being read or sent.
    #[tokio::test(start_paused = true)]
    async fn push_of_an_oversized_file_sends_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("big.bin");
        let file = std::fs::File::create(&source).unwrap();
        file.set_len(PUSH_MAX_BYTES + 1).unwrap();
        let server = StagingServer::new();

        let outcome = push_workdir_file(&server, &source, SessionId::new(), "a".into(), None)
            .await
            .unwrap();

        assert_eq!(outcome, PushOutcome::UploadRefused(UploadResult::TooLarge));
        assert!(server.calls().is_empty(), "{:?}", server.calls());
    }

    /// A refused upload stops the push: nothing is written.
    #[tokio::test(start_paused = true)]
    async fn push_stops_at_a_refused_upload() {
        let dir = tempfile::tempdir().unwrap();
        let source = source_file(dir.path(), b"device bytes");
        let sha = ContentHash::of(b"device bytes");
        let server = StagingServer {
            put_answer: Some(UploadResult::DiskLow),
            ..StagingServer::new()
        };

        let outcome = push_workdir_file(&server, &source, SessionId::new(), "a".into(), None)
            .await
            .unwrap();

        assert_eq!(server.calls(), [head_call(sha), put_call(sha)]);
        assert_eq!(outcome, PushOutcome::UploadRefused(UploadResult::DiskLow));
        assert!(server.writes.lock().unwrap().is_empty());
    }

    /// The server's answer to the write is the push's answer, whatever it is.
    #[tokio::test(start_paused = true)]
    async fn push_passes_the_write_answer_through() {
        let dir = tempfile::tempdir().unwrap();
        let source = source_file(dir.path(), b"device bytes");
        let answer = WriteFileResult::Exists {
            sha256: ContentHash::of(b"old"),
            size: 3,
        };
        let server = StagingServer {
            write_answer: Some(answer.clone()),
            ..StagingServer::new()
        };

        let outcome = push_workdir_file(&server, &source, SessionId::new(), "a".into(), None)
            .await
            .unwrap();

        assert_eq!(outcome, PushOutcome::Written(answer));
    }

    /// A draft image is uploaded as soon as it is attached, and only once
    /// per link: a local one is PUT, one the server holds is only checked,
    /// and nothing repeats while the draft stays as it is.
    #[tokio::test(start_paused = true)]
    async fn draft_images_are_staged_once_per_link() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let a = put_file(dir.path(), b"image a");
        let b = ContentHash::of(b"image b");
        let server = StagingServer::new();
        server.staged.lock().unwrap().insert(b);
        let session = SessionId::new();
        attach_draft_image(&seam, session, b"image a");
        attach_draft_image(&seam, session, b"image b");

        stage_for_a_minute(&sender_ctx(&seam), &server).await;
        assert_eq!(server.calls(), [head_call(a), put_call(a), head_call(b)]);
    }

    /// A failed upload waits a backoff, then tries again from the head
    /// check; the stager does not end over it.
    #[tokio::test(start_paused = true)]
    async fn a_failed_draft_upload_is_tried_again_after_a_backoff() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let a = put_file(dir.path(), b"image a");
        let mut server = StagingServer::new();
        *server.put_failures.lock().unwrap() = 1;
        let puts = Arc::new(Mutex::new(Vec::new()));
        let at_put = puts.clone();
        server.on_put = Some(Box::new(move || {
            at_put.lock().unwrap().push(tokio::time::Instant::now());
        }));
        attach_draft_image(&seam, SessionId::new(), b"image a");
        // Jitter fixed at its top, so the first retry waits exactly `base`.
        let ctx = LinkCtx {
            config: Arc::new(EngineConfig {
                backoff: Backoff {
                    base: Duration::from_secs(1),
                    cap: Duration::from_secs(8),
                    jitter: Jitter::fixed(1.0),
                },
                ..EngineConfig::default()
            }),
            ..sender_ctx(&seam)
        };

        stage_for_a_minute(&ctx, &server).await;
        assert_eq!(
            server.calls(),
            [head_call(a), put_call(a), head_call(a), put_call(a)]
        );
        let puts = puts.lock().unwrap();
        assert!(
            puts[1] - puts[0] >= Duration::from_secs(1),
            "retried after {:?}, not after a backoff",
            puts[1] - puts[0]
        );
    }

    /// An image the server refuses is left for the outbox to report when its
    /// message is sent: the stager does not upload it again.
    #[tokio::test(start_paused = true)]
    async fn a_refused_draft_image_is_left_to_the_outbox() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let a = put_file(dir.path(), b"not really a picture");
        let mut server = StagingServer::new();
        server.put_answer = Some(UploadResult::HashMismatch);
        attach_draft_image(&seam, SessionId::new(), b"not really a picture");

        stage_for_a_minute(&sender_ctx(&seam), &server).await;
        assert_eq!(server.calls(), [head_call(a), put_call(a)]);
    }

    /// An image attached while the stager waits wakes it at once.
    #[tokio::test(start_paused = true)]
    async fn an_attached_image_wakes_the_stager() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        let a = put_file(dir.path(), b"image a");
        let server = StagingServer::new();
        let ctx = sender_ctx(&seam);
        let session = SessionId::new();
        let attach = async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            attach_draft_image(&seam, session, b"image a");
            ctx.drafts.notify_one();
        };

        let ((), staged) = tokio::join!(attach, async {
            tokio::time::timeout(Duration::from_secs(60), stage_drafts(&ctx, &server)).await
        });
        assert!(staged.is_err(), "the stager only ends on an error");
        assert_eq!(server.calls(), [head_call(a), put_call(a)]);
    }

    /// A command whose request never gets through can be taken back while
    /// the sender waits to resend it: the wait ends there, the command
    /// behind it goes out at once, and the link stays up.
    #[tokio::test(start_paused = true)]
    async fn taking_back_a_stuck_command_lets_the_next_go() {
        let dir = tempfile::tempdir().unwrap();
        let seam = paired_seam(dir.path());
        create(&seam, "/slow");
        create(&seam, "/later");
        let stuck = seam.pending_outbox().unwrap()[0].command.id;

        let server = Arc::new(StallingServer {
            stalls: AtomicUsize::new(usize::MAX),
            hellos: AtomicUsize::new(0),
            pulled: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
        });
        let fake = server.clone();
        let (wall_clock, _) = paused_wall();
        let config = EngineConfig {
            wait_secs: 10,
            backoff: Backoff {
                base: Duration::from_secs(60),
                cap: Duration::from_secs(60),
                jitter: Jitter::fixed(1.0),
            },
            connector: Connector::new(move |_, _, _, _| Ok(fake.clone() as Arc<dyn Transport>)),
            wall_clock,
            ..EngineConfig::default()
        };
        let ctx = link_ctx(&seam, config, Arc::new(Notify::new()));
        let outbox = ctx.outbox.clone();
        let link = tokio::spawn(run_link(ctx));

        // The first try stalls out at 30 s; the resend waits until 90 s.
        tokio::time::sleep(Duration::from_secs(40)).await;
        assert_eq!(server.seen.lock().unwrap().len(), 1);
        assert!(seam.pending_outbox().unwrap()[0].tries > 0);
        seam.core.dispatch(crate::Action::Discard(stuck)).unwrap();
        // What the engine does with the dispatch's `OutboxChanged`.
        outbox.notify_one();

        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(seam.pending_outbox().unwrap().is_empty());
        link.abort();
        let workdirs: Vec<_> = server
            .seen
            .lock()
            .unwrap()
            .iter()
            .map(|(w, _)| w.clone())
            .collect();
        assert_eq!(workdirs, ["/slow", "/later"]);
        assert_eq!(server.hellos.load(Ordering::SeqCst), 1, "one connection");
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
                        worktree: None,
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
        fn hello(
            &self,
            _since: Option<skywave_proto::RunId>,
        ) -> BoxFuture<'_, Result<HelloResponse, TransportError>> {
            Box::pin(async move {
                self.hellos.fetch_add(1, Ordering::SeqCst);
                Ok(HelloResponse {
                    protocol_version: PROTOCOL_VERSION,
                    server_id: ServerId::new(),
                    device_id: DeviceId::new(),
                    head: Seq(self.log.lock().unwrap().len() as u64),
                    log: one_run(),
                    // Named, so no event asks `hello` again.
                    devices: vec![skywave_proto::DeviceInfo {
                        id: self.device,
                        name: "phone".into(),
                    }],
                    shell: false,
                    screen: false,
                    bypass: false,
                    login_expires: None,
                    agent_login_offered: false,
                    agent_logged_out: false,
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
                                attachments: Vec::new(),
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
                connector: Connector::new(move |_, _, _, _| Ok(fake.clone() as Arc<dyn Transport>)),
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
                attachments: Vec::new(),
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
