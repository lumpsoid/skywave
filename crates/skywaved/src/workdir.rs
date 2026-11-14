//! Read-only views of a session's workdir (`GET /v1/workdir/changes`,
//! `/v1/workdir/log`, `/v1/workdir/list`, `/v1/workdir/file` and
//! `/v1/workdir/download`, D36): git changes, the commit log, listings, file
//! contents, and the content-deduplicated store their blobs go to.
//!
//! **One snapshot at a time.** [`WorkdirReader`] owns its own database
//! connection behind a mutex, apart from the API's, and a whole snapshot
//! runs with it locked, so a device mashing refresh costs one snapshot's CPU
//! and disk at a time and never holds the API's connection across file I/O.
//! Every request carries one deadline, taken when its handler starts. A
//! request that gets the lock past its deadline returns
//! [`Error::WorkdirDeadline`] at once, without touching the database or the
//! file system: its client already got a 503, and running it anyway would
//! make N refreshes cost N snapshots. The lock is waited for with
//! `try_lock` every [`LOCK_POLL`], not `lock`, so a request whose deadline
//! passes while a snapshot stuck on a hung file system still holds the lock
//! returns the same error and frees its blocking thread, rather than waiting
//! as long as the stuck one. Work inside a snapshot checks the deadline
//! between steps (directory entries, file chunks).
//!
//! **Resolution**, in this order; the first failing step answers:
//! 1. The session's stored `workdir` must exist: else `UnknownSession`.
//! 2. It must still canonicalise, under the roots, to exactly the stored
//!    path (as `ForkContext` checks): else `WorkdirGone`. A workdir that was
//!    deleted, or replaced by a symlink to elsewhere, is gone.
//! 3. The requested `path` is checked lexically: an absolute path, or one
//!    whose `..` components climb above the workdir, is `NotAllowed`
//!    whether or not its target exists.
//! 4. The workdir joined with `path` is canonicalised; any failure (missing,
//!    a component that is not a directory, no permission) is `NotFound`.
//! 5. The canonical target must lie under the canonical workdir: else
//!    `NotAllowed`. This catches symlinks, and `..` after a symlink, that
//!    lead out. A target whose relative path is not UTF-8 is `NotAllowed`.
//!
//! A path inside that leads through a symlink to a missing target outside is
//! `NotFound` by step 4, not `NotAllowed`; it reveals only that something
//! outside is missing, which is not worth a second walk to hide.
//!
//! Answers name the **canonical** target, relative to the canonical workdir
//! (empty for the workdir itself), not the requested path.
//!
//! **Listings** read at most [`SCAN_CAP`] entries, sort them by name and
//! then keep the first [`limits::WORKDIR_LIST_MAX`], so a capped listing is
//! the same subset on every call. Names that are not UTF-8 or hold control
//! characters are skipped (a head prints them). Dotfiles and `.git` are
//! included; heads decide what to hide. An entry's kind comes from
//! `symlink_metadata`; its `size` is `Some` for a regular file only (not for
//! a link to one). A symlink's target is classified by canonicalising it:
//! failure is `Dangling`, a target outside the canonical workdir `Outside`,
//! a directory `Dir`, and anything else under the workdir `File` (opening a
//! link to a FIFO then answers `NotAFile`).
//!
//! **Files**: the canonical target must be a regular file by `metadata`
//! (which follows links and never blocks): a directory is `IsADirectory`,
//! anything else `NotAFile`, and neither is opened, as a FIFO read would
//! block forever while holding the workdir lock. The file is then opened
//! non-blocking and checked again on the open handle, so a FIFO swapped in
//! after the check is still refused. Over [`limits::WORKDIR_FILE_MAX_BYTES`]
//! by `metadata` is `TooLarge` without a read; otherwise the read goes
//! through `take(cap + 1)`, so a file that grew past the cap since is
//! `TooLarge` too, never an unbounded read. A NUL in the first
//! [`BINARY_SNIFF_BYTES`] is `Binary` (git's heuristic). Neither gets a blob.
//! Open failures answer `NotFound`; a read failing midway is a server fault.
//! **Downloads** (`download`, v36) resolve and read the same way, but serve
//! binary content too, and their cap is the reader's download cap
//! ([`WorkdirReader::with_download_max`]), not `WORKDIR_FILE_MAX_BYTES`.
//! Their blob is recorded under [`AnswerKind::Download`], not `File`.
//!
//! The checks and the open are not atomic: whoever can write the workdir can
//! swap a symlink between them. That is the agent, which can read the target
//! itself, so it is accepted (D36).
//!
//! **Changes** ([`Snapshot::changes`]) resolve the workdir by steps 1 and 2
//! (there is no `path`), then run the `git` CLI there. Every child is
//! `git -C <workdir> -c core.quotePath=false -c core.fsmonitor=false
//! -c diff.external= -c diff.submodule=short --no-optional-locks --no-pager
//! <sub> ...`, with stdin
//! null, `GIT_TERMINAL_PROMPT=0`, `GIT_OPTIONAL_LOCKS=0`,
//! `GIT_NO_LAZY_FETCH=1`, `LC_ALL=C`, and every inherited `GIT_*` variable
//! removed except `GIT_CONFIG_GLOBAL`, `GIT_CONFIG_SYSTEM`,
//! `GIT_CONFIG_NOSYSTEM` and `GIT_EXEC_PATH` (a daemon started from a git
//! hook would otherwise read another repository, index or config). It leads
//! its own process group, SIGKILLed at the snapshot's deadline. Any deadline
//! reached inside `changes` (a killed child, or a check between steps)
//! answers `Failed { "timed out" }`, a 200 like git's other failures; only a
//! request that gets the lock too late is a 503, as for list and file.
//! 1. `rev-parse --is-inside-work-tree`, decided by its output: `true` goes
//!    on, `false` (inside a git dir, or a bare repo) is `NotGit`, a fatal
//!    starting `fatal: not a git repository` is `NotGit`, and anything else
//!    `Failed` with git's first stderr line (so a `safe.directory` refusal
//!    says so). A missing `git` is `Failed { "git is not installed" }`.
//! 2. **Filters**: `config --show-scope -z --get-regexp '^filter\.'` (exit
//!    1: none). Every driver with a `clean` or `process` in `local` or
//!    `worktree` scope gets `clean`, `process` emptied and `required` off on
//!    every later call, through `GIT_CONFIG_COUNT` (command scope like `-c`,
//!    but any name fits). Global and system drivers (git-lfs) run.
//! 3. Branch (`symbolic-ref`, `None` when detached), head (`rev-parse
//!    --verify`, `None` when unborn); the base is `HEAD`, or the empty tree
//!    by `hash-object -t tree /dev/null` (so a sha256 repo works).
//! 4. Entries from `diff <base> -z --raw --numstat` (raw records, then
//!    numstat records in the same order, counted to match, and checked to
//!    name the same paths for the entries kept),
//!    with the shared diff flags: `--relative` (the workdir's subtree, in its
//!    own terms), `-M`, `--no-ext-diff --no-textconv --no-color
//!    --ignore-submodules=dirty --submodule=short`, `-O/dev/null` and
//!    explicit `a/` `b/` prefixes (so the repo's config cannot reorder or
//!    reprefix headers). `--submodule=short` matters most: the repo's
//!    `diff.submodule=diff` would run an inner diff inside the submodule,
//!    without `--no-ext-diff`, so the submodule's own diff driver would run;
//!    and `log` prints lines with no `diff --git` header, which would land in
//!    the previous file's section. `short` prints a submodule as an ordinary
//!    section of `Subproject commit` lines.
//!    Paths in `ls-files --unmerged` are `Conflicted` (`git diff HEAD` says
//!    `M`). One that no raw record lists (its worktree equals `HEAD`: a
//!    modify/delete conflict with ours kept, or `checkout --ours` without
//!    `add`) is still a `Conflicted` entry, counted `0`/`0`, with the
//!    per-file diff (normally empty, so an empty blob). Untracked ones come from `ls-files --others --exclude-standard`,
//!    `lstat`ed: a symlink or a regular file, anything else skipped (a
//!    nested repository, listed `dir/`, without a call). Paths that are not
//!    UTF-8, or longer than 32 KiB, are left out (and not counted).
//! 5. Sorted by path (tracked before untracked on a tie: `git rm --cached f`
//!    with `f` on disk is both), capped at [`limits::WORKDIR_CHANGES_MAX`];
//!    `total` counts all. Both listings are read as they stream, NUL by NUL,
//!    into a max-heap holding only the entries that sort first, so memory is
//!    bounded by the cap, not by the workdir; git still runs to the end so
//!    `total` is exact. Only an untracked path the heap would take is
//!    `lstat`ed: one past the cap is counted as listed (so a FIFO or a file
//!    gone since the listing counts there).
//! 6. **Patches**: one streamed `diff <base>`, split at lines starting
//!    `diff --git ` (a body line starts with ` `, `+`, `-`, `\` or `@@`, and
//!    a path holding a newline is quoted). A section goes to the entry whose
//!    exact expected header `diff --git a/<from or path> b/<path>` it
//!    carries, never by position; consecutive sections with one header are a
//!    type change and concatenate. An entry with a path git quotes, a header
//!    shared with another kept entry, a header seen again non-consecutively
//!    or more often than its raw record gives sections (one, two for a type
//!    change: an entry past the cap may share the header), no section, or
//!    no raw record (the conflict above) takes its own `diff <base> -- :(literal)<from>
//!    :(literal)<path>`, keeping only the sections whose header is the
//!    entry's own as git prints it (quoted or not): `:(literal)p` also
//!    matches everything under a directory `p/`. A section keeps at most
//!    [`limits::WORKDIR_DIFF_MAX_BYTES`] (past it only counted:
//!    `TooLarge`), and nothing once [`limits::WORKDIR_DIFFS_MAX_BYTES`] are
//!    held (`Omitted`), so the patch is never in memory whole. A diff past
//!    the per-file cap is `TooLarge { bytes }` even when the budget was
//!    spent before it streamed. Binary (by
//!    numstat) has no blob.
//! 7. **Untracked diffs** are synthesised as new-file patches, git's
//!    format: a symlink's is its link text (`read_link`, never followed),
//!    a regular file over the per-diff cap by `lstat` is `TooLarge` (its
//!    size) unread, else it is opened `O_NOFOLLOW`, read through `take`, and
//!    a NUL in the first [`BINARY_SNIFF_BYTES`] is `Binary`. Only entries
//!    inside the cap are read; their counts are `None` when unread.
//! 8. In path order, diffs are stored until the snapshot total would pass
//!    [`limits::WORKDIR_DIFFS_MAX_BYTES`]; from there on every text diff is
//!    `Omitted`.
//!
//! **Log** ([`Snapshot::log`]) resolves the workdir by steps 1 and 2 (there
//! is no `path`), then runs `git` there with the same environment and
//! deadline as changes. The work tree is decided as in Changes step 1
//! (`NotGit`) and the branch as in Changes step 3 (`None` when detached).
//! `symbolic-ref -q HEAD` names the branch's full ref (`HEAD` when detached),
//! and `rev-parse -q --verify <that>^{commit}` its commit: exit 1 is an unborn
//! branch, an empty log, not a failure. The bare name `HEAD` is never passed
//! on, since on an unborn branch a ref called `HEAD` elsewhere would answer
//! it. Then `git log -z --no-show-signature --no-color --encoding=UTF-8
//! -n<cap + 1> --format=%H%n%h%n%at%n%an%n%s <sha> --` lists the newest
//! commits of the whole repo (a workdir below its top does not narrow it): at
//! most [`limits::WORKDIR_LOG_MAX`], with `more` when one more came back. The
//! fields are one per line (an ident holds no newline), and a bad author date
//! is no time rather than a failure.
//! `--no-show-signature` matters: a repo's `log.showSignature` with its
//! `gpg.program` would run that program. The answer names no blob, so nothing
//! is recorded (**Garbage**, above).
//!
//! The snapshot is not atomic (the agent may edit between git calls). The
//! header pairing keeps each diff its own file's for edits made in passing.
//! An agent that swaps renames between the raw call and the patch call on
//! purpose can mispair two files it created itself, both inside the
//! workdir. That is accepted, like the check-then-open race (D36).
//!
//! **Blobs, deduplicated by content** ([`Snapshot::store`]): the content is
//! hashed in memory (it is capped) and `(sha256, size)` looked up in
//! `workdir_blobs`. A hit reuses that blob, so an unchanged file keeps its
//! [`BlobId`](skywave_proto::BlobId) across refreshes and no device fetches
//! it twice. A miss writes and syncs the blob file first, holding no
//! database lock, then inserts its `blobs` and `workdir_blobs` rows in one
//! short transaction (removing the file if that fails). Only workdir content is ever looked up: event blobs (tool
//! output) keep their own random ids, so the core's per-session blob
//! deletion can never delete a blob a snapshot still names, and no event
//! ever names a workdir blob.
//!
//! **Garbage.** A workdir blob is live while the latest answer to some
//! read names it, whatever the session: `workdir_answer_blobs` keeps, per
//! read (session, `changes`, `file` or `download`, the path as requested,
//! `''` for changes), the blobs its latest answer named, the key and the rule the
//! core keeps its stored answers by. A changes, file or download answer
//! replaces the read's rows, except `ChangesResult::Failed`, which the core never
//! stores either; a list names no blob. In the same transaction, every
//! `workdir_blobs` row no answer names goes with its `blobs` row; the
//! files are removed after the commit. So a refresh deletes the content
//! the answer before it named and nothing else names, and a snapshot that
//! stored blobs and then failed leaves them to the next answer's pass.
//! [`collect_garbage`] runs the same pass at startup. A device that still
//! holds a replaced answer (another device refreshed the read, or the reply
//! was lost) gets a 404 for its blob and shows the download failed until
//! it refreshes.
//! This never loses a live blob to a concurrent store: every lookup,
//! registration, answer and collection runs on this module's connection
//! under the snapshot mutex, and a snapshot records its answer before it
//! lets go, so no pass can run between a store reusing a blob and the
//! answer naming it. (The startup pass runs before the reader exists.)
//! Event blobs are never in `workdir_blobs`, so never collected.

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, TryLockError};
use std::time::{Duration, Instant};

