//! Blob file store: large content kept as files in `blobs/`, named by
//! [`BlobId`], with size and SHA-256 recorded in the `blobs` table.
//!
//! A blob exists once its row exists. Files are written to a temp file in
//! `blobs/`, synced and renamed into place before the row is inserted, so a
//! row never points at a partial file. A crash between rename and insert can
//! leave an orphan file, which is never served.
//!
//! **Deleting.** The row first, in a transaction, then the file once it
//! commits: a workdir blob when it becomes unnamed (`workdir.rs`,
//! "Garbage"), an event blob when a device's `DeleteBlob` deletes it
//! (`commands.rs`). The event that referenced a deleted event blob keeps
//! its preview, and its `event_blobs` row stays. At startup, before anything can be writing,
//! [`BlobStore::sweep`] removes every `.tmp-*` file and every blob file
//! without a row: what an interrupted write, a crash between rename and
//! insert, or one between a deletion's commit and its unlink left behind.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use skywave_proto::{BlobId, BlobRef, ContentHash};

use crate::error::{Error, IoContext, Result};

/// What the `blobs` table records about one blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobMeta {
    pub id: BlobId,
    pub size: u64,
    /// SHA-256 of the content; lowercase hex in the table.
    pub sha256: ContentHash,
}

impl BlobMeta {
    /// The wire reference to this blob.
    pub fn blob_ref(&self) -> BlobRef {
        BlobRef {
            id: self.id,
            size: self.size,
            sha256: self.sha256,
        }
    }
}

/// An open blob: seek to serve byte ranges, then read.
#[derive(Debug)]
pub struct BlobFile {
    pub meta: BlobMeta,
    file: File,
}

impl BlobFile {
    /// The underlying file, positioned where reads and seeks left it; for
    /// serving it asynchronously.
    pub fn into_file(self) -> File {
        self.file
    }
}

impl Read for BlobFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }
}

impl Seek for BlobFile {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.file.seek(pos)
    }
}

/// The `blobs/` directory of a data dir.
#[derive(Debug, Clone)]
pub struct BlobStore {
    dir: PathBuf,
}

impl BlobStore {
    /// A store over an existing directory (see [`crate::data_dir::DataDir`]).
    pub fn new(dir: impl Into<PathBuf>) -> BlobStore {
        BlobStore { dir: dir.into() }
    }

    fn path(&self, id: BlobId) -> PathBuf {
        self.dir.join(id.0.to_string())
    }

    /// Stores `content` as a new blob and records it in `conn`: [`write`]
    /// then [`record`].
    ///
    /// [`write`]: BlobStore::write
    /// [`record`]: BlobStore::record
    pub fn put(&self, conn: &Connection, content: impl Read) -> Result<BlobMeta> {
        let meta = self.write(content)?;
        self.record(conn, &meta)?;
        Ok(meta)
    }

    /// Writes `content` as the synced file of a new blob, not yet recorded:
    /// no database access, so it holds no lock. Follow with [`record`], or
    /// the file is an orphan that is never served.
    ///
    /// [`record`]: BlobStore::record
    pub fn write(&self, mut content: impl Read) -> Result<BlobMeta> {
        let id = BlobId::new();
        let path = self.path(id);
        let (size, sha256) = self
            .write_atomically(&path, &mut content)
            .context(|| format!("writing blob {}", path.display()))?;
        Ok(BlobMeta { id, size, sha256 })
    }

    /// Inserts the `blobs` row of a blob [`write`](BlobStore::write) made;
    /// on failure the file is removed.
    pub fn record(&self, conn: &Connection, meta: &BlobMeta) -> Result<()> {
        if let Err(e) = conn.execute(
            "INSERT INTO blobs (id, size, sha256) VALUES (?1, ?2, ?3)",
            params![
                meta.id.0.to_string(),
                sql_size(meta.size),
                meta.sha256.to_hex()
            ],
        ) {
            self.discard(meta.id);
            return Err(e.into());
        }
        tracing::debug!(blob = %meta.id.0, size = meta.size, "stored blob");
        Ok(())
    }

    /// Removes the file of a blob that was written but never committed, or
    /// whose row was deleted.
    pub fn discard(&self, id: BlobId) {
        let _ = std::fs::remove_file(self.path(id));
    }

