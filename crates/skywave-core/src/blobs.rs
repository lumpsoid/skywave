//! The blob downloader (architecture.md "Client core", D6): fetches blob content into the
//! core's data directory, apart from event sync.
//!
//! **What gets downloaded.** [`BlobQueue`] holds the blobs waiting for a
//! download. A blob the user asks for (`Action::DownloadBlob`) goes to the
//! front. With [`BlobPolicy::Eager`], every blob a pulled batch registers
//! goes to the back, as does every blob neither available nor failed when
//! the policy switches to `Eager` (on startup too); switching back to `OnDemand` drops the waiting
//! blobs nobody asked for, and stops those a worker holds: a download in
//! progress at its next chunk (or as its request waits), one backing off
//! at once. Bytes are dear over a bad network, so a dropped blob keeps
//! what it got: its `.part` file stays and its row goes from `Downloading`
//! to `BlobStatus::Stopped` with the recorded progress, which the views
//! show as not downloaded (no download under way). Asking for it, or
//! `Eager` again, resumes from those bytes. On startup a blob left
//! `Downloading` in the replica is queued again under either policy: the
//! restart resumes what the store says was under way. The store does not
//! say who started it, so the policy decides: under `OnDemand` it resumes
//! as asked for (only a request could have started it, bar a kill
//! between a switch and its stop), under `Eager` as a background download
//! (a workdir blob's always as asked for), which a later switch to
//! `OnDemand` stops; a user who asked before the restart asks again. A
//! `Stopped` one is queued only by `Eager`, so a restart under `OnDemand`
//! does not resume a download nobody asked for.
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
//! downloader. Every request names the run the link expects
//! ([`ExpectedRun`](crate::ExpectedRun)), so a server in another run
//! refuses it ([`TransportError::RunChanged`], retryable): by a later try
//! the link has met the new run, or stopped on a lost log. The queue outlives the downloader, so a suspend, a
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
//! blob `BlobStatus::Failed` with that reason until it is asked for again.
//! The store keeps it, so a restart neither resumes it nor, with `Eager`,
//! queues it: a blob that keeps failing its hash or is gone from the server
//! costs nothing more until someone asks.
//!
//! **Disk full.** A blob file write that finds the device's disk full
//! (`ENOSPC`, `ErrorKind::StorageFull`, or a quota: `QuotaExceeded`), or a
//! progress report the replica refuses with `SQLITE_FULL`, is no failure
//! of the blob: deleting its `.part` would free the space for the next
//! eager blob to fill, so the whole queue would be fetched and thrown away
//! in turn, and retrying a full replica would refetch the same span
//! forever. The blob keeps its `.part` and its recorded progress (it stays
//! `Downloading`, or `NotDownloaded` if no report made it; a file write
//! that fails records what the synced file holds, never more, as tokio
//! reports a failed background write only on the next one), goes back to
//! the front of the queue, and every download pauses:
//! no worker takes a blob, and one backing off lets its blob go the same
//! way. Nothing measures free space. The pause ends on a definitive signal
//! only: a download asked for ([`BlobQueue::want`], the user acting once
//! space may be free; if it is not, the next write pauses again), or a
//! restart (the pause is in memory; the store resumes what was
//! `Downloading`). The views show it (`Pause::DiskFull` on a download in
//! progress, `SettingsView::blobs_disk_full`), reported by the queue under
//! its lock so they follow it in order.
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
//! (its session deleted from this device, no stored workdir answer
//! naming it any more, or deleted or cropped on the server) loses both
//! files; a download of it stops, without a
//! retry, when its next report finds the blob unknown, and removes what it
//! wrote meanwhile.
//!
//! **Startup sweep.** A purge commits the rows first and removes the files
//! after, so a crash in between leaves files no row owns. On startup,
//! before the downloader runs, [`BlobQueue::restore`] removes them
//! ([`sweep_blob_files`]), from the rows it reads anyway: a row owns its
//! complete file when `Available`, its `.part` when `Downloading` or
//! `Stopped`. A `.part` beside a `NotDownloaded` or `Failed` row goes too:
//! no recorded progress points into it, so a try would truncate it.
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
//! **Local crops.** A server crop of a blob this device holds (available,
//! or itself the `into` of a crop still pending: a crop of a crop) is made
//! here, not fetched ([`PendingCrop`]). Its `into` is queued at the front,
//! whatever the policy, as a job with the source `LocalCrop { from }`, and
//! a worker claims it like any download, so no eager download or request
//! of `into` writes its `.part` file meanwhile. Whatever queued `into` (a
//! request or `Eager` can before the engine queues the crop), the worker
//! claiming it asks the replica for a pending crop and makes that first.
//! A job waits while the blob it reads is still queued or held, so a crop
//! of a crop runs after the first. An eager worker backing off from a
//! blob lets it go to a crop waiting (queued again behind the crops), so a
//! crop never waits on a link that is down. The worker crops `from`'s file into `blobs/<into>.part` on a
//! blocking thread, checks size and hash, renames it and marks `into`
//! `Available` under its lease, as a download completes. Made or not, the
//! old blob is then purged ([`Seam::local_crop_ended`]). A missing file, a
//! mismatch or an I/O error deletes the `.part`, and the same claim falls
//! through to a fetch of `into` if it is wanted (asked for, or `Eager`)
//! and has no fate of its own (the server no longer has a crop's middle
//! blob); otherwise `into` stays `NotDownloaded`. An `into` purged meanwhile fails
//! the lease check: its files go, and the store purged the old blob with
//! it. A pending crop is derived from the store (a `Cropped` fate whose
//! blob still has a row), so a restart queues it again.
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
use crate::store::{BlobPolicy, BlobRecord, BlobStatus, PendingCrop, io_disk_full};
use crate::transport::{ExpectedRun, Transport, TransportError};
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

/// Where a queued blob's bytes come from first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    /// The server.
    Network,
    /// A server crop of `from`, whose bytes this device holds: made here
    /// (module docs, "Local crops"), then fetched only if that fails and
    /// the blob is wanted.
    LocalCrop { from: BlobId },
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
    source: Source,
}

impl Queued {
    fn eager(id: BlobId) -> Queued {
        Queued {
            id,
            wanted: false,
            reserved: false,
            source: Source::Network,
        }
    }

    fn wanted(id: BlobId) -> Queued {
        Queued {
            wanted: true,
            ..Queued::eager(id)
        }
    }

    fn local_crop(crop: PendingCrop) -> Queued {
        Queued {
            source: Source::LocalCrop { from: crop.from },
            ..Queued::eager(crop.into)
        }
    }

    /// A local crop waits under any policy: it needs no network.
    fn kept_on_demand(&self) -> bool {
        self.wanted || matches!(self.source, Source::LocalCrop { .. })
    }
}

/// The blobs waiting, in the order workers take them, each at most once,
/// with their ids beside for a constant-time membership check. Every
/// change goes through these methods, so the two never disagree.
#[derive(Debug, Default)]
struct Waiting {
    order: VecDeque<Queued>,
    ids: HashSet<BlobId>,
}

impl Waiting {
    fn contains(&self, id: BlobId) -> bool {
        self.ids.contains(&id)
    }

    fn iter(&self) -> impl DoubleEndedIterator<Item = &Queued> + ExactSizeIterator {
        self.order.iter()
    }

    fn admit(&mut self, id: BlobId) {
        let fresh = self.ids.insert(id);
        debug_assert!(fresh, "{id:?} waits twice");
    }

    fn push_front(&mut self, item: Queued) {
        self.admit(item.id);
        self.order.push_front(item);
    }

    fn push_back(&mut self, item: Queued) {
        self.admit(item.id);
        self.order.push_back(item);
    }

    fn insert(&mut self, at: usize, item: Queued) {
        self.admit(item.id);
        self.order.insert(at, item);
    }

    fn remove_at(&mut self, at: usize) -> Option<Queued> {
        let item = self.order.remove(at)?;
        self.ids.remove(&item.id);
        Some(item)
    }

    /// Takes `id` out; a scan only when it is there.
    fn remove(&mut self, id: BlobId) -> Option<Queued> {
        if !self.ids.remove(&id) {
            return None;
        }
        let at = self.order.iter().position(|q| q.id == id);
        at.and_then(|at| self.order.remove(at))
    }

    /// Keeps the entries `keep` says, in order.
    fn retain(&mut self, mut keep: impl FnMut(&Queued) -> bool) {
        let ids = &mut self.ids;
        self.order.retain(|q| {
            let kept = keep(q);
            if !kept {
                ids.remove(&q.id);
            }
            kept
        });
        debug_assert_eq!(self.order.len(), self.ids.len());
    }

    /// The order and the ids name the same blobs, each once.
    #[cfg(test)]
    fn consistent(&self) -> bool {
        self.order.len() == self.ids.len() && self.order.iter().all(|q| self.ids.contains(&q.id))
    }
}

#[derive(Debug, Default)]
struct Queue {
    policy: BlobPolicy,
    waiting: Waiting,
    /// Blobs whose download may have started since the store was read: a
    /// worker took them, or the store left them `Downloading`. Only these
    /// can be `Downloading` with nobody holding them, so only these are
    /// stopped when `OnDemand` drops them ([`BlobQueue::set_policy`]),
    /// rather than one replica read per blob dropped.
    started: HashSet<BlobId>,
    /// Blobs a worker holds.
    active: HashSet<BlobId>,
    /// Blobs asked for while a worker held them: queued again, as asked
    /// for, once it lets go.
    again: HashSet<BlobId>,
    /// The [`Slot::Wanted`] worker waits for a blob.
    wanted_idle: bool,
    /// A write failed on a full device disk: no worker takes a blob until
    /// one is asked for (module docs, "Disk full").
    disk_full: bool,
}

impl Queue {
    fn holds(&self, id: BlobId) -> bool {
        self.active.contains(&id) || self.waiting.contains(id)
    }

    /// Whether a worker holding `item` lets it go now: the policy is
    /// `OnDemand`, and nobody asked for it (nor is it a local crop).
    fn drops(&self, item: &Queued) -> bool {
        self.policy == BlobPolicy::OnDemand
            && !item.kept_on_demand()
            && !self.again.contains(&item.id)
    }

    fn push_back(&mut self, id: BlobId) {
        if !self.holds(id) {
            self.waiting.push_back(Queued::eager(id));
        }
    }

    /// Whether a worker may take `item` now: a local crop waits while the
    /// blob it reads is still queued or held (the `into` of an earlier
    /// crop, a crop of a crop), so the two run in log order.
    fn ready(&self, item: &Queued) -> bool {
        match item.source {
            Source::Network => true,
            Source::LocalCrop { from } => !self.holds(from),
        }
    }

