//! The blob downloader (spec §7 Blobs, D6): fetches blob content into the
//! core's data directory, apart from event sync.
//!
//! **What gets downloaded.** [`BlobQueue`] holds the blobs waiting for a
//! download. A blob the user asks for (`Action::DownloadBlob`) goes to the
//! front. With [`BlobPolicy::Eager`], every blob a pulled batch registers
//! goes to the back, as does every blob not yet available when the policy
//! switches to `Eager`; switching back to `OnDemand` drops the waiting
//! blobs nobody asked for (a download in progress finishes). On startup a
//! blob left `Downloading` is queued again, under either policy.
//!
//! **How.** The downloader runs on its own task beside the link, with its
//! own [`Transport`] (built by the engine's connector on first use), so
//! blob traffic never shares a connection with the long-poll or the outbox
//! sender, and the link's `CatchingUp`/`Online` never wait on a blob:
//! metadata synced counts as synced (D6). Two workers take whatever comes
//! first in the queue; a third takes only blobs the user asked for, so a
//! request never waits on eager downloads that are slow, stalled or
//! waiting out a retry.
//! A retryable failure (D7) is retried after [`Backoff::delay`], the
//! attempt count resetting whenever a try received some bytes; a rejection
//! is retried the same way until the link, which sees it too, stops the
//! downloader. The queue outlives the downloader, so a suspend, a
//! reconnect or a restart picks up where it left off.
//!
//! **Files.** `<data_dir>/blobs/<id>.part` while downloading, renamed to
//! `<data_dir>/blobs/<id>` once complete. A try resumes from the `.part`
//! file's length (`Range: bytes=<len>-`); if the server sends the whole
//! blob instead, the file starts over. The content must have exactly the
//! size the event's `BlobRef` gave, and the server must agree on it; a
//! mismatch, an unknown blob or a local file error deletes the `.part`
//! file, logs an error and leaves the blob `NotDownloaded`, failed with
//! that reason, until it is asked for again. A blob whose session is deleted
//! from this device loses both files; a download of it stops, without a
//! retry, when its next report finds the blob unknown, and removes what it
//! wrote meanwhile.
//!
//! **Status.** `Downloading { got, path }` (`path` the `.part` file) is
//! reported when a response starts, then at most every
//! [`PROGRESS_EVERY`], and when a try ends early;
//! `Available { path }` carries the absolute path of the complete file.

use std::collections::{HashSet, VecDeque};
use std::convert::Infallible;
use std::fs::File;
use std::io::{Read, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Notify, OnceCell};

use crate::client::{CoreError, lock};
use crate::engine::EngineConfig;
use crate::identity::Identity;
use crate::seam::Seam;
use crate::store::{BlobPolicy, BlobRecord, BlobStatus};
use crate::transport::{Transport, TransportError};
use crate::view::BlobId;

/// How often a running download reports its progress, at most.
pub(crate) const PROGRESS_EVERY: Duration = Duration::from_millis(250);

/// Where a data directory keeps blob files.
pub fn blob_dir(data_dir: &Path) -> PathBuf {
    std::path::absolute(data_dir)
        .unwrap_or_else(|_| data_dir.to_owned())
        .join("blobs")
}

/// The head of a downloaded blob's file, as heads show and search it: its
/// first `max` bytes as text (invalid UTF-8 replaced), and the file's size.
/// One bounded reader, so a huge blob never gets read whole.
pub fn read_head(path: &Path, max: u64) -> std::io::Result<(String, u64)> {
    let file = File::open(path)?;
    let size = file.metadata()?.len();
    let mut head = Vec::new();
    file.take(max).read_to_end(&mut head)?;
    Ok((String::from_utf8_lossy(&head).into_owned(), size))
}

/// A blob waiting for a download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Queued {
    id: BlobId,
    /// Asked for by the user (or resumed): kept when the policy goes back
    /// to `OnDemand`.
    wanted: bool,
}

#[derive(Debug, Default)]
struct Queue {
    policy: BlobPolicy,
    waiting: VecDeque<Queued>,
    /// Blobs a worker holds.
    active: HashSet<BlobId>,
}

