//! The Skywave server daemon (architecture.md "Server").
//!
//! The binary is a thin CLI over this library so later server pieces and
//! integration tests share one implementation.

pub mod agent;
pub mod api;
pub mod attachments;
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
pub mod safety_feedback;
pub mod screen;
pub mod server;
pub mod shell;
pub mod storage;
pub mod supervisor;
pub mod workdir;
pub mod workdir_write;
pub mod worktree;

use std::io::Read;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, PoisonError};

pub use error::{Error, Result};
pub use event_log::EventLog;
pub use supervisor::{Activity, Supervisor, SupervisorConfig, SupervisorHandle};

use blobs::{BlobFile, BlobStore};
use data_dir::DataDir;
use db::Db;
use skywave_proto::{BlobId, BlobRef, CommandResult};
use workdir::Git;

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
    /// row records ([`BlobStore::sweep`]), then the staged uploads too old
    /// or half-written ([`attachments::Staging::sweep`]). The caller must
    /// hold the data dir's [`ServeLock`](data_dir::ServeLock), so no other
    /// daemon is writing blobs or uploads here.
    pub fn collect_blobs(&mut self) -> Result<()> {
        let collected = workdir::collect_garbage(&mut self.db, &self.blobs)?;
        let swept = self.blobs.sweep(self.db.conn())?;
        let staged = attachments::Staging::new(self.dir.attachments_dir())
            .sweep(attachments::STAGING_MAX_AGE)?;
        tracing::info!(
            collected,
            swept,
            staged,
            "collected unused blobs and uploads"
        );
        Ok(())
    }

    /// `serve`'s startup undo of the worktrees a crash left between `git
    /// worktree add` and a session's commit, one row per intent. A command
    /// the log accepted committed its session, so its row is only dropped.
    /// Any other row goes to [`worktree::sweep`], which touches only what git
    /// still matches. Every row is dropped once handled. The caller must hold
    /// the data dir's [`ServeLock`](data_dir::ServeLock), and run this before
    /// the API can create worktrees.
    pub fn sweep_worktrees(&mut self, git: &Git) -> Result<()> {
        let conn = self.db.conn();
        let intents = event_log::worktree_intents_in(conn)?;
        let swept = intents.len();
        for (intent, command) in intents {
            let accepted = matches!(
                commands::stored_result(conn, command)?,
                Some(CommandResult::Accepted { .. })
            );
            if !accepted {
                worktree::sweep(git, &intent);
            }
            event_log::forget_worktree_intent_in(
                conn,
                &intent.repo,
                &intent.branch,
                &intent.attempt,
            )?;
        }
        tracing::info!(swept, "swept worktree intents");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use skywave_proto::CommandId;

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

    /// Records a worktree intent for `branch` as `create` would, for `command`.
    fn record_intent(
        storage: &Storage,
        repo: &Path,
        branch: &str,
        target: &Path,
        command: CommandId,
    ) {
        let intent = worktree::Intent {
            repo: repo.to_str().unwrap().to_owned(),
            branch: branch.to_owned(),
            target: target.to_str().unwrap().to_owned(),
            start: "0".repeat(40),
            attempt: "a".to_owned(),
        };
        event_log::record_worktree_intent_in(storage.db.conn(), &intent, command).unwrap();
    }

    fn intent_rows(storage: &Storage) -> i64 {
        let count = "SELECT COUNT(*) FROM worktree_intents";
        storage
            .db
            .conn()
            .query_row(count, [], |r| r.get(0))
            .unwrap()
    }

    /// A command the log accepted committed its session: its half-made
    /// directory is left alone and its row dropped. The same directory under
    /// a command with no stored result is undone, which is the control.
    #[test]
    fn the_worktree_sweep_skips_git_for_an_accepted_command() {
        let tmp = tempfile::tempdir().unwrap();
        let mut storage = Storage::open(tmp.path().join("data")).unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir(&repo).unwrap();
        let (kept, swept) = (tmp.path().join("kept"), tmp.path().join("swept"));
        for dir in [&kept, &swept] {
            fs::create_dir(dir).unwrap();
            fs::write(dir.join(".git"), "gitdir: nowhere\n").unwrap();
        }
        let accepted = CommandId::new();
        record_intent(&storage, &repo, "feat/kept", &kept, accepted);
        record_intent(&storage, &repo, "feat/swept", &swept, CommandId::new());
        let result = r#"{"result":"accepted","seq":1}"#;
        let sql = "INSERT INTO commands (command_id, device_id, result) VALUES (?1, ?2, ?3)";
        let params = rusqlite::params![accepted.0.to_string(), "device", result];
        storage.db.conn().execute(sql, params).unwrap();
        assert_eq!(intent_rows(&storage), 2);
        storage.sweep_worktrees(&Git::default()).unwrap();
        assert_eq!(intent_rows(&storage), 0);
        assert!(kept.join(".git").exists());
        assert!(!swept.exists());
    }

    #[test]
    fn the_worktree_sweep_forgets_every_row() {
        let tmp = tempfile::tempdir().unwrap();
        let mut storage = Storage::open(tmp.path().join("data")).unwrap();
        let gone = tmp.path().join("gone");
        record_intent(
            &storage,
            &gone.join("a"),
            "feat/x",
            &gone.join("x"),
            CommandId::new(),
        );
        record_intent(
            &storage,
            &gone.join("b"),
            "feat/y",
            &gone.join("y"),
            CommandId::new(),
        );
        assert_eq!(intent_rows(&storage), 2);
        storage.sweep_worktrees(&Git::default()).unwrap();
        assert_eq!(intent_rows(&storage), 0);
    }
}