use chrono::Utc;
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use skywave_proto::{
    BlobId, BlobRef, ContentHash, DirEntry, EntryKind, FileBody, FileResult, LinkTarget,
    ListResult, SessionId, WorkdirQuery, limits,
};

use crate::blobs::{BlobMeta, BlobStore};
use crate::db::Db;
use crate::dirs;
use crate::error::{Error, IoContext, Result};
use crate::event_log::{self, parse_id};

mod changes;
pub(crate) mod git;
mod log;
mod worktrees;

pub use git::Git;

/// How long one snapshot may run, counted from when its handler started
/// (queueing for the lock included). Shorter than the API's outer
/// `WORKDIR_TIMEOUT`, so a snapshot normally stops before its client is
/// told 503.
pub const SNAPSHOT_DEADLINE: Duration = Duration::from_secs(15);
/// Directory entries read at most per listing, as for `GET /v1/dirs`.
pub const SCAN_CAP: usize = dirs::SCAN_CAP;
/// Content with a NUL in its first this many bytes is binary.
pub const BINARY_SNIFF_BYTES: usize = 8 * 1024;
/// Largest file `GET /v1/workdir/download` serves unless configured otherwise, in bytes.
pub const DEFAULT_DOWNLOAD_MAX: u64 = 64 * 1024 * 1024;
/// Read size for file contents; the deadline is checked between reads.
const READ_CHUNK: usize = 64 * 1024;
/// How often a snapshot waiting for the lock tries it again.
const LOCK_POLL: Duration = Duration::from_millis(10);

