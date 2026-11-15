//! Staged uploads: files a device uploads before the `SendMessage` that
//! names them by sha256 (`PUT` and `HEAD /v1/attachments/{sha256}`).
//!
//! A staged upload is the file `<data dir>/attachments/<sha256 hex>`, of any
//! type. There is no table: the hash is the name, and a file already there is
//! trusted only while it still hashes to its name. A message that names one as
//! an image sniffs it then. The bytes go to a temp file in the same directory,
//! which is synced and renamed into place, so a name never holds a partial
//! file.
//!
//! Staging is a cache. [`Staging::stage_file`] keeps it under [`STAGING_MAX_BYTES`]
//! by evicting the oldest files, and [`Staging::sweep`] removes files older
//! than [`STAGING_MAX_AGE`] at startup. Eviction is garbage collection, not an
//! outcome: a message naming an image that is no longer staged is refused as
//! `UnknownAttachment` when its command arrives.
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

/// Prefix of the temp files a write is made in; never a staged image.
const TEMP_PREFIX: &str = ".tmp-";
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

    /// Stages `bytes` under `sha256` and answers what the upload route
    /// answers: the bytes go to a temp file, which [`Staging::stage_file`]
    /// stages. Nothing is left behind on any refusal.
    pub fn stage(&self, sha256: ContentHash, bytes: &[u8]) -> Result<UploadResult> {
        let mut file = self
            .temp()
            .context(|| format!("staging in {}", self.dir.display()))?;
        file.write_all(bytes)
            .context(|| format!("staging in {}", self.dir.display()))?;
        self.stage_file(sha256, file)
    }

    /// A temp file in the staging directory: never a staged file, and deleted
    /// when dropped unless [`Staging::stage_file`] persists it.
    pub fn temp(&self) -> io::Result<tempfile::NamedTempFile> {
        tempfile::Builder::new()
            .prefix(TEMP_PREFIX)
            .tempfile_in(&self.dir)
    }

    /// Stages the temp file `file`, holding the bytes that `sha256` names, and
    /// answers what the upload route answers. A file already staged under
    /// `sha256` is answered as stored, and `file` is deleted unwritten.
    /// Otherwise `file` must hash to `sha256`; it is then synced and renamed
    /// to its name, and the oldest files evicted down to
    /// [`STAGING_MAX_BYTES`]. A refused `file` is deleted.
    pub fn stage_file(
        &self,
        sha256: ContentHash,
        file: tempfile::NamedTempFile,
    ) -> Result<UploadResult> {
        let path = self.path(sha256);
        if let Some(stored) = self.existing(&path, sha256)? {
            return Ok(stored);
        }
        let opened =
            File::open(file.path()).context(|| format!("opening {}", file.path().display()))?;
        let hash =
            ContentHash::read(opened).context(|| format!("reading {}", file.path().display()))?;
        if hash != sha256 {
            return Ok(UploadResult::HashMismatch);
        }
        let size = file
            .as_file()
            .metadata()
            .context(|| format!("reading {}", file.path().display()))?
            .len();
        file.as_file()
            .sync_all()
            .context(|| format!("syncing {}", file.path().display()))?;
        file.persist(&path)
            .map_err(|e| e.error)
            .context(|| format!("staging {}", path.display()))?;
        if let Err(e) = File::open(&self.dir).and_then(|dir| dir.sync_all()) {
            let _ = fs::remove_file(&path);
            return Err(e).context(|| format!("staging {}", path.display()));
        }
        self.evict(STAGING_MAX_BYTES)?;
        Ok(UploadResult::Stored { size })
    }

    /// Removes the oldest staged files, by modification time, until they
    /// total at most `cap` bytes; returns how many were removed. Temp files
    /// are not staged files: they are neither removed nor counted.
    pub fn evict(&self, cap: u64) -> Result<usize> {
        let mut staged: Vec<Entry> = self.files()?.into_iter().filter(|f| !f.temp).collect();
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

    /// Removes the temp files and the files last modified more than
    /// `max_age` ago; returns how many were removed. A missing directory has
    /// nothing to remove. Startup only: a write in flight is a temp file, so
    /// nothing may be staging meanwhile.
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
            files.push(Entry {
                temp: entry.file_name().to_string_lossy().starts_with(TEMP_PREFIX),
                path,
                size: meta.len(),
                modified,
            });
        }
        Ok(files)
    }
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
    /// staged, is no longer an image, or no longer hashes to its name: the
    /// message then names an image it does not have.
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
            return Ok(None);
        }
        Ok(Some((meta, media_type)))
    }

    /// Whether the message may be logged, or why not, checked in the order
    /// `apply` refuses: `DiskLow`, `TooManyAttachments` (more hashes than
    /// [`limits::ATTACHMENTS_MAX`]), `AttachmentsTooLarge` (the images found
    /// total over [`limits::ATTACHMENTS_MAX_TOTAL_BYTES`]), then
    /// `UnknownAttachment` for any hash not staged.
    pub(crate) fn decide(&self) -> std::result::Result<(), RejectReason> {
        if self.disk_low {
            return Err(RejectReason::DiskLow);
        }
        if self.staged.len() > limits::ATTACHMENTS_MAX {
            return Err(RejectReason::TooManyAttachments);
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
}
