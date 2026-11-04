//! The blob downloader (architecture.md "Client core", D6): fetches blob content into the
//! core's data directory, apart from event sync.
//!
//! **What gets downloaded.** [`BlobQueue`] holds the blobs waiting for a
//! download. A blob the user asks for (`Action::DownloadBlob`) goes to the
//! front. With [`BlobPolicy::Eager`], every blob a pulled batch registers
//! goes to the back, as does every blob not yet available when the policy
//! switches to `Eager`; switching back to `OnDemand` drops the waiting
//! blobs nobody asked for (a download in progress finishes, but is not
//! queued again if the downloader stops first). On startup a blob left
//! `Downloading` in the replica is queued again, under either policy: the
//! restart resumes what the store says was under way.
//! Workdir blobs (named by a stored workdir answer, `BlobRecord::workdir`)
//! are never fetched eagerly, whatever the policy: only a request for one
//! queues it, so a Changes refresh never starts megabytes of diffs.
//!
//! **How.** The downloader runs on its own task beside the link, with its
//! own [`Transport`] (built by the engine's connector on first use), so
//! blob traffic never shares a connection with the long-poll or the outbox
//! sender, and the link's `CatchingUp`/`Online` never wait on a blob:
//! metadata synced counts as synced (D6). Two workers take whatever comes
//! first in the queue; a third takes only blobs the user asked for, so a
//! request never waits on eager downloads that are slow, stalled or
//! waiting out a retry. A blob asked for while one of the first two holds
//! it moves to the third when a try there fails (a stall ends in the
//! transport's timeout), if that worker is idle: the eager worker lets it
//! go instead of backing off, and the blob waits for the third worker
//! alone, so it is never fetched twice at once and the eager worker never
//! takes it back to back off again. The hand-over takes the idle worker
//! (one blob per idle spell), which takes that blob before any other.
//! A retryable failure (D7) is retried after [`Backoff::delay`], the
//! attempt count resetting whenever a try received some bytes; a rejection
//! is retried the same way until the link, which sees it too, stops the
//! downloader. The queue outlives the downloader, so a suspend, a
//! reconnect or a restart picks up where it left off.
//!
//! **Files.** `<data_dir>/blobs/<id>.part` while downloading, renamed to
//! `<data_dir>/blobs/<id>` once complete. Every `Downloading { got }` the
//! store records follows an fsync of those `got` bytes, so the recorded
//! count never runs past durable data. A try resumes from that count (or
//! the file's length, if shorter; `Range: bytes=<from>-`) and truncates
//! the file there: bytes written after the last report, or a length a
//! power loss kept without its data, are fetched again rather than trusted.
//! If the server sends the whole blob instead of the range, the file starts
//! over. The content must have exactly the size the event's `BlobRef`
//! gave, and the server must agree on it; a mismatch, an unknown blob or a
//! local file error deletes the `.part` file, logs an error and leaves the
//! blob `NotDownloaded`, failed with that reason, until it is asked for
//! again.
//!
//! **Checked by hash.** The complete `.part` file must hash to the
//! `BlobRef`'s `sha256` before it is renamed; nothing else checks the
//! bytes a try resumed after (`BlobId` is no content hash). On a mismatch
//! the file is deleted at once. The first mismatch a worker sees while it
//! holds the blob may be the device's own (bytes kept from an earlier try
//! or another run), so the blob is fetched again from zero straight away.
//! A second one in the same hold is the server's: every byte hashed was
//! fetched from it after the first was discarded. The blob then fails as
//! above (`Failed` with the mismatch as its reason), never looping on a
//! server that keeps serving the wrong bytes; asking for it again starts
//! over with a fresh allowance. A blob that leaves the replica
//! (its session deleted from this device, or no stored workdir answer
//! naming it any more) loses both files; a download of it stops, without a
//! retry, when its next report finds the blob unknown, and removes what it
//! wrote meanwhile.
//!
//! **Purged, then registered again.** Workdir blob ids recur: the server
//! names content by its hash, so a refresh can drop a blob and the next
//! one name it again while a worker still writes the old download. A row
//! with that id existing again is no proof the download is current, so
//! each try reads the blob's record together with a lease (the blob's
//! purge count, kept beside the replica under the same lock), and every
//! report carries it: once the blob was purged, the report fails as for
//! an unknown blob, whatever row has the id now, and the worker lets go
//! as above. A request for the blob made while a worker holds it is kept
//! and queued once the worker lets go, so asking for the registered-again
//! blob is not lost to the stale download. A waiting request needs no
//! lease: the core hands requests over in the order of the changes that
//! made them (`Shared::update` sends under the state lock), so the purge's
//! `BlobsPurged` always reaches the queue before a later change's request
//! for the same id, which then stays queued, and after any earlier one,
//! which it drops.
//!
//! **Status.** `Downloading { got, path }` (`path` the `.part` file) is
//! reported when a response starts, then at most every
//! [`PROGRESS_EVERY`] (also while the body stalls, if bytes arrived since
//! the last report), and when a try ends early;
//! `Available { path }` carries the absolute path of the complete file.

use std::collections::{HashSet, VecDeque};
use std::convert::Infallible;
use std::fs::File;
use std::io::{Read, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use skywave_proto::ContentHash;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Notify, OnceCell};

use crate::client::{BlobLease, CoreError, lock};
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
    /// Let go by an eager worker to the [`Slot::Wanted`] worker: only that
    /// one takes it.
    reserved: bool,
}

impl Queued {
    fn eager(id: BlobId) -> Queued {
        Queued {
            id,
            wanted: false,
            reserved: false,
        }
    }

    fn wanted(id: BlobId) -> Queued {
        Queued {
            id,
            wanted: true,
            reserved: false,
        }
    }
}