/// Reads sessions' workdirs over its own database connection, one snapshot
/// at a time. See the module docs.
pub struct WorkdirReader {
    db: Mutex<Db>,
    blobs: BlobStore,
    roots: Vec<PathBuf>,
    git: Git,
    download_max: u64,
}

impl WorkdirReader {
    /// A reader over `db`, a connection of its own; `roots` are the
    /// configured session roots. It runs `git` from `PATH`.
    pub fn new(db: Db, blobs: BlobStore, roots: Vec<PathBuf>) -> WorkdirReader {
        WorkdirReader {
            db: Mutex::new(db),
            blobs,
            roots,
            git: Git::default(),
            download_max: DEFAULT_DOWNLOAD_MAX,
        }
    }

    /// This reader running git as `git` says.
    pub fn with_git(mut self, git: Git) -> WorkdirReader {
        self.git = git;
        self
    }

    /// This reader serving downloads up to `max` bytes.
    pub fn with_download_max(mut self, max: u64) -> WorkdirReader {
        self.download_max = max;
        self
    }

    /// Runs `f` as one snapshot, holding the workdir connection. Blocking.
    ///
    /// [`Error::WorkdirDeadline`] without running `f` when the lock is still
    /// held by another snapshot at `deadline`, or was got at or past it.
    pub fn snapshot<T>(
        &self,
        deadline: Instant,
        f: impl FnOnce(&mut Snapshot<'_>) -> Result<T>,
    ) -> Result<T> {
        let mut db = loop {
            match self.db.try_lock() {
                Ok(db) => break db,
                Err(TryLockError::Poisoned(poisoned)) => break poisoned.into_inner(),
                Err(TryLockError::WouldBlock) => {
                    if Instant::now() >= deadline {
                        return Err(Error::WorkdirDeadline);
                    }
                    std::thread::sleep(LOCK_POLL);
                }
            }
        };
        let mut snapshot = Snapshot {
            db: &mut db,
            blobs: &self.blobs,
            roots: &self.roots,
            git: &self.git,
            download_max: self.download_max,
            deadline,
        };
        snapshot.check_deadline()?;
        f(&mut snapshot)
    }
}

/// One snapshot in progress: the locked workdir connection and the request's
/// deadline.
pub struct Snapshot<'a> {
    db: &'a mut Db,
    blobs: &'a BlobStore,
    roots: &'a [PathBuf],
    git: &'a Git,
    download_max: u64,
    deadline: Instant,
}

/// Where a request's `path` led.
#[derive(Debug, PartialEq, Eq)]
enum Located {
    /// The canonical target and its path relative to the canonical workdir.
    Found {
        workdir: PathBuf,
        target: PathBuf,
        path: String,
    },
    UnknownSession,
    WorkdirGone,
    NotAllowed,
    NotFound,
}