impl Queue {
    fn holds(&self, id: BlobId) -> bool {
        self.active.contains(&id) || self.waiting.iter().any(|q| q.id == id)
    }

    fn push_back(&mut self, id: BlobId) {
        if !self.holds(id) {
            self.waiting.push_back(Queued { id, wanted: false });
        }
    }
}

/// The blobs waiting for the downloader; see the module docs.
#[derive(Debug, Default)]
pub(crate) struct BlobQueue {
    queue: Mutex<Queue>,
    wake: Notify,
}

impl BlobQueue {
    /// The queue as the replica leaves it: blobs left `Downloading` first,
    /// then, with `Eager`, every blob not downloaded yet.
    pub(crate) fn restore(seam: &Seam) -> BlobQueue {
        let this = BlobQueue::default();
        let (settings, blobs) = match seam.settings().and_then(|s| Ok((s, seam.blobs()?))) {
            Ok(found) => found,
            Err(error) => {
                tracing::warn!(%error, "cannot read blobs; none queued");
                return this;
            }
        };
        {
            let mut queue = lock(&this.queue);
            for blob in &blobs {
                if matches!(blob.status, BlobStatus::Downloading { .. }) {
                    queue.waiting.push_back(Queued {
                        id: blob.id,
                        wanted: true,
                    });
                }
            }
        }
        this.set_policy(settings.blob_policy, &blobs);
        this
    }

    /// The user asked for this blob: it goes first.
    pub(crate) fn want(&self, id: BlobId) {
        let mut queue = lock(&self.queue);
        if queue.active.contains(&id) {
            return;
        }
        queue.waiting.retain(|q| q.id != id);
        queue.waiting.push_front(Queued { id, wanted: true });
        drop(queue);
        self.wake.notify_waiters();
    }

    /// The policy changed; `blobs` is every blob the replica knows.
    pub(crate) fn set_policy(&self, policy: BlobPolicy, blobs: &[BlobRecord]) {
        let mut queue = lock(&self.queue);
        queue.policy = policy;
        match policy {
            BlobPolicy::OnDemand => queue.waiting.retain(|q| q.wanted),
            BlobPolicy::Eager => {
                // Newest first: ids are time-ordered.
                for blob in blobs.iter().rev() {
                    if !matches!(blob.status, BlobStatus::Available { .. }) {
                        queue.push_back(blob.id);
                    }
                }
            }
        }
        drop(queue);
        self.wake.notify_waiters();
    }

    /// A pulled batch registered these blobs; with `Eager` they are queued.
    pub(crate) fn registered(&self, ids: &[BlobId]) {
        let mut queue = lock(&self.queue);
        if queue.policy != BlobPolicy::Eager || ids.is_empty() {
            return;
        }
        for id in ids {
            queue.push_back(*id);
        }
        drop(queue);
        self.wake.notify_waiters();
    }

    /// These blobs left the replica (a session deleted from this device):
    /// they wait no longer. One a worker holds is let go by the worker
    /// itself, when its next report finds the blob unknown.
    pub(crate) fn purged(&self, ids: &[BlobId]) {
        lock(&self.queue).waiting.retain(|q| !ids.contains(&q.id));
    }

    /// Waits for the next blob `slot` takes and holds it.
    async fn next(&self, slot: Slot) -> Claim<'_> {
        loop {
            let notified = self.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut queue = lock(&self.queue);
                let at = match slot {
                    Slot::Any => (!queue.waiting.is_empty()).then_some(0),
                    Slot::Wanted => queue.waiting.iter().position(|q| q.wanted),
                };
                if let Some(item) = at.and_then(|at| queue.waiting.remove(at)) {
                    queue.active.insert(item.id);
                    return Claim {
                        queue: self,
                        item,
                        done: false,
                    };
                }
            }
            notified.await;
        }
    }
}

/// Which blobs a worker takes.
#[derive(Debug, Clone, Copy)]
enum Slot {
    /// The front of the queue, asked for or not.
    Any,
    /// Only blobs the user asked for (or resumed).
    Wanted,
}

/// A blob a worker holds. Dropped unfinished (the downloader stopped), it
/// goes back to the front of the queue.
struct Claim<'a> {
    queue: &'a BlobQueue,
    item: Queued,
    done: bool,
}

