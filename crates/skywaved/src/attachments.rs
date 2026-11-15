//! Staged uploads: files a device uploads before the `SendMessage` that
//! names them by sha256 (`PUT` and `HEAD /v1/attachments/{sha256}`).
//!
//! A staged upload is the file `<data dir>/attachments/<sha256 hex>`, of any
//! type. There is no table: the hash is the name, and a file already there is
//! trusted only while it still hashes to its name. A message that names one as
//! an image sniffs it then. The bytes go to the partial `.part-<sha256 hex>` in
//! the same directory, which is synced and renamed into place once it hashes
//! to its name, so a name never holds a partial file. An interrupted upload's
//! partial is kept, so the next `PUT` resumes it at [`Staging::resume`].
//!
//! Staging is a cache. [`Staging::finish`] keeps it under
//! [`STAGING_MAX_BYTES`] by evicting the oldest files, and [`Staging::sweep`]
//! removes files older than [`STAGING_MAX_AGE`] at startup, along with the
//! temp files a server before v37 left behind (staging writes only partials
//! now). Eviction is garbage collection, not an outcome: a message naming an
//! image that is no longer staged is refused as `UnknownAttachment` when its
//! command arrives.
//!
//! A `SendMessage` naming staged images takes each as a new blob: [`prepare`]
//! hard-links every one into the blob store before the command's transaction
//! ([`Prepared`]), and the transaction records those blobs. The staged files
//! are removed once it commits; a refused message keeps them, so a resend
//! needs no upload.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rusqlite::Connection;
use skywave_proto::{Attachment, ContentHash, ImageType, RejectReason, UploadResult, limits};

use crate::blobs::{BlobMeta, BlobStore};
use crate::error::{Error, IoContext, Result};

/// Most bytes the staged files may total; past it the oldest are evicted.
pub const STAGING_MAX_BYTES: u64 = 256 * 1024 * 1024;
/// Staged files last modified longer ago than this are removed at startup.
pub const STAGING_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Prefix of the temp files a server before v37 wrote uploads in; never a
/// staged image. Nothing writes them since v37: the startup sweep removes them.
const TEMP_PREFIX: &str = ".tmp-";
/// Prefix of the file an interrupted upload left, followed by its sha256 hex;
/// never a staged file.
const PARTIAL_PREFIX: &str = ".part-";
/// The bytes [`ImageType::sniff`] needs to tell all four types apart.
const SNIFF_LEN: u64 = 12;

/// The directory of staged uploads.
#[derive(Debug, Clone)]
pub struct Staging {
    dir: PathBuf,
}

impl Staging {
    /// A store over an existing directory (see [`crate::data_dir::DataDir`]).
    pub fn new(dir: impl Into<PathBuf>) -> Staging {
        Staging { dir: dir.into() }
    }

    /// Where the image with `sha256` is, or would be, staged.
    pub fn path(&self, sha256: ContentHash) -> PathBuf {
        self.dir.join(sha256.to_hex())
    }

    /// Whether an image with `sha256` is staged.
    pub fn contains(&self, sha256: ContentHash) -> bool {
        self.path(sha256).is_file()
    }

    /// Stages `bytes` under `sha256` as one complete upload, and answers what
    /// the upload route answers, as a `PUT` at offset 0 would: a file already
    /// staged is answered with nothing written; otherwise the partial
    /// restarts, is written and is finished. Takes no upload lock: a caller
    /// must not race a `PUT` of the same hash (tests and tools only). Nothing
    /// is left behind on any refusal.
    pub fn stage(&self, sha256: ContentHash, bytes: &[u8]) -> Result<UploadResult> {
        let mut file = match self.resume(sha256, 0)? {
            Resume::Append(file) => file,
            Resume::Staged(answer) => return Ok(answer),
            Resume::Mismatch { .. } => {
                return Err(io::Error::other("a restart at offset 0 was refused"))
                    .context(|| format!("staging in {}", self.dir.display()));
            }
        };
        let written = file
            .write_all(bytes)
            .context(|| format!("staging in {}", self.dir.display()));
        drop(file);
        if let Err(e) = written {
            let _ = self.drop_partial(sha256);
            return Err(e);
        }
        self.finish(sha256)
    }

    /// Removes the oldest staged files, by modification time, until they
    /// total at most `cap` bytes; returns how many were removed. Temp files
    /// (left by a server before v37) and partials are not staged files: they
    /// are neither removed nor counted.
    pub fn evict(&self, cap: u64) -> Result<usize> {
        let mut staged: Vec<Entry> = self
            .files()?
            .into_iter()
            .filter(|f| !f.temp && !f.partial)
            .collect();
        let mut total: u64 = staged.iter().map(|f| f.size).sum();
        staged.sort_by(|a, b| (a.modified, &a.path).cmp(&(b.modified, &b.path)));
        let mut removed = 0;
        for file in staged {
            if total <= cap {
                break;
            }
            if remove(&file.path)? {
                removed += 1;
            }
            total -= file.size;
        }
        Ok(removed)
    }

