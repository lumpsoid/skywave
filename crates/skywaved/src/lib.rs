//! The Skywave server daemon (spec §5).
//!
//! The binary is a thin CLI over this library so later server pieces and
//! integration tests share one implementation.

pub mod blobs;
pub mod config;
pub mod data_dir;
pub mod db;
pub mod error;
pub mod logging;

use std::path::PathBuf;

pub use error::{Error, Result};

use blobs::BlobStore;
use data_dir::DataDir;
use db::Db;

/// Everything the daemon persists: the data dir, its database and blobs.
#[derive(Debug)]
pub struct Storage {
    pub dir: DataDir,
    pub db: Db,
    pub blobs: BlobStore,
}

impl Storage {
    /// Opens the data directory at `root`, creating the layout and the
    /// database schema on first use. Reopening changes nothing.
    pub fn open(root: impl Into<PathBuf>) -> Result<Storage> {
        let dir = DataDir::create(root)?;
        let db = Db::open(&dir.db_path())?;
        let blobs = BlobStore::new(dir.blobs_dir());
        Ok(Storage { dir, db, blobs })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opening_a_fresh_data_dir_creates_layout_and_schema() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("data");
        let storage = Storage::open(&root).unwrap();
        assert!(root.join("skywave.db").is_file());
        for sub in ["blobs", "ca", "server"] {
            assert!(root.join(sub).is_dir());
        }
        assert_eq!(storage.db.schema_version().unwrap(), db::SCHEMA_VERSION);
        let blob = storage.blobs.put(storage.db.conn(), &b"kept"[..]).unwrap();
        drop(storage);

        let storage = Storage::open(&root).unwrap();
        assert_eq!(storage.db.schema_version().unwrap(), db::SCHEMA_VERSION);
        assert!(
            storage
                .blobs
                .open(storage.db.conn(), blob.id)
                .unwrap()
                .is_some()
        );
    }
}