    /// Queues local crops at the front, in the order given, ahead of
    /// anything queued for the same blob. One a worker holds already is
    /// left to it: it was queued after the batch that made the crop pending
    /// was stored, so the worker, asking the replica on its claim, makes
    /// it locally ([`worker`]).
    fn push_crops(&mut self, crops: &[PendingCrop]) {
        let fresh: Vec<_> = crops
            .iter()
            .filter(|crop| !self.active.contains(&crop.into))
            .copied()
            .collect();
        for crop in fresh.iter().rev() {
            let wanted = self.waiting.remove(crop.into).is_some_and(|q| q.wanted);
            self.waiting.push_front(Queued {
                wanted,
                ..Queued::local_crop(*crop)
            });
        }
    }

    /// A local crop a worker may take now is waiting.
    fn crop_ready(&self) -> bool {
        self.waiting
            .iter()
            .any(|q| matches!(q.source, Source::LocalCrop { .. }) && self.ready(q))
    }
}

/// The blobs waiting for the downloader; see the module docs.
#[derive(Default)]
pub(crate) struct BlobQueue {
    queue: Mutex<Queue>,
    wake: Notify,
    /// Where the queue says it paused on a full disk, or resumed, under
    /// its lock, so the views follow it in order; none in tests that only
    /// drive the queue.
    view: Option<Seam>,
    /// How many times [`BlobQueue::stop`] was called.
    #[cfg(test)]
    stops: std::sync::atomic::AtomicUsize,
}

impl BlobQueue {
    /// The queue as the replica leaves it: pending local crops first, in
    /// log order, then blobs left `Downloading` (as asked for under
    /// `OnDemand`, and workdir ones always; under `Eager` an event blob's
    /// as the background's), then, with `Eager`, every event blob not
    /// downloaded yet. It reports a disk-full pause to `seam`'s views.
    /// First it sweeps the blob files no row owns ([`sweep_blob_files`]):
    /// it runs once per engine, before the downloader starts.
    pub(crate) fn restore(seam: &Seam) -> BlobQueue {
        let this = BlobQueue {
            view: Some(seam.clone()),
            ..BlobQueue::default()
        };
        let read = seam
            .settings()
            .and_then(|s| Ok((s, seam.blobs()?, seam.pending_crops()?)));
        let (settings, blobs, crops) = match read {
            Ok(found) => found,
            Err(error) => {
                tracing::warn!(%error, "cannot read blobs; none queued");
                return this;
            }
        };
        // Before any worker writes a blob file: the rows just read say
        // which files are owned.
        sweep_blob_files(&seam.data_dir(), &blobs);
        {
            let mut queue = lock(&this.queue);
            // The store does not say who started a download. Under
            // `OnDemand` only a request could have (bar a kill between a
            // switch and its stop); under `Eager` an event blob's may be
            // the background's, so it resumes as one and a later switch to
            // `OnDemand` stops it, `Stopped` with its bytes: a user who
            // asked before the restart asks again. A workdir blob is never
            // fetched in the background: asked for.
            let on_demand = settings.blob_policy == BlobPolicy::OnDemand;
            for blob in &blobs {
                if matches!(blob.status, BlobStatus::Downloading { .. }) {
                    queue.started.insert(blob.id);
                    queue.waiting.push_back(if on_demand || blob.workdir {
                        Queued::wanted(blob.id)
                    } else {
                        Queued::eager(blob.id)
                    });
                }
            }
            queue.push_crops(&crops);
        }
        this.set_policy(settings.blob_policy, &blobs);
        this
    }

    /// The user asked for this blob: it goes first. One a worker holds is
    /// marked asked for, and an eager worker backing off from it lets it go.
    /// A disk-full pause ends: the user may have freed space.
    pub(crate) fn want(&self, id: BlobId) {
        let mut queue = lock(&self.queue);
        self.set_disk_full(&mut queue, false);
        if queue.active.contains(&id) {
            queue.again.insert(id);
        } else {
            let found = queue.waiting.remove(id);
            let reserved = found.is_some_and(|q| q.reserved);
            let source = found.map_or(Source::Network, |q| q.source);
            queue.waiting.push_front(Queued {
                reserved,
                source,
                ..Queued::wanted(id)
            });
        }
        drop(queue);
        self.wake.notify_waiters();
    }