    /// Removes the temp files a server before v37 left, and the files last
    /// modified more than `max_age` ago; returns how many were removed. A
    /// partial is kept until it is that old, so an upload resumes across a
    /// restart. A missing directory has nothing to remove. Startup only: a
    /// temp file is removed whatever its age, so nothing may be writing one
    /// meanwhile (no server since v37 writes any).
    pub fn sweep(&self, max_age: Duration) -> Result<usize> {
        let now = SystemTime::now();
        let mut removed = 0;
        for file in self.files()? {
            let old = now
                .duration_since(file.modified)
                .is_ok_and(|age| age > max_age);
            if (file.temp || old) && remove(&file.path)? {
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// The answer for a file already staged at `path` under `sha256`, if
    /// there is one. A file there that no longer hashes to its name is
    /// removed: it is not the upload, so the upload goes on as a new one.
    fn existing(&self, path: &Path, sha256: ContentHash) -> Result<Option<UploadResult>> {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).context(|| format!("opening {}", path.display())),
        };
        let size = file
            .metadata()
            .context(|| format!("reading {}", path.display()))?
            .len();
        let hash = ContentHash::read(file).context(|| format!("reading {}", path.display()))?;
        if hash == sha256 {
            return Ok(Some(UploadResult::Stored { size }));
        }
        remove(path)?;
        Ok(None)
    }

    /// Every regular file in the directory, temp files included. A missing
    /// directory has none.
    fn files(&self) -> Result<Vec<Entry>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).context(|| format!("listing {}", self.dir.display())),
        };
        let mut files = Vec::new();
        for entry in entries {
            let entry = entry.context(|| format!("listing {}", self.dir.display()))?;
            let path = entry.path();
            let meta = match entry.metadata() {
                Ok(meta) => meta,
                // Removed since it was listed.
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e).context(|| format!("reading {}", path.display())),
            };
            if !meta.is_file() {
                continue;
            }
            let modified = meta
                .modified()
                .context(|| format!("reading {}", path.display()))?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            files.push(Entry {
                temp: name.starts_with(TEMP_PREFIX),
                partial: name.starts_with(PARTIAL_PREFIX),
                path,
                size: meta.len(),
                modified,
            });
        }
        Ok(files)
    }

    /// Where an interrupted upload of `sha256` is kept until it is complete.
    pub fn partial(&self, sha256: ContentHash) -> PathBuf {
        self.dir
            .join(format!("{PARTIAL_PREFIX}{}", sha256.to_hex()))
    }

    /// The length of the partial of `sha256`; 0 when there is none.
    pub fn partial_len(&self, sha256: ContentHash) -> Result<u64> {
        let path = self.partial(sha256);
        match fs::metadata(&path) {
            Ok(meta) => Ok(meta.len()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(e).context(|| format!("reading {}", path.display())),
        }
    }

    /// Opens the partial of `sha256` for a `PUT` whose body starts at
    /// `offset`. A file already staged under `sha256` comes first, at any
    /// offset (a `HEAD` and a `PUT` raced, or a re-send): it is a
    /// [`Resume::Staged`] with its answer, nothing is written, and a leftover
    /// partial is removed. Otherwise, at the partial's length, the file opens
    /// for appending. At 0, the partial restarts empty and the file opens for
    /// writing from there. Any other offset is a [`Resume::Mismatch`]: nothing
    /// is opened or changed.
    pub fn resume(&self, sha256: ContentHash, offset: u64) -> Result<Resume> {
        if let Some(stored) = self.existing(&self.path(sha256), sha256)? {
            remove(&self.partial(sha256))?;
            return Ok(Resume::Staged(stored));
        }
        let have = self.partial_len(sha256)?;
        let path = self.partial(sha256);
        if offset == have {
            let file = File::options()
                .create(true)
                .append(true)
                .open(&path)
                .context(|| format!("opening {}", path.display()))?;
            return Ok(Resume::Append(file));
        }
        if offset == 0 {
            let file = File::options()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&path)
                .context(|| format!("opening {}", path.display()))?;
            return Ok(Resume::Append(file));
        }
        Ok(Resume::Mismatch { have })
    }

    /// Stages the complete partial of `sha256` and answers what the upload
    /// route answers. A file already staged under `sha256` is answered as
    /// stored. Otherwise the partial must hash to `sha256`; it is then synced
    /// and renamed to its name, and the oldest files evicted down to
    /// [`STAGING_MAX_BYTES`]. The partial is gone after any answer.
    pub fn finish(&self, sha256: ContentHash) -> Result<UploadResult> {
        let path = self.path(sha256);
        let partial = self.partial(sha256);
        if let Some(stored) = self.existing(&path, sha256)? {
            remove(&partial)?;
            return Ok(stored);
        }
        let file = File::open(&partial).context(|| format!("opening {}", partial.display()))?;
        let size = file
            .metadata()
            .context(|| format!("reading {}", partial.display()))?
            .len();
        let hash = ContentHash::read(&file).context(|| format!("reading {}", partial.display()))?;
        if hash != sha256 {
            remove(&partial)?;
            return Ok(UploadResult::HashMismatch);
        }
        // Staging is ordered by modification time, and a partial appended to
        // long ago must not look old to eviction or the sweep.
        file.set_modified(SystemTime::now())
            .context(|| format!("stamping {}", partial.display()))?;
        file.sync_all()
            .context(|| format!("syncing {}", partial.display()))?;
        fs::rename(&partial, &path).context(|| format!("staging {}", path.display()))?;
        if let Err(e) = File::open(&self.dir).and_then(|dir| dir.sync_all()) {
            let _ = fs::remove_file(&path);
            return Err(e).context(|| format!("staging {}", path.display()));
        }
        self.evict(STAGING_MAX_BYTES)?;
        Ok(UploadResult::Stored { size })
    }

    /// Removes the partial of `sha256`, if there is one: for an upload refused
    /// as too large or for a low disk.
    pub fn drop_partial(&self, sha256: ContentHash) -> Result<()> {
        remove(&self.partial(sha256))?;
        Ok(())
    }
}