impl Claim<'_> {
    fn finish(mut self) {
        self.done = true;
    }
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        let mut queue = lock(&self.queue.queue);
        queue.active.remove(&self.item.id);
        if !self.done {
            queue.waiting.push_front(self.item);
        }
        drop(queue);
        self.queue.wake.notify_waiters();
    }
}

/// What the downloader needs.
pub(crate) struct BlobCtx {
    pub(crate) seam: Seam,
    pub(crate) config: Arc<EngineConfig>,
    pub(crate) address: String,
    pub(crate) identity: Arc<dyn Identity>,
    pub(crate) queue: Arc<BlobQueue>,
}

/// Downloads queued blobs until stopped: two of any kind at a time, plus
/// one the user asked for.
pub(crate) async fn run_downloader(ctx: BlobCtx) -> Infallible {
    let transport = OnceCell::new();
    let (never, _, _) = tokio::join!(
        worker(&ctx, &transport, Slot::Any),
        worker(&ctx, &transport, Slot::Any),
        worker(&ctx, &transport, Slot::Wanted),
    );
    never
}

async fn worker(ctx: &BlobCtx, transport: &OnceCell<Arc<dyn Transport>>, slot: Slot) -> Infallible {
    loop {
        let claim = ctx.queue.next(slot).await;
        let id = claim.item.id;
        let mut attempt = 0u32;
        loop {
            let mut progressed = false;
            let error = match fetch(ctx, transport, id, &mut progressed).await {
                Ok(()) => break,
                Err(error) => error,
            };
            if error.is_purged() {
                // Whatever this try wrote may have outlived the deletion.
                tracing::debug!(?id, "blob purged during its download");
                remove_blob_files(&ctx.seam.data_dir(), id);
                break;
            }
            if !error.is_retryable() {
                give_up(ctx, id, &error).await;
                break;
            }
            attempt = if progressed {
                1
            } else {
                attempt.saturating_add(1)
            };
            let next_in = ctx.config.backoff.delay(attempt);
            tracing::debug!(?id, %error, attempt, ?next_in, "blob download interrupted");
            tokio::time::sleep(next_in).await;
        }
        claim.finish();
    }
}

/// Why one try at a blob failed.
#[derive(Debug, thiserror::Error)]
enum FetchError {
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error(transparent)]
    Core(#[from] CoreError),
    #[error("blob file: {0}")]
    File(#[from] std::io::Error),
    #[error("blob size mismatch: expected {expected} bytes, the server has {actual}")]
    Size { expected: u64, actual: u64 },
    #[error("asked for the blob from byte {asked}, the server sent it from {sent}")]
    Offset { asked: u64, sent: u64 },
}

impl FetchError {
    /// The blob left the replica mid-download (its session was deleted from
    /// this device, or the device paired with another server): nothing to
    /// retry, and its files must go.
    fn is_purged(&self) -> bool {
        matches!(
            self,
            FetchError::Core(
                CoreError::Store(crate::store::StoreError::UnknownBlob(_)) | CoreError::Superseded
            )
        )
    }

    fn is_retryable(&self) -> bool {
        match self {
            // A rejection is the link's to report; wait for it to stop us.
            FetchError::Transport(e) => e.is_retryable() || e.rejection().is_some(),
            FetchError::Core(_) => true,
            FetchError::File(_) | FetchError::Size { .. } | FetchError::Offset { .. } => false,
        }
    }
}

/// Deletes what was fetched of a blob and leaves it `NotDownloaded`, with
/// the reason shown until it is asked for again.
async fn give_up(ctx: &BlobCtx, id: BlobId, error: &FetchError) {
    tracing::error!(?id, %error, "blob download failed");
    let part = part_path(&blob_dir(&ctx.seam.data_dir()), id);
    let _ = tokio::fs::remove_file(&part).await;
    match ctx.seam.blob_failed(id, error.to_string()) {
        Ok(())
        | Err(CoreError::Store(crate::store::StoreError::UnknownBlob(_)) | CoreError::Superseded) =>
            {}
        Err(error) => tracing::warn!(?id, %error, "cannot reset the blob's status"),
    }
}

fn part_path(dir: &Path, id: BlobId) -> PathBuf {
    dir.join(format!("{}.part", id.0))
}

/// Removes a blob's files, complete and partial, from `data_dir`; a file
/// already gone is no error.
pub(crate) fn remove_blob_files(data_dir: &Path, id: BlobId) {
    let dir = blob_dir(data_dir);
    for path in [dir.join(id.0.to_string()), part_path(&dir, id)] {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(?id, %error, "cannot remove a blob file"),
        }
    }
}