impl Snapshot<'_> {
    /// [`Error::WorkdirDeadline`] once the deadline is reached.
    pub fn check_deadline(&self) -> Result<()> {
        if Instant::now() >= self.deadline {
            return Err(Error::WorkdirDeadline);
        }
        Ok(())
    }

    /// The canonical workdir of `session`, steps 1 and 2 of the module
    /// docs: `Err` holds the answer when there is none.
    fn workdir(&self, session: SessionId) -> Result<Result<PathBuf, Located>> {
        let Some(info) = event_log::session_in(self.db.conn(), session)? else {
            return Ok(Err(Located::UnknownSession));
        };
        Ok(
            match dirs::canonical_under_roots(self.roots, &info.workdir) {
                Ok(canonical) if canonical == info.workdir => Ok(PathBuf::from(canonical)),
                _ => Err(Located::WorkdirGone),
            },
        )
    }

    /// Resolves `query` (module docs, "Resolution").
    fn locate(&self, query: &WorkdirQuery) -> Result<Located> {
        let workdir = match self.workdir(query.session)? {
            Ok(workdir) => workdir,
            Err(answer) => return Ok(answer),
        };
        let requested = Path::new(&query.path);
        if requested.is_absolute() || climbs_out(requested) {
            return Ok(Located::NotAllowed);
        }
        let Ok(target) = std::fs::canonicalize(workdir.join(requested)) else {
            return Ok(Located::NotFound);
        };
        let Some(path) = target
            .strip_prefix(&workdir)
            .ok()
            .and_then(|rel| rel.to_str())
            .map(str::to_owned)
        else {
            return Ok(Located::NotAllowed);
        };
        Ok(Located::Found {
            workdir,
            target,
            path,
        })
    }

    /// `GET /v1/workdir/list` (module docs, "Listings").
    pub fn list(&mut self, query: &WorkdirQuery) -> Result<ListResult> {
        let (workdir, target, path) = match self.locate(query)? {
            Located::Found {
                workdir,
                target,
                path,
            } => (workdir, target, path),
            Located::UnknownSession => return Ok(ListResult::UnknownSession),
            Located::WorkdirGone => return Ok(ListResult::WorkdirGone),
            Located::NotAllowed => return Ok(ListResult::NotAllowed),
            Located::NotFound => return Ok(ListResult::NotFound),
        };
        if !target.is_dir() {
            return Ok(ListResult::NotADirectory);
        }
        let Ok(read) = std::fs::read_dir(&target) else {
            return Ok(ListResult::NotFound);
        };
        let mut entries = Vec::new();
        let mut truncated = false;
        for (scanned, entry) in read.enumerate() {
            self.check_deadline()?;
            if scanned == SCAN_CAP {
                truncated = true;
                break;
            }
            let Ok(entry) = entry else { continue };
            if let Some(entry) = dir_entry(&workdir, &entry) {
                entries.push(entry);
            }
        }
        entries.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        if entries.len() > limits::WORKDIR_LIST_MAX {
            entries.truncate(limits::WORKDIR_LIST_MAX);
            truncated = true;
        }
        Ok(ListResult::Listed {
            at: Utc::now(),
            path,
            entries,
            truncated,
        })
    }

    /// `GET /v1/workdir/file` (module docs, "Files"), recorded as the read's
    /// latest answer (module docs, "Garbage").
    pub fn file(&mut self, query: &WorkdirQuery) -> Result<FileResult> {
        let answer = self.read_file(query, limits::WORKDIR_FILE_MAX_BYTES, true)?;
        self.answer_file(query, AnswerKind::File, answer)
    }

    /// `GET /v1/workdir/download` (module docs, "Files"): any file as a blob up
    /// to the download cap, recorded apart from `file` so neither answer frees
    /// the other's blob.
    pub fn download(&mut self, query: &WorkdirQuery) -> Result<FileResult> {
        let answer = self.read_file(query, self.download_max, false)?;
        self.answer_file(query, AnswerKind::Download, answer)
    }

    /// Records `answer`'s blob, if it names one, as the latest answer of `kind`
    /// to the read, then returns it.
    fn answer_file(
        &mut self,
        query: &WorkdirQuery,
        kind: AnswerKind,
        answer: FileResult,
    ) -> Result<FileResult> {
        let blobs = match &answer {
            FileResult::File {
                content: FileBody::Blob(blob),
                ..
            } => vec![blob.id],
            _ => Vec::new(),
        };
        self.answered(query.session, kind, &query.path, &blobs)?;
        Ok(answer)
    }

    /// The read behind `file` and `download`: `cap` is the largest content
    /// served, and `text_only` refuses binary content (`Binary`).
    fn read_file(&mut self, query: &WorkdirQuery, cap: u64, text_only: bool) -> Result<FileResult> {
        let (target, path) = match self.locate(query)? {
            Located::Found { target, path, .. } => (target, path),
            Located::UnknownSession => return Ok(FileResult::UnknownSession),
            Located::WorkdirGone => return Ok(FileResult::WorkdirGone),
            Located::NotAllowed => return Ok(FileResult::NotAllowed),
            Located::NotFound => return Ok(FileResult::NotFound),
        };
        let Ok(meta) = std::fs::metadata(&target) else {
            return Ok(FileResult::NotFound);
        };
        if meta.is_dir() {
            return Ok(FileResult::IsADirectory);
        }
        if !meta.is_file() {
            return Ok(FileResult::NotAFile);
        }
        let file_answer = |size, content| FileResult::File {
            at: Utc::now(),
            path: path.clone(),
            size,
            content,
        };
        if meta.len() > cap {
            return Ok(file_answer(meta.len(), FileBody::TooLarge));
        }
        let Ok(mut file) = open_nonblocking(&target) else {
            return Ok(FileResult::NotFound);
        };
        if !file.metadata().is_ok_and(|m| m.is_file()) {
            return Ok(FileResult::NotAFile);
        }
        let content = self.read_capped(&mut file, &target, cap)?;
        if content.len() as u64 > cap {
            let size = file
                .metadata()
                .map_or(content.len() as u64, |m| m.len())
                .max(content.len() as u64);
            return Ok(file_answer(size, FileBody::TooLarge));
        }
        let size = content.len() as u64;
        if text_only && looks_binary(&content) {
            return Ok(file_answer(size, FileBody::Binary));
        }
        let blob = self.store(&content)?;
        Ok(file_answer(size, FileBody::Blob(blob)))
    }

    /// At most `cap + 1` bytes of `file`, checking the deadline between
    /// chunks.
    fn read_capped(&self, file: &mut File, path: &Path, cap: u64) -> Result<Vec<u8>> {
        let mut capped = file.take(cap + 1);
        let mut content = Vec::new();
        let mut chunk = vec![0u8; READ_CHUNK];
        loop {
            self.check_deadline()?;
            let n = match capped.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    return Err(e).context(|| format!("reading {}", path.display()));
                }
            };
            content.extend_from_slice(&chunk[..n]);
        }
        Ok(content)
    }

    /// Stores workdir `content` as a blob, reusing the blob of identical
    /// content stored before (module docs, "Blobs"). Never for event blobs.
    pub fn store(&mut self, content: &[u8]) -> Result<BlobRef> {
        let sha256 = ContentHash::of(content);
        let size = content.len() as u64;
        let sql_size = i64::try_from(size).unwrap_or(i64::MAX);
        // A plain read, no write lock. Only this connection writes
        // `workdir_blobs`, and every workdir request is serialised by the
        // snapshot mutex, so no other store can insert this content between
        // this lookup and the insert below.
        let known: Option<String> = self
            .db
            .conn()
            .query_row(
                "SELECT blob_id FROM workdir_blobs WHERE sha256 = ?1 AND size = ?2",
                params![sha256.to_hex(), sql_size],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = known {
            return Ok(BlobRef {
                id: parse_id(&id)?,
                size,
                sha256,
            });
        }
        // The file (up to the cap, plus its fsyncs) is written before the
        // write lock is taken, so other connections wait only for the two
        // inserts, never for the disk.
        self.check_deadline()?;
        let meta = self.blobs.write(content)?;
        if let Err(e) = self.check_deadline().and_then(|()| self.record(&meta)) {
            self.blobs.discard(meta.id);
            return Err(e);
        }
        Ok(meta.blob_ref())
    }

    /// Inserts the `blobs` and `workdir_blobs` rows of a written blob in one
    /// short transaction.
    fn record(&mut self, meta: &BlobMeta) -> Result<()> {
        let tx = self
            .db
            .conn_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        self.blobs.record(&tx, meta)?;
        tx.execute(
            "INSERT INTO workdir_blobs (sha256, size, blob_id) VALUES (?1, ?2, ?3)",
            params![
                meta.sha256.to_hex(),
                i64::try_from(meta.size).unwrap_or(i64::MAX),
                meta.id.0.to_string()
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Records `blobs` as all that the latest answer to the read
    /// (`session`, `kind`, `path`) names, replacing what the answer before
    /// named, then deletes every workdir blob no answer names (module docs,
    /// "Garbage"). One short transaction; the files go after it.
    pub(crate) fn answered(
        &mut self,
        session: SessionId,
        kind: AnswerKind,
        path: &str,
        blobs: &[BlobId],
    ) -> Result<()> {
        let tx = self
            .db
            .conn_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let key = params![session.0.to_string(), kind.as_str(), path];
        tx.execute(
            "DELETE FROM workdir_answer_blobs
             WHERE session_id = ?1 AND kind = ?2 AND path = ?3",
            key,
        )?;
        for blob in blobs {
            tx.execute(
                "INSERT OR IGNORE INTO workdir_answer_blobs (session_id, kind, path, blob_id)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    session.0.to_string(),
                    kind.as_str(),
                    path,
                    blob.0.to_string()
                ],
            )?;
        }
        let unnamed = delete_unnamed(&tx)?;
        tx.commit()?;
        remove_files(self.blobs, &unnamed);
        Ok(())
    }
}

/// The reads whose answers name blobs, as `workdir_answer_blobs.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AnswerKind {
    Changes,
    File,
    Download,
}

impl AnswerKind {
    fn as_str(self) -> &'static str {
        match self {
            AnswerKind::Changes => "changes",
            AnswerKind::File => "file",
            AnswerKind::Download => "download",
        }
    }
}

/// Deletes every workdir blob no stored answer names, as after an answer
/// (module docs, "Garbage"); returns how many. For startup, before a
/// [`WorkdirReader`] exists: run beside one, a blob it just stored but has
/// not yet named in an answer would go.
pub fn collect_garbage(db: &mut Db, blobs: &BlobStore) -> Result<usize> {
    let tx = db
        .conn_mut()
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    let unnamed = delete_unnamed(&tx)?;
    tx.commit()?;
    remove_files(blobs, &unnamed);
    Ok(unnamed.len())
}