/// Where a `PUT` of an upload starts writing, from [`Staging::resume`].
#[derive(Debug)]
pub enum Resume {
    /// Write the body to this file, open for appending at the offset the
    /// `PUT` named.
    Append(File),
    /// The `PUT` named another offset than the `have` bytes held; nothing
    /// was opened or changed.
    Mismatch { have: u64 },
    /// The image was staged meanwhile, so the upload is done: the answer it
    /// was finished with, and nothing is written.
    Staged(UploadResult),
}

/// A `SendMessage`'s images, linked into the blob store before its
/// transaction ([`prepare`]). It owns the blob files it linked: dropped
/// before [`Prepared::kept`], they are removed.
#[derive(Debug)]
pub struct Prepared {
    /// Per hash, in order: its new blob and sniffed type, or `None` if it is
    /// not a staged image. Empty when nothing was linked.
    links: Vec<Option<(BlobMeta, ImageType)>>,
    /// The hashes the message names, in order.
    staged: Vec<ContentHash>,
    disk_low: bool,
    blobs: BlobStore,
    staging: Staging,
    /// Set once the blobs are recorded, in the transaction that commits them.
    logged: bool,
}

/// Links each staged image `hashes` names into `blobs` as a new blob, not
/// recorded. Links nothing while `disk_low`, or when `hashes` names more than
/// [`limits::ATTACHMENTS_MAX`]: [`Prepared::decide`] refuses either before a
/// link is needed. Blocking: call it off the async runtime.
pub fn prepare(
    staging: &Staging,
    blobs: &BlobStore,
    disk_low: bool,
    hashes: &[ContentHash],
) -> Result<Prepared> {
    let mut prepared = Prepared {
        links: Vec::new(),
        staged: hashes.to_vec(),
        disk_low,
        blobs: blobs.clone(),
        staging: staging.clone(),
        logged: false,
    };
    if disk_low || hashes.len() > limits::ATTACHMENTS_MAX {
        return Ok(prepared);
    }
    for &hash in hashes {
        let link = prepared.link(hash)?;
        prepared.links.push(link);
    }
    Ok(prepared)
}