/// Removes every blob file, complete and partial, from `data_dir`; a
/// directory already gone is no error.
pub(crate) fn remove_all_blob_files(data_dir: &Path) {
    match std::fs::remove_dir_all(blob_dir(data_dir)) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(%error, "cannot remove the blob files"),
    }
}

/// One try at a blob: resume its `.part` file and finish it.
/// `progressed` is set once some bytes arrived.
async fn fetch(
    ctx: &BlobCtx,
    transport: &OnceCell<Arc<dyn Transport>>,
    id: BlobId,
    progressed: &mut bool,
) -> Result<(), FetchError> {
    let Some(record) = ctx.seam.blob(id)? else {
        tracing::warn!(?id, "asked to download an unknown blob");
        return Ok(());
    };
    let dir = blob_dir(&ctx.seam.data_dir());
    let done = dir.join(id.0.to_string());
    if let BlobStatus::Available { path } = &record.status
        && tokio::fs::try_exists(path).await.unwrap_or(false)
    {
        return Ok(());
    }
    tokio::fs::create_dir_all(&dir).await?;
    let part = part_path(&dir, id);
    let size = record.size;
    let have = match tokio::fs::metadata(&part).await {
        Ok(meta) if meta.len() <= size => Some(meta.len()),
        // Longer than the blob: not ours to trust.
        Ok(_) => None,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if have == Some(size) {
        return complete(ctx, id, &part, &done, size).await;
    }
    let from = have.unwrap_or(0);

    let transport = transport
        .get_or_try_init(|| async {
            ctx.config
                .connector
                .connect(&ctx.address, ctx.identity.as_ref(), &ctx.config.transport)
        })
        .await?;
    let mut download = match transport.blob(id, from).await {
        Ok(download) => download,
        Err(TransportError::RangeNotSatisfiable { size: actual }) => {
            return Err(FetchError::Size {
                expected: size,
                actual: actual.unwrap_or(0),
            });
        }
        Err(error) => return Err(error.into()),
    };
    if download.total != size {
        return Err(FetchError::Size {
            expected: size,
            actual: download.total,
        });
    }
    let mut got = match download.offset {
        offset if offset == from => from,
        0 => 0,
        sent => return Err(FetchError::Offset { asked: from, sent }),
    };
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&part)
        .await?;
    file.set_len(got).await?;
    file.seek(SeekFrom::Start(got)).await?;
    let report = |got: u64| {
        ctx.seam.blob_status(
            id,
            BlobStatus::Downloading {
                got,
                path: part.clone(),
            },
        )
    };
    report(got)?;
    let mut reported = tokio::time::Instant::now();
    loop {
        let chunk = match download.body.next_chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(error) => {
                file.flush().await?;
                report(got)?;
                return Err(error.into());
            }
        };
        let len = chunk.len() as u64;
        if got + len > size {
            return Err(FetchError::Size {
                expected: size,
                actual: got + len,
            });
        }
        file.write_all(&chunk).await?;
        got += len;
        *progressed |= len > 0;
        if reported.elapsed() >= PROGRESS_EVERY {
            report(got)?;
            reported = tokio::time::Instant::now();
        }
    }
    if got != size {
        // The body ended early without an error: a broken response.
        file.flush().await?;
        report(got)?;
        return Err(TransportError::Io(format!("blob body ended at {got} of {size} bytes")).into());
    }
    file.sync_all().await?;
    drop(file);
    complete(ctx, id, &part, &done, size).await
}