/// Deletes the `workdir_blobs` and `blobs` rows of every workdir blob no
/// answer names, within the caller's transaction; returns their ids, whose
/// files go once it commits.
fn delete_unnamed(tx: &rusqlite::Transaction<'_>) -> Result<Vec<BlobId>> {
    let ids: Vec<String> = {
        let mut stmt = tx.prepare(
            "SELECT blob_id FROM workdir_blobs w WHERE NOT EXISTS
               (SELECT 1 FROM workdir_answer_blobs a WHERE a.blob_id = w.blob_id)",
        )?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut unnamed = Vec::with_capacity(ids.len());
    for id in ids {
        tx.execute("DELETE FROM workdir_blobs WHERE blob_id = ?1", [&id])?;
        tx.execute("DELETE FROM blobs WHERE id = ?1", [&id])?;
        unnamed.push(parse_id(&id)?);
    }
    Ok(unnamed)
}

/// Removes the files of blobs whose rows were deleted. A file left behind
/// (a crash before this) is never served, and the startup sweep removes it.
fn remove_files(blobs: &BlobStore, ids: &[BlobId]) {
    for &id in ids {
        blobs.discard(id);
    }
    if !ids.is_empty() {
        tracing::debug!(blobs = ids.len(), "deleted workdir blobs no answer names");
    }
}

/// Whether a relative `path` has a `..` that climbs above where it starts.
fn climbs_out(path: &Path) -> bool {
    let mut depth = 0usize;
    for component in path.components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir => match depth.checked_sub(1) {
                Some(up) => depth = up,
                None => return true,
            },
            Component::RootDir | Component::Prefix(_) => return true,
        }
    }
    false
}

/// The listing entry for `entry`, or `None` when it is skipped.
fn dir_entry(workdir: &Path, entry: &std::fs::DirEntry) -> Option<DirEntry> {
    let name = entry.file_name().into_string().ok()?;
    if name.chars().any(char::is_control) {
        return None;
    }
    let meta = entry.metadata().ok()?; // does not follow a symlink
    let kind = meta.file_type();
    let (kind, size) = if kind.is_symlink() {
        let to = match std::fs::canonicalize(entry.path()) {
            Err(_) => LinkTarget::Dangling,
            Ok(target) if !target.starts_with(workdir) => LinkTarget::Outside,
            Ok(target) if target.is_dir() => LinkTarget::Dir,
            Ok(_) => LinkTarget::File,
        };
        (EntryKind::Link { to }, None)
    } else if kind.is_dir() {
        (EntryKind::Dir, None)
    } else if kind.is_file() {
        (EntryKind::File, Some(meta.len()))
    } else {
        (EntryKind::Other, None)
    };
    Some(DirEntry { name, kind, size })
}

/// Opens `path` for reading without blocking on a FIFO.
fn open_nonblocking(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    options.open(path)
}