impl Prepared {
    /// Links the staged image `hash` as a new blob. `None` when it is not
    /// staged or is no longer an image: the message then names an image it
    /// does not have. A staged file that no longer hashes to its name is
    /// removed as well, so the device's next `HEAD` misses and uploads it
    /// again; `HEAD` checks only that the file exists, and would otherwise
    /// keep answering for a file that is not the upload.
    fn link(&self, hash: ContentHash) -> Result<Option<(BlobMeta, ImageType)>> {
        let path = self.staging.path(hash);
        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).context(|| format!("opening {}", path.display())),
        };
        let mut head = Vec::new();
        Read::take(&mut file, SNIFF_LEN)
            .read_to_end(&mut head)
            .context(|| format!("reading {}", path.display()))?;
        let Some(media_type) = ImageType::sniff(&head) else {
            return Ok(None);
        };
        let meta = match self.blobs.link(&path) {
            Ok(meta) => meta,
            // Evicted since it was opened.
            Err(Error::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        if meta.sha256 != hash {
            self.blobs.discard(meta.id);
            remove(&path)?;
            return Ok(None);
        }
        Ok(Some((meta, media_type)))
    }

    /// Whether the message may be logged, or why not, checked in the order
    /// `apply` refuses: `DiskLow`, `TooManyAttachments` (more hashes than
    /// [`limits::ATTACHMENTS_MAX`]), `AttachmentsTooLarge` (an image found
    /// over [`limits::ATTACHMENT_MAX_BYTES`], or the images found total over
    /// [`limits::ATTACHMENTS_MAX_TOTAL_BYTES`]), then `UnknownAttachment` for
    /// any hash not staged.
    pub(crate) fn decide(&self) -> std::result::Result<(), RejectReason> {
        if self.disk_low {
            return Err(RejectReason::DiskLow);
        }
        if self.staged.len() > limits::ATTACHMENTS_MAX {
            return Err(RejectReason::TooManyAttachments);
        }
        if self
            .links
            .iter()
            .flatten()
            .any(|(meta, _)| meta.size > limits::ATTACHMENT_MAX_BYTES)
        {
            return Err(RejectReason::AttachmentsTooLarge);
        }
        let total: u64 = self.links.iter().flatten().map(|(meta, _)| meta.size).sum();
        if total > limits::ATTACHMENTS_MAX_TOTAL_BYTES {
            return Err(RejectReason::AttachmentsTooLarge);
        }
        if self.links.iter().any(Option::is_none) {
            return Err(RejectReason::UnknownAttachment);
        }
        Ok(())
    }

    /// Records the linked blobs in the caller's transaction and returns the
    /// attachments they make, in order. Only after [`Prepared::decide`]
    /// passed. Their files stay this one's until [`Prepared::kept`].
    pub(crate) fn log(&mut self, tx: &Connection) -> Result<Vec<Attachment>> {
        let mut attachments = Vec::with_capacity(self.links.len());
        for (meta, media_type) in self.links.iter().flatten() {
            self.blobs.record(tx, meta)?;
            attachments.push(Attachment {
                blob: meta.blob_ref(),
                media_type: *media_type,
            });
        }
        self.logged = true;
        Ok(attachments)
    }

    /// Called once the transaction [`Prepared::log`] wrote to is committed:
    /// the staged files go, and the blobs are the log's. A crash before this
    /// leaves the staged files, which eviction removes. One never logged
    /// keeps nothing: its blob files go when it is dropped.
    pub(crate) fn kept(mut self) {
        if !self.logged {
            return;
        }
        for hash in &self.staged {
            let path = self.staging.path(*hash);
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "removing a staged image");
                }
            }
        }
        self.links.clear();
    }
}

impl Drop for Prepared {
    fn drop(&mut self) {
        for (meta, _) in self.links.iter().flatten() {
            self.blobs.discard(meta.id);
        }
    }
}

/// A file in the staging directory, as [`Staging::files`] lists it.
struct Entry {
    path: PathBuf,
    size: u64,
    modified: SystemTime,
    temp: bool,
    partial: bool,
}

