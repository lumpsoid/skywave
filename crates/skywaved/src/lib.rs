//! The Skywave server daemon (architecture.md "Server").
//!
//! The binary is a thin CLI over this library so later server pieces and
//! integration tests share one implementation.

pub mod agent;
pub mod api;
pub mod blobs;
pub mod commands;
pub mod config;
pub mod control;
pub mod crop;
pub mod data_dir;
pub mod db;
pub mod devices;
pub mod dirs;
pub mod disk;
mod encoding;
pub mod error;
pub mod event_log;
pub mod identity;
pub mod logging;
pub mod login;
pub mod pairing;
mod process;
pub mod server;
pub mod shell;
pub mod storage;
pub mod supervisor;
pub mod workdir;

use std::io::Read;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, PoisonError};

pub use error::{Error, Result};
pub use event_log::EventLog;
pub use supervisor::{Activity, Supervisor, SupervisorConfig, SupervisorHandle};

use blobs::{BlobFile, BlobStore};
use data_dir::DataDir;
use db::Db;
use skywave_proto::{BlobId, BlobRef};

/// Locks shared server state, recovering it from a poisoned mutex.
///
/// A request handler that panics on a blocking thread poisons the mutex it
/// held. Failing on poison would turn that one bad request into a 500 for
/// every later one while the process stays up. The guarded state is still
/// consistent: a database transaction rolls back when it is dropped during
/// the unwind, and the other guarded maps are updated in single steps.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

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

    /// Stores `content` as a new blob, for an event to reference.
    pub fn put_blob(&self, content: impl Read) -> Result<BlobRef> {
        Ok(self.blobs.put(self.db.conn(), content)?.blob_ref())
    }

    /// Opens blob `id` for `GET /v1/blobs/{id}`; `None` if it is unknown.
    pub fn open_blob(&self, id: BlobId) -> Result<Option<BlobFile>> {
        self.blobs.open(self.db.conn(), id)
    }

    /// `serve`'s startup pass over the blobs, once its address is bound and
    /// before anything that writes them starts: deletes the workdir blobs no
    /// answer names ([`workdir::collect_garbage`]), then sweeps the files no
    /// row records ([`BlobStore::sweep`]). The caller must hold the data
    /// dir's [`ServeLock`](data_dir::ServeLock), so no other daemon is
    /// writing blobs here.
    pub fn collect_blobs(&mut self) -> Result<()> {
        let collected = workdir::collect_garbage(&mut self.db, &self.blobs)?;
        let swept = self.blobs.sweep(self.db.conn())?;
        tracing::info!(collected, swept, "collected unused blobs");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_recovers_a_mutex_poisoned_by_a_panic() {
        let shared = std::sync::Arc::new(Mutex::new(1));
        let held = shared.clone();
        let panicked = std::thread::spawn(move || {
            let mut value = held.lock().unwrap();
            *value = 2;
            panic!("a request handler panics while holding the lock");
        })
        .join();
        assert!(panicked.is_err());
        assert!(shared.is_poisoned());
        assert_eq!(*lock(&shared), 2);
    }

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

    #[test]
    fn blob_put_then_get() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::open(tmp.path()).unwrap();
        let blob = storage.put_blob(&b"tool output"[..]).unwrap();
        assert_eq!(blob.size, 11);
        let mut content = String::new();
        storage
            .open_blob(blob.id)
            .unwrap()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        assert_eq!(content, "tool output");
        assert!(storage.open_blob(BlobId::new()).unwrap().is_none());
    }
}