#[derive(Debug, Default)]
struct Queue {
    policy: BlobPolicy,
    waiting: VecDeque<Queued>,
    /// Blobs a worker holds.
    active: HashSet<BlobId>,
    /// Blobs asked for while a worker held them: queued again, as asked
    /// for, once it lets go.
    again: HashSet<BlobId>,
    /// The [`Slot::Wanted`] worker waits for a blob.
    wanted_idle: bool,
}

impl Queue {
    fn holds(&self, id: BlobId) -> bool {
        self.active.contains(&id) || self.waiting.iter().any(|q| q.id == id)
    }

    fn push_back(&mut self, id: BlobId) {
        if !self.holds(id) {
            self.waiting.push_back(Queued::eager(id));
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
    /// The queue as the replica leaves it: blobs left `Downloading` first
    /// (workdir ones too: the user asked for them), then, with `Eager`,
    /// every event blob not downloaded yet.
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
                    queue.waiting.push_back(Queued::wanted(blob.id));
                }
            }
        }
        this.set_policy(settings.blob_policy, &blobs);
        this
    }

    /// The user asked for this blob: it goes first. One a worker holds is
    /// marked asked for, and an eager worker backing off from it lets it go.
    pub(crate) fn want(&self, id: BlobId) {
        let mut queue = lock(&self.queue);
        if queue.active.contains(&id) {
            queue.again.insert(id);
        } else {
            let reserved = queue.waiting.iter().any(|q| q.id == id && q.reserved);
            queue.waiting.retain(|q| q.id != id);
            queue.waiting.push_front(Queued {
                reserved,
                ..Queued::wanted(id)
            });
        }
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
                // Newest first: ids are time-ordered. Workdir blobs only
                // download when asked for.
                for blob in blobs.iter().rev() {
                    if !blob.workdir && !matches!(blob.status, BlobStatus::Available { .. }) {
                        queue.push_back(blob.id);
                    }
                }
            }
        }
        drop(queue);
        self.wake.notify_waiters();
    }

    /// A pulled batch registered these blobs; with `Eager` they are queued.
    /// Only for event blobs: the blobs a stored workdir answer registers
    /// never come here, as they download only when asked for.
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

    /// These blobs left the replica (a session deleted from this device, or
    /// a refreshed workdir answer no longer naming them): they wait no
    /// longer. One a worker holds is let go by the worker itself, when its
    /// next report finds the blob unknown.
    pub(crate) fn purged(&self, ids: &[BlobId]) {
        let mut queue = lock(&self.queue);
        queue.waiting.retain(|q| !ids.contains(&q.id));
        queue.again.retain(|id| !ids.contains(id));
    }

    /// Waits for the next blob `slot` takes and holds it.
    async fn next(&self, slot: Slot) -> Claim<'_> {
        // However this ends (a blob taken, or the downloader stopped), the
        // Wanted worker is no longer idle.
        let _idle = matches!(slot, Slot::Wanted).then_some(WantedIdle(self));
        loop {
            let notified = self.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let became_idle = {
                let mut queue = lock(&self.queue);
                let at = match slot {
                    Slot::Any => queue.waiting.iter().position(|q| !q.reserved),
                    // The blob handed over first: it was asked for before
                    // anything queued since, and nothing else may take it.
                    Slot::Wanted => (queue.waiting.iter().position(|q| q.reserved))
                        .or_else(|| queue.waiting.iter().position(|q| q.wanted)),
                };
                if let Some(mut item) = at.and_then(|at| queue.waiting.remove(at)) {
                    item.reserved = false;
                    queue.active.insert(item.id);
                    return Claim {
                        queue: self,
                        item,
                        released: false,
                    };
                }
                matches!(slot, Slot::Wanted) && !std::mem::replace(&mut queue.wanted_idle, true)
            };
            if became_idle {
                // An eager worker backing off from a blob asked for can
                // let it go now.
                self.wake.notify_waiters();
            }
            notified.await;
        }
    }

    /// Waits out an eager worker's backoff `delay` before it tries `claim`
    /// again, and gives the claim back; or, once the blob is asked for and
    /// the Wanted worker idle, hands the blob over to that worker and
    /// returns `None`. The hand-over takes the idle Wanted worker in the
    /// same critical section, so one idle period takes one blob, and that
    /// worker takes it before anything else.
    async fn back_off<'a>(&'a self, mut claim: Claim<'a>, delay: Duration) -> Option<Claim<'a>> {
        let sleep = tokio::time::sleep(delay);
        tokio::pin!(sleep);
        loop {
            let notified = self.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut queue = lock(&self.queue);
                let wanted = claim.item.wanted || queue.again.contains(&claim.item.id);
                if wanted && queue.wanted_idle {
                    queue.wanted_idle = false;
                    claim.let_go(&mut queue, LetGo::HandedOver);
                    drop(queue);
                    self.wake.notify_waiters();
                    return None;
                }
            }
            tokio::select! {
                () = &mut sleep => return Some(claim),
                () = notified => {}
            }
        }
    }
}

/// The Wanted worker waiting in [`BlobQueue::next`]; no longer idle once
/// dropped.
struct WantedIdle<'a>(&'a BlobQueue);

impl Drop for WantedIdle<'_> {
    fn drop(&mut self) {
        lock(&self.0.queue).wanted_idle = false;
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

/// A blob a worker holds. Asked for again meanwhile, or dropped unfinished
/// (the downloader stopped) while asked for or under `Eager`, it goes back
/// to the front of the queue; handed over, it goes there for the Wanted
/// worker alone.
struct Claim<'a> {
    queue: &'a BlobQueue,
    item: Queued,
    /// Let go already: dropping it does nothing.
    released: bool,
}

/// How a worker lets go of a blob.
#[derive(Debug, Clone, Copy)]
enum LetGo {
    Finished,
    /// The downloader stopped.
    Unfinished,
    /// To the Wanted worker; see [`BlobQueue::back_off`].
    HandedOver,
}