    /// Removes every file in the store that is not a recorded blob: `.tmp-*`
    /// files and blob files without a row (module docs, "Deleting"); returns
    /// how many. Startup only: a write in flight has exactly such files, so
    /// nothing may be writing blobs to this directory meanwhile. Files
    /// named otherwise are left alone.
    pub fn sweep(&self, conn: &Connection) -> Result<usize> {
        let entries = std::fs::read_dir(&self.dir)
            .context(|| format!("listing blobs in {}", self.dir.display()))?;
        let mut removed = 0;
        for entry in entries {
            let entry = entry.context(|| format!("listing blobs in {}", self.dir.display()))?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let stale = if name.starts_with(".tmp-") {
                true
            } else if let Ok(id) = uuid::Uuid::parse_str(name) {
                self.meta(conn, BlobId(id))?.is_none()
            } else {
                false
            };
            if !stale || !entry.file_type().is_ok_and(|t| t.is_file()) {
                continue;
            }
            match std::fs::remove_file(entry.path()) {
                Ok(()) => removed += 1,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(e).context(|| format!("removing {}", entry.path().display()));
                }
            }
        }
        Ok(removed)
    }

    fn write_atomically(
        &self,
        path: &Path,
        content: &mut impl Read,
    ) -> io::Result<(u64, ContentHash)> {
        let mut tmp = tempfile::Builder::new()
            .prefix(".tmp-")
            .tempfile_in(&self.dir)?;
        let mut hasher = Sha256::new();
        let mut size = 0u64;
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = match content.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            hasher.update(&buf[..n]);
            tmp.write_all(&buf[..n])?;
            size += n as u64;
        }
        tmp.as_file().sync_all()?;
        tmp.persist(path).map_err(|e| e.error)?;
        #[cfg(unix)]
        File::open(&self.dir)?.sync_all()?;
        Ok((size, ContentHash::from_digest(hasher.finalize().into())))
    }

    /// The recorded metadata of `id`, if it exists.
    pub fn meta(&self, conn: &Connection, id: BlobId) -> Result<Option<BlobMeta>> {
        Ok(conn
            .query_row(
                "SELECT size, sha256 FROM blobs WHERE id = ?1",
                [id.0.to_string()],
                |r| {
                    Ok(BlobMeta {
                        id,
                        size: r.get::<_, i64>(0)?.unsigned_abs(), // CHECK (size >= 0)
                        sha256: r.get::<_, String>(1)?.parse().map_err(|e| {
                            rusqlite::Error::FromSqlConversionFailure(
                                1,
                                rusqlite::types::Type::Text,
                                Box::new(e),
                            )
                        })?,
                    })
                },
            )
            .optional()?)
    }

    /// Opens blob `id` for reading, positioned at the start.
    ///
    /// `None` if no such blob is recorded, including one deleted between
    /// reading its row and opening its file (a collected workdir blob). A
    /// recorded blob whose file is missing or has the wrong size is an
    /// error.
    pub fn open(&self, conn: &Connection, id: BlobId) -> Result<Option<BlobFile>> {
        let Some(meta) = self.meta(conn, id)? else {
            return Ok(None);
        };
        let path = self.path(id);
        let file = match File::open(&path) {
            Ok(file) => file,
            // Deleted since its row was read: the row went first.
            Err(e) if e.kind() == io::ErrorKind::NotFound && self.meta(conn, id)?.is_none() => {
                return Ok(None);
            }
            Err(e) => return Err(e).context(|| format!("opening blob {}", path.display())),
        };
        let actual = file
            .metadata()
            .context(|| format!("reading blob metadata {}", path.display()))?
            .len();
        if actual != meta.size {
            return Err(Error::BlobSizeMismatch {
                id: id.0.to_string(),
                expected: meta.size,
                actual,
            });
        }
        Ok(Some(BlobFile { meta, file }))
    }
}

/// SQLite integers are i64; no real file reaches `i64::MAX` bytes.
fn sql_size(size: u64) -> i64 {
    i64::try_from(size).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Storage;

    #[test]
    fn put_then_open_round_trips_with_ranges() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::open(tmp.path()).unwrap();
        let content = b"hello, skywave blob store";
        let meta = storage.blobs.put(storage.db.conn(), &content[..]).unwrap();
        assert_eq!(meta.size, content.len() as u64);
        assert_eq!(
            meta.sha256.to_hex(),
            hex::encode(Sha256::digest(content)),
            "sha256 is the content's SHA-256"
        );
        assert_eq!(
            meta.blob_ref(),
            BlobRef {
                id: meta.id,
                size: meta.size,
                sha256: meta.sha256
            },
            "the wire reference carries the stored hash"
        );
        let stored: String = storage
            .db
            .conn()
            .query_row(
                "SELECT sha256 FROM blobs WHERE id = ?1",
                [meta.id.0.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            stored,
            hex::encode(Sha256::digest(content)),
            "lowercase hex in the table"
        );

        let mut file = storage
            .blobs
            .open(storage.db.conn(), meta.id)
            .unwrap()
            .unwrap();
        assert_eq!(file.meta, meta);
        let mut all = Vec::new();
        file.read_to_end(&mut all).unwrap();
        assert_eq!(all, content);

        file.seek(SeekFrom::Start(7)).unwrap();
        let mut range = [0u8; 7];
        file.read_exact(&mut range).unwrap();
        assert_eq!(&range, b"skywave");
    }

    #[test]
    fn put_leaves_only_the_named_file() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::open(tmp.path()).unwrap();
        let meta = storage.blobs.put(storage.db.conn(), &b"x"[..]).unwrap();
        let names: Vec<String> = std::fs::read_dir(storage.dir.blobs_dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, [meta.id.0.to_string()]);
    }

    #[test]
    fn empty_blob_is_allowed() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::open(tmp.path()).unwrap();
        let meta = storage.blobs.put(storage.db.conn(), io::empty()).unwrap();
        assert_eq!(meta.size, 0);
        assert!(
            storage
                .blobs
                .open(storage.db.conn(), meta.id)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn unknown_blob_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::open(tmp.path()).unwrap();
        let conn = storage.db.conn();
        assert!(storage.blobs.meta(conn, BlobId::new()).unwrap().is_none());
        assert!(storage.blobs.open(conn, BlobId::new()).unwrap().is_none());
    }

    #[test]
    fn truncated_file_is_detected() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::open(tmp.path()).unwrap();
        let meta = storage
            .blobs
            .put(storage.db.conn(), &b"abcdef"[..])
            .unwrap();
        std::fs::write(storage.dir.blobs_dir().join(meta.id.0.to_string()), b"abc").unwrap();
        assert!(matches!(
            storage.blobs.open(storage.db.conn(), meta.id),
            Err(Error::BlobSizeMismatch {
                expected: 6,
                actual: 3,
                ..
            })
        ));
    }
}