/// git's heuristic: a NUL in the first [`BINARY_SNIFF_BYTES`].
pub fn looks_binary(content: &[u8]) -> bool {
    content[..content.len().min(BINARY_SNIFF_BYTES)].contains(&0)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;

    use skywave_proto::{
        Command, CommandId, CommandKind, CommandResult, ContextId, DeviceId, EventKind,
    };

    use super::*;
    use crate::{EventLog, Storage};

    /// A temp dir holding `root/` (the only root), `root/project/` (the
    /// session's workdir) and `outside/`, with a data dir and a session.
    pub(super) struct Tree {
        _tmp: tempfile::TempDir,
        pub(super) root: PathBuf,
        pub(super) workdir: PathBuf,
        pub(super) outside: PathBuf,
        pub(super) storage: Storage,
        reader: WorkdirReader,
        session: SessionId,
    }

    impl Tree {
        pub(super) fn new() -> Tree {
            let tmp = tempfile::tempdir().unwrap();
            let base = std::fs::canonicalize(tmp.path()).unwrap();
            let root = base.join("root");
            let workdir = root.join("project");
            let outside = base.join("outside");
            std::fs::create_dir_all(workdir.join("src")).unwrap();
            std::fs::create_dir_all(&outside).unwrap();
            let mut storage = Storage::open(base.join("data")).unwrap();
            let log = EventLog::open(&storage.db, vec![root.clone()]).unwrap();
            let create = Command {
                id: CommandId::new(),
                kind: CommandKind::CreateSession {
                    workdir: workdir.to_str().unwrap().to_owned(),
                    conversation: None,
                    worktree: None,
                },
            };
            log.handle(&mut storage.db, DeviceId::new(), &create)
                .unwrap();
            let session = log.sessions(&storage.db).unwrap()[0].id;
            let reader = WorkdirReader::new(
                Db::open(&storage.dir.db_path()).unwrap(),
                storage.blobs.clone(),
                vec![root.clone()],
            );
            Tree {
                _tmp: tmp,
                root,
                workdir,
                outside,
                storage,
                reader,
                session,
            }
        }

        pub(super) fn query(&self, path: &str) -> WorkdirQuery {
            WorkdirQuery {
                session: self.session,
                path: path.to_owned(),
            }
        }

        pub(super) fn later() -> Instant {
            Instant::now() + SNAPSHOT_DEADLINE
        }

        fn list(&self, path: &str) -> ListResult {
            let query = self.query(path);
            self.reader
                .snapshot(Tree::later(), |s| s.list(&query))
                .unwrap()
        }

        fn file(&self, path: &str) -> FileResult {
            let query = self.query(path);
            self.reader
                .snapshot(Tree::later(), |s| s.file(&query))
                .unwrap()
        }

        fn download(&self, path: &str) -> FileResult {
            let query = self.query(path);
            self.reader
                .snapshot(Tree::later(), |s| s.download(&query))
                .unwrap()
        }

        fn entries(&self, path: &str) -> (String, Vec<DirEntry>, bool) {
            match self.list(path) {
                ListResult::Listed {
                    path,
                    entries,
                    truncated,
                    ..
                } => (path, entries, truncated),
                other => panic!("expected a listing, got {other:?}"),
            }
        }

        /// The path and body of a file answer.
        fn read(&self, path: &str) -> (String, u64, FileBody) {
            match self.file(path) {
                FileResult::File {
                    path,
                    size,
                    content,
                    ..
                } => (path, size, content),
                other => panic!("expected a file, got {other:?}"),
            }
        }

        fn blob_rows(&self) -> i64 {
            self.storage
                .db
                .conn()
                .query_row("SELECT COUNT(*) FROM blobs", [], |r| r.get(0))
                .unwrap()
        }

        /// Whether blob `id` is still stored: its row, checked against its
        /// file (a row without its file, or the reverse, fails the test).
        pub(super) fn stored(&self, id: BlobId) -> bool {
            let row = self.storage.open_blob(id).unwrap().is_some();
            let file = self.storage.dir.blobs_dir().join(id.0.to_string()).exists();
            assert_eq!(row, file, "blob {} has a row xor a file", id.0);
            row
        }

        /// The blob of the file answer to `path` read in `session`.
        fn blob_in(&self, session: SessionId, path: &str) -> BlobId {
            let query = WorkdirQuery {
                session,
                path: path.to_owned(),
            };
            match self.reader.snapshot(Tree::later(), |s| s.file(&query)) {
                Ok(FileResult::File {
                    content: FileBody::Blob(blob),
                    ..
                }) => blob.id,
                other => panic!("expected a file blob, got {other:?}"),
            }
        }

        /// Another session in the same workdir.
        fn second_session(&mut self) -> SessionId {
            let log = EventLog::open(&self.storage.db, vec![self.root.clone()]).unwrap();
            let create = Command {
                id: CommandId::new(),
                kind: CommandKind::CreateSession {
                    workdir: self.workdir.to_str().unwrap().to_owned(),
                    conversation: None,
                    worktree: None,
                },
            };
            log.handle(&mut self.storage.db, DeviceId::new(), &create)
                .unwrap();
            let sessions = log.sessions(&self.storage.db).unwrap();
            let other = sessions.iter().find(|s| s.id != self.session).unwrap();
            other.id
        }

        pub(super) fn blob_content(&self, blob: &BlobRef) -> Vec<u8> {
            let mut file = self
                .storage
                .open_blob(blob.id)
                .unwrap()
                .expect("blob stored");
            let mut all = Vec::new();
            file.read_to_end(&mut all).unwrap();
            all
        }
    }

    fn entry(name: &str, kind: EntryKind, size: Option<u64>) -> DirEntry {
        DirEntry {
            name: name.to_owned(),
            kind,
            size,
        }
    }

    #[test]
    fn escaping_paths_are_not_allowed_for_list_and_file() {
        let t = Tree::new();
        std::fs::write(t.outside.join("secret"), "s").unwrap();
        symlink(&t.outside, t.workdir.join("out")).unwrap();
        symlink(t.outside.join("secret"), t.workdir.join("secret-link")).unwrap();
        let outside = t.outside.to_str().unwrap();
        for path in [
            "..",
            "../outside",
            "../does-not-exist",
            "src/../../nope",
            outside,
            "/etc",
            "out",
            "out/secret",
            "secret-link",
            "src/../out",
        ] {
            assert_eq!(t.list(path), ListResult::NotAllowed, "list {path}");
            assert_eq!(t.file(path), FileResult::NotAllowed, "file {path}");
        }
        // Positive controls: `..` that stays inside, and a missing path.
        assert_eq!(t.entries("src/..").0, "");
        assert_eq!(t.list("missing"), ListResult::NotFound);
        assert_eq!(t.file("src/missing"), FileResult::NotFound);
    }

    #[test]
    fn a_link_to_a_file_inside_opens_under_its_canonical_path() {
        let t = Tree::new();
        std::fs::write(t.workdir.join("src/main.rs"), "fn main() {}\n").unwrap();
        symlink(t.workdir.join("src/main.rs"), t.workdir.join("entry")).unwrap();
        let (path, size, body) = t.read("entry");
        assert_eq!(path, "src/main.rs");
        assert_eq!(size, 13);
        let FileBody::Blob(blob) = body else {
            panic!("expected a blob, got {body:?}");
        };
        assert_eq!(blob.size, 13);
        assert_eq!(t.blob_content(&blob), b"fn main() {}\n");
        assert_eq!(t.read("./src/../src/main.rs").0, "src/main.rs");
    }

    #[test]
    fn listing_classifies_every_kind_of_entry() {
        let t = Tree::new();
        std::fs::write(t.workdir.join("a.txt"), "hello").unwrap();
        std::fs::write(t.workdir.join(".env"), "K=V").unwrap();
        symlink(t.workdir.join("src"), t.workdir.join("l-dir")).unwrap();
        symlink(t.workdir.join("a.txt"), t.workdir.join("l-file")).unwrap();
        symlink(&t.outside, t.workdir.join("l-out")).unwrap();
        symlink(t.workdir.join("gone"), t.workdir.join("l-dangling")).unwrap();
        mkfifo(&t.workdir.join("pipe"));
        std::fs::create_dir(t.workdir.join("evil\x1b[2J")).unwrap();
        let (path, entries, truncated) = t.entries("");
        assert_eq!(path, "");
        assert!(!truncated);
        let link = |to| EntryKind::Link { to };
        assert_eq!(
            entries,
            vec![
                entry(".env", EntryKind::File, Some(3)),
                entry("a.txt", EntryKind::File, Some(5)),
                entry("l-dangling", link(LinkTarget::Dangling), None),
                entry("l-dir", link(LinkTarget::Dir), None),
                entry("l-file", link(LinkTarget::File), None),
                entry("l-out", link(LinkTarget::Outside), None),
                entry("pipe", EntryKind::Other, None),
                entry("src", EntryKind::Dir, None),
            ]
        );
        // A subdirectory answers with its canonical path; a file is no
        // directory.
        assert_eq!(t.entries("l-dir").0, "src");
        assert_eq!(t.list("a.txt"), ListResult::NotADirectory);
    }

    #[test]
    fn a_workdir_gone_or_replaced_and_an_unknown_session_are_answered() {
        let t = Tree::new();
        let other = WorkdirQuery {
            session: SessionId::new(),
            path: String::new(),
        };
        let list = |q: &WorkdirQuery| t.reader.snapshot(Tree::later(), |s| s.list(q)).unwrap();
        assert_eq!(list(&other), ListResult::UnknownSession);
        assert_eq!(
            t.reader
                .snapshot(Tree::later(), |s| s.file(&other))
                .unwrap(),
            FileResult::UnknownSession
        );

        // Replaced by a symlink to another directory under the root.
        std::fs::create_dir(t.root.join("elsewhere")).unwrap();
        std::fs::rename(&t.workdir, t.root.join("moved")).unwrap();
        symlink(t.root.join("elsewhere"), &t.workdir).unwrap();
        assert_eq!(t.list(""), ListResult::WorkdirGone);
        assert_eq!(t.file("x"), FileResult::WorkdirGone);

        // Deleted.
        std::fs::remove_file(&t.workdir).unwrap();
        assert_eq!(t.list(""), ListResult::WorkdirGone);
        assert_eq!(t.file("x"), FileResult::WorkdirGone);

        // Positive control: back in place, it lists again.
        std::fs::rename(t.root.join("moved"), &t.workdir).unwrap();
        assert_eq!(t.entries("").1.len(), 1);
    }

    #[test]
    fn files_that_are_not_text_or_too_large_get_no_blob() {
        let t = Tree::new();
        let max = limits::WORKDIR_FILE_MAX_BYTES;
        std::fs::write(t.workdir.join("big"), vec![b'a'; max as usize + 1]).unwrap();
        std::fs::write(t.workdir.join("edge"), vec![b'a'; max as usize]).unwrap();
        let mut late_nul = vec![b'a'; BINARY_SNIFF_BYTES];
        late_nul.push(0);
        std::fs::write(t.workdir.join("late-nul"), &late_nul).unwrap();
        std::fs::write(t.workdir.join("bin"), b"ab\0cd").unwrap();
        let before = t.blob_rows();
        assert_eq!(t.read("big"), ("big".into(), max + 1, FileBody::TooLarge));
        assert_eq!(t.read("bin"), ("bin".into(), 5, FileBody::Binary));
        assert_eq!(t.blob_rows(), before);
        assert_eq!(t.file("src"), FileResult::IsADirectory);
        // Positive controls: exactly the cap, and a NUL past the sniff, are
        // served.
        assert!(matches!(t.read("edge").2, FileBody::Blob(b) if b.size == max));
        assert!(matches!(t.read("late-nul").2, FileBody::Blob(_)));
    }

    #[test]
    fn downloads_serve_any_file_up_to_the_download_cap() {
        let mut t = Tree::new();
        t.reader.download_max = 16;
        std::fs::write(t.workdir.join("bin"), b"ab\0cd").unwrap();
        std::fs::write(t.workdir.join("edge"), vec![b'a'; 16]).unwrap();
        std::fs::write(t.workdir.join("big"), vec![b'a'; 17]).unwrap();
        // Binary content is a blob to a download, where `file` refuses it.
        let FileResult::File {
            size: 5,
            content: FileBody::Blob(bin),
            ..
        } = t.download("bin")
        else {
            panic!("expected a binary blob");
        };
        assert_eq!(t.blob_content(&bin), b"ab\0cd");
        assert_eq!(t.read("bin"), ("bin".into(), 5, FileBody::Binary));
        // Positive control: exactly the cap is served, one byte past it is not.
        assert!(matches!(
            t.download("edge"),
            FileResult::File {
                size: 16,
                content: FileBody::Blob(_),
                ..
            }
        ));
        assert!(matches!(
            t.download("big"),
            FileResult::File {
                content: FileBody::TooLarge,
                ..
            }
        ));
        assert_eq!(t.download("src"), FileResult::IsADirectory);
        // A file answer to the same path does not free the download's blob:
        // the two answers are recorded under their own kinds.
        t.file("bin");
        assert!(t.stored(bin.id));
    }

    #[test]
    fn a_fifo_is_not_a_file_and_is_answered_at_once() {
        let t = Tree::new();
        mkfifo(&t.workdir.join("pipe"));
        symlink(t.workdir.join("pipe"), t.workdir.join("pipe-link")).unwrap();
        let (done, answers) = mpsc::channel();
        let reader = t.reader;
        let (q1, q2) = (
            WorkdirQuery {
                session: t.session,
                path: "pipe".into(),
            },
            WorkdirQuery {
                session: t.session,
                path: "pipe-link".into(),
            },
        );
        // Detached: were the read to block, the test fails on the timeout
        // instead of hanging.
        std::thread::spawn(move || {
            for q in [q1, q2] {
                let answer = reader.snapshot(Tree::later(), |s| s.file(&q)).unwrap();
                done.send(answer).unwrap();
            }
        });
        for _ in 0..2 {
            let answer = answers.recv_timeout(Duration::from_secs(10)).unwrap();
            assert_eq!(answer, FileResult::NotAFile);
        }
    }

    #[test]
    fn a_capped_listing_keeps_the_first_entries_by_name() {
        let t = Tree::new();
        let dir = t.workdir.join("many");
        std::fs::create_dir(&dir).unwrap();
        let total = limits::WORKDIR_LIST_MAX + 10;
        for i in 0..total {
            std::fs::write(dir.join(format!("f{i:05}")), "").unwrap();
        }
        let (_, first, truncated) = t.entries("many");
        assert!(truncated);
        let names: Vec<String> = first.iter().map(|e| e.name.clone()).collect();
        let want: Vec<String> = (0..limits::WORKDIR_LIST_MAX)
            .map(|i| format!("f{i:05}"))
            .collect();
        assert_eq!(names, want);
        assert_eq!(t.entries("many").1, first);
    }

    #[test]
    fn unchanged_content_reuses_its_blob_and_event_blobs_are_never_reused() {
        let t = Tree::new();
        // An event blob with the very content the file will have.
        let tool_output = t.storage.put_blob(&b"same bytes"[..]).unwrap();
        std::fs::write(t.workdir.join("f"), "same bytes").unwrap();
        let blob = |t: &Tree| match t.read("f").2 {
            FileBody::Blob(blob) => blob,
            other => panic!("expected a blob, got {other:?}"),
        };
        let first = blob(&t);
        assert_ne!(first.id, tool_output.id);
        let rows = t.blob_rows();
        assert_eq!(first.sha256, ContentHash::of(b"same bytes"));
        let second = blob(&t);
        assert_eq!(second, first, "a hit names the stored blob and its hash");
        assert_eq!(t.blob_rows(), rows, "a hit adds no blob");
        // A copy elsewhere in the workdir is the same content too.
        std::fs::write(t.workdir.join("src/copy"), "same bytes").unwrap();
        assert_eq!(t.read("src/copy").2, FileBody::Blob(first.clone()));

        std::fs::write(t.workdir.join("f"), "other bytes").unwrap();
        let changed = blob(&t);
        assert_ne!(changed.id, first.id);
        assert_eq!(t.blob_rows(), rows + 1);
        assert_eq!(t.blob_content(&changed), b"other bytes");
        assert_eq!(changed.sha256, ContentHash::of(b"other bytes"));
    }

    #[test]
    fn a_replaced_answer_deletes_the_blobs_nothing_else_names() {
        let mut t = Tree::new();
        let other = t.second_session();
        let mine = t.session;
        // An event blob with content a workdir file will have too.
        let tool_output = t.storage.put_blob(&b"shared"[..]).unwrap();
        std::fs::write(t.workdir.join("f"), "shared").unwrap();
        std::fs::write(t.workdir.join("g"), "alone").unwrap();
        let shared = t.blob_in(mine, "f");
        assert_eq!(t.blob_in(other, "f"), shared, "one blob for both sessions");
        let alone = t.blob_in(mine, "g");

        // Replacing the only answer naming a blob deletes it.
        std::fs::write(t.workdir.join("g"), "alone, changed").unwrap();
        let changed = t.blob_in(mine, "g");
        assert!(!t.stored(alone), "the replaced answer's blob is gone");
        assert!(t.stored(changed));
        assert!(matches!(t.file("g"), FileResult::File { .. }));
        // An answer that names no blob replaces one that did.
        std::fs::remove_file(t.workdir.join("g")).unwrap();
        assert_eq!(t.file("g"), FileResult::NotFound);
        assert!(!t.stored(changed));

        // Another session's answer keeps a shared blob.
        std::fs::write(t.workdir.join("f"), "mine now").unwrap();
        t.blob_in(mine, "f");
        assert!(t.stored(shared), "the other session still names it");
        t.blob_in(other, "f");
        assert!(!t.stored(shared), "no answer names it any more");

        // Event blobs are never collected, whatever their content.
        assert!(t.stored(tool_output.id));
    }

    #[test]
    fn the_startup_pass_sweeps_stray_files_and_collects_unnamed_blobs() {
        let mut t = Tree::new();
        let event = t.storage.put_blob(&b"tool output"[..]).unwrap();
        std::fs::write(t.workdir.join("f"), "named").unwrap();
        let named = t.blob_in(t.session, "f");
        // Stored by a snapshot that then failed: no answer names it.
        let stray = t
            .reader
            .snapshot(Tree::later(), |s| s.store(b"stray"))
            .unwrap();
        let dir = t.storage.dir.blobs_dir();
        std::fs::write(dir.join(".tmp-interrupted"), "partial").unwrap();
        let orphan = BlobId::new();
        std::fs::write(dir.join(orphan.0.to_string()), "renamed, never recorded").unwrap();
        std::fs::write(dir.join("not-a-blob"), "left alone").unwrap();
        assert!(t.stored(stray.id), "control: a store alone keeps it");
        // A `DeleteBlob` that committed but never removed its file: a crash
        // between the two, here a log with no store to remove it from.
        let deleted = t.storage.put_blob(&b"shell output"[..]).unwrap();
        let log = EventLog::open(&t.storage.db, vec![t.root.clone()]).unwrap();
        log.append(
            &mut t.storage.db,
            t.session,
            ContextId::first_of(t.session),
            EventKind::ToolResult(skywave_proto::ToolResult {
                call: None,
                is_error: false,
                status: None,
                preview: "shell".into(),
                lines: 1,
                output: Some(skywave_proto::Full::Blob(deleted.clone())),
            }),
        )
        .unwrap();
        let delete = Command {
            id: CommandId::new(),
            kind: CommandKind::DeleteBlob { blob: deleted.id },
        };
        assert!(matches!(
            log.handle(&mut t.storage.db, DeviceId::new(), &delete)
                .unwrap(),
            CommandResult::Accepted { .. }
        ));
        let deleted_file = dir.join(deleted.id.0.to_string());
        assert!(deleted_file.exists(), "control: the row went, the file not");

        t.storage.collect_blobs().unwrap();
        assert!(!t.stored(stray.id));
        assert!(!deleted_file.exists());
        assert!(!dir.join(".tmp-interrupted").exists());
        assert!(!dir.join(orphan.0.to_string()).exists());
        assert!(dir.join("not-a-blob").exists());
        assert!(t.stored(event.id));
        assert!(t.stored(named));
        // The named blob is still the one its content reuses.
        assert_eq!(t.blob_in(t.session, "f"), named);
    }

    #[test]
    fn a_pass_waits_for_the_snapshot_that_reused_a_blob_to_name_it() {
        let t = Tree::new();
        std::fs::write(t.workdir.join("f"), "content").unwrap();
        let first = t.blob_in(t.session, "f");
        // Refreshing `f` now leaves `first` unnamed, unless the snapshot
        // below has named it for `copy` first.
        std::fs::write(t.workdir.join("f"), "changed").unwrap();
        let (calling, called) = mpsc::channel();
        let refreshed = AtomicBool::new(false);
        let (reader, query) = (&t.reader, t.query("f"));
        std::thread::scope(|scope| {
            let refresh = reader
                .snapshot(Tree::later(), |s| {
                    let reused = s.store(b"content")?;
                    assert_eq!(reused.id, first);
                    let refresh = scope.spawn(|| {
                        calling.send(()).unwrap();
                        let answer = reader.snapshot(Tree::later(), |s| s.file(&query)).unwrap();
                        refreshed.store(true, Ordering::SeqCst);
                        answer
                    });
                    called.recv().unwrap();
                    // The refresh, and its pass, wait for this snapshot.
                    assert!(!refreshed.load(Ordering::SeqCst));
                    s.answered(t.session, AnswerKind::File, "copy", &[reused.id])?;
                    Ok(refresh)
                })
                .unwrap();
            assert!(matches!(refresh.join().unwrap(), FileResult::File { .. }));
        });
        assert!(t.stored(first), "named by `copy` before the pass ran");
    }

    #[test]
    fn a_pass_outside_the_snapshot_lock_would_lose_a_reused_blob() {
        // Why `collect_garbage` is for startup only: run beside a snapshot
        // it deletes a blob a store just reused, and naming it then fails.
        let t = Tree::new();
        // Stored by a snapshot that then failed: unnamed, not yet collected.
        let first = t
            .reader
            .snapshot(Tree::later(), |s| s.store(b"content"))
            .unwrap()
            .id;
        t.reader
            .snapshot(Tree::later(), |s| {
                let reused = s.store(b"content")?;
                assert_eq!(reused.id, first);
                let mut beside = Db::open(&t.storage.dir.db_path()).unwrap();
                assert_eq!(collect_garbage(&mut beside, &t.storage.blobs).unwrap(), 1);
                assert!(
                    s.answered(t.session, AnswerKind::File, "copy", &[reused.id])
                        .is_err(),
                    "the answer names a blob that is gone"
                );
                Ok(())
            })
            .unwrap();
        assert!(!t.stored(first));
    }

    #[test]
    fn a_request_past_its_deadline_when_it_gets_the_lock_does_nothing() {
        let t = Tree::new();
        // A workdir that is gone answers `WorkdirGone` only by reading the
        // database and the file system; past the deadline it must not.
        std::fs::remove_dir_all(&t.workdir).unwrap();
        let query = t.query("");
        let ran = AtomicBool::new(false);
        let held = t.reader.db.lock().unwrap();
        std::thread::scope(|scope| {
            let waiting = scope.spawn(|| {
                // Past its deadline by the time it can take the lock.
                t.reader.snapshot(Instant::now(), |s| {
                    ran.store(true, Ordering::SeqCst);
                    s.list(&query)
                })
            });
            drop(held);
            assert!(matches!(
                waiting.join().unwrap(),
                Err(Error::WorkdirDeadline)
            ));
        });
        assert!(
            !ran.load(Ordering::SeqCst),
            "the snapshot ran past its deadline"
        );
        // Positive control: in time, the same request does the work.
        assert_eq!(t.list(""), ListResult::WorkdirGone);
    }

    #[test]
    fn a_request_stops_waiting_for_a_stuck_lock_at_its_deadline() {
        let t = Tree::new();
        let query = t.query("");
        let ran = AtomicBool::new(false);
        let (answered, answer) = mpsc::channel();
        std::thread::scope(|scope| {
            // Held for the whole wait, like a snapshot stuck on a hung mount;
            // dropped on unwind before the scope joins, so a failure
            // cannot deadlock the test.
            let held = t.reader.db.lock().unwrap();
            scope.spawn(|| {
                let deadline = Instant::now() + Duration::from_millis(100);
                answered
                    .send(t.reader.snapshot(deadline, |s| {
                        ran.store(true, Ordering::SeqCst);
                        s.list(&query)
                    }))
                    .unwrap();
            });
            let answer = answer
                .recv_timeout(Duration::from_secs(10))
                .expect("the request waited past its deadline");
            assert!(matches!(answer, Err(Error::WorkdirDeadline)));
            drop(held);
        });
        assert!(!ran.load(Ordering::SeqCst), "the snapshot ran");
        // Positive control: with the lock free, the same request answers.
        assert!(matches!(t.list(""), ListResult::Listed { .. }));
    }

    #[test]
    fn a_read_checks_the_deadline_between_chunks() {
        let t = Tree::new();
        std::fs::write(t.workdir.join("f"), "x").unwrap();
        let query = t.query("f");
        // The lock is taken in time; the deadline passes during the work.
        let deadline = Instant::now() + Duration::from_millis(50);
        let answer = t.reader.snapshot(deadline, |s| {
            std::thread::sleep(Duration::from_millis(100));
            s.file(&query)
        });
        assert!(matches!(answer, Err(Error::WorkdirDeadline)));
    }

    #[test]
    fn climbing_is_decided_lexically() {
        for (path, out) in [
            ("", false),
            ("a/b/../..", false),
            ("./a/./..", false),
            ("..", true),
            ("a/../..", true),
            ("a/../../a", true),
            ("/a", true),
        ] {
            assert_eq!(climbs_out(Path::new(path)), out, "{path}");
        }
    }

    fn mkfifo(path: &Path) {
        let status = std::process::Command::new("mkfifo")
            .arg(path)
            .status()
            .unwrap();
        assert!(status.success());
    }
}