impl Claim<'_> {
    fn finish(mut self) {
        let owner = self.queue;
        let mut queue = lock(&owner.queue);
        self.let_go(&mut queue, LetGo::Finished);
        drop(queue);
        owner.wake.notify_waiters();
    }

    /// Lets go of the blob under the queue's lock, which `queue` is.
    fn let_go(&mut self, queue: &mut Queue, how: LetGo) {
        self.released = true;
        let id = self.item.id;
        queue.active.remove(&id);
        let again = queue.again.remove(&id);
        match how {
            LetGo::HandedOver => queue.waiting.push_front(Queued {
                reserved: true,
                ..Queued::wanted(id)
            }),
            _ if again => queue.waiting.push_front(Queued::wanted(id)),
            // Not asked for, it waits only while the policy is Eager: one
            // switched to OnDemand while a worker held it goes, as the
            // switch dropped the waiting ones.
            LetGo::Unfinished if self.item.wanted || queue.policy == BlobPolicy::Eager => {
                queue.waiting.push_front(self.item);
            }
            LetGo::Unfinished | LetGo::Finished => {}
        }
    }
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        let owner = self.queue;
        let mut queue = lock(&owner.queue);
        self.let_go(&mut queue, LetGo::Unfinished);
        drop(queue);
        owner.wake.notify_waiters();
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
        let mut claim = ctx.queue.next(slot).await;
        let id = claim.item.id;
        let mut attempt = 0u32;
        // A hash mismatch already seen in this hold (module docs, "Checked
        // by hash").
        let mut mismatched = false;
        loop {
            let mut progressed = false;
            let mut lease = None;
            let error = match fetch(ctx, transport, id, &mut lease, &mut progressed).await {
                Ok(()) => break claim.finish(),
                Err(error) => error,
            };
            if error.is_purged() {
                // Whatever this try wrote may have outlived the deletion.
                tracing::debug!(?id, "blob purged during its download");
                remove_blob_files(&ctx.seam.data_dir(), id);
                break claim.finish();
            }
            if matches!(error, FetchError::Hash { .. }) && !mismatched {
                // `complete` deleted the file: the next try starts at zero.
                tracing::warn!(?id, %error, "blob content mismatch: fetching it again");
                mismatched = true;
                continue;
            }
            if !error.is_retryable() {
                give_up(ctx, id, lease, &error).await;
                break claim.finish();
            }
            attempt = if progressed {
                1
            } else {
                attempt.saturating_add(1)
            };
            let next_in = ctx.config.backoff.delay(attempt);
            tracing::debug!(?id, %error, attempt, ?next_in, "blob download interrupted");
            match slot {
                Slot::Any => match ctx.queue.back_off(claim, next_in).await {
                    Some(kept) => claim = kept,
                    None => {
                        tracing::debug!(?id, "blob asked for: handed to the wanted slot");
                        break;
                    }
                },
                Slot::Wanted => tokio::time::sleep(next_in).await,
            }
        }
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
    #[error("blob content hash mismatch: expected {expected}, got {actual}")]
    Hash {
        expected: ContentHash,
        actual: ContentHash,
    },
}

impl FetchError {
    /// The blob left the replica mid-download (its session was deleted from
    /// this device, no workdir answer names it any more, or the device
    /// paired with another server), even if its id is registered again
    /// since: nothing to retry, and its files must go.
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
            FetchError::File(_)
            | FetchError::Size { .. }
            | FetchError::Offset { .. }
            | FetchError::Hash { .. } => false,
        }
    }
}

