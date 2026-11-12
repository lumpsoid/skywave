//! Staged uploads: images a device uploads before the `SendMessage` that
//! names them by sha256 (`PUT` and `HEAD /v1/attachments/{sha256}`).
//!
//! A staged image is the file `<data dir>/attachments/<sha256 hex>`. There is
//! no table: the hash is the name, and the type is sniffed again whenever the
//! file is used. The bytes go to a temp file in the same directory, which is
//! synced and renamed into place, so a name never holds a partial image.
//!
//! Staging is a cache. [`Staging::stage`] keeps it under [`STAGING_MAX_BYTES`]
//! by evicting the oldest files, and [`Staging::sweep`] removes files older
//! than [`STAGING_MAX_AGE`] at startup. Eviction is garbage collection, not an
//! outcome: a message naming an image that is no longer staged is refused as
//! `UnknownAttachment` when its command arrives.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use skywave_proto::{ContentHash, ImageType, UploadResult, limits};

use crate::error::{IoContext, Result};

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
    /// answers. An image already staged under `sha256` is answered as stored
    /// and not rewritten. Otherwise the bytes must be at most
    /// [`limits::ATTACHMENT_MAX_BYTES`], hash to `sha256` and sniff as an
    /// image; then they are written, and the oldest files evicted down to
    /// [`STAGING_MAX_BYTES`]. Nothing is written on any refusal.
    pub fn stage(&self, sha256: ContentHash, bytes: &[u8]) -> Result<UploadResult> {
        if bytes.len() as u64 > limits::ATTACHMENT_MAX_BYTES {
            return Ok(UploadResult::TooLarge);
        }
        let path = self.path(sha256);
        if let Some(stored) = self.existing(&path)? {
            return Ok(stored);
        }
        if ContentHash::of(bytes) != sha256 {
            return Ok(UploadResult::HashMismatch);
        }
        let Some(media_type) = ImageType::sniff(bytes) else {
            return Ok(UploadResult::NotAnImage);
        };
        write_file(&self.dir, &path, bytes).context(|| format!("staging {}", path.display()))?;
        self.evict(STAGING_MAX_BYTES)?;
        Ok(UploadResult::Stored {
            size: bytes.len() as u64,
            media_type,
        })
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

    /// The answer for an image already staged at `path`, if there is one. A
    /// file there that no longer sniffs as an image is removed: it is not
    /// one, so the upload goes on as a new one.
    fn existing(&self, path: &Path) -> Result<Option<UploadResult>> {
        let mut file = match File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).context(|| format!("opening {}", path.display())),
        };
        let size = file
            .metadata()
            .context(|| format!("reading {}", path.display()))?
            .len();
        let mut head = Vec::new();
        Read::take(&mut file, SNIFF_LEN)
            .read_to_end(&mut head)
            .context(|| format!("reading {}", path.display()))?;
        match ImageType::sniff(&head) {
            Some(media_type) => Ok(Some(UploadResult::Stored { size, media_type })),
            None => {
                remove(path)?;
                Ok(None)
            }
        }
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

/// A file in the staging directory, as [`Staging::files`] lists it.
struct Entry {
    path: PathBuf,
    size: u64,
    modified: SystemTime,
    temp: bool,
}

/// Writes `bytes` to `path` the way a blob is written: to a temp file in
/// `dir`, synced and renamed into place, then `dir` synced. On error nothing
/// is left at `path`.
fn write_file(dir: &Path, path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut out = tempfile::Builder::new()
        .prefix(TEMP_PREFIX)
        .tempfile_in(dir)?;
    out.write_all(bytes)?;
    out.as_file().sync_all()?;
    out.persist(path).map_err(|e| e.error)?;
    if let Err(e) = File::open(dir).and_then(|dir| dir.sync_all()) {
        let _ = fs::remove_file(path);
        return Err(e);
    }
    Ok(())
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
            media_type: ImageType::Png,
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
    fn a_body_must_hash_to_its_path_and_be_an_image() {
        let tmp = tempfile::tempdir().unwrap();
        let staging = Staging::new(tmp.path());
        let bytes = png();
        assert_eq!(
            staging.stage(ContentHash::of(b"other"), &bytes).unwrap(),
            UploadResult::HashMismatch
        );
        let pdf = b"%PDF-1.4 not an image".to_vec();
        assert_eq!(
            staging.stage(ContentHash::of(&pdf), &pdf).unwrap(),
            UploadResult::NotAnImage
        );
        assert!(names(tmp.path()).is_empty(), "nothing is written");

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
    fn an_oversize_body_is_too_large_and_nothing_is_written() {
        let tmp = tempfile::tempdir().unwrap();
        let staging = Staging::new(tmp.path());
        let mut bytes = png();
        bytes.resize(limits::ATTACHMENT_MAX_BYTES as usize + 1, 0);
        let hash = ContentHash::of(&bytes);
        assert_eq!(staging.stage(hash, &bytes).unwrap(), UploadResult::TooLarge);
        assert!(names(tmp.path()).is_empty());
    }

    #[test]
    fn a_staged_file_that_is_no_longer_an_image_is_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let staging = Staging::new(tmp.path());
        let bytes = png();
        let hash = ContentHash::of(&bytes);
        fs::write(staging.path(hash), b"damaged").unwrap();
        assert_eq!(
            staging.stage(hash, &bytes).unwrap(),
            UploadResult::Stored {
                size: bytes.len() as u64,
                media_type: ImageType::Png,
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
}