/// The `.part` file holds the whole blob: check its size, move it in place
/// and mark the blob available.
async fn complete(
    ctx: &BlobCtx,
    id: BlobId,
    part: &Path,
    done: &Path,
    size: u64,
) -> Result<(), FetchError> {
    let actual = tokio::fs::metadata(part).await?.len();
    if actual != size {
        return Err(FetchError::Size {
            expected: size,
            actual,
        });
    }
    tokio::fs::rename(part, done).await?;
    ctx.seam.blob_status(
        id,
        BlobStatus::Available {
            path: done.to_owned(),
        },
    )?;
    tracing::debug!(?id, size, "blob downloaded");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use bytes::Bytes;
    use chrono::Utc;
    use skywave_proto::{
        BlobId as ProtoBlobId, BlobRef, Command, CommandResult, Event, EventBatch, EventKind,
        EventsQuery, HelloResponse, Seq, SessionId, ToolResult,
    };

    use super::*;
    use crate::backoff::{Backoff, Jitter};
    use crate::engine::Connector;
    use crate::identity::PairedCredentials;
    use crate::transport::{BlobBody, BlobDownload, BoxFuture};
    use crate::view::{BlobState, DeviceId, Entry, Screen, TimelineItem};
    use crate::{Action, Core};

    /// Serves one blob's content (claiming `total` as its size), honouring
    /// `from`, in chunks of `chunk` bytes; records each request's `from`.
    struct BlobServer {
        content: Vec<u8>,
        total: u64,
        chunk: usize,
        /// Chunks served before the connection "breaks", once.
        break_after: Mutex<Option<usize>>,
        froms: Mutex<Vec<u64>>,
        calls: AtomicUsize,
        /// Runs once, as the first response starts.
        on_blob: Mutex<Option<Box<dyn FnOnce() + Send>>>,
        /// Blobs never served.
        held: Mutex<HashMap<ProtoBlobId, Hold>>,
    }

    /// How a held blob is never served.
    #[derive(Debug, Clone, Copy)]
    enum Hold {
        /// The response never starts.
        Stall,
        /// Every try fails as if offline.
        Offline,
    }

    struct Chunks {
        chunks: VecDeque<Result<Bytes, TransportError>>,
    }

    impl BlobBody for Chunks {
        fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Bytes>, TransportError>> {
            Box::pin(async move { self.chunks.pop_front().transpose() })
        }
    }

    impl Transport for BlobServer {
        fn hello(&self) -> BoxFuture<'_, Result<HelloResponse, TransportError>> {
            Box::pin(async { Err(TransportError::Io("unused".into())) })
        }

        fn send_command<'a>(
            &'a self,
            _command: &'a Command,
        ) -> BoxFuture<'a, Result<CommandResult, TransportError>> {
            Box::pin(async { Err(TransportError::Io("unused".into())) })
        }

        fn events(&self, _query: EventsQuery) -> BoxFuture<'_, Result<EventBatch, TransportError>> {
            Box::pin(async { Err(TransportError::Io("unused".into())) })
        }

        fn blob(
            &self,
            id: ProtoBlobId,
            from: u64,
        ) -> BoxFuture<'_, Result<BlobDownload, TransportError>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let hold = self.held.lock().unwrap().get(&id).copied();
                match hold {
                    Some(Hold::Stall) => return std::future::pending().await,
                    Some(Hold::Offline) => return Err(TransportError::Io("offline".into())),
                    None => {}
                }
                self.froms.lock().unwrap().push(from);
                if let Some(hook) = self.on_blob.lock().unwrap().take() {
                    hook();
                }
                let rest = &self.content[from as usize..];
                let mut chunks: VecDeque<_> = rest
                    .chunks(self.chunk)
                    .map(|c| Ok(Bytes::copy_from_slice(c)))
                    .collect();
                if let Some(after) = self.break_after.lock().unwrap().take() {
                    chunks.truncate(after);
                    chunks.push_back(Err(TransportError::Io("cut".into())));
                }
                Ok(BlobDownload {
                    offset: from,
                    total: self.total,
                    body: Box::new(Chunks { chunks }),
                })
            })
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        core: Core,
        seam: Seam,
        session_id: SessionId,
        blob: BlobRef,
        server: Arc<BlobServer>,
        queue: Arc<BlobQueue>,
    }

    impl Fixture {
        /// A paired replica with one blob of `size` bytes registered; the
        /// server has `content` for it and claims `total` bytes.
        fn new(size: u64, content: Vec<u8>, total: u64) -> Fixture {
            let dir = tempfile::tempdir().unwrap();
            let core = Core::open_detached(dir.path()).unwrap();
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
            let blob = BlobRef {
                id: ProtoBlobId::new(),
                size,
            };
            let session_id = SessionId::new();
            let event = |kind| Event {
                session_id,
                at: Utc::now(),
                kind,
            };
            seam.apply_events(&EventBatch {
                from: Seq(1),
                events: vec![
                    event(EventKind::SessionCreated {
                        workdir: "/srv".into(),
                    }),
                    event(EventKind::ToolResult(ToolResult {
                        call: None,
                        is_error: false,
                        status: None,
                        preview: "big".into(),
                        lines: 1,
                        output: Some(blob.clone()),
                    })),
                ],
            })
            .unwrap();
            let server = Arc::new(BlobServer {
                content,
                total,
                chunk: 1000,
                break_after: Mutex::new(None),
                froms: Mutex::new(Vec::new()),
                calls: AtomicUsize::new(0),
                on_blob: Mutex::new(None),
                held: Mutex::new(HashMap::new()),
            });
            Fixture {
                _dir: dir,
                core,
                seam,
                session_id,
                blob,
                server,
                queue: Arc::new(BlobQueue::default()),
            }
        }

        fn ctx(&self) -> BlobCtx {
            let server = self.server.clone();
            BlobCtx {
                seam: self.seam.clone(),
                config: Arc::new(EngineConfig {
                    backoff: Backoff {
                        base: Duration::from_millis(1),
                        cap: Duration::from_millis(5),
                        jitter: Jitter::fixed(1.0),
                    },
                    connector: Connector::new(move |_, _, _| {
                        Ok(server.clone() as Arc<dyn Transport>)
                    }),
                    ..EngineConfig::default()
                }),
                address: "srv:7443".into(),
                identity: self.seam.identity().unwrap(),
                queue: self.queue.clone(),
            }
        }

        fn status(&self) -> BlobStatus {
            self.seam.blob(self.blob.id).unwrap().unwrap().status
        }

        /// Runs the downloader until the blob's status satisfies `done`.
        async fn run_until(&self, done: impl Fn(&BlobStatus) -> bool) {
            let downloader = tokio::spawn(run_downloader(self.ctx()));
            tokio::time::timeout(Duration::from_secs(10), async {
                while !done(&self.status()) || !lock(&self.queue.queue).active.is_empty() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("blob status stuck at {:?}", self.status()));
            downloader.abort();
        }

        /// The blob as the open session shows it.
        fn shown(&self) -> BlobState {
            self.core
                .dispatch(Action::OpenSession(self.session_id))
                .unwrap();
            match self.core.view().screen {
                Screen::Session(view) => match &view.timeline[..] {
                    [
                        TimelineItem::Logged {
                            entry:
                                Entry::ToolResult {
                                    output: Some(blob), ..
                                },
                            ..
                        },
                    ] => blob.state.clone(),
                    other => panic!("expected the tool result, got {other:?}"),
                },
                other => panic!("expected the session, got {other:?}"),
            }
        }

        fn part(&self) -> PathBuf {
            part_path(&blob_dir(&self.seam.data_dir()), self.blob.id)
        }

        /// Registers `count` more blobs, from `from`, held by the server.
        fn held_blobs(&self, from: u64, count: usize, hold: Hold) -> Vec<BlobId> {
            let ids: Vec<_> = (0..count).map(|_| ProtoBlobId::new()).collect();
            let events = ids.iter().map(|id| Event {
                session_id: self.session_id,
                at: Utc::now(),
                kind: EventKind::ToolResult(ToolResult {
                    call: None,
                    is_error: false,
                    status: None,
                    preview: "held".into(),
                    lines: 1,
                    output: Some(BlobRef { id: *id, size: 10 }),
                }),
            });
            self.seam
                .apply_events(&EventBatch {
                    from: Seq(from),
                    events: events.collect(),
                })
                .unwrap();
            let mut held = self.server.held.lock().unwrap();
            for id in &ids {
                held.insert(*id, hold);
            }
            ids
        }

        /// Both eager slots take a held blob; then the user asks for the
        /// fixture's blob, which must still arrive.
        async fn want_past_held_eager(&self, hold: Hold) {
            let held = self.held_blobs(3, 2, hold);
            self.queue.set_policy(BlobPolicy::Eager, &[]);
            self.queue.registered(&held);
            let downloader = tokio::spawn(run_downloader(self.ctx()));
            tokio::time::timeout(Duration::from_secs(10), async {
                while lock(&self.queue.queue).active.len() < 2
                    || self.server.calls.load(Ordering::SeqCst) < 2
                {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("both eager slots take a held blob");

            self.queue.want(self.blob.id);
            tokio::time::timeout(Duration::from_secs(10), async {
                while !matches!(self.status(), BlobStatus::Available { .. }) {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("blob status stuck at {:?}", self.status()));
            let active = lock(&self.queue.queue).active.clone();
            downloader.abort();
            assert_eq!(active, held.into_iter().collect(), "still held");
        }
    }

    fn content(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn read_head_reads_at_most_max_bytes_lossily() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob");
        std::fs::write(&path, b"ab\xffcdef").unwrap();
        assert_eq!(read_head(&path, 4).unwrap(), ("ab\u{fffd}c".to_owned(), 7));
        assert_eq!(
            read_head(&path, 100).unwrap(),
            ("ab\u{fffd}cdef".to_owned(), 7)
        );
        assert!(read_head(&dir.path().join("gone"), 4).is_err());
    }

    #[tokio::test]
    async fn a_broken_body_resumes_from_the_part_file() {
        let bytes = content(10_500);
        let f = Fixture::new(10_500, bytes.clone(), 10_500);
        *f.server.break_after.lock().unwrap() = Some(4);
        f.queue.want(f.blob.id);
        f.run_until(|s| matches!(s, BlobStatus::Available { .. }))
            .await;
        assert_eq!(*f.server.froms.lock().unwrap(), [0, 4000]);
        let BlobStatus::Available { path } = f.status() else {
            unreachable!()
        };
        assert!(path.is_absolute());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert!(!f.part().exists());
    }

    #[tokio::test]
    async fn a_server_size_other_than_the_events_is_refused() {
        // The server claims more than the event did.
        let f = Fixture::new(5000, content(6000), 6000);
        f.queue.want(f.blob.id);
        f.run_until(|_| f.server.calls.load(Ordering::SeqCst) >= 1)
            .await;
        assert_eq!(f.status(), BlobStatus::NotDownloaded);
        assert!(!f.part().exists());
        assert_eq!(f.server.calls.load(Ordering::SeqCst), 1, "not retried");
        // It says why it stopped, until asked for again.
        assert_eq!(
            f.shown(),
            BlobState::Failed {
                size: 5000,
                reason: "blob size mismatch: expected 5000 bytes, the server has 6000".into()
            }
        );
        f.core.dispatch(Action::DownloadBlob(f.blob.id)).unwrap();
        assert_eq!(f.shown(), BlobState::Requested { size: 5000 });
    }

    #[tokio::test]
    async fn a_body_longer_than_the_blob_is_refused() {
        // The server claims the right size but sends more bytes.
        let f = Fixture::new(5000, content(7000), 5000);
        f.queue.want(f.blob.id);
        f.run_until(|_| f.server.calls.load(Ordering::SeqCst) >= 1)
            .await;
        assert_eq!(f.status(), BlobStatus::NotDownloaded);
        assert!(!f.part().exists());
    }

    #[tokio::test]
    async fn an_oversized_part_file_starts_over() {
        let bytes = content(3000);
        let f = Fixture::new(3000, bytes.clone(), 3000);
        std::fs::create_dir_all(blob_dir(&f.seam.data_dir())).unwrap();
        std::fs::write(f.part(), vec![7u8; 4000]).unwrap();
        f.queue.want(f.blob.id);
        f.run_until(|s| matches!(s, BlobStatus::Available { .. }))
            .await;
        assert_eq!(*f.server.froms.lock().unwrap(), [0]);
        let BlobStatus::Available { path } = f.status() else {
            unreachable!()
        };
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }

    #[tokio::test]
    async fn purge_during_download_leaves_no_file_and_no_retry() {
        let f = Fixture::new(5000, content(5000), 5000);
        f.seam
            .apply_events(&EventBatch {
                from: Seq(3),
                events: vec![Event {
                    session_id: f.session_id,
                    at: Utc::now(),
                    kind: EventKind::SessionArchived {
                        archived: true,
                        by: DeviceId::new(),
                    },
                }],
            })
            .unwrap();
        // The session is deleted here while the server starts sending.
        let (core, session_id) = (f.core.clone(), f.session_id);
        *f.server.on_blob.lock().unwrap() = Some(Box::new(move || {
            core.dispatch(Action::DeleteSessionLocally { session_id })
                .unwrap();
        }));
        f.queue.want(f.blob.id);
        let downloader = tokio::spawn(run_downloader(f.ctx()));
        tokio::time::timeout(Duration::from_secs(10), async {
            while f.server.calls.load(Ordering::SeqCst) == 0
                || !lock(&f.queue.queue).active.is_empty()
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the worker lets the blob go");
        // Well past the backoff a retry would wait.
        tokio::time::sleep(Duration::from_millis(50)).await;
        downloader.abort();

        assert_eq!(f.server.calls.load(Ordering::SeqCst), 1, "not retried");
        assert!(lock(&f.queue.queue).waiting.is_empty());
        assert_eq!(f.seam.blob(f.blob.id).unwrap(), None);
        assert!(!f.part().exists(), "the part file it wrote is gone");
        let done = blob_dir(&f.seam.data_dir()).join(f.blob.id.0.to_string());
        assert!(!done.exists());
    }

    #[tokio::test]
    async fn a_wanted_blob_passes_stalled_eager_downloads() {
        let bytes = content(3000);
        let f = Fixture::new(3000, bytes.clone(), 3000);
        f.want_past_held_eager(Hold::Stall).await;
        let BlobStatus::Available { path } = f.status() else {
            unreachable!()
        };
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }

    #[tokio::test]
    async fn a_wanted_blob_passes_eager_downloads_retrying_offline() {
        let f = Fixture::new(3000, content(3000), 3000);
        f.want_past_held_eager(Hold::Offline).await;
    }

    #[test]
    fn the_queue_follows_the_policy() {
        let queue = BlobQueue::default();
        let record = |status| BlobRecord {
            id: ProtoBlobId::new(),
            size: 1,
            status,
        };
        let a = record(BlobStatus::NotDownloaded);
        let b = record(BlobStatus::Available { path: "/x".into() });
        let c = record(BlobStatus::NotDownloaded);
        let waiting = |q: &BlobQueue| -> Vec<(BlobId, bool)> {
            lock(&q.queue)
                .waiting
                .iter()
                .map(|q| (q.id, q.wanted))
                .collect()
        };

        // OnDemand: registered blobs wait for the user.
        queue.registered(&[a.id]);
        assert!(waiting(&queue).is_empty());
        queue.want(a.id);
        assert_eq!(waiting(&queue), [(a.id, true)]);

        // Eager: everything not available, newest first, no duplicates;
        // a user request jumps the queue.
        queue.set_policy(BlobPolicy::Eager, &[a.clone(), b.clone(), c.clone()]);
        assert_eq!(waiting(&queue), [(a.id, true), (c.id, false)]);
        let d = ProtoBlobId::new();
        queue.registered(&[d, c.id]);
        assert_eq!(waiting(&queue), [(a.id, true), (c.id, false), (d, false)]);
        queue.want(d);
        assert_eq!(waiting(&queue), [(d, true), (a.id, true), (c.id, false)]);

        // Back to OnDemand: only what the user asked for stays.
        queue.set_policy(BlobPolicy::OnDemand, &[]);
        assert_eq!(waiting(&queue), [(d, true), (a.id, true)]);

        // A purged blob waits no longer, asked for or not.
        queue.purged(&[d, c.id]);
        assert_eq!(waiting(&queue), [(a.id, true)]);
    }
}