/// Deletes what was fetched of a blob and leaves it `NotDownloaded`, with
/// the reason shown until it is asked for again.
async fn give_up(ctx: &BlobCtx, id: BlobId, lease: Option<BlobLease>, error: &FetchError) {
    tracing::error!(?id, %error, "blob download failed");
    let part = part_path(&blob_dir(&ctx.seam.data_dir()), id);
    let _ = tokio::fs::remove_file(&part).await;
    let failed = match lease {
        Some(lease) => ctx.seam.leased_blob_failed(id, lease, error.to_string()),
        // Failed before the record was read: whatever registration the
        // blob has now never started.
        None => ctx.seam.blob_failed(id, error.to_string()),
    };
    match failed {
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

/// One try at a blob: resume its `.part` file and finish it. `lease` is
/// set once the record is read; every report holds it. `progressed` is set
/// once some bytes arrived.
async fn fetch(
    ctx: &BlobCtx,
    transport: &OnceCell<Arc<dyn Transport>>,
    id: BlobId,
    lease_out: &mut Option<BlobLease>,
    progressed: &mut bool,
) -> Result<(), FetchError> {
    let Some((record, lease)) = ctx.seam.leased_blob(id)? else {
        tracing::warn!(?id, "asked to download an unknown blob");
        return Ok(());
    };
    *lease_out = Some(lease);
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
    // Only the recorded progress is durable: every report follows an fsync
    // of the bytes it counts. The file may be longer (written after the
    // last report) and, after a power loss, hold a kept length over lost
    // data; the excess is truncated below.
    let recorded = match &record.status {
        BlobStatus::Downloading { got, .. } => *got,
        BlobStatus::NotDownloaded | BlobStatus::Available { .. } => 0,
    };
    let from = match tokio::fs::metadata(&part).await {
        Ok(meta) => recorded.min(meta.len()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => return Err(error.into()),
    };
    if from == size {
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .open(&part)
            .await?;
        file.set_len(size).await?;
        file.sync_all().await?;
        drop(file);
        return complete(ctx, id, lease, &part, &done, &record).await;
    }

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
    // Records `got` bytes as downloaded, once they are durable: a restart
    // resumes from the recorded count, never past what survived.
    let report = async |file: &mut tokio::fs::File, got: u64| -> Result<(), FetchError> {
        file.flush().await?;
        file.sync_data().await?;
        ctx.seam.leased_blob_status(
            id,
            lease,
            BlobStatus::Downloading {
                got,
                path: part.clone(),
            },
        )?;
        Ok(())
    };
    report(&mut file, got).await?;
    let mut reported = tokio::time::Instant::now();
    let mut recorded = got;
    loop {
        // A stalled body still records what it got, so a kill while it
        // waits keeps that progress.
        let next = {
            let mut next = download.body.next_chunk();
            loop {
                tokio::select! {
                    chunk = &mut next => break chunk,
                    () = tokio::time::sleep_until(reported + PROGRESS_EVERY), if got > recorded => {
                        report(&mut file, got).await?;
                        (reported, recorded) = (tokio::time::Instant::now(), got);
                    }
                }
            }
        };
        let chunk = match next {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(error) => {
                report(&mut file, got).await?;
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
            report(&mut file, got).await?;
            (reported, recorded) = (tokio::time::Instant::now(), got);
        }
    }
    if got != size {
        // The body ended early without an error: a broken response.
        report(&mut file, got).await?;
        return Err(TransportError::Io(format!("blob body ended at {got} of {size} bytes")).into());
    }
    file.sync_all().await?;
    drop(file);
    complete(ctx, id, lease, &part, &done, &record).await
}

/// The `.part` file holds the whole blob: check its size and hash, move it
/// in place and mark the blob available. A file whose hash is wrong is
/// deleted, so the next try starts from zero.
async fn complete(
    ctx: &BlobCtx,
    id: BlobId,
    lease: BlobLease,
    part: &Path,
    done: &Path,
    record: &BlobRecord,
) -> Result<(), FetchError> {
    let size = record.size;
    let actual = tokio::fs::metadata(part).await?.len();
    if actual != size {
        return Err(FetchError::Size {
            expected: size,
            actual,
        });
    }
    let hashed = part.to_owned();
    let actual = tokio::task::spawn_blocking(move || ContentHash::read(File::open(hashed)?))
        .await
        .map_err(std::io::Error::other)??;
    if actual != record.sha256 {
        tokio::fs::remove_file(part).await?;
        return Err(FetchError::Hash {
            expected: record.sha256,
            actual,
        });
    }
    tokio::fs::rename(part, done).await?;
    ctx.seam.leased_blob_status(
        id,
        lease,
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
        /// Runs once, before the first response's second chunk.
        mid_body: Mutex<Option<Box<dyn FnOnce() + Send>>>,
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
        served: usize,
        mid_body: Option<Box<dyn FnOnce() + Send>>,
    }

    impl BlobBody for Chunks {
        fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Bytes>, TransportError>> {
            Box::pin(async move {
                if self.served == 1
                    && let Some(hook) = self.mid_body.take()
                {
                    hook();
                }
                self.served += 1;
                self.chunks.pop_front().transpose()
            })
        }
    }

    impl Transport for BlobServer {
        fn expect_run(&self, _run: skywave_proto::RunId) {}

        fn hello(
            &self,
            _since: Option<skywave_proto::RunId>,
        ) -> BoxFuture<'_, Result<HelloResponse, TransportError>> {
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
                    body: Box::new(Chunks {
                        chunks,
                        served: 0,
                        mid_body: self.mid_body.lock().unwrap().take(),
                    }),
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
        /// A paired replica with one blob of `size` bytes registered, whose
        /// hash is that of the first `size` bytes of `content`; the server
        /// has `content` for it and claims `total` bytes.
        fn new(size: u64, content: Vec<u8>, total: u64) -> Fixture {
            let hashed = &content[..content.len().min(size as usize)];
            let sha256 = ContentHash::of(hashed);
            Fixture::hashed(size, sha256, content, total)
        }

        /// As [`Fixture::new`], with the event's `BlobRef` promising `sha256`.
        fn hashed(size: u64, sha256: ContentHash, content: Vec<u8>, total: u64) -> Fixture {
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
                sha256,
            };
            let session_id = SessionId::new();
            let event = |kind: EventKind| Event {
                session_id,
                at: Utc::now(),
                command_id: kind.logged_by_command().then(skywave_proto::CommandId::new),
                context: skywave_proto::ContextId::first_of(session_id),
                kind,
            };
            seam.apply_events(&EventBatch {
                from: Seq(1),
                events: vec![
                    event(EventKind::SessionCreated {
                        workdir: "/srv".into(),
                        context_name: "start".into(),
                        from: None,
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
                mid_body: Mutex::new(None),
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
            self.ctx_backing_off(Duration::from_millis(1), Duration::from_millis(5))
        }

        fn ctx_backing_off(&self, base: Duration, cap: Duration) -> BlobCtx {
            let server = self.server.clone();
            BlobCtx {
                seam: self.seam.clone(),
                config: Arc::new(EngineConfig {
                    backoff: Backoff {
                        base,
                        cap,
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
                command_id: None,
                context: skywave_proto::ContextId::first_of(self.session_id),
                kind: EventKind::ToolResult(ToolResult {
                    call: None,
                    is_error: false,
                    status: None,
                    preview: "held".into(),
                    lines: 1,
                    output: Some(BlobRef {
                        id: *id,
                        size: 10,
                        sha256: ContentHash::of(&[0; 10]),
                    }),
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

    /// Bytes inside the recorded range are only checked by the hash: a
    /// part file whose kept bytes are wrong is caught at the end, deleted
    /// and fetched again from zero, and the right content then renamed.
    #[tokio::test]
    async fn a_resumed_part_file_with_wrong_bytes_is_fetched_again_from_zero() {
        let bytes = content(5000);
        for kept in [2000u64, 5000] {
            let f = Fixture::new(5000, bytes.clone(), 5000);
            std::fs::create_dir_all(blob_dir(&f.seam.data_dir())).unwrap();
            std::fs::write(f.part(), vec![7u8; kept as usize]).unwrap();
            f.seam
                .blob_status(
                    f.blob.id,
                    BlobStatus::Downloading {
                        got: kept,
                        path: f.part(),
                    },
                )
                .unwrap();
            f.queue.want(f.blob.id);
            f.run_until(|s| matches!(s, BlobStatus::Available { .. }))
                .await;
            // A whole kept file needs no request before its check.
            let froms: &[u64] = if kept == 5000 { &[0] } else { &[kept, 0] };
            assert_eq!(*f.server.froms.lock().unwrap(), froms, "kept {kept}");
            let BlobStatus::Available { path } = f.status() else {
                unreachable!()
            };
            assert_eq!(std::fs::read(path).unwrap(), bytes, "kept {kept}");
            assert!(!f.part().exists());
        }
    }

    /// A server that keeps serving bytes other than the hash promises gets
    /// two tries from zero, then the blob fails, with no file left.
    #[tokio::test]
    async fn content_that_keeps_hashing_wrong_fails_after_a_second_try() {
        let promised = ContentHash::of(b"other content");
        let f = Fixture::hashed(5000, promised, content(5000), 5000);
        f.queue.want(f.blob.id);
        let downloader = tokio::spawn(run_downloader(f.ctx()));
        tokio::time::timeout(Duration::from_secs(10), async {
            while f.server.calls.load(Ordering::SeqCst) < 2
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

        assert_eq!(
            *f.server.froms.lock().unwrap(),
            [0, 0],
            "two tries, no more"
        );
        assert_eq!(f.status(), BlobStatus::NotDownloaded);
        assert!(!f.part().exists());
        let done = blob_dir(&f.seam.data_dir()).join(f.blob.id.0.to_string());
        assert!(!done.exists(), "never renamed");
        let actual = ContentHash::of(&content(5000));
        assert_eq!(
            f.shown(),
            BlobState::Failed {
                size: 5000,
                reason: format!("blob content hash mismatch: expected {promised}, got {actual}"),
            }
        );
        // Asking again starts over with a fresh allowance.
        f.core.dispatch(Action::DownloadBlob(f.blob.id)).unwrap();
        assert_eq!(f.shown(), BlobState::Requested { size: 5000 });
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
                    command_id: Some(skywave_proto::CommandId::new()),
                    context: skywave_proto::ContextId::first_of(f.session_id),
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

    /// Workdir blob ids recur: a refresh purges the blob a worker is
    /// writing and the next one names it again. The stale download must not
    /// write for the new registration, and the request the pane makes for
    /// the new one, while the worker still holds the blob, is kept and
    /// downloaded afresh once it lets go.
    #[tokio::test]
    async fn a_blob_purged_and_registered_again_while_held_downloads_afresh() {
        use std::sync::mpsc::Receiver;

        use skywave_proto::{ChangesResult, FileBody, FileResult};

        use crate::Request;
        use crate::view::{WorkdirBody, WorkdirPane};
        use crate::workdir::{WorkdirAnswer, WorkdirAsk};

        let bytes = content(5000);
        let f = Fixture::new(5000, bytes.clone(), 5000);
        let requests = f.seam.take_requests().unwrap();
        let blob = BlobRef {
            id: ProtoBlobId::new(),
            size: 5000,
            sha256: ContentHash::of(&bytes),
        };
        // Plays the engine for the requests waiting: answers every workdir
        // read (the file's with `content`), returns the downloads asked for.
        let serve = {
            let seam = f.seam.clone();
            move |requests: &Receiver<Request>, content: FileBody| {
                let mut downloads = Vec::new();
                while let Ok(request) = requests.try_recv() {
                    match request {
                        Request::Workdir { req, ask } => {
                            let answer = match ask {
                                WorkdirAsk::Changes { .. } => {
                                    WorkdirAnswer::Changes(ChangesResult::NotGit)
                                }
                                _ => WorkdirAnswer::File(FileResult::File {
                                    at: Utc::now(),
                                    path: "a.txt".into(),
                                    size: 5000,
                                    content: content.clone(),
                                }),
                            };
                            seam.workdir_answered(req, ask, Ok(answer)).unwrap();
                        }
                        Request::DownloadBlob(id) => downloads.push(id),
                        _ => {}
                    }
                }
                downloads
            }
        };
        f.core
            .dispatch(Action::OpenWorkdir {
                session_id: f.session_id,
            })
            .unwrap();
        f.core
            .dispatch(Action::ShowWorkdir(WorkdirPane::File {
                path: "a.txt".into(),
            }))
            .unwrap();
        assert_eq!(serve(&requests, FileBody::Blob(blob.clone())), [blob.id]);
        f.queue.want(blob.id);

        let (core, queue, again) = (f.core.clone(), f.queue.clone(), blob.clone());
        let seam = f.seam.clone();
        *f.server.mid_body.lock().unwrap() = Some(Box::new(move || {
            // What the running download holds.
            let (_, held) = seam.leased_blob(again.id).unwrap().unwrap();
            // Refreshed twice mid-download: gone, then named again.
            core.dispatch(Action::RefreshWorkdir).unwrap();
            assert!(serve(&requests, FileBody::Binary).is_empty());
            core.dispatch(Action::RefreshWorkdir).unwrap();
            let downloads = serve(&requests, FileBody::Blob(again.clone()));
            // The row is back, but not for the download running: what it
            // would report is refused and the new row stays untouched.
            let done = BlobStatus::Available {
                path: "/gone".into(),
            };
            assert!(matches!(
                seam.leased_blob_status(again.id, held, done),
                Err(CoreError::Store(crate::store::StoreError::UnknownBlob(_)))
            ));
            assert_eq!(
                seam.blob(again.id).unwrap().map(|b| b.status),
                Some(BlobStatus::NotDownloaded)
            );
            // The pane on screen asks for it again, as the engine passes on.
            assert_eq!(downloads, [again.id]);
            queue.want(again.id);
        }));
        let downloader = tokio::spawn(run_downloader(f.ctx()));
        let status = || f.seam.blob(blob.id).unwrap().map(|b| b.status);
        tokio::time::timeout(Duration::from_secs(10), async {
            while !matches!(status(), Some(BlobStatus::Available { .. }))
                || !lock(&f.queue.queue).active.is_empty()
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("blob status stuck at {:?}", status()));
        downloader.abort();

        assert_eq!(*f.server.froms.lock().unwrap(), [0, 0], "downloaded afresh");
        let Some(BlobStatus::Available { path }) = status() else {
            unreachable!()
        };
        assert_eq!(std::fs::read(path).unwrap(), bytes);
        assert!(!part_path(&blob_dir(&f.seam.data_dir()), blob.id).exists());
        let Screen::Workdir(view) = f.core.view().screen else {
            panic!("expected the workdir screen");
        };
        let WorkdirBody::File {
            blob: Some(shown), ..
        } = view.body
        else {
            panic!("expected the file, got {:?}", view.body);
        };
        assert!(matches!(shown.state, BlobState::Available { .. }));
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

    /// An eager worker waiting out an hour's backoff on a blob the user
    /// then asks for lets it go at once, and the Wanted worker fetches it.
    #[tokio::test]
    async fn an_eager_worker_backing_off_hands_a_blob_asked_for_to_the_wanted_slot() {
        let bytes = content(3000);
        let f = Fixture::new(3000, bytes.clone(), 3000);
        f.server
            .held
            .lock()
            .unwrap()
            .insert(f.blob.id, Hold::Offline);
        f.queue.set_policy(BlobPolicy::Eager, &[]);
        f.queue.registered(&[f.blob.id]);
        let hour = Duration::from_secs(3600);
        let downloader = tokio::spawn(run_downloader(f.ctx_backing_off(hour, hour)));
        tokio::time::timeout(Duration::from_secs(10), async {
            while f.server.calls.load(Ordering::SeqCst) < 1 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("an eager worker tries the blob");
        assert!(lock(&f.queue.queue).active.contains(&f.blob.id));

        f.server.held.lock().unwrap().clear();
        f.queue.want(f.blob.id);
        tokio::time::timeout(Duration::from_secs(10), async {
            while !matches!(f.status(), BlobStatus::Available { .. }) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("blob status stuck at {:?}", f.status()));
        downloader.abort();
        assert_eq!(f.server.calls.load(Ordering::SeqCst), 2, "one more try");
        let BlobStatus::Available { path } = f.status() else {
            unreachable!()
        };
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }

    /// Polls `future` once: `Some` if it is ready.
    async fn ready_now<F: Future>(future: F) -> Option<F::Output> {
        tokio::time::timeout(Duration::ZERO, future).await.ok()
    }

    #[tokio::test]
    async fn asking_for_a_blob_an_eager_worker_holds_survives_on_demand() {
        let queue = BlobQueue::default();
        let id = ProtoBlobId::new();
        queue.set_policy(BlobPolicy::Eager, &[]);
        queue.registered(&[id]);
        let other = ProtoBlobId::new();
        queue.set_policy(BlobPolicy::Eager, &[]);
        queue.registered(&[id, other]);
        let claim = ready_now(queue.next(Slot::Any)).await.expect("queued");
        let unasked = ready_now(queue.next(Slot::Any)).await.expect("queued");
        assert_eq!((claim.item.id, unasked.item.id), (id, other));
        queue.want(id);
        // The downloader stops, then the policy goes back to OnDemand:
        // the blob asked for waits, the other one goes.
        drop(claim);
        drop(unasked);
        assert_eq!(waiting(&queue), [(other, false), (id, true)]);
        queue.set_policy(BlobPolicy::OnDemand, &[]);
        assert_eq!(waiting(&queue), [(id, true)]);

        // The other order: switched while held, then stopped.
        let claim = ready_now(queue.next(Slot::Any)).await.expect("queued");
        queue.want(id);
        queue.set_policy(BlobPolicy::OnDemand, &[]);
        drop(claim);
        assert_eq!(waiting(&queue), [(id, true)]);
    }

    /// An eager blob a worker holds while the policy goes to OnDemand,
    /// left unfinished when the downloader stops, is not queued again.
    #[tokio::test]
    async fn an_unfinished_eager_blob_held_across_a_switch_to_on_demand_goes() {
        let queue = BlobQueue::default();
        let id = ProtoBlobId::new();
        queue.set_policy(BlobPolicy::Eager, &[]);
        queue.registered(&[id]);
        let claim = ready_now(queue.next(Slot::Any)).await.expect("queued");
        queue.set_policy(BlobPolicy::OnDemand, &[]);
        drop(claim);
        assert!(waiting(&queue).is_empty());
        assert!(!lock(&queue.queue).holds(id), "let go");
    }

    /// The same, asked for while held: it is queued again, as asked for.
    #[tokio::test]
    async fn an_unfinished_eager_blob_asked_for_across_a_switch_to_on_demand_stays() {
        let queue = BlobQueue::default();
        let id = ProtoBlobId::new();
        queue.set_policy(BlobPolicy::Eager, &[]);
        queue.registered(&[id]);
        let claim = ready_now(queue.next(Slot::Any)).await.expect("queued");
        queue.set_policy(BlobPolicy::OnDemand, &[]);
        queue.want(id);
        drop(claim);
        assert_eq!(waiting(&queue), [(id, true)]);
    }

    /// Under Eager, an unfinished eager blob is queued again, at the front.
    #[tokio::test]
    async fn an_unfinished_eager_blob_under_eager_is_queued_again() {
        let queue = BlobQueue::default();
        let (id, other) = (ProtoBlobId::new(), ProtoBlobId::new());
        queue.set_policy(BlobPolicy::Eager, &[]);
        queue.registered(&[id, other]);
        let claim = ready_now(queue.next(Slot::Any)).await.expect("queued");
        assert_eq!(claim.item.id, id);
        drop(claim);
        assert_eq!(waiting(&queue), [(id, false), (other, false)]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_wanted_blob_whose_eager_try_fails_goes_to_the_wanted_slot_next() {
        let queue = BlobQueue::default();
        let id = ProtoBlobId::new();
        let hour = Duration::from_secs(3600);
        queue.set_policy(BlobPolicy::Eager, &[]);
        queue.registered(&[id]);
        let claim = ready_now(queue.next(Slot::Any)).await.expect("queued");
        let wanted = queue.next(Slot::Wanted);
        tokio::pin!(wanted);
        assert!(ready_now(&mut wanted).await.is_none(), "nothing asked for");

        // Not asked for: the eager worker waits out its backoff.
        let start = tokio::time::Instant::now();
        let claim = queue.back_off(claim, hour).await.expect("kept");
        assert_eq!(start.elapsed(), hour);

        // Asked for: it lets the blob go at once, to the Wanted worker
        // alone.
        queue.want(id);
        let start = tokio::time::Instant::now();
        assert!(queue.back_off(claim, hour).await.is_none(), "handed over");
        assert_eq!(start.elapsed(), Duration::ZERO);
        assert_eq!(waiting(&queue), [(id, true)]);
        assert!(ready_now(queue.next(Slot::Any)).await.is_none(), "reserved");
        let taken = ready_now(&mut wanted)
            .await
            .expect("the Wanted worker takes it");
        assert_eq!(taken.item, Queued::wanted(id));
    }

    /// While the Wanted worker is busy, an eager worker keeps the blob
    /// asked for and backs off as before.
    #[tokio::test(start_paused = true)]
    async fn a_wanted_blob_stays_with_its_eager_worker_while_the_wanted_slot_is_busy() {
        let queue = BlobQueue::default();
        let (busy, id) = (ProtoBlobId::new(), ProtoBlobId::new());
        let hour = Duration::from_secs(3600);
        queue.want(busy);
        let _busy = ready_now(queue.next(Slot::Wanted))
            .await
            .expect("asked for");
        queue.set_policy(BlobPolicy::Eager, &[]);
        queue.registered(&[id]);
        let claim = ready_now(queue.next(Slot::Any)).await.expect("queued");
        queue.want(id);
        let start = tokio::time::Instant::now();
        assert!(queue.back_off(claim, hour).await.is_some(), "kept");
        assert_eq!(start.elapsed(), hour);
    }

    /// Two eager workers back off from blobs asked for while the Wanted
    /// worker is idle: it takes one, and the other stays with its eager
    /// worker rather than waiting behind the first.
    #[tokio::test(start_paused = true)]
    async fn one_idle_wanted_slot_takes_one_handed_over_blob() {
        let queue = BlobQueue::default();
        let (a, b) = (ProtoBlobId::new(), ProtoBlobId::new());
        let hour = Duration::from_secs(3600);
        // Both resumed (asked for), both taken by the eager workers first.
        queue.want(b);
        queue.want(a);
        let claim_a = ready_now(queue.next(Slot::Any)).await.expect("queued");
        let claim_b = ready_now(queue.next(Slot::Any)).await.expect("queued");
        assert_eq!((claim_a.item.id, claim_b.item.id), (a, b));
        let wanted = queue.next(Slot::Wanted);
        tokio::pin!(wanted);
        assert!(ready_now(&mut wanted).await.is_none(), "idle");

        // Both tries fail before the Wanted worker runs again.
        assert!(queue.back_off(claim_a, hour).await.is_none(), "handed over");
        let start = tokio::time::Instant::now();
        let _claim_b = queue.back_off(claim_b, hour).await.expect("kept");
        assert_eq!(start.elapsed(), hour);
        let taken = ready_now(&mut wanted).await.expect("takes a");
        assert_eq!(taken.item.id, a);
        assert!(waiting(&queue).is_empty());
    }

    /// A blob asked for after one was handed over does not pass it: the
    /// Wanted worker takes the handed-over blob first.
    #[tokio::test(start_paused = true)]
    async fn the_wanted_slot_takes_a_handed_over_blob_before_a_newer_request() {
        let queue = BlobQueue::default();
        let (a, x) = (ProtoBlobId::new(), ProtoBlobId::new());
        queue.set_policy(BlobPolicy::Eager, &[]);
        queue.registered(&[a]);
        let claim = ready_now(queue.next(Slot::Any)).await.expect("queued");
        let wanted = queue.next(Slot::Wanted);
        tokio::pin!(wanted);
        assert!(ready_now(&mut wanted).await.is_none(), "idle");
        queue.want(a);
        assert!(
            queue
                .back_off(claim, Duration::from_secs(3600))
                .await
                .is_none()
        );

        queue.want(x);
        assert_eq!(waiting(&queue), [(x, true), (a, true)]);
        let taken = ready_now(&mut wanted).await.expect("takes a");
        assert_eq!(taken.item.id, a);
        // Positive control: the newer request is next, for any worker.
        let next = ready_now(queue.next(Slot::Any)).await.expect("queued");
        assert_eq!(next.item.id, x);
    }

    /// Changes on many threads hand their requests to the engine in the
    /// order they were made, so a purge's `BlobsPurged` can never land
    /// after the `DownloadBlob` of a later change re-registering the id.
    #[test]
    fn requests_reach_the_engine_in_the_order_of_their_changes() {
        let dir = tempfile::tempdir().unwrap();
        let core = Core::open_detached(dir.path()).unwrap();
        let requests = core.seam().take_requests().unwrap();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let core = core.clone();
                std::thread::spawn(move || {
                    for _ in 0..2000 {
                        core.shared
                            .update::<(), Infallible>(|state| {
                                let req = state.version;
                                Ok((
                                    (),
                                    vec![crate::Request::ListDirs {
                                        req,
                                        dir: String::new(),
                                    }],
                                ))
                            })
                            .unwrap_or_else(|never| match never {});
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let order: Vec<u64> = requests
            .try_iter()
            .map(|request| match request {
                crate::Request::ListDirs { req, .. } => req,
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(order.len(), 16_000);
        assert!(order.windows(2).all(|w| w[0] < w[1]), "out of change order");
    }

    /// What the engine then sees for a blob purged by one answer and
    /// registered again (and asked for) by the next: the purge first, so
    /// the request stays queued.
    #[test]
    fn a_purge_before_a_request_for_the_registered_again_blob_keeps_it_queued() {
        let queue = BlobQueue::default();
        let id = ProtoBlobId::new();
        queue.want(id);
        queue.purged(&[id]);
        assert!(waiting(&queue).is_empty(), "the old request goes");
        queue.want(id);
        assert_eq!(waiting(&queue), [(id, true)]);
    }

    #[test]
    fn the_queue_follows_the_policy() {
        let queue = BlobQueue::default();
        let record = |status| BlobRecord {
            id: ProtoBlobId::new(),
            size: 1,
            sha256: ContentHash::of(b""),
            status,
            workdir: false,
        };
        let a = record(BlobStatus::NotDownloaded);
        let b = record(BlobStatus::Available { path: "/x".into() });
        let c = record(BlobStatus::NotDownloaded);

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

    fn waiting(queue: &BlobQueue) -> Vec<(BlobId, bool)> {
        lock(&queue.queue)
            .waiting
            .iter()
            .map(|q| (q.id, q.wanted))
            .collect()
    }

    #[test]
    fn switching_to_eager_queues_no_workdir_blob() {
        let queue = BlobQueue::default();
        let record = |workdir| BlobRecord {
            id: ProtoBlobId::new(),
            size: 1,
            sha256: ContentHash::of(b""),
            status: BlobStatus::NotDownloaded,
            workdir,
        };
        let event = record(false);
        let workdir = record(true);
        queue.set_policy(BlobPolicy::Eager, &[event.clone(), workdir.clone()]);
        assert_eq!(waiting(&queue), [(event.id, false)]);
        // Asked for, a workdir blob downloads like any other.
        queue.want(workdir.id);
        assert_eq!(waiting(&queue), [(workdir.id, true), (event.id, false)]);
    }

    #[test]
    fn restore_under_eager_queues_no_workdir_blob_but_resumes_one() {
        use crate::store::Store;
        use crate::workdir::{WorkdirAnswer, WorkdirAsk};
        use skywave_proto::{ChangeSet, ChangedFile, DiffBody, FileBody, FileChange, FileResult};

        let dir = tempfile::tempdir().unwrap();
        let session_id = SessionId::new();
        let event_blob = BlobRef {
            id: ProtoBlobId::new(),
            size: 3,
            sha256: ContentHash::of(b""),
        };
        let file_blob = BlobRef {
            id: ProtoBlobId::new(),
            size: 4,
            sha256: ContentHash::of(b""),
        };
        let diff_blob = BlobRef {
            id: ProtoBlobId::new(),
            size: 5,
            sha256: ContentHash::of(b""),
        };
        {
            let mut store = Store::open(dir.path()).unwrap();
            let event = |kind: EventKind| Event {
                session_id,
                at: Utc::now(),
                command_id: None,
                context: skywave_proto::ContextId::first_of(session_id),
                kind,
            };
            store
                .apply_batch(
                    &EventBatch {
                        from: Seq(1),
                        events: vec![
                            event(EventKind::SessionCreated {
                                workdir: "/srv".into(),
                                context_name: "start".into(),
                                from: None,
                            }),
                            event(EventKind::ToolResult(ToolResult {
                                call: None,
                                is_error: false,
                                status: None,
                                preview: "big".into(),
                                lines: 1,
                                output: Some(event_blob.clone()),
                            })),
                        ],
                    },
                    Utc::now(),
                )
                .unwrap();
            let file = FileResult::File {
                at: Utc::now(),
                path: "a.txt".into(),
                size: 4,
                content: FileBody::Blob(file_blob.clone()),
            };
            let ask = WorkdirAsk::File {
                session: session_id,
                path: "a.txt".into(),
            };
            assert!(
                store
                    .put_workdir(&ask, &WorkdirAnswer::File(file))
                    .unwrap()
                    .is_some()
            );
            let changes = skywave_proto::ChangesResult::Changes(ChangeSet {
                at: Utc::now(),
                branch: None,
                head: None,
                files: vec![ChangedFile {
                    path: "b.txt".into(),
                    from: None,
                    change: FileChange::Modified,
                    added: Some(1),
                    removed: Some(0),
                    diff: DiffBody::Blob(diff_blob.clone()),
                }],
                total: 1,
            });
            let ask = WorkdirAsk::Changes {
                session: session_id,
            };
            store
                .put_workdir(&ask, &WorkdirAnswer::Changes(changes))
                .unwrap();
            // The user opened the diff and the app stopped mid-download.
            store
                .set_blob_status(
                    diff_blob.id,
                    &BlobStatus::Downloading {
                        got: 1,
                        path: "/x.part".into(),
                    },
                )
                .unwrap();
            store.set_blob_policy(BlobPolicy::Eager).unwrap();
        }
        let core = Core::open_detached(dir.path()).unwrap();
        let queue = BlobQueue::restore(&core.seam());
        assert_eq!(
            waiting(&queue),
            [(diff_blob.id, true), (event_blob.id, false)]
        );
    }
}