    /// The policy changed; `blobs` is every blob the replica knows. With
    /// `OnDemand`, the waiting blobs nobody asked for go, a download one
    /// had started becoming `Stopped`; workers holding one let it go
    /// ([`BlobQueue::dropped`], [`BlobQueue::back_off`]).
    pub(crate) fn set_policy(&self, policy: BlobPolicy, blobs: &[BlobRecord]) {
        let mut queue = lock(&self.queue);
        queue.policy = policy;
        match policy {
            BlobPolicy::OnDemand => {
                let mut dropped = Vec::new();
                queue.waiting.retain(|q| {
                    let kept = q.kept_on_demand();
                    if !kept {
                        dropped.push(q.id);
                    }
                    kept
                });
                // Every dropped one that may have started, whatever
                // `blobs` says: that snapshot was read before this lock,
                // and a worker may have recorded progress and let the blob
                // go since (or the read failed and `blobs` is empty).
                // `stop` reads the row itself, so it is called only for
                // the few started ones ([`Queue::started`]), not for each
                // of a big eager queue. Under the queue's lock, so a
                // request made meanwhile resumes from the stopped row,
                // never races its stop.
                for id in dropped {
                    if queue.started.remove(&id) {
                        self.stop(id);
                    }
                }
            }
            BlobPolicy::Eager => {
                // Newest first: ids are time-ordered. Workdir blobs only
                // download when asked for, as does a blob that failed.
                for blob in blobs.iter().rev() {
                    let eager = matches!(
                        blob.status,
                        BlobStatus::NotDownloaded
                            | BlobStatus::Stopped { .. }
                            | BlobStatus::Downloading { .. }
                    );
                    if !blob.workdir && eager {
                        if matches!(blob.status, BlobStatus::Downloading { .. }) {
                            queue.started.insert(blob.id);
                        }
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

    /// A pulled batch left these crops pending: they go to the front, under
    /// any policy, as they need no network.
    pub(crate) fn local_crops(&self, crops: &[PendingCrop]) {
        if crops.is_empty() {
            return;
        }
        lock(&self.queue).push_crops(crops);
        self.wake.notify_waiters();
    }

    /// Pauses (or resumes) every download for a full device disk, under
    /// the queue's lock, which `queue` is; a change reaches the views in the
    /// same critical section, so they never disagree with the queue for
    /// long.
    fn set_disk_full(&self, queue: &mut Queue, full: bool) {
        if std::mem::replace(&mut queue.disk_full, full) == full {
            return;
        }
        if let Some(view) = &self.view {
            view.blobs_disk_full(full);
        }
    }

    /// Marks `id`, dropped under `OnDemand` with nobody holding it,
    /// `Stopped` if its download had started ([`Seam::blob_stopped`]).
    /// Called under the queue's lock (queue, then state, as
    /// [`BlobQueue::set_disk_full`]), so a request for it waits until the
    /// row says what it kept.
    fn stop(&self, id: BlobId) {
        #[cfg(test)]
        self.stops
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let Some(view) = &self.view else { return };
        match view.blob_stopped(id) {
            Ok(()) | Err(CoreError::Superseded) => {}
            Err(error) => tracing::warn!(?id, %error, "cannot mark the blob's download stopped"),
        }
    }

    /// Resolves once a worker holding `item` must let it go under
    /// `OnDemand` ([`Queue::drops`]); never for a blob asked for.
    async fn dropped(&self, item: &Queued) {
        loop {
            let notified = self.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if lock(&self.queue).drops(item) {
                return;
            }
            notified.await;
        }
    }

    /// A worker's write failed on a full device disk: it lets go of the
    /// blob, which waits at the front, and every download pauses until one
    /// is asked for. What the blob got stays, `.part` and recorded progress.
    fn disk_full(&self, mut claim: Claim<'_>) {
        let mut queue = lock(&self.queue);
        claim.let_go(&mut queue, LetGo::Unfinished);
        self.set_disk_full(&mut queue, true);
        drop(queue);
        self.wake.notify_waiters();
    }

    /// Whether a worker may take `item` now ([`Queue::ready`]).
    fn ready(&self, item: &Queued) -> bool {
        lock(&self.queue).ready(item)
    }

    /// Whether a blob a worker holds is fetched once its local crop failed:
    /// asked for, or the policy is `Eager`.
    fn fetch_wanted(&self, item: &Queued) -> bool {
        let queue = lock(&self.queue);
        item.wanted || queue.again.contains(&item.id) || queue.policy == BlobPolicy::Eager
    }

    /// These blobs left the replica (a session deleted from this device, or
    /// a refreshed workdir answer no longer naming them): they wait no
    /// longer. One a worker holds is let go by the worker itself, when its
    /// next report finds the blob unknown.
    pub(crate) fn purged(&self, ids: &[BlobId]) {
        let ids: HashSet<BlobId> = ids.iter().copied().collect();
        let mut queue = lock(&self.queue);
        queue.waiting.retain(|q| !ids.contains(&q.id));
        queue.again.retain(|id| !ids.contains(id));
        // One a worker holds may record progress (on a row registered
        // again) until it lets go: it stays started.
        let Queue {
            started, active, ..
        } = &mut *queue;
        started.retain(|id| !ids.contains(id) || active.contains(id));
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
                let ready = |q: &Queued| queue.ready(q);
                let at = match slot {
                    // Paused: whatever it takes would write to the full disk.
                    _ if queue.disk_full => None,
                    Slot::Any => queue.waiting.iter().position(|q| !q.reserved && ready(q)),
                    // The blob handed over first: it was asked for before
                    // anything queued since, and nothing else may take it.
                    Slot::Wanted => (queue.waiting.iter().position(|q| q.reserved))
                        .or_else(|| queue.waiting.iter().position(|q| q.wanted && ready(q))),
                };
                if let Some(mut item) = at.and_then(|at| queue.waiting.remove_at(at)) {
                    item.reserved = false;
                    queue.active.insert(item.id);
                    queue.started.insert(item.id);
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
    /// worker takes it before anything else. A local crop waiting takes
    /// the worker too: the blob is let go, queued again behind the crops,
    /// so a crop (which needs no network) never waits on a link down. A
    /// blob nobody asked for, once the policy is `OnDemand`, is let go
    /// for good ([`Queue::drops`]).
    async fn back_off<'a>(&'a self, mut claim: Claim<'a>, delay: Duration) -> Option<Claim<'a>> {
        let sleep = tokio::time::sleep(delay);
        tokio::pin!(sleep);
        loop {
            let notified = self.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut queue = lock(&self.queue);
                if queue.disk_full {
                    // Paused meanwhile: the blob waits at the front, as
                    // the one whose write found the disk full does.
                    claim.let_go(&mut queue, LetGo::Unfinished);
                    drop(queue);
                    self.wake.notify_waiters();
                    return None;
                }
                if queue.drops(&claim.item) {
                    // `OnDemand` now and nobody asked: no more tries. The
                    // blob goes, stopped with what it got (`let_go`).
                    claim.let_go(&mut queue, LetGo::Unfinished);
                    drop(queue);
                    self.wake.notify_waiters();
                    return None;
                }
                let wanted = claim.item.wanted || queue.again.contains(&claim.item.id);
                if wanted && queue.wanted_idle {
                    queue.wanted_idle = false;
                    claim.let_go(&mut queue, LetGo::HandedOver);
                    drop(queue);
                    self.wake.notify_waiters();
                    return None;
                }
                if queue.crop_ready() {
                    claim.let_go(&mut queue, LetGo::Yielded);
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
    /// The downloader stopped, the disk is full, or (under `OnDemand`,
    /// nobody asking) the blob is dropped.
    Unfinished,
    /// To the Wanted worker; see [`BlobQueue::back_off`].
    HandedOver,
    /// To a local crop waiting, which needs no network; queued again
    /// behind it.
    Yielded,
}

impl Claim<'_> {
    fn finish(self) {
        self.release(LetGo::Finished);
    }

    /// Lets go of a local crop that must wait for the blob it reads; it
    /// waits in the queue, as it would have unclaimed.
    fn wait(self) {
        self.release(LetGo::Unfinished);
    }

    /// Lets go of a blob whose try stopped as it was dropped
    /// ([`BlobQueue::dropped`]): it goes, `Stopped`, unless asked for or
    /// `Eager` again meanwhile, when it waits at the front.
    fn stopped(self) {
        self.release(LetGo::Unfinished);
    }

    fn release(mut self, how: LetGo) {
        let owner = self.queue;
        let mut queue = lock(&owner.queue);
        self.let_go(&mut queue, how);
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
            // Behind the local crops, which it let go for.
            LetGo::Yielded
                if again || self.item.kept_on_demand() || queue.policy == BlobPolicy::Eager =>
            {
                let at = (queue.waiting.iter())
                    .rposition(|q| matches!(q.source, Source::LocalCrop { .. }))
                    .map_or(0, |at| at + 1);
                queue.waiting.insert(
                    at,
                    Queued {
                        wanted: self.item.wanted || again,
                        ..self.item
                    },
                );
            }
            LetGo::Yielded => {
                queue.started.remove(&id);
                self.queue.stop(id);
            }
            // A local crop not yet tried keeps its source.
            _ if again => queue.waiting.push_front(Queued {
                wanted: true,
                reserved: false,
                ..self.item
            }),
            // Not asked for, it waits only while the policy is Eager (or it
            // is a local crop): one switched to OnDemand while a worker
            // held it goes, as the switch dropped the waiting ones.
            LetGo::Unfinished
                if self.item.kept_on_demand() || queue.policy == BlobPolicy::Eager =>
            {
                queue.waiting.push_front(self.item);
            }
            // Dropped under `OnDemand`: what it got stays, `Stopped`.
            LetGo::Unfinished => {
                queue.started.remove(&id);
                self.queue.stop(id);
            }
            // Available, failed or gone: nothing left to stop.
            LetGo::Finished => {
                queue.started.remove(&id);
            }
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
    /// The link's; the downloader only reads it.
    pub(crate) run: ExpectedRun,
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
        // However the blob was queued (a request or `Eager` may queue it
        // before the engine queues its crop), a crop pending for it is
        // made here first; the replica says which.
        let pending = ctx.seam.pending_crop_into(id).unwrap_or_else(|error| {
            tracing::warn!(?id, %error, "cannot read the blob's pending crop");
            match claim.item.source {
                Source::LocalCrop { from } => Some(PendingCrop { into: id, from }),
                Source::Network => None,
            }
        });
        let queued_as_crop = matches!(claim.item.source, Source::LocalCrop { .. });
        claim.item.source = match pending {
            Some(crop) => Source::LocalCrop { from: crop.from },
            None => Source::Network,
        };
        if let Some(crop) = pending {
            if !ctx.queue.ready(&claim.item) {
                claim.wait();
                continue;
            }
            let done = crop_locally(ctx, crop).await;
            claim.item.source = Source::Network;
            if done || !fetch_after_crop(ctx, &claim.item) {
                claim.finish();
                continue;
            }
        } else if queued_as_crop && !fetch_after_crop(ctx, &claim.item) {
            // Made already, or gone: fetched only if wanted.
            claim.finish();
            continue;
        }
        let mut attempt = 0u32;
        // A hash mismatch already seen in this hold (module docs, "Checked
        // by hash").
        let mut mismatched = false;
        loop {
            let mut progressed = false;
            let mut lease = None;
            let tried = fetch(ctx, transport, &claim.item, &mut lease, &mut progressed).await;
            let error = match tried {
                Ok(()) => break claim.finish(),
                Err(FetchError::Stopped) => {
                    tracing::debug!(?id, "blob download stopped: nobody asked for it");
                    break claim.stopped();
                }
                Err(error) => error,
            };
            if error.is_purged() {
                // Whatever this try wrote may have outlived the deletion.
                tracing::debug!(?id, "blob purged during its download");
                remove_blob_files(&ctx.seam.data_dir(), id);
                break claim.finish();
            }
            if error.is_disk_full() {
                // Not a failure of this blob: give_up would delete what it
                // got, freeing the space the next blob then fills.
                tracing::warn!(?id, %error, "device disk full: blob downloads paused");
                ctx.queue.disk_full(claim);
                break;
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
                        tracing::debug!(?id, "blob let go to the wanted slot or a local crop");
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
    /// Nobody wants the blob any more ([`BlobQueue::dropped`]): no
    /// failure, and no retry.
    #[error("blob download stopped: nobody asked for it")]
    Stopped,
}

impl FetchError {
    /// The blob left the replica mid-download (its session was deleted from
    /// this device, no workdir answer names it any more, the server deleted
    /// or cropped it, or the device paired with another server), even if
    /// its id is registered again
    /// since: nothing to retry, and its files must go.
    fn is_purged(&self) -> bool {
        matches!(
            self,
            FetchError::Core(
                CoreError::Store(crate::store::StoreError::UnknownBlob(_)) | CoreError::Superseded
            )
        )
    }

    /// A write, to the blob file or to the replica recording its progress,
    /// found the device's disk (or the user's quota) full: nothing wrong
    /// with the blob, and no try succeeds until space is freed. Checked
    /// before [`FetchError::is_retryable`], which would retry a full
    /// replica forever, refetching the span since the last report each
    /// time.
    fn is_disk_full(&self) -> bool {
        match self {
            FetchError::File(error) => io_disk_full(error),
            FetchError::Core(CoreError::Store(error)) => error.disk_full(),
            _ => false,
        }
    }

    fn is_retryable(&self) -> bool {
        match self {
            // A rejection is the link's to report; wait for it to stop us.
            FetchError::Transport(e) => e.is_retryable() || e.rejection().is_some(),
            FetchError::Core(_) => true,
            FetchError::File(_)
            | FetchError::Size { .. }
            | FetchError::Offset { .. }
            | FetchError::Hash { .. }
            | FetchError::Stopped => false,
        }
    }
}

/// Deletes what was fetched of a blob and leaves it `Failed`, stored with
/// its reason, until it is asked for again: a restart does not queue it,
/// nor does `Eager`.
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

/// A file name in the blob directory, as the downloader and local crops
/// write them: `<id>` or `<id>.part`, the id hyphenated as `Display` gives
/// it. Anything else is not ours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlobFile {
    Complete(BlobId),
    Part(BlobId),
}

impl BlobFile {
    fn parse(name: &str) -> Option<BlobFile> {
        let (stem, part) = match name.strip_suffix(".part") {
            Some(stem) => (stem, true),
            None => (name, false),
        };
        let id = uuid::Uuid::try_parse(stem).ok()?;
        // Only the exact spelling we write: `try_parse` also takes the
        // simple, braced and URN forms, and upper case.
        if id.to_string() != stem {
            return None;
        }
        let id = BlobId(id);
        Some(if part {
            BlobFile::Part(id)
        } else {
            BlobFile::Complete(id)
        })
    }

    /// Whether a row in `status` owns this file: an `Available` blob its
    /// complete file, a `Downloading` or `Stopped` one its `.part`, which
    /// holds the recorded progress. `NotDownloaded` and `Failed` own none:
    /// a try for them starts from zero and truncates any `.part`, so one
    /// left beside them (a disk-full pause before the first report, a crash
    /// before a failure's delete) holds nothing anybody resumes from. A
    /// complete file whose row is not `Available` (a crash between the
    /// rename and the report) is fetched again over.
    fn owned_by(self, status: &BlobStatus) -> bool {
        matches!(
            (self, status),
            (BlobFile::Complete(_), BlobStatus::Available { .. })
                | (
                    BlobFile::Part(_),
                    BlobStatus::Downloading { .. } | BlobStatus::Stopped { .. }
                )
        )
    }
}

/// Removes every file in `data_dir`'s blob directory that no row in
/// `blobs` (the whole replica's, event and workdir blobs alike) owns
/// ([`BlobFile::owned_by`]): what a crash between a purge's commit and its
/// file removal leaves, as the server's `BlobStore::sweep` does on its
/// side. Returns how many went. Startup only, before the downloader runs:
/// a download or a crop in flight writes exactly such files. A purge
/// meanwhile only removes files, and a row registered meanwhile has none.
/// A name that is not a blob file's, or a directory, is left alone and
/// logged.
pub(crate) fn sweep_blob_files(data_dir: &Path, blobs: &[BlobRecord]) -> usize {
    let dir = blob_dir(data_dir);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return 0,
        Err(error) => {
            tracing::warn!(%error, "cannot list the blob files; none swept");
            return 0;
        }
    };
    let statuses: std::collections::HashMap<BlobId, &BlobStatus> =
        blobs.iter().map(|b| (b.id, &b.status)).collect();
    let mut removed = 0;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::warn!(%error, "cannot list the blob files; sweep cut short");
                break;
            }
        };
        let path = entry.path();
        let file = entry.file_name().to_str().and_then(BlobFile::parse);
        let Some(file) = file.filter(|_| entry.file_type().is_ok_and(|t| t.is_file())) else {
            tracing::info!(path = %path.display(), "not a blob file; left alone");
            continue;
        };
        let id = match file {
            BlobFile::Complete(id) | BlobFile::Part(id) => id,
        };
        if statuses
            .get(&id)
            .is_some_and(|status| file.owned_by(status))
        {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {
                tracing::debug!(path = %path.display(), "removed a blob file no row owns");
                removed += 1;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "cannot remove a blob file")
            }
        }
    }
    if removed > 0 {
        tracing::info!(removed, "swept blob files no row owns");
    }
    removed
}

/// Whether a blob whose local crop did not make it is fetched: when wanted
/// (asked for, or `Eager`), and the server still has it. One with a fate
/// of its own (the middle of a crop of a crop) is gone from the server;
/// the next crop, reading it, purges it.
fn fetch_after_crop(ctx: &BlobCtx, item: &Queued) -> bool {
    ctx.queue.fetch_wanted(item)
        && !ctx.seam.blob_fated(item.id).unwrap_or_else(|error| {
            tracing::warn!(id = ?item.id, %error, "cannot read the blob's fate");
            false
        })
}

/// Makes `crop.into` from the bytes of `crop.from` (module docs, "Local
/// crops"), then lets `crop.from` go, made or not. Whether `into` needs no
/// fetch: made, or gone from the replica.
async fn crop_locally(ctx: &BlobCtx, crop: PendingCrop) -> bool {
    let done = match make_crop(ctx, crop).await {
        Ok(()) => true,
        Err(error) if error.is_purged() => {
            // Whatever it wrote may have outlived the purge.
            tracing::debug!(?crop, "cropped blob purged during its local crop");
            remove_blob_files(&ctx.seam.data_dir(), crop.into);
            true
        }
        Err(error) => {
            tracing::warn!(?crop, %error, "cannot crop the blob here");
            let part = part_path(&blob_dir(&ctx.seam.data_dir()), crop.into);
            let _ = tokio::fs::remove_file(&part).await;
            false
        }
    };
    match ctx.seam.local_crop_ended(crop) {
        Ok(()) | Err(CoreError::Superseded) => {}
        Err(error) => tracing::warn!(?crop, %error, "cannot purge the cropped blob"),
    }
    done
}

/// Crops `crop.from`'s file into `crop.into`'s `.part` file on a blocking
/// thread, then checks and completes it as a download ([`complete`]),
/// under `into`'s lease. `into` made already (before a restart) or gone
/// needs nothing.
async fn make_crop(ctx: &BlobCtx, crop: PendingCrop) -> Result<(), FetchError> {
    let Some((record, lease, source)) = ctx.seam.local_crop(crop)? else {
        return Ok(());
    };
    let dir = blob_dir(&ctx.seam.data_dir());
    let done = dir.join(crop.into.0.to_string());
    if let BlobStatus::Available { path } = &record.status
        && tokio::fs::try_exists(path).await.unwrap_or(false)
    {
        return Ok(());
    }
    let Some(source) = source else {
        return Err(FetchError::File(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "the cropped blob is not here",
        )));
    };
    tokio::fs::create_dir_all(&dir).await?;
    let part = part_path(&dir, crop.into);
    let written = part.clone();
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        let content = std::io::BufReader::new(File::open(&source.path)?);
        let mut out = File::create(&written)?;
        skywave_proto::crop::crop(content, &mut out, &source.kept)
            .map_err(std::io::Error::other)?;
        out.sync_all()
    })
    .await
    .map_err(std::io::Error::other)??;
    complete(ctx, crop.into, lease, &part, &done, &record).await
}

/// One try at `item`, a blob its worker holds: resume its `.part` file and
/// finish it. `lease` is set once the record is read; every report holds
/// it. `progressed` is set once some bytes arrived. Once nobody wants it
/// ([`BlobQueue::dropped`]), a request waiting or a body under way ends
/// the try with [`FetchError::Stopped`], a body after recording what it
/// got; checking and completing a whole file is never cut short.
async fn fetch(
    ctx: &BlobCtx,
    transport: &OnceCell<Arc<dyn Transport>>,
    item: &Queued,
    lease_out: &mut Option<BlobLease>,
    progressed: &mut bool,
) -> Result<(), FetchError> {
    let id = item.id;
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
        BlobStatus::Downloading { got, .. } | BlobStatus::Stopped { got, .. } => *got,
        BlobStatus::NotDownloaded | BlobStatus::Failed { .. } | BlobStatus::Available { .. } => 0,
    };
    let from = match tokio::fs::metadata(&part).await {
        Ok(meta) => recorded.min(meta.len()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => return Err(error.into()),
    };
    // A whole kept file needs no request, only its check. An empty blob is
    // whole before any byte arrives, with no `.part` file yet: create it.
    if from == size {
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&part)
            .await?;
        file.set_len(size).await?;
        file.sync_all().await?;
        drop(file);
        return complete(ctx, id, lease, &part, &done, &record).await;
    }

    let transport = transport
        .get_or_try_init(|| async {
            ctx.config.connector.connect(
                &ctx.address,
                ctx.identity.as_ref(),
                &ctx.config.transport,
                &ctx.run,
            )
        })
        .await?;
    // Dropped first, here and below: a blob dropped meanwhile takes no
    // more bytes, however fast they come.
    let asked = tokio::select! {
        biased;
        () = ctx.queue.dropped(item) => return Err(FetchError::Stopped),
        asked = transport.blob(id, from) => asked,
    };
    let mut download = match asked {
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
        #[cfg(test)]
        tests::replica_room(id, got)?;
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
                    biased;
                    () = ctx.queue.dropped(item) => {
                        // Keep what it paid for: the blob stops from here.
                        if got > recorded {
                            report(&mut file, got).await?;
                        }
                        return Err(FetchError::Stopped);
                    }
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
        if let Err(error) = write_chunk(&mut file, id, got, &chunk).await {
            let error = FetchError::from(error);
            if error.is_disk_full() {
                // Keep what made it, if the disk lets the sync through, so
                // the try resumes from there once space is freed. Only what
                // the synced file holds: tokio's write returns once it
                // queued the bytes, and a background write that failed
                // reports on the next one, so `got` may count bytes that
                // never landed.
                let held = async {
                    file.flush().await?;
                    file.sync_data().await?;
                    file.metadata().await.map(|meta| meta.len())
                }
                .await;
                match held {
                    Ok(len) if len.min(got) > recorded => {
                        if let Err(report_error) = report(&mut file, len.min(got)).await {
                            tracing::debug!(?id, %report_error, "cannot record progress on a full disk");
                        }
                    }
                    Ok(_) => {}
                    Err(sync_error) => {
                        tracing::debug!(?id, %sync_error, "cannot sync a blob on a full disk");
                    }
                }
            }
            return Err(error);
        }
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

/// Appends `chunk`, which starts at byte `at` of blob `id`, to its `.part`
/// file. Tests fill the disk here (`tests::disk_room`), the write failing
/// at once or, as tokio's background write does, only on the next one.
async fn write_chunk(
    file: &mut tokio::fs::File,
    id: BlobId,
    at: u64,
    chunk: &[u8],
) -> std::io::Result<()> {
    #[cfg(test)]
    if tests::disk_room(id, at + chunk.len() as u64)? == tests::Write::Lost {
        return Ok(());
    }
    #[cfg(not(test))]
    let _ = (id, at);
    file.write_all(chunk).await
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
        EventsQuery, Full, HelloResponse, LineSpan, Seq, SessionId, ToolResult,
    };

    use super::*;
    use crate::backoff::{Backoff, Jitter};
    use crate::engine::Connector;
    use crate::identity::PairedCredentials;
    use crate::transport::{BlobBody, BlobDownload, BoxFuture};
    use crate::view::{BlobState, DeviceId, Entry, FullView, Pause, Screen, TimelineItem};
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
                        output: Some(Full::Blob(blob.clone())),
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
            // As the engine builds it: the views follow its pauses.
            let queue = Arc::new(BlobQueue::restore(&seam));
            Fixture {
                _dir: dir,
                core,
                seam,
                session_id,
                blob,
                server,
                queue,
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
                    connector: Connector::new(move |_, _, _, _| {
                        Ok(server.clone() as Arc<dyn Transport>)
                    }),
                    ..EngineConfig::default()
                }),
                address: "srv:7443".into(),
                identity: self.seam.identity().unwrap(),
                run: ExpectedRun::default(),
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
            // Gone once this returns, with the core handle it held.
            let _ = downloader.await;
        }

        /// The device restarts: the core opens the same data directory
        /// again and the blob queue is restored from it, as the engine does.
        /// The server stays.
        fn restarted(self) -> Fixture {
            let Fixture {
                _dir: dir,
                core,
                seam,
                session_id,
                blob,
                server,
                queue,
            } = self;
            drop((core, seam, queue));
            let core = Core::open_detached(dir.path()).unwrap();
            let seam = core.seam();
            let queue = Arc::new(BlobQueue::restore(&seam));
            Fixture {
                _dir: dir,
                core,
                seam,
                session_id,
                blob,
                server,
                queue,
            }
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
                                    output: Some(FullView::Blob(blob)),
                                    ..
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
                    output: Some(Full::Blob(BlobRef {
                        id: *id,
                        size: 10,
                        sha256: ContentHash::of(&[0; 10]),
                    })),
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

    /// `count` lines of text.
    fn text(count: usize) -> Vec<u8> {
        (0..count)
            .flat_map(|i| format!("line {i}\n").into_bytes())
            .collect()
    }

    impl Fixture {
        /// Makes the fixture's blob `Available` with the server's content,
        /// then applies, at `seq`, the server's crop of it to `keep`.
        /// Returns the crop, pending, and what it makes.
        fn held_and_cropped(&self, seq: u64, keep: &[LineSpan]) -> (PendingCrop, Vec<u8>) {
            let dir = blob_dir(&self.seam.data_dir());
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join(self.blob.id.0.to_string());
            std::fs::write(&path, &self.server.content).unwrap();
            self.seam
                .blob_status(self.blob.id, BlobStatus::Available { path })
                .unwrap();
            let mut made = Vec::new();
            let lines = skywave_proto::crop::crop(&self.server.content[..], &mut made, keep)
                .unwrap()
                .lines;
            let into = BlobRef {
                id: ProtoBlobId::new(),
                size: made.len() as u64,
                sha256: ContentHash::of(&made),
            };
            let applied = self
                .seam
                .apply_events(&EventBatch {
                    from: Seq(seq),
                    events: vec![Event {
                        session_id: self.session_id,
                        at: Utc::now(),
                        command_id: Some(skywave_proto::CommandId::new()),
                        context: skywave_proto::ContextId::first_of(self.session_id),
                        kind: EventKind::BlobCropped {
                            blob: self.blob.id,
                            holds: skywave_proto::BlobUse::ToolOutput,
                            into,
                            kept: keep.to_vec(),
                            lines,
                            by: DeviceId::new(),
                        },
                    }],
                })
                .unwrap();
            let [crop] = applied.local_crops[..] else {
                panic!("expected one pending crop: {applied:?}");
            };
            (crop, made)
        }

        /// Runs `downloader` until the local crop of `crop` ended (its old
        /// blob gone) and every worker let go.
        async fn until_crop_ended(&self, crop: PendingCrop) {
            tokio::time::timeout(Duration::from_secs(10), async {
                while self.seam.blob(crop.from).unwrap().is_some()
                    || !lock(&self.queue.queue).active.is_empty()
                {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("the crop never ended: {:?}", lock(&self.queue.queue)));
        }
    }

    /// A worker that claims a blob queued for a fetch (asked for before
    /// the engine queued its crop) makes the pending crop instead; the
    /// crop queued late finds nothing left to do.
    #[tokio::test]
    async fn a_blob_claimed_for_a_fetch_is_cropped_locally_first() {
        let bytes = text(40);
        let f = Fixture::new(bytes.len() as u64, bytes.clone(), bytes.len() as u64);
        let (crop, made) = f.held_and_cropped(3, &[LineSpan { first: 2, count: 5 }]);
        f.queue.want(crop.into);
        let downloader = tokio::spawn(run_downloader(f.ctx()));
        f.until_crop_ended(crop).await;
        f.queue.local_crops(&[crop]);
        tokio::time::timeout(Duration::from_secs(10), async {
            while !waiting(&f.queue).is_empty() || !lock(&f.queue.queue).active.is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the late crop is let go");
        downloader.abort();

        let BlobStatus::Available { path } = f.seam.blob(crop.into).unwrap().unwrap().status else {
            panic!("the crop is made");
        };
        assert_eq!(std::fs::read(path).unwrap(), made);
        assert_eq!(f.server.calls.load(Ordering::SeqCst), 0, "nothing fetched");
    }

    /// Both eager workers backing off from blobs the server cannot serve
    /// (here for a minute) let go of them for a local crop, which needs no
    /// network: it is made at once.
    #[tokio::test]
    async fn a_local_crop_never_waits_on_eager_workers_backing_off() {
        let bytes = text(40);
        let f = Fixture::new(bytes.len() as u64, bytes.clone(), bytes.len() as u64);
        let held = f.held_blobs(3, 2, Hold::Offline);
        f.queue.set_policy(BlobPolicy::Eager, &[]);
        f.queue.registered(&held);
        let minute = Duration::from_secs(60);
        let downloader = tokio::spawn(run_downloader(f.ctx_backing_off(minute, minute)));
        tokio::time::timeout(Duration::from_secs(10), async {
            while lock(&f.queue.queue).active.len() < 2 || f.server.calls.load(Ordering::SeqCst) < 2
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("both eager slots back off from a held blob");

        let (crop, made) = f.held_and_cropped(5, &[LineSpan { first: 0, count: 3 }]);
        f.queue.local_crops(&[crop]);
        tokio::time::timeout(Duration::from_secs(10), async {
            while !matches!(
                f.seam.blob(crop.into).unwrap().unwrap().status,
                BlobStatus::Available { .. }
            ) || f.seam.blob(crop.from).unwrap().is_some()
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the crop is made while the eager workers back off");
        let kept: Vec<_> = {
            let queue = lock(&f.queue.queue);
            let waiting = queue.waiting.iter().map(|q| q.id);
            waiting.chain(queue.active.iter().copied()).collect()
        };
        downloader.abort();
        let BlobStatus::Available { path } = f.seam.blob(crop.into).unwrap().unwrap().status else {
            unreachable!()
        };
        assert_eq!(std::fs::read(path).unwrap(), made);
        for id in &held {
            assert!(kept.contains(id), "{id:?} is queued again or taken back");
        }
    }

    /// A held copy that no longer crops to the server's bytes, under
    /// `OnDemand`: the bad crop is deleted, the old blob purged, and the
    /// cropped blob, not asked for, is not fetched. Checked once the
    /// worker let go of it, after it decided.
    #[tokio::test]
    async fn a_failed_local_crop_fetches_nothing_unless_wanted() {
        let bytes = text(40);
        let f = Fixture::new(bytes.len() as u64, bytes.clone(), bytes.len() as u64);
        let (crop, _) = f.held_and_cropped(3, &[LineSpan { first: 2, count: 5 }]);
        let BlobStatus::Available { path } = f.status() else {
            unreachable!()
        };
        std::fs::write(&path, bytes.to_ascii_uppercase()).unwrap();
        f.queue.local_crops(&[crop]);
        let downloader = tokio::spawn(run_downloader(f.ctx()));
        f.until_crop_ended(crop).await;
        downloader.abort();

        assert_eq!(f.server.calls.load(Ordering::SeqCst), 0, "nothing fetched");
        assert_eq!(
            f.seam.blob(crop.into).unwrap().unwrap().status,
            BlobStatus::NotDownloaded
        );
        let dir = blob_dir(&f.seam.data_dir());
        for id in [crop.into, crop.from] {
            assert!(!dir.join(id.0.to_string()).exists());
            assert!(!part_path(&dir, id).exists());
        }
    }

    fn content(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    /// A blob's disk filling up: its `.part` writes find it full once they
    /// would reach past `room` bytes.
    #[derive(Debug, Clone, Copy)]
    struct FullDisk {
        id: BlobId,
        room: u64,
        /// The first write past `room` returns as if it landed and is
        /// lost, as tokio's background write does; the next one fails.
        late: bool,
    }

    /// The full disks, by blob id. Ids are fresh per test, so tests
    /// running at once never share an entry.
    static FULL_DISK: Mutex<Vec<FullDisk>> = Mutex::new(Vec::new());

    /// Blobs whose progress reports find the replica full (`SQLITE_FULL`)
    /// once they count more than the bytes given; by id, as above.
    static FULL_REPLICA: Mutex<Vec<(BlobId, u64)>> = Mutex::new(Vec::new());

    /// What a write [`write_chunk`] makes does, as [`disk_room`] says.
    #[derive(Debug, PartialEq, Eq)]
    pub(super) enum Write {
        Lands,
        /// Returns as if written; nothing reaches the file.
        Lost,
    }

    /// The fault [`write_chunk`] injects: a write of `id` up to byte `end`
    /// fails as `ENOSPC` does once that blob's disk is full (a late disk
    /// loses that write and fails the next).
    pub(super) fn disk_room(id: BlobId, end: u64) -> std::io::Result<Write> {
        let mut disks = lock(&FULL_DISK);
        let Some(disk) = disks.iter_mut().find(|d| d.id == id && end > d.room) else {
            return Ok(Write::Lands);
        };
        if std::mem::replace(&mut disk.late, false) {
            return Ok(Write::Lost);
        }
        Err(std::io::ErrorKind::StorageFull.into())
    }

    /// The fault a progress report injects: the replica refuses a report
    /// of `got` bytes of `id` as SQLite does on a full disk.
    pub(super) fn replica_room(id: BlobId, got: u64) -> Result<(), CoreError> {
        let full = (lock(&FULL_REPLICA).iter()).any(|&(blob, room)| blob == id && got > room);
        if !full {
            return Ok(());
        }
        let full = rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_FULL);
        Err(crate::StoreError::Sqlite(rusqlite::Error::SqliteFailure(full, None)).into())
    }

    fn fill_disk(id: BlobId, room: u64) {
        lock(&FULL_DISK).push(FullDisk {
            id,
            room,
            late: false,
        });
    }

    fn free_disk(id: BlobId) {
        lock(&FULL_DISK).retain(|disk| disk.id != id);
        lock(&FULL_REPLICA).retain(|&(blob, _)| blob != id);
    }

    impl Fixture {
        /// Neither worker slot would take a blob now: asked directly, the
        /// queue's `next` hands out nothing on its first look.
        async fn takes_nothing(&self) -> bool {
            for slot in [Slot::Any, Slot::Wanted] {
                let first_look = tokio::time::timeout(Duration::ZERO, self.queue.next(slot));
                if first_look.await.is_ok() {
                    return false;
                }
            }
            true
        }
    }

    /// Waits for `done`, polling.
    async fn until(what: &str, done: impl Fn() -> bool) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !done() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("never {what}"));
    }

    impl Fixture {
        /// The queue paused on a full disk, every worker idle.
        fn paused_idle(&self) -> bool {
            let queue = lock(&self.queue.queue);
            queue.disk_full && queue.active.is_empty()
        }

        fn settings(&self) -> crate::SettingsView {
            self.core
                .dispatch(Action::Navigate(crate::Route::Settings))
                .unwrap();
            match self.core.view().screen {
                Screen::Settings(view) => view,
                other => panic!("expected settings, got {other:?}"),
            }
        }
    }

    /// The fault the disk-full rule reads: the OS's `ENOSPC` and quota
    /// errors, nothing else.
    #[cfg(unix)]
    #[test]
    fn only_a_full_disk_or_quota_is_disk_full() {
        let file = |raw| FetchError::File(std::io::Error::from_raw_os_error(raw));
        assert!(file(libc::ENOSPC).is_disk_full());
        assert!(file(libc::EDQUOT).is_disk_full());
        assert!(!file(libc::EACCES).is_disk_full());
        assert!(!file(libc::ENOENT).is_disk_full());
        let size = FetchError::Size {
            expected: 1,
            actual: 2,
        };
        assert!(!size.is_disk_full());
        // The replica's own writes: SQLite's full disk, not its other faults.
        let sqlite = |code| {
            let error = rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), None);
            FetchError::Core(crate::StoreError::Sqlite(error).into())
        };
        assert!(sqlite(rusqlite::ffi::SQLITE_FULL).is_disk_full());
        assert!(!sqlite(rusqlite::ffi::SQLITE_BUSY).is_disk_full());
        assert!(!FetchError::Core(CoreError::Superseded).is_disk_full());
    }

    /// A report the replica refuses as full (`SQLITE_FULL`) pauses like a
    /// full blob file, instead of retrying it forever; the `.part` stays.
    #[tokio::test]
    async fn a_full_replica_pauses_downloads_too() {
        let f = Fixture::new(5000, content(5000), 5000);
        // Two chunks, then the body breaks: the try reports what it got.
        *f.server.break_after.lock().unwrap() = Some(2);
        lock(&FULL_REPLICA).push((f.blob.id, 0));
        f.queue.want(f.blob.id);
        let downloader = tokio::spawn(run_downloader(f.ctx()));
        until("paused", || f.paused_idle()).await;
        assert!(f.takes_nothing().await);
        assert_eq!(f.server.calls.load(Ordering::SeqCst), 1, "not retried");
        let started = BlobStatus::Downloading {
            got: 0,
            path: f.part(),
        };
        assert_eq!(f.status(), started, "only the report that made it");
        assert_eq!(std::fs::metadata(f.part()).unwrap().len(), 2000, "kept");

        free_disk(f.blob.id);
        f.queue.want(f.blob.id);
        until("downloaded", || {
            matches!(f.status(), BlobStatus::Available { .. })
        })
        .await;
        downloader.abort();
    }

    /// A write that failed in the background reports on the next one: the
    /// progress recorded then counts only what the synced file holds, not
    /// the lost chunk the write before had counted.
    #[tokio::test]
    async fn a_late_disk_full_records_only_what_landed() {
        let f = Fixture::new(5000, content(5000), 5000);
        lock(&FULL_DISK).push(FullDisk {
            id: f.blob.id,
            room: 2500,
            late: true,
        });
        f.queue.want(f.blob.id);
        let downloader = tokio::spawn(run_downloader(f.ctx()));
        until("paused", || f.paused_idle()).await;
        downloader.abort();
        let _ = downloader.await;
        // Chunk three "landed", chunk four failed: 3000 counted, 2000 held.
        let kept = BlobStatus::Downloading {
            got: 2000,
            path: f.part(),
        };
        assert_eq!(f.status(), kept);
        assert_eq!(std::fs::metadata(f.part()).unwrap().len(), 2000);

        free_disk(f.blob.id);
        f.queue.want(f.blob.id);
        f.run_until(|status| matches!(status, BlobStatus::Available { .. }))
            .await;
        assert_eq!(*f.server.froms.lock().unwrap(), [0, 2000], "resumed");
    }

    /// A write that finds the disk full keeps what the blob got (`.part`
    /// and recorded progress, `Downloading`, not `Failed`) and pauses every
    /// download, eager ones registered meanwhile too; the views say so.
    /// Asking for a download resumes them all, the blob from what it kept.
    #[tokio::test]
    async fn a_full_disk_keeps_the_download_and_pauses_every_download() {
        let f = Fixture::new(5000, content(5000), 5000);
        fill_disk(f.blob.id, 2500);
        f.queue
            .set_policy(BlobPolicy::Eager, &f.seam.blobs().unwrap());
        let downloader = tokio::spawn(run_downloader(f.ctx()));
        until("paused", || f.paused_idle()).await;

        let kept = BlobStatus::Downloading {
            got: 2000,
            path: f.part(),
        };
        assert_eq!(f.status(), kept, "the two chunks written are recorded");
        assert!(std::fs::metadata(f.part()).unwrap().len() >= 2000);
        assert_eq!(waiting(&f.queue), [(f.blob.id, false)], "at the front");
        assert_eq!(
            f.shown(),
            BlobState::Downloading {
                got: 2000,
                size: 5000,
                paused: Some(Pause::DiskFull),
            }
        );
        assert!(f.settings().blobs_disk_full);

        // Eager blobs registered while paused wait too.
        let later = f.held_blobs(3, 2, Hold::Offline);
        f.queue.registered(&later);
        let queued = [(f.blob.id, false), (later[0], false), (later[1], false)];
        assert_eq!(waiting(&f.queue), queued);
        assert!(f.takes_nothing().await, "no slot takes one while paused");
        assert_eq!(waiting(&f.queue), queued, "and none is taken");
        assert!(lock(&f.queue.queue).active.is_empty());
        assert_eq!(f.server.calls.load(Ordering::SeqCst), 1, "none tried");

        // The user frees space and asks for it.
        free_disk(f.blob.id);
        f.core.dispatch(Action::DownloadBlob(f.blob.id)).unwrap();
        f.queue.want(f.blob.id);
        assert!(!f.settings().blobs_disk_full, "resumed at once");
        until("downloaded, eager blobs tried", || {
            matches!(f.status(), BlobStatus::Available { .. })
                && f.server.calls.load(Ordering::SeqCst) >= 3
        })
        .await;
        downloader.abort();
        assert_eq!(*f.server.froms.lock().unwrap(), [0, 2000], "resumed");
    }

    /// A disk still full when asked again pauses again at the first write,
    /// keeping the same progress; a restart, which forgets the pause,
    /// resumes what the store says was downloading.
    #[tokio::test]
    async fn a_restart_resumes_a_download_paused_on_a_full_disk() {
        let f = Fixture::new(5000, content(5000), 5000);
        fill_disk(f.blob.id, 2500);
        f.queue.want(f.blob.id);
        let downloader = tokio::spawn(run_downloader(f.ctx()));
        until("paused", || f.paused_idle()).await;
        assert_eq!(waiting(&f.queue), [(f.blob.id, true)]);

        f.queue.want(f.blob.id);
        until("tried again", || f.server.calls.load(Ordering::SeqCst) >= 2).await;
        until("paused again", || f.paused_idle()).await;
        downloader.abort();
        let _ = downloader.await;
        let kept = BlobStatus::Downloading {
            got: 2000,
            path: f.part(),
        };
        assert_eq!(f.status(), kept);
        assert_eq!(*f.server.froms.lock().unwrap(), [0, 2000]);

        let f = f.restarted();
        assert!(!lock(&f.queue.queue).disk_full);
        assert!(!f.settings().blobs_disk_full);
        assert_eq!(waiting(&f.queue), [(f.blob.id, true)]);
        free_disk(f.blob.id);
        f.run_until(|status| matches!(status, BlobStatus::Available { .. }))
            .await;
        assert_eq!(*f.server.froms.lock().unwrap(), [0, 2000, 2000]);
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

    /// Local crops go to the front in log order, survive a switch to
    /// `OnDemand` and a request (which keeps their source), and a crop of
    /// a crop waits while the blob it reads is queued.
    #[test]
    fn local_crops_queue_in_order_under_any_policy() {
        let queue = BlobQueue::default();
        let (eager, a, b, c) = (
            ProtoBlobId::new(),
            ProtoBlobId::new(),
            ProtoBlobId::new(),
            ProtoBlobId::new(),
        );
        queue.set_policy(BlobPolicy::Eager, &[]);
        queue.registered(&[eager]);
        let (first, second) = (
            PendingCrop { into: b, from: a },
            PendingCrop { into: c, from: b },
        );
        queue.local_crops(&[first, second]);
        queue.set_policy(BlobPolicy::OnDemand, &[]);
        queue.want(c);
        let q = lock(&queue.queue);
        let waiting: Vec<Queued> = q.waiting.iter().copied().collect();
        assert_eq!(
            waiting,
            [
                Queued {
                    wanted: true,
                    ..Queued::local_crop(second)
                },
                Queued::local_crop(first),
            ]
        );
        assert!(q.waiting.consistent());
        assert!(!q.ready(&waiting[0]), "waits for the crop that makes b");
        assert!(q.ready(&waiting[1]));
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
        let actual = ContentHash::of(&content(5000));
        let reason = format!("blob content hash mismatch: expected {promised}, got {actual}");
        assert_eq!(
            f.status(),
            BlobStatus::Failed {
                reason: reason.clone()
            }
        );
        assert!(!f.part().exists());
        let done = blob_dir(&f.seam.data_dir()).join(f.blob.id.0.to_string());
        assert!(!done.exists(), "never renamed");
        assert_eq!(f.shown(), BlobState::Failed { size: 5000, reason });
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
        assert!(matches!(f.status(), BlobStatus::Failed { .. }));
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

    /// The reason a blob of the fixture's fails with: the server has more
    /// than the event promised.
    const TOO_BIG: &str = "blob size mismatch: expected 5000 bytes, the server has 6000";

    /// A fixture whose blob was asked for and given up on at the first try.
    async fn failed_fixture() -> Fixture {
        let f = Fixture::new(5000, content(6000), 6000);
        f.queue.want(f.blob.id);
        f.run_until(|_| f.server.calls.load(Ordering::SeqCst) >= 1)
            .await;
        let failed = BlobStatus::Failed {
            reason: TOO_BIG.into(),
        };
        assert_eq!(f.status(), failed);
        f
    }

    /// A blob given up on stays `Failed`, with its reason, over a restart,
    /// and `Eager` does not queue it again, on startup or when switched
    /// to; a blob that never failed is queued as ever.
    #[tokio::test]
    async fn a_failed_blob_stays_failed_over_a_restart() {
        let f = failed_fixture().await;
        let [never_tried] = f.held_blobs(3, 1, Hold::Offline)[..] else {
            unreachable!()
        };
        f.core
            .dispatch(Action::SetBlobPolicy(BlobPolicy::Eager))
            .unwrap();

        let f = f.restarted();
        let failed = BlobStatus::Failed {
            reason: TOO_BIG.into(),
        };
        assert_eq!(f.status(), failed);
        assert_eq!(waiting(&f.queue), [(never_tried, false)]);
        f.queue
            .set_policy(BlobPolicy::Eager, &f.seam.blobs().unwrap());
        assert_eq!(waiting(&f.queue), [(never_tried, false)], "nor switched to");
        assert_eq!(
            f.server.calls.load(Ordering::SeqCst),
            1,
            "never tried again"
        );
    }

    /// Only asking again tries a blob that failed before a restart: it is
    /// fetched once more, and fails again for the same reason.
    #[tokio::test]
    async fn asking_for_a_failed_blob_after_a_restart_tries_it_again() {
        let f = failed_fixture().await.restarted();
        assert!(waiting(&f.queue).is_empty(), "nothing queued it");
        assert_eq!(
            f.shown(),
            BlobState::Failed {
                size: 5000,
                reason: TOO_BIG.into()
            },
            "and it still says why"
        );

        f.core.dispatch(Action::DownloadBlob(f.blob.id)).unwrap();
        assert_eq!(f.shown(), BlobState::Requested { size: 5000 });
        // The engine's part: the request reaches the queue.
        f.queue.want(f.blob.id);
        f.run_until(|_| f.server.calls.load(Ordering::SeqCst) >= 2)
            .await;
        assert_eq!(*f.server.froms.lock().unwrap(), [0, 0]);
        assert_eq!(
            f.status(),
            BlobStatus::Failed {
                reason: TOO_BIG.into()
            }
        );
    }

    #[tokio::test]
    async fn a_body_longer_than_the_blob_is_refused() {
        // The server claims the right size but sends more bytes.
        let f = Fixture::new(5000, content(7000), 5000);
        f.queue.want(f.blob.id);
        f.run_until(|_| f.server.calls.load(Ordering::SeqCst) >= 1)
            .await;
        assert!(matches!(f.status(), BlobStatus::Failed { .. }));
        assert!(!f.part().exists());
    }

    /// An empty blob (the server makes them, e.g. a conflicted file's diff)
    /// is whole before any byte arrives: it settles without a request,
    /// through the same size and hash check, as an empty file.
    #[tokio::test]
    async fn an_empty_blob_settles_available_without_a_request() {
        let f = Fixture::new(0, Vec::new(), 0);
        f.queue.want(f.blob.id);
        f.run_until(|s| matches!(s, BlobStatus::Available { .. }))
            .await;
        assert_eq!(f.server.calls.load(Ordering::SeqCst), 0);
        let BlobStatus::Available { path } = f.status() else {
            unreachable!()
        };
        assert_eq!(std::fs::read(&path).unwrap(), b"");
        assert!(!f.part().exists());
        assert_eq!(f.shown(), BlobState::Available { path });
    }

    /// The empty blob still goes through the hash check: one promising
    /// other content fails rather than settling.
    #[tokio::test]
    async fn an_empty_blob_promising_other_content_fails() {
        let promised = ContentHash::of(b"other content");
        let f = Fixture::hashed(0, promised, Vec::new(), 0);
        f.queue.want(f.blob.id);
        f.run_until(|_| matches!(f.shown(), BlobState::Failed { .. }))
            .await;
        let actual = ContentHash::of(b"");
        let reason = format!("blob content hash mismatch: expected {promised}, got {actual}");
        assert_eq!(
            f.status(),
            BlobStatus::Failed {
                reason: reason.clone()
            }
        );
        assert!(!f.part().exists());
        assert_eq!(f.shown(), BlobState::Failed { size: 0, reason });
        assert_eq!(f.server.calls.load(Ordering::SeqCst), 0);
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
        assert!(waiting(&f.queue).is_empty());
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
        let failed = record(BlobStatus::Failed {
            reason: "gone".into(),
        });

        // OnDemand: registered blobs wait for the user.
        queue.registered(&[a.id]);
        assert!(waiting(&queue).is_empty());
        queue.want(a.id);
        assert_eq!(waiting(&queue), [(a.id, true)]);

        // Eager: everything neither available nor failed, newest first, no
        // duplicates; a user request jumps the queue.
        queue.set_policy(
            BlobPolicy::Eager,
            &[a.clone(), b.clone(), c.clone(), failed.clone()],
        );
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
        let queue = lock(&queue.queue);
        assert!(queue.waiting.consistent(), "order and ids agree");
        queue.waiting.iter().map(|q| (q.id, q.wanted)).collect()
    }

    fn stops(queue: &BlobQueue) -> usize {
        queue.stops.load(Ordering::Relaxed)
    }

    /// A first sync's worth of blobs: registering them (twice), switching
    /// to `Eager` over them, purging half and switching to `OnDemand` each
    /// check membership per blob, which must not scan the queue (50k
    /// blobs scanned per blob would be billions of comparisons, minutes in
    /// a debug build). The switch to `OnDemand` stops only the one blob a
    /// worker took, not every blob it drops.
    #[tokio::test]
    async fn a_large_queue_registers_switches_and_purges_without_scanning() {
        const COUNT: usize = 50_000;
        let queue = BlobQueue::default();
        queue.set_policy(BlobPolicy::Eager, &[]);
        let ids: Vec<BlobId> = (0..COUNT).map(|_| ProtoBlobId::new()).collect();
        queue.registered(&ids);
        queue.registered(&ids);
        let records: Vec<BlobRecord> = (ids.iter())
            .map(|&id| BlobRecord {
                id,
                size: 1,
                sha256: ContentHash::of(b""),
                status: BlobStatus::NotDownloaded,
                workdir: false,
            })
            .collect();
        queue.set_policy(BlobPolicy::Eager, &records);
        let ids_waiting: Vec<BlobId> = waiting(&queue).into_iter().map(|(id, _)| id).collect();
        assert_eq!(ids_waiting, ids, "registration order, no duplicates");

        // A worker takes the first and lets it go unfinished: it waits at
        // the front again, started.
        let claim = ready_now(queue.next(Slot::Any)).await.expect("queued");
        assert_eq!(claim.item.id, ids[0]);
        drop(claim);
        assert_eq!(waiting(&queue).len(), COUNT);

        let (gone, kept) = ids.split_at(COUNT / 2);
        queue.purged(&gone[1..]);
        let left: Vec<BlobId> = waiting(&queue).into_iter().map(|(id, _)| id).collect();
        assert_eq!(left[0], ids[0]);
        assert_eq!(left[1..], *kept);

        let before = stops(&queue);
        queue.set_policy(BlobPolicy::OnDemand, &records);
        assert!(waiting(&queue).is_empty());
        assert_eq!(stops(&queue) - before, 1, "only the started blob");
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
                                output: Some(Full::Blob(event_blob.clone())),
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

    /// Startup removes the blob files no row owns, by the row's status,
    /// workdir blobs included, and leaves names that are no blob file's.
    #[test]
    fn restore_sweeps_the_blob_files_no_row_owns() {
        use crate::store::Store;
        use crate::workdir::{WorkdirAnswer, WorkdirAsk};
        use skywave_proto::{FileBody, FileResult};

        let dir = tempfile::tempdir().unwrap();
        let blobs = blob_dir(dir.path());
        let session_id = SessionId::new();
        let blob = |size| BlobRef {
            id: ProtoBlobId::new(),
            size,
            sha256: ContentHash::of(b""),
        };
        let [available, downloading, stopped, failed, not_downloaded] = [1, 2, 3, 4, 5].map(blob);
        let workdir = blob(6);
        let orphan = blob(7);
        let done = |b: &BlobRef| blobs.join(b.id.0.to_string());
        let part = |b: &BlobRef| part_path(&blobs, b.id);
        {
            let mut store = Store::open(dir.path()).unwrap();
            let event = |kind: EventKind| Event {
                session_id,
                at: Utc::now(),
                command_id: None,
                context: skywave_proto::ContextId::first_of(session_id),
                kind,
            };
            let mut events = vec![event(EventKind::SessionCreated {
                workdir: "/srv".into(),
                context_name: "start".into(),
                from: None,
            })];
            for b in [&available, &downloading, &stopped, &failed, &not_downloaded] {
                events.push(event(EventKind::ToolResult(ToolResult {
                    call: None,
                    is_error: false,
                    status: None,
                    preview: "big".into(),
                    lines: 1,
                    output: Some(Full::Blob(b.clone())),
                })));
            }
            store
                .apply_batch(
                    &EventBatch {
                        from: Seq(1),
                        events,
                    },
                    Utc::now(),
                )
                .unwrap();
            let file = FileResult::File {
                at: Utc::now(),
                path: "a.txt".into(),
                size: 6,
                content: FileBody::Blob(workdir.clone()),
            };
            let ask = WorkdirAsk::File {
                session: session_id,
                path: "a.txt".into(),
            };
            store
                .put_workdir(&ask, &WorkdirAnswer::File(file))
                .unwrap()
                .unwrap();
            for (b, status) in [
                (
                    &available,
                    BlobStatus::Available {
                        path: done(&available),
                    },
                ),
                (
                    &downloading,
                    BlobStatus::Downloading {
                        got: 1,
                        path: part(&downloading),
                    },
                ),
                (
                    &stopped,
                    BlobStatus::Stopped {
                        got: 1,
                        path: part(&stopped),
                    },
                ),
                (
                    &failed,
                    BlobStatus::Failed {
                        reason: "gone".into(),
                    },
                ),
                (
                    &workdir,
                    BlobStatus::Available {
                        path: done(&workdir),
                    },
                ),
            ] {
                store.set_blob_status(b.id, &status).unwrap();
            }
        }
        std::fs::create_dir_all(&blobs).unwrap();
        let every = [
            &available,
            &downloading,
            &stopped,
            &failed,
            &not_downloaded,
            &workdir,
            &orphan,
        ];
        for b in every {
            std::fs::write(done(b), b"x").unwrap();
            std::fs::write(part(b), b"x").unwrap();
        }
        let others = [
            blobs.join("notes.txt"),
            blobs.join(orphan.id.0.to_string().to_uppercase()),
            blobs.join(orphan.id.0.simple().to_string()),
            blobs.join(format!("{}.part.tmp", orphan.id.0)),
        ];
        for path in &others {
            std::fs::write(path, b"x").unwrap();
        }
        // A directory under a blob's name is no blob file either.
        let subdir = blobs.join(ProtoBlobId::new().0.to_string());
        std::fs::create_dir(&subdir).unwrap();

        let core = Core::open_detached(dir.path()).unwrap();
        let _queue = BlobQueue::restore(&core.seam());

        let kept = |path: &PathBuf| path.exists();
        let expect = [
            (done(&available), true),
            (part(&available), false),
            (done(&downloading), false),
            (part(&downloading), true),
            (done(&stopped), false),
            (part(&stopped), true),
            (done(&failed), false),
            (part(&failed), false),
            (done(&not_downloaded), false),
            (part(&not_downloaded), false),
            (done(&workdir), true),
            (part(&workdir), false),
            (done(&orphan), false),
            (part(&orphan), false),
        ];
        for (path, want) in &expect {
            assert_eq!(kept(path), *want, "{}", path.display());
        }
        for path in &others {
            assert!(kept(path), "{} is no blob file", path.display());
        }
        assert!(subdir.is_dir());
        // The statuses the sweep read are untouched.
        assert!(matches!(
            core.seam().blob(stopped.id).unwrap().unwrap().status,
            BlobStatus::Stopped { got: 1, .. }
        ));
    }

    #[test]
    fn a_missing_blob_directory_sweeps_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(sweep_blob_files(dir.path(), &[]), 0);
        assert!(!blob_dir(dir.path()).exists());
    }

    impl Fixture {
        /// The blob is `Stopped` and no worker holds it.
        fn stopped_idle(&self) -> bool {
            matches!(self.status(), BlobStatus::Stopped { .. })
                && lock(&self.queue.queue).active.is_empty()
        }

        /// Switches the queue to `OnDemand` before the first response's
        /// second chunk, as the engine does on the setting.
        fn on_demand_mid_body(&self) {
            let queue = self.queue.clone();
            *self.server.mid_body.lock().unwrap() = Some(Box::new(move || {
                queue.set_policy(BlobPolicy::OnDemand, &[])
            }));
        }
    }

    /// An eager download in flight when the policy turns `OnDemand` stops
    /// at its next chunk, keeping what it got (`.part` and progress,
    /// `Stopped`), and shows as not downloaded. A restart under `OnDemand`
    /// leaves it; asking for it resumes from the kept bytes.
    #[tokio::test]
    async fn an_eager_download_in_flight_stops_when_the_policy_turns_on_demand() {
        let bytes = content(5000);
        let f = Fixture::new(5000, bytes.clone(), 5000);
        f.queue
            .set_policy(BlobPolicy::Eager, &f.seam.blobs().unwrap());
        f.on_demand_mid_body();
        let downloader = tokio::spawn(run_downloader(f.ctx()));
        until("stopped", || f.stopped_idle()).await;
        let stopped = BlobStatus::Stopped {
            got: 2000,
            path: f.part(),
        };
        assert_eq!(f.status(), stopped, "the two chunks it took are kept");
        assert_eq!(std::fs::metadata(f.part()).unwrap().len(), 2000);
        assert!(waiting(&f.queue).is_empty(), "not queued again");
        assert!(f.takes_nothing().await);
        assert_eq!(f.shown(), BlobState::NotDownloaded { size: 5000 });
        downloader.abort();
        let _ = downloader.await;

        // The store keeps `OnDemand`: a restart resumes nothing.
        let f = f.restarted();
        assert_eq!(f.status(), stopped);
        assert!(waiting(&f.queue).is_empty());
        assert!(f.takes_nothing().await);

        f.queue.want(f.blob.id);
        f.run_until(|status| matches!(status, BlobStatus::Available { .. }))
            .await;
        assert_eq!(*f.server.froms.lock().unwrap(), [0, 2000], "resumed");
        let BlobStatus::Available { path } = f.status() else {
            unreachable!()
        };
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }

    /// Positive control: a download the user asked for keeps going when
    /// the policy turns `OnDemand` mid-body.
    #[tokio::test]
    async fn a_download_asked_for_keeps_going_when_the_policy_turns_on_demand() {
        let f = Fixture::new(5000, content(5000), 5000);
        f.queue
            .set_policy(BlobPolicy::Eager, &f.seam.blobs().unwrap());
        f.queue.want(f.blob.id);
        f.on_demand_mid_body();
        f.run_until(|status| matches!(status, BlobStatus::Available { .. }))
            .await;
        assert_eq!(*f.server.froms.lock().unwrap(), [0], "one try, whole");
    }

    /// An eager worker retrying a blob while the link is down lets it go
    /// once the policy turns `OnDemand`, keeping the bytes an earlier try
    /// got; asking for it resumes from them.
    #[tokio::test]
    async fn an_eager_worker_retrying_offline_drops_its_blob_on_demand() {
        let f = Fixture::new(5000, content(5000), 5000);
        // The first try gets two chunks, then the link goes down.
        *f.server.break_after.lock().unwrap() = Some(2);
        let server = f.server.clone();
        let id = f.blob.id;
        *f.server.mid_body.lock().unwrap() = Some(Box::new(move || {
            server.held.lock().unwrap().insert(id, Hold::Offline);
        }));
        f.queue
            .set_policy(BlobPolicy::Eager, &f.seam.blobs().unwrap());
        let downloader = tokio::spawn(run_downloader(f.ctx()));
        until("retrying offline", || {
            f.server.calls.load(Ordering::SeqCst) >= 3
        })
        .await;
        assert!(lock(&f.queue.queue).active.contains(&id), "still held");

        f.queue
            .set_policy(BlobPolicy::OnDemand, &f.seam.blobs().unwrap());
        until("stopped", || f.stopped_idle()).await;
        assert_eq!(
            f.status(),
            BlobStatus::Stopped {
                got: 2000,
                path: f.part()
            }
        );
        assert!(waiting(&f.queue).is_empty());
        assert!(f.takes_nothing().await);
        downloader.abort();
        let _ = downloader.await;

        f.server.held.lock().unwrap().clear();
        f.queue.want(id);
        f.run_until(|status| matches!(status, BlobStatus::Available { .. }))
            .await;
        assert_eq!(*f.server.froms.lock().unwrap(), [0, 2000], "resumed");
    }

    /// The backoff itself: under `OnDemand`, an eager worker lets a blob
    /// nobody asked for go at once and for good; one asked for it keeps
    /// (the Wanted worker busy) and waits out its backoff.
    #[tokio::test(start_paused = true)]
    async fn backing_off_under_on_demand_drops_only_a_blob_nobody_asked_for() {
        let queue = BlobQueue::default();
        let (busy, eager, asked) = (ProtoBlobId::new(), ProtoBlobId::new(), ProtoBlobId::new());
        let hour = Duration::from_secs(3600);
        queue.want(busy);
        let _busy = ready_now(queue.next(Slot::Wanted))
            .await
            .expect("asked for");
        queue.set_policy(BlobPolicy::Eager, &[]);
        queue.registered(&[eager, asked]);
        let claim = ready_now(queue.next(Slot::Any)).await.expect("queued");
        let asked_claim = ready_now(queue.next(Slot::Any)).await.expect("queued");
        assert_eq!((claim.item.id, asked_claim.item.id), (eager, asked));
        queue.want(asked);
        queue.set_policy(BlobPolicy::OnDemand, &[]);

        let start = tokio::time::Instant::now();
        assert!(queue.back_off(claim, hour).await.is_none(), "dropped");
        assert_eq!(start.elapsed(), Duration::ZERO);
        assert!(!lock(&queue.queue).holds(eager), "for good");

        let start = tokio::time::Instant::now();
        assert!(queue.back_off(asked_claim, hour).await.is_some(), "kept");
        assert_eq!(start.elapsed(), hour);
    }

    /// A waiting blob the switch to `OnDemand` drops is `Stopped` if its
    /// download had started (here: let go when a downloader stopped under
    /// `Eager`); a restart under `OnDemand` leaves it, `Eager` queues it.
    #[tokio::test]
    async fn a_waiting_started_download_dropped_on_demand_is_stopped() {
        let f = Fixture::new(5000, content(5000), 5000);
        let started = BlobStatus::Downloading {
            got: 1000,
            path: f.part(),
        };
        f.seam.blob_status(f.blob.id, started).unwrap();
        f.queue
            .set_policy(BlobPolicy::Eager, &f.seam.blobs().unwrap());
        assert_eq!(waiting(&f.queue), [(f.blob.id, false)]);

        f.queue
            .set_policy(BlobPolicy::OnDemand, &f.seam.blobs().unwrap());
        assert!(waiting(&f.queue).is_empty());
        let stopped = BlobStatus::Stopped {
            got: 1000,
            path: f.part(),
        };
        assert_eq!(f.status(), stopped);
        assert_eq!(f.shown(), BlobState::NotDownloaded { size: 5000 });
        // Asked for, it shows requested until a worker reports.
        f.core.dispatch(Action::DownloadBlob(f.blob.id)).unwrap();
        assert_eq!(f.shown(), BlobState::Requested { size: 5000 });

        let f = f.restarted();
        assert!(waiting(&f.queue).is_empty(), "not resumed under OnDemand");
        f.queue
            .set_policy(BlobPolicy::Eager, &f.seam.blobs().unwrap());
        assert_eq!(waiting(&f.queue), [(f.blob.id, false)], "Eager takes it");
    }

    /// The engine reads the blobs before the queue's lock: a blob whose
    /// worker recorded progress and let it go after that read (a yield to
    /// a crop, a full disk) is still stopped, as is every dropped blob a
    /// worker took when the read failed and the list is empty.
    #[tokio::test]
    async fn the_switch_to_on_demand_stops_what_a_stale_snapshot_missed() {
        for empty in [false, true] {
            let f = Fixture::new(5000, content(5000), 5000);
            let before = f.seam.blobs().unwrap();
            f.queue.set_policy(BlobPolicy::Eager, &before);
            assert_eq!(waiting(&f.queue), [(f.blob.id, false)]);
            // A worker takes it, records progress and lets it go, unfinished:
            // under `Eager` it waits at the front again.
            let claim = ready_now(f.queue.next(Slot::Any)).await.expect("queued");
            let started = BlobStatus::Downloading {
                got: 1000,
                path: f.part(),
            };
            f.seam.blob_status(f.blob.id, started).unwrap();
            drop(claim);
            assert_eq!(waiting(&f.queue), [(f.blob.id, false)]);

            let snapshot: &[BlobRecord] = if empty { &[] } else { &before };
            assert_eq!(
                snapshot.first().map(|b| &b.status),
                (!empty).then_some(&BlobStatus::NotDownloaded),
                "stale"
            );
            f.queue.set_policy(BlobPolicy::OnDemand, snapshot);
            assert!(waiting(&f.queue).is_empty());
            assert_eq!(
                f.status(),
                BlobStatus::Stopped {
                    got: 1000,
                    path: f.part()
                },
                "empty snapshot: {empty}"
            );
        }
    }

    /// A restart under `Eager` resumes an event blob's download as a
    /// background one, which a switch to `OnDemand` then stops, keeping
    /// its bytes; under `OnDemand` it resumes as asked for and is kept.
    #[tokio::test]
    async fn a_download_resumed_under_eager_stops_on_demand() {
        for policy in [BlobPolicy::Eager, BlobPolicy::OnDemand] {
            let f = Fixture::new(5000, content(5000), 5000);
            f.core.dispatch(Action::SetBlobPolicy(policy)).unwrap();
            let started = BlobStatus::Downloading {
                got: 1000,
                path: f.part(),
            };
            f.seam.blob_status(f.blob.id, started.clone()).unwrap();

            let f = f.restarted();
            let asked = policy == BlobPolicy::OnDemand;
            assert_eq!(waiting(&f.queue), [(f.blob.id, asked)], "{policy:?}");
            f.queue
                .set_policy(BlobPolicy::OnDemand, &f.seam.blobs().unwrap());
            if asked {
                assert_eq!(waiting(&f.queue), [(f.blob.id, true)], "kept");
                assert_eq!(f.status(), started);
            } else {
                assert!(waiting(&f.queue).is_empty(), "dropped");
                let stopped = BlobStatus::Stopped {
                    got: 1000,
                    path: f.part(),
                };
                assert_eq!(f.status(), stopped);
            }
        }
    }
}