/// Removes `path`; whether it was there to remove.
fn remove(path: &Path) -> Result<bool> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).context(|| format!("removing {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Storage;
    use skywave_proto::BlobId;

    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    /// A PNG: its signature and a little more.
    fn png() -> Vec<u8> {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        bytes.extend_from_slice(b"IHDR and the rest");
        bytes
    }

    /// The names of the files in `dir`, sorted.
    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// Makes `path` look last modified `age` ago.
    fn set_age(path: &Path, age: Duration) {
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(SystemTime::now() - age)
            .unwrap();
    }

    fn modified(path: &Path) -> SystemTime {
        fs::metadata(path).unwrap().modified().unwrap()
    }

    #[test]
    fn a_second_stage_of_the_same_image_leaves_the_file_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let staging = Staging::new(tmp.path());
        let bytes = png();
        let hash = ContentHash::of(&bytes);
        let stored = UploadResult::Stored {
            size: bytes.len() as u64,
        };
        assert!(!staging.contains(hash));
        assert_eq!(staging.stage(hash, &bytes).unwrap(), stored);
        assert!(staging.contains(hash));
        assert_eq!(fs::read(staging.path(hash)).unwrap(), bytes);
        assert_eq!(names(tmp.path()), [hash.to_hex()], "no temp file is left");

        set_age(&staging.path(hash), DAY);
        let before = modified(&staging.path(hash));
        assert_eq!(staging.stage(hash, &bytes).unwrap(), stored);
        assert_eq!(modified(&staging.path(hash)), before, "not rewritten");
    }

    #[test]
    fn a_body_must_hash_to_its_path_and_may_be_any_file() {
        let tmp = tempfile::tempdir().unwrap();
        let staging = Staging::new(tmp.path());
        let bytes = png();
        assert_eq!(
            staging.stage(ContentHash::of(b"other"), &bytes).unwrap(),
            UploadResult::HashMismatch
        );
        assert!(names(tmp.path()).is_empty(), "nothing is written");

        let pdf = b"%PDF-1.4 not an image".to_vec();
        let pdf_hash = ContentHash::of(&pdf);
        assert_eq!(
            staging.stage(pdf_hash, &pdf).unwrap(),
            UploadResult::Stored {
                size: pdf.len() as u64,
            }
        );
        assert!(staging.contains(pdf_hash), "the pdf is staged");

        let hash = ContentHash::of(&bytes);
        assert!(
            matches!(
                staging.stage(hash, &bytes).unwrap(),
                UploadResult::Stored { .. }
            ),
            "control: the same image under its own hash is stored"
        );
    }

    #[test]
    fn a_staged_file_that_no_longer_hashes_to_its_name_is_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let staging = Staging::new(tmp.path());
        let bytes = png();
        let hash = ContentHash::of(&bytes);
        fs::write(staging.path(hash), b"damaged").unwrap();
        assert_eq!(
            staging.stage(hash, &bytes).unwrap(),
            UploadResult::Stored {
                size: bytes.len() as u64,
            }
        );
        assert_eq!(fs::read(staging.path(hash)).unwrap(), bytes);
    }

    #[test]
    fn stage_leaves_no_partial_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let staging = Staging::new(tmp.path());
        let bytes = png();
        let hash = ContentHash::of(&bytes);
        assert_eq!(
            staging.stage(hash, &bytes).unwrap(),
            UploadResult::Stored {
                size: bytes.len() as u64,
            },
            "control: the image under its own hash is stored"
        );
        assert!(!staging.partial(hash).exists(), "the partial was renamed");

        let wrong = ContentHash::of(b"other bytes");
        assert_eq!(
            staging.stage(wrong, &bytes).unwrap(),
            UploadResult::HashMismatch
        );
        assert!(
            !staging.partial(wrong).exists(),
            "the refused partial is gone"
        );
        assert!(
            !staging.contains(wrong),
            "nothing is staged under the wrong hash"
        );
    }

    #[test]
    fn eviction_removes_the_oldest_until_the_total_fits() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let staging = Staging::new(dir);
        // Created in an order that is not the age order.
        for (name, age) in [("new", 1), ("old", 3), ("middle", 2)] {
            fs::write(dir.join(name), [0u8; 100]).unwrap();
            set_age(&dir.join(name), DAY * age);
        }
        // A temp file is no staged file: big and oldest, but left alone.
        fs::write(dir.join(".tmp-x"), vec![0u8; 10_000]).unwrap();
        set_age(&dir.join(".tmp-x"), DAY * 9);

        assert_eq!(staging.evict(250).unwrap(), 1);
        assert_eq!(names(dir), [".tmp-x", "middle", "new"]);
        assert_eq!(staging.evict(250).unwrap(), 0, "already within the cap");
        assert_eq!(staging.evict(100).unwrap(), 1, "the cap binds again");
        assert_eq!(names(dir), [".tmp-x", "new"]);
    }

    #[test]
    fn sweep_removes_temp_files_and_old_files_and_keeps_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let staging = Staging::new(dir);
        fs::write(dir.join("stale"), b"x").unwrap();
        set_age(&dir.join("stale"), DAY * 8);
        fs::write(dir.join("fresh"), b"x").unwrap();
        set_age(&dir.join("fresh"), DAY * 6);
        fs::write(dir.join(".tmp-x"), b"x").unwrap();

        assert_eq!(staging.sweep(STAGING_MAX_AGE).unwrap(), 2);
        assert_eq!(names(dir), ["fresh"]);
        assert_eq!(
            Staging::new(dir.join("missing"))
                .sweep(STAGING_MAX_AGE)
                .unwrap(),
            0,
            "a missing directory has nothing to sweep"
        );
    }

    /// An image: its PNG signature and `size` bytes of `tag`, so each tag is
    /// different content.
    fn image(size: usize, tag: u8) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.resize(size, tag);
        bytes
    }

    /// A data dir's store and the staging directory beside its blobs.
    fn stores() -> (tempfile::TempDir, Storage, Staging) {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::open(tmp.path()).unwrap();
        let staging = Staging::new(storage.dir.attachments_dir());
        (tmp, storage, staging)
    }

    /// Stages `bytes`, returning their hash.
    fn stage(staging: &Staging, bytes: &[u8]) -> ContentHash {
        let hash = ContentHash::of(bytes);
        assert!(matches!(
            staging.stage(hash, bytes).unwrap(),
            UploadResult::Stored { .. }
        ));
        hash
    }

    /// How many files the blob store holds.
    fn blob_files(storage: &Storage) -> usize {
        std::fs::read_dir(storage.dir.blobs_dir()).unwrap().count()
    }

    #[test]
    fn linked_images_are_new_blobs_and_the_staged_files_stay() {
        let (_tmp, storage, staging) = stores();
        let (a, b) = (
            stage(&staging, &image(100, 1)),
            stage(&staging, &image(100, 2)),
        );

        let prepared = prepare(&staging, &storage.blobs, false, &[a, b, a]).unwrap();
        assert_eq!(prepared.decide(), Ok(()));
        let ids: Vec<BlobId> = prepared
            .links
            .iter()
            .map(|link| link.as_ref().unwrap().0.id)
            .collect();
        assert_ne!(ids[0], ids[1]);
        assert_ne!(ids[0], ids[2], "the same image twice is two blobs");
        assert_ne!(ids[1], ids[2]);
        assert_eq!(blob_files(&storage), 3);
        assert!(
            staging.contains(a) && staging.contains(b),
            "staged files stay"
        );
        assert_eq!(prepared.links[2].as_ref().unwrap().0.sha256, a);
        assert_eq!(prepared.links[1].as_ref().unwrap().1, ImageType::Png);
    }

    #[test]
    fn decide_gives_each_refusal_in_its_order() {
        let (_tmp, storage, staging) = stores();
        let a = stage(&staging, &image(100, 1));
        let missing = ContentHash::of(b"never staged");
        let blobs = &storage.blobs;

        // Control: a staged image alone is accepted.
        assert_eq!(
            prepare(&staging, blobs, false, &[a]).unwrap().decide(),
            Ok(())
        );
        assert_eq!(blob_files(&storage), 0, "dropped unlogged: no file left");

        assert_eq!(
            prepare(&staging, blobs, true, &[a]).unwrap().decide(),
            Err(RejectReason::DiskLow)
        );
        assert_eq!(
            prepare(&staging, blobs, false, &[a, missing])
                .unwrap()
                .decide(),
            Err(RejectReason::UnknownAttachment)
        );
        let nine = vec![missing; limits::ATTACHMENTS_MAX + 1];
        assert_eq!(
            prepare(&staging, blobs, false, &nine).unwrap().decide(),
            Err(RejectReason::TooManyAttachments),
            "counted before the unstaged hashes are looked at"
        );
        assert_eq!(
            prepare(&staging, blobs, true, &nine).unwrap().decide(),
            Err(RejectReason::DiskLow),
            "the disk is checked first"
        );
        assert_eq!(blob_files(&storage), 0, "nothing refused was linked");

        let big: Vec<ContentHash> = (1..=3)
            .map(|tag| stage(&staging, &image(3_600_000, tag)))
            .collect();
        assert_eq!(
            prepare(&staging, blobs, false, &big[..2]).unwrap().decide(),
            Ok(()),
            "control: two images of 7.2 MB fit"
        );
        assert_eq!(
            prepare(&staging, blobs, false, &big).unwrap().decide(),
            Err(RejectReason::AttachmentsTooLarge)
        );
    }

    #[test]
    fn one_image_over_the_image_limit_is_too_large() {
        let (_tmp, storage, staging) = stores();
        let over = stage(
            &staging,
            &image(limits::ATTACHMENT_MAX_BYTES as usize + 1, 1),
        );
        assert_eq!(
            prepare(&staging, &storage.blobs, false, &[over])
                .unwrap()
                .decide(),
            Err(RejectReason::AttachmentsTooLarge)
        );

        let at_limit = stage(&staging, &image(limits::ATTACHMENT_MAX_BYTES as usize, 2));
        assert_eq!(
            prepare(&staging, &storage.blobs, false, &[at_limit])
                .unwrap()
                .decide(),
            Ok(()),
            "control: an image of exactly the limit is accepted"
        );
    }

    #[test]
    fn a_staged_file_that_no_longer_hashes_to_its_name_is_removed_when_a_message_names_it() {
        let (_tmp, storage, staging) = stores();
        let hash = stage(&staging, &image(100, 1));
        let intact = prepare(&staging, &storage.blobs, false, &[hash]).unwrap();
        assert_eq!(
            intact.decide(),
            Ok(()),
            "control: the staged image is accepted"
        );
        drop(intact);

        // Other PNG bytes: the sniff still passes, and the hash check fails.
        fs::write(staging.path(hash), image(100, 2)).unwrap();
        let prepared = prepare(&staging, &storage.blobs, false, &[hash]).unwrap();
        assert_eq!(prepared.decide(), Err(RejectReason::UnknownAttachment));
        assert!(
            !staging.contains(hash),
            "the file that is not the upload is removed"
        );
        drop(prepared);
        assert_eq!(blob_files(&storage), 0, "no blob is left");
    }

    #[test]
    fn a_staged_file_that_is_not_an_image_is_unknown_to_a_message() {
        let (_tmp, storage, staging) = stores();
        let img = stage(&staging, &image(100, 1));
        let pdf = stage(&staging, b"%PDF-1.4 not an image");
        assert_eq!(
            prepare(&staging, &storage.blobs, false, &[img])
                .unwrap()
                .decide(),
            Ok(()),
            "control: a staged image is accepted"
        );
        assert_eq!(
            prepare(&staging, &storage.blobs, false, &[pdf])
                .unwrap()
                .decide(),
            Err(RejectReason::UnknownAttachment)
        );
        assert_eq!(blob_files(&storage), 0, "nothing is linked");
        assert!(staging.contains(pdf), "the staged file stays");
    }

    #[test]
    fn dropping_an_unlogged_preparation_removes_its_blobs_and_keeps_the_staged_files() {
        let (_tmp, storage, staging) = stores();
        let a = stage(&staging, &image(100, 1));
        let prepared = prepare(&staging, &storage.blobs, false, &[a, a]).unwrap();
        assert_eq!(blob_files(&storage), 2);
        drop(prepared);
        assert_eq!(blob_files(&storage), 0);
        assert!(staging.contains(a));
    }

    #[test]
    fn a_logged_then_kept_preparation_keeps_its_blobs_and_removes_the_staged_files() {
        let (_tmp, storage, staging) = stores();
        let (a, b) = (
            stage(&staging, &image(100, 1)),
            stage(&staging, &image(100, 2)),
        );
        let mut prepared = prepare(&staging, &storage.blobs, false, &[a, b]).unwrap();
        prepared.decide().unwrap();
        let logged = prepared.log(storage.db.conn()).unwrap();
        assert_eq!(logged.len(), 2);
        assert_eq!(logged[1].blob.sha256, b);
        prepared.kept();

        assert_eq!(blob_files(&storage), 2, "the blobs are the log's");
        for attachment in &logged {
            let meta = storage.blobs.meta(storage.db.conn(), attachment.blob.id);
            assert!(meta.unwrap().is_some(), "each has its row");
        }
        assert!(
            !staging.contains(a) && !staging.contains(b),
            "staged files went"
        );
    }

    #[test]
    fn a_partial_upload_resumes_from_its_length() {
        let tmp = tempfile::tempdir().unwrap();
        let staging = Staging::new(tmp.path());
        let bytes = png().repeat(4);
        let hash = ContentHash::of(&bytes);

        let Resume::Append(mut first) = staging.resume(hash, 0).unwrap() else {
            panic!("a new upload appends");
        };
        first.write_all(&bytes[..40]).unwrap();
        drop(first);
        assert_eq!(staging.partial_len(hash).unwrap(), 40);

        let Resume::Append(mut rest) = staging.resume(hash, 40).unwrap() else {
            panic!("a resume at the partial's length appends");
        };
        rest.write_all(&bytes[40..]).unwrap();
        drop(rest);
        assert_eq!(
            staging.finish(hash).unwrap(),
            UploadResult::Stored {
                size: bytes.len() as u64,
            }
        );
        assert_eq!(fs::read(staging.path(hash)).unwrap(), bytes);
        assert_eq!(names(tmp.path()), [hash.to_hex()], "no partial is left");
    }

    #[test]
    fn a_resume_from_another_offset_is_a_mismatch_and_zero_restarts() {
        let tmp = tempfile::tempdir().unwrap();
        let staging = Staging::new(tmp.path());
        let bytes = png();
        let hash = ContentHash::of(&bytes);

        let Resume::Append(mut file) = staging.resume(hash, 0).unwrap() else {
            panic!("a new upload appends");
        };
        file.write_all(&bytes[..10]).unwrap();
        drop(file);

        for offset in [5, 11] {
            assert!(
                matches!(
                    staging.resume(hash, offset).unwrap(),
                    Resume::Mismatch { have: 10 }
                ),
                "an offset that is neither 0 nor the length is a mismatch"
            );
        }
        assert_eq!(staging.partial_len(hash).unwrap(), 10, "nothing changed");

        let Resume::Append(_restart) = staging.resume(hash, 0).unwrap() else {
            panic!("offset 0 restarts");
        };
        assert_eq!(staging.partial_len(hash).unwrap(), 0, "restarted empty");
    }

    #[test]
    fn a_finished_partial_that_does_not_hash_is_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let staging = Staging::new(tmp.path());
        let bytes = png();
        let hash = ContentHash::of(&bytes);

        let Resume::Append(mut file) = staging.resume(hash, 0).unwrap() else {
            panic!("a new upload appends");
        };
        file.write_all(b"other bytes").unwrap();
        drop(file);
        assert_eq!(staging.finish(hash).unwrap(), UploadResult::HashMismatch);
        assert!(names(tmp.path()).is_empty(), "the partial is removed");

        let Resume::Append(mut file) = staging.resume(hash, 0).unwrap() else {
            panic!("a new upload appends");
        };
        file.write_all(&bytes).unwrap();
        drop(file);
        assert_eq!(
            staging.finish(hash).unwrap(),
            UploadResult::Stored {
                size: bytes.len() as u64,
            },
            "control: the same upload with its own bytes is stored"
        );
    }

    #[test]
    fn finishing_a_partial_of_a_staged_file_answers_stored_and_removes_it() {
        let tmp = tempfile::tempdir().unwrap();
        let staging = Staging::new(tmp.path());
        let bytes = png();
        let hash = stage(&staging, &bytes);

        fs::write(staging.partial(hash), &bytes[..10]).unwrap();
        assert_eq!(
            staging.finish(hash).unwrap(),
            UploadResult::Stored {
                size: bytes.len() as u64,
            }
        );
        assert_eq!(names(tmp.path()), [hash.to_hex()], "the partial is removed");
    }

    #[test]
    fn a_resume_of_a_staged_image_answers_it_and_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let staging = Staging::new(tmp.path());
        let bytes = png();
        let hash = stage(&staging, &bytes);

        for offset in [5, 0, bytes.len() as u64] {
            let Resume::Staged(answer) = staging.resume(hash, offset).unwrap() else {
                panic!("a PUT at offset {offset} of a staged image is answered");
            };
            assert_eq!(
                answer,
                UploadResult::Stored {
                    size: bytes.len() as u64,
                }
            );
            assert_eq!(names(tmp.path()), [hash.to_hex()], "nothing is written");
        }
    }

    #[test]
    fn a_resume_of_a_staged_image_removes_a_leftover_partial() {
        let tmp = tempfile::tempdir().unwrap();
        let staging = Staging::new(tmp.path());
        let bytes = png();
        let hash = stage(&staging, &bytes);
        fs::write(staging.partial(hash), &bytes[..10]).unwrap();

        assert!(matches!(
            staging.resume(hash, 0).unwrap(),
            Resume::Staged(UploadResult::Stored { .. })
        ));
        assert_eq!(
            names(tmp.path()),
            [hash.to_hex()],
            "the leftover partial is removed"
        );
    }

    #[test]
    fn finishing_stamps_the_staged_file_with_the_time_of_staging() {
        let tmp = tempfile::tempdir().unwrap();
        let staging = Staging::new(tmp.path());
        let bytes = png();
        let hash = ContentHash::of(&bytes);

        let Resume::Append(mut file) = staging.resume(hash, 0).unwrap() else {
            panic!("a new upload appends");
        };
        file.write_all(&bytes).unwrap();
        file.set_modified(SystemTime::UNIX_EPOCH).unwrap();
        drop(file);
        assert!(matches!(
            staging.finish(hash).unwrap(),
            UploadResult::Stored { .. }
        ));
        let modified = fs::metadata(staging.path(hash))
            .unwrap()
            .modified()
            .unwrap();
        assert!(
            modified.elapsed().unwrap() < Duration::from_secs(60),
            "an old partial is staged as new"
        );
    }

    #[test]
    fn sweep_keeps_a_fresh_partial_and_eviction_never_counts_one() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let staging = Staging::new(dir);
        fs::write(dir.join(".part-fresh"), [0u8; 100]).unwrap();
        set_age(&dir.join(".part-fresh"), DAY * 6);
        fs::write(dir.join(".part-stale"), b"x").unwrap();
        set_age(&dir.join(".part-stale"), DAY * 8);

        assert_eq!(staging.sweep(STAGING_MAX_AGE).unwrap(), 1);
        assert_eq!(names(dir), [".part-fresh"]);
        assert_eq!(staging.evict(0).unwrap(), 0, "a partial is not counted");
        assert_eq!(names(dir), [".part-fresh"], "nor evicted");
    }
}
