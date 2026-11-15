//! Writing a staged upload into a session's workdir: the file-system half of
//! `POST` [`skywave_proto::path::WORKDIR_WRITE`]. [`write`] only answers; the
//! route, the `FileWritten` event and the `DiskLow` check are the caller's.
//!
//! **Target**, from the request's `path`, all refused as `NotAllowed` before
//! the file system is touched: empty, absolute, climbing out of the workdir
//! ([`climbs_out`]), any component `.git` (a device must never write into
//! git's own directory, where hooks would run code), or a last component that
//! is not a normal name (`a/..`). The parent directory is then canonicalised:
//! failure, or not a directory, is `NotFound`; a parent outside the canonical
//! workdir (a symlink out) is `NotAllowed`. The canonical path is checked for
//! `.git` too, so a link into `.git` is refused.
//!
//! **Occupant**: what is at the target, read through one open handle. The file
//! is opened with `O_NOFOLLOW | O_NONBLOCK`: a symlink there is `NotAllowed`
//! (a write never goes through or over a link), and a FIFO cannot block. The
//! kind is then taken from that handle's `fstat`: a directory is
//! `IsADirectory`, another non-regular file `NotAFile`. A regular file is
//! hashed through the same handle, in chunks, with the deadline checked before
//! each, so a large occupant cannot hold the request past it.
//!
//! **Replace rule**: a file that already holds the content is `Same`, and
//! nothing is written. Otherwise a file is replaced only when `replace` names
//! its hash; any other answer is `Exists` with its hash and size. No file
//! there: the write creates it, even if `replace` names a file now gone.
//!
//! **Copy**: the staged upload is copied into a temp file in the target's
//! directory, in [`COPY_CHUNK`] pieces, hashed as it goes, and the deadline is
//! checked before each read. A staged file that does not hash to the content
//! is `NotStaged`, and the temp file goes with the error. The staged file is
//! kept: staging is a cache.
//!
//! **Placing**: the temp file is synced and renamed to the target. A new file
//! is placed with `persist_noclobber`, so nothing is renamed over a file the
//! write did not read. A file that appears meanwhile is read once and answered
//! by the replace rule; if it vanishes again, one more `persist_noclobber` is
//! tried, and a file there then gets the same answer. A file that changes
//! again after that is an error, not a retry. A replaced file keeps its mode
//! (a new one is `0o644`, since `tempfile` makes its file `0o600`). The
//! directory is then synced. Between hashing an occupant and the rename there
//! is a window in which the file may change; it is accepted, as on the read
//! path.
//!
//! **Known limit**: a parent directory swapped for a symlink during the copy
//! is not caught. Whoever can do that runs as the same user and gains nothing
//! by it.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use skywave_proto::{ContentHash, WriteFileRequest, WriteFileResult};
use tempfile::NamedTempFile;

use crate::attachments::Staging;
use crate::error::{Error, IoContext, Result};
use crate::workdir::climbs_out;

/// How long one write may run, counted from when its handler started, the
/// lookup of the workdir included: as [`crate::workdir::DOWNLOAD_DEADLINE`], since a
/// write copies a staged file of up to the upload cap.
pub const WRITE_DEADLINE: Duration = Duration::from_secs(120);
/// Prefix of the temp files a write is made in. One left by a crash stays in
/// the workdir, listed like any dotfile.
const TEMP_PREFIX: &str = ".skywave-write-";
/// Chunk size of the copy; the deadline is checked before each read.
const COPY_CHUNK: usize = 64 * 1024;

/// What a write made: the file at `path`, or the answer that wrote nothing.
#[derive(Debug, PartialEq, Eq)]
pub enum WriteOutcome {
    /// Written: the next step logs it and answers `Written` with the seq.
    Wrote {
        path: String,
        sha256: ContentHash,
        size: u64,
        /// The hash of the file that was there and got replaced, if any.
        replaced: Option<ContentHash>,
    },
    /// Every other answer; nothing written.
    Answer(WriteFileResult),
}

/// Where a write lands.
struct Target {
    /// The canonical directory the file is in; the temp file is made there.
    parent: PathBuf,
    /// The file itself, under `parent`.
    file: PathBuf,
    /// The file's path relative to the canonical workdir, `/` separated.
    path: String,
}

/// The regular file at a target, as [`inspect`] found it.
#[derive(Debug)]
struct Occupant {
    sha256: ContentHash,
    size: u64,
    permissions: fs::Permissions,
}

/// What [`inspect`] found at a target: nothing, a regular file, or the answer.
type Probe = std::result::Result<Option<Occupant>, WriteFileResult>;

/// Writes the staged upload `req.content` to `req.path` under the canonical
/// `workdir`, by the rules in the module docs. `deadline` covers the copy.
pub fn write(
    workdir: &Path,
    staging: &Staging,
    req: &WriteFileRequest,
    deadline: Instant,
) -> Result<WriteOutcome> {
    let target = match resolve(workdir, &req.path) {
        Ok(target) => target,
        Err(answer) => return Ok(WriteOutcome::Answer(answer)),
    };
    let occupant = match inspect(&target.file, deadline)? {
        Ok(occupant) => occupant,
        Err(answer) => return Ok(WriteOutcome::Answer(answer)),
    };
    if let Some(answer) = refusal(req, &target.path, occupant.as_ref()) {
        return Ok(WriteOutcome::Answer(answer));
    }
    let (temp, size) = match copy_staged(staging, req.content, &target.parent, deadline)? {
        Ok(copied) => copied,
        Err(answer) => return Ok(WriteOutcome::Answer(answer)),
    };
    temp.as_file()
        .sync_all()
        .context(|| format!("syncing {}", temp.path().display()))?;
    let replaced = match put(temp, &target, req, occupant.as_ref(), deadline)? {
        Ok(replaced) => replaced,
        Err(answer) => return Ok(WriteOutcome::Answer(answer)),
    };
    sync_dir(&target.parent)?;
    Ok(WriteOutcome::Wrote {
        path: target.path,
        sha256: req.content,
        size,
        replaced,
    })
}

/// Resolves `rel` under the canonical `workdir` to a [`Target`], or the
/// answer that refuses it. Checks the request lexically before the file
/// system, then the canonical parent.
fn resolve(workdir: &Path, rel: &str) -> std::result::Result<Target, WriteFileResult> {
    let requested = Path::new(rel);
    if requested.is_absolute() || climbs_out(requested) || has_git(requested) {
        return Err(WriteFileResult::NotAllowed);
    }
    let Some(Component::Normal(name)) = requested.components().next_back() else {
        return Err(WriteFileResult::NotAllowed);
    };
    let dir = requested.parent().unwrap_or(Path::new(""));
    let Ok(parent) = fs::canonicalize(workdir.join(dir)) else {
        return Err(WriteFileResult::NotFound);
    };
    if !parent.is_dir() {
        return Err(WriteFileResult::NotFound);
    }
    let Ok(in_workdir) = parent.strip_prefix(workdir) else {
        return Err(WriteFileResult::NotAllowed);
    };
    let rel_file = in_workdir.join(name);
    if has_git(&rel_file) {
        return Err(WriteFileResult::NotAllowed);
    }
    let Some(path) = rel_file.to_str() else {
        return Err(WriteFileResult::NotAllowed);
    };
    let file = parent.join(name);
    Ok(Target {
        path: path.to_owned(),
        file,
        parent,
    })
}

/// Whether `path` has a component `.git`: git's own directory, or a file
/// named so.
fn has_git(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == ".git")
}

/// What is at `file`: nothing, the answer when it is not a regular file, or
/// its hash, size and permissions. Everything is read through one handle, so
/// the kind and the bytes are those of the same open file.
fn inspect(file: &Path, deadline: Instant) -> Result<Probe> {
    let mut opened = match open_target(file) {
        Ok(Some(opened)) => opened,
        Ok(None) => return Ok(Err(WriteFileResult::NotAllowed)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Ok(None)),
        Err(e) => return Err(e).context(|| format!("reading {}", file.display())),
    };
    let meta = opened
        .metadata()
        .context(|| format!("reading {}", file.display()))?;
    let kind = meta.file_type();
    if kind.is_dir() {
        return Ok(Err(WriteFileResult::IsADirectory));
    }
    if !kind.is_file() {
        return Ok(Err(WriteFileResult::NotAFile));
    }
    let mut hasher = Sha256::new();
    let size = for_each_chunk(&mut opened, deadline, file, |chunk| {
        hasher.update(chunk);
        Ok(())
    })?;
    Ok(Ok(Some(Occupant {
        sha256: ContentHash::from_digest(hasher.finalize().into()),
        size,
        permissions: meta.permissions(),
    })))
}

/// Opens `file` for reading. `Ok(None)` when it is a symlink: `O_NOFOLLOW`
/// refuses one with `ELOOP`. `O_NONBLOCK` keeps a FIFO from blocking the open.
#[cfg(unix)]
fn open_target(file: &Path) -> io::Result<Option<File>> {
    use std::os::unix::fs::OpenOptionsExt;
    let opened = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(file);
    match opened {
        Ok(opened) => Ok(Some(opened)),
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Without `O_NOFOLLOW` a symlink is followed. The server runs on unix; this
/// keeps the crate building elsewhere.
#[cfg(not(unix))]
fn open_target(file: &Path) -> io::Result<Option<File>> {
    File::open(file).map(Some)
}

/// Feeds `from` to `each` in [`COPY_CHUNK`] pieces until its end, checking
/// `deadline` before each read. Returns the bytes fed. `path` names what is
/// read, for errors.
fn for_each_chunk(
    from: &mut impl Read,
    deadline: Instant,
    path: &Path,
    mut each: impl FnMut(&[u8]) -> Result<()>,
) -> Result<u64> {
    let mut buf = vec![0u8; COPY_CHUNK];
    let mut size = 0u64;
    loop {
        if Instant::now() >= deadline {
            return Err(Error::WorkdirDeadline);
        }
        let n = match from.read(&mut buf) {
            Ok(0) => return Ok(size),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e).context(|| format!("reading {}", path.display())),
        };
        each(&buf[..n])?;
        size += n as u64;
    }
}

/// The answer the replace rule gives a write over `occupant`, if it refuses:
/// `Same` when the file already holds the content, `Exists` when `replace`
/// does not name the file. `None` means the write goes ahead.
fn refusal(
    req: &WriteFileRequest,
    path: &str,
    occupant: Option<&Occupant>,
) -> Option<WriteFileResult> {
    let old = occupant?;
    if old.sha256 == req.content {
        return Some(WriteFileResult::Same {
            path: path.to_owned(),
            size: old.size,
        });
    }
    if req.replace == Some(old.sha256) {
        return None;
    }
    Some(WriteFileResult::Exists {
        sha256: old.sha256,
        size: old.size,
    })
}

/// Copies the staged upload `content` into a temp file in `parent`, hashing
/// it as it goes; returns the temp file and its size. `Err` holds `NotStaged`
/// when there is no such upload, or its bytes are no longer `content`.
fn copy_staged(
    staging: &Staging,
    content: ContentHash,
    parent: &Path,
    deadline: Instant,
) -> Result<std::result::Result<(NamedTempFile, u64), WriteFileResult>> {
    let staged_path = staging.path(content);
    let mut staged = match File::open(&staged_path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok(Err(WriteFileResult::NotStaged));
        }
        Err(e) => return Err(e).context(|| format!("opening {}", staged_path.display())),
    };
    let mut temp = tempfile::Builder::new()
        .prefix(TEMP_PREFIX)
        .tempfile_in(parent)
        .context(|| format!("writing in {}", parent.display()))?;
    let mut hasher = Sha256::new();
    let size = for_each_chunk(&mut staged, deadline, &staged_path, |chunk| {
        hasher.update(chunk);
        temp.write_all(chunk)
            .context(|| format!("writing in {}", parent.display()))
    })?;
    if ContentHash::from_digest(hasher.finalize().into()) != content {
        return Ok(Err(WriteFileResult::NotStaged));
    }
    Ok(Ok((temp, size)))
}

/// The placing's result: the hash of the file replaced, if one was, or the
/// answer that refused the write.
type Put = std::result::Result<Option<ContentHash>, WriteFileResult>;

/// Puts `temp` at the target's file: over `occupant` when there is one, else
/// as a new file.
fn put(
    temp: NamedTempFile,
    target: &Target,
    req: &WriteFileRequest,
    occupant: Option<&Occupant>,
    deadline: Instant,
) -> Result<Put> {
    match occupant {
        Some(old) => replace_found(temp, target, req, old),
        None => put_new(temp, target, req, deadline),
    }
}

/// [`put`] for a target that had no file when the write began.
fn put_new(
    temp: NamedTempFile,
    target: &Target,
    req: &WriteFileRequest,
    deadline: Instant,
) -> Result<Put> {
    set_mode(&temp, None)?;
    let temp = match temp.persist_noclobber(&target.file) {
        Ok(_) => return Ok(Ok(None)),
        Err(e) if e.error.kind() == io::ErrorKind::AlreadyExists => e.file,
        Err(e) => return Err(e.error).context(|| format!("writing {}", target.file.display())),
    };
    // Something appeared since the write began: the replace rule decides on it.
    match inspect(&target.file, deadline)? {
        Ok(Some(found)) => replace_found(temp, target, req, &found),
        Ok(None) => create_again(temp, target, req, deadline),
        Err(answer) => Ok(Err(answer)),
    }
}

/// The file [`put_new`] found was gone when read. One more `persist_noclobber`;
/// a file there then gets the replace rule. A file gone again after that is an
/// error: the write is not retried further.
fn create_again(
    temp: NamedTempFile,
    target: &Target,
    req: &WriteFileRequest,
    deadline: Instant,
) -> Result<Put> {
    let temp = match temp.persist_noclobber(&target.file) {
        Ok(_) => return Ok(Ok(None)),
        Err(e) if e.error.kind() == io::ErrorKind::AlreadyExists => e.file,
        Err(e) => return Err(e.error).context(|| format!("writing {}", target.file.display())),
    };
    match inspect(&target.file, deadline)? {
        Ok(Some(found)) => replace_found(temp, target, req, &found),
        Ok(None) => Err(io::Error::other("the file vanished twice during the write"))
            .context(|| format!("writing {}", target.file.display())),
        Err(answer) => Ok(Err(answer)),
    }
}

/// Renames `temp` over `found`, a file the write read, if `replace` names its
/// hash. Otherwise the answer the replace rule gives: nothing is renamed over
/// a file the write did not name.
fn replace_found(
    temp: NamedTempFile,
    target: &Target,
    req: &WriteFileRequest,
    found: &Occupant,
) -> Result<Put> {
    if let Some(answer) = refusal(req, &target.path, Some(found)) {
        return Ok(Err(answer));
    }
    set_mode(&temp, Some(&found.permissions))?;
    rename(temp, &target.file)?;
    Ok(Ok(Some(found.sha256)))
}

/// Gives `temp` the mode of the file it replaces, or `0o644` for a new file.
fn set_mode(temp: &NamedTempFile, kept: Option<&fs::Permissions>) -> Result<()> {
    let Some(mode) = mode_for(kept) else {
        return Ok(());
    };
    temp.as_file()
        .set_permissions(mode)
        .context(|| format!("writing {}", temp.path().display()))
}

/// The permissions a file written over `kept` gets; `None` leaves the temp
/// file's as they are (non-unix, where there is no mode to keep).
#[cfg(unix)]
fn mode_for(kept: Option<&fs::Permissions>) -> Option<fs::Permissions> {
    use std::os::unix::fs::PermissionsExt;
    const NEW_FILE_MODE: u32 = 0o644;
    Some(
        kept.cloned()
            .unwrap_or_else(|| fs::Permissions::from_mode(NEW_FILE_MODE)),
    )
}

/// See the unix version.
#[cfg(not(unix))]
fn mode_for(kept: Option<&fs::Permissions>) -> Option<fs::Permissions> {
    kept.cloned()
}

/// Renames `temp` over `file`, replacing it if there is one.
fn rename(temp: NamedTempFile, file: &Path) -> Result<()> {
    temp.persist(file)
        .map_err(|e| e.error)
        .context(|| format!("writing {}", file.display()))?;
    Ok(())
}

/// Syncs `dir`, so a rename into it is durable.
fn sync_dir(dir: &Path) -> Result<()> {
    File::open(dir)
        .and_then(|dir| dir.sync_all())
        .context(|| format!("syncing {}", dir.display()))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::process::Command;
    use std::time::Duration;

    use skywave_proto::SessionId;
    use tempfile::TempDir;

    use super::*;

    /// A temp dir holding `workdir/` (canonical, with `sub/` in it), an
    /// `outside/` and a staging directory.
    struct Fixture {
        _tmp: TempDir,
        workdir: PathBuf,
        outside: PathBuf,
        staging: Staging,
    }

    impl Fixture {
        fn new() -> Fixture {
            let tmp = tempfile::tempdir().unwrap();
            let base = fs::canonicalize(tmp.path()).unwrap();
            let workdir = base.join("workdir");
            let outside = base.join("outside");
            let staging_dir = base.join("staging");
            fs::create_dir_all(workdir.join("sub")).unwrap();
            fs::create_dir_all(&outside).unwrap();
            fs::create_dir_all(&staging_dir).unwrap();
            Fixture {
                _tmp: tmp,
                workdir,
                outside,
                staging: Staging::new(staging_dir),
            }
        }

        /// Stages `bytes`, returning their hash.
        fn stage(&self, bytes: &[u8]) -> ContentHash {
            let hash = ContentHash::of(bytes);
            self.staging.stage(hash, bytes).unwrap();
            hash
        }

        fn request(
            path: &str,
            content: ContentHash,
            replace: Option<ContentHash>,
        ) -> WriteFileRequest {
            WriteFileRequest {
                session: SessionId::new(),
                path: path.to_owned(),
                content,
                replace,
            }
        }

        fn run(&self, req: &WriteFileRequest) -> WriteOutcome {
            let deadline = Instant::now() + Duration::from_secs(10);
            write(&self.workdir, &self.staging, req, deadline).unwrap()
        }
    }

    /// The names in `dir`, sorted.
    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn answer(result: WriteFileResult) -> WriteOutcome {
        WriteOutcome::Answer(result)
    }

    fn mkfifo(path: &Path) {
        let status = Command::new("mkfifo").arg(path).status().unwrap();
        assert!(status.success());
    }

    #[test]
    fn a_new_file_is_written_with_mode_644_and_no_temp_file() {
        let fx = Fixture::new();
        let content = fx.stage(b"hello");
        let outcome = fx.run(&Fixture::request("sub/a.txt", content, None));
        assert_eq!(
            outcome,
            WriteOutcome::Wrote {
                path: "sub/a.txt".to_owned(),
                sha256: content,
                size: 5,
                replaced: None,
            }
        );
        let target = fx.workdir.join("sub/a.txt");
        assert_eq!(fs::read(&target).unwrap(), b"hello");
        let mode = fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o644);
        assert_eq!(names(&fx.workdir.join("sub")), vec!["a.txt"]);
        assert!(
            fx.staging.contains(content),
            "a write keeps the staged upload"
        );
    }

    #[test]
    fn the_same_content_is_answered_and_not_rewritten() {
        let fx = Fixture::new();
        let target = fx.workdir.join("a.txt");
        fs::write(&target, b"same").unwrap();
        let inode = fs::metadata(&target).unwrap().ino();
        let content = fx.stage(b"same");
        let outcome = fx.run(&Fixture::request("a.txt", content, None));
        let same = WriteFileResult::Same {
            path: "a.txt".to_owned(),
            size: 4,
        };
        assert_eq!(outcome, answer(same));
        assert_eq!(fs::metadata(&target).unwrap().ino(), inode);
    }

    #[test]
    fn a_different_file_is_replaced_only_when_replace_names_it() {
        let fx = Fixture::new();
        let target = fx.workdir.join("a.txt");
        fs::write(&target, b"old").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        let old = ContentHash::of(b"old");
        let new = fx.stage(b"newer");
        let exists = answer(WriteFileResult::Exists {
            sha256: old,
            size: 3,
        });
        assert_eq!(fx.run(&Fixture::request("a.txt", new, None)), exists);
        assert_eq!(fs::read(&target).unwrap(), b"old");

        let wrong = ContentHash::of(b"not it");
        assert_eq!(fx.run(&Fixture::request("a.txt", new, Some(wrong))), exists);
        assert_eq!(fs::read(&target).unwrap(), b"old");

        let outcome = fx.run(&Fixture::request("a.txt", new, Some(old)));
        assert_eq!(
            outcome,
            WriteOutcome::Wrote {
                path: "a.txt".to_owned(),
                sha256: new,
                size: 5,
                replaced: Some(old),
            }
        );
        assert_eq!(fs::read(&target).unwrap(), b"newer");
        let mode = fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "a replaced file keeps its mode");
    }

    #[test]
    fn paths_that_are_absolute_git_or_outside_are_not_allowed_and_write_nothing() {
        let fx = Fixture::new();
        fs::create_dir_all(fx.workdir.join(".git/hooks")).unwrap();
        fs::create_dir_all(fx.workdir.join("sub/.git")).unwrap();
        fs::write(fx.workdir.join("a.txt"), b"a").unwrap();
        symlink("../outside", fx.workdir.join("link")).unwrap();
        symlink(".git/hooks", fx.workdir.join("hooks")).unwrap();
        symlink("a.txt", fx.workdir.join("b.txt")).unwrap();
        let content = fx.stage(b"x");
        let refused = [
            "",
            "/etc/x",
            "../x",
            "a/..",
            ".git/hooks/pre-commit",
            "sub/.git/x",
            "link/x",
            "hooks/pre-commit",
            "b.txt",
        ];
        for path in refused {
            let outcome = fx.run(&Fixture::request(path, content, None));
            let not_allowed = answer(WriteFileResult::NotAllowed);
            assert_eq!(outcome, not_allowed, "path {path:?}");
        }
        assert!(names(&fx.outside).is_empty());
        assert!(!fx.workdir.join(".git/hooks/pre-commit").exists());
        assert_eq!(fs::read(fx.workdir.join("a.txt")).unwrap(), b"a");
    }

    #[test]
    fn a_missing_or_non_directory_parent_is_not_found() {
        let fx = Fixture::new();
        fs::write(fx.workdir.join("a.txt"), b"a").unwrap();
        let content = fx.stage(b"x");
        for path in ["nope/x", "a.txt/x"] {
            let outcome = fx.run(&Fixture::request(path, content, None));
            assert_eq!(outcome, answer(WriteFileResult::NotFound), "path {path:?}");
        }
    }

    #[test]
    fn a_directory_or_a_fifo_at_the_target_is_refused() {
        let fx = Fixture::new();
        let content = fx.stage(b"x");
        let outcome = fx.run(&Fixture::request("sub", content, None));
        assert_eq!(outcome, answer(WriteFileResult::IsADirectory));
        mkfifo(&fx.workdir.join("pipe"));
        let outcome = fx.run(&Fixture::request("pipe", content, None));
        assert_eq!(outcome, answer(WriteFileResult::NotAFile));
    }

    #[test]
    fn content_not_staged_or_no_longer_its_bytes_is_not_staged_and_leaves_no_file() {
        let fx = Fixture::new();
        let unstaged = ContentHash::of(b"never staged");
        let outcome = fx.run(&Fixture::request("a.txt", unstaged, None));
        assert_eq!(outcome, answer(WriteFileResult::NotStaged));

        let staged = fx.stage(b"good");
        fs::write(fx.staging.path(staged), b"bad!").unwrap();
        let outcome = fx.run(&Fixture::request("a.txt", staged, None));
        assert_eq!(outcome, answer(WriteFileResult::NotStaged));
        assert_eq!(names(&fx.workdir), vec!["sub"], "no file and no temp file");
    }

    #[test]
    fn a_past_deadline_stops_the_copy_and_leaves_no_file() {
        let fx = Fixture::new();
        let bytes = vec![7u8; COPY_CHUNK * 2 + 1];
        let content = fx.stage(&bytes);
        let req = Fixture::request("a.txt", content, None);
        let result = write(&fx.workdir, &fx.staging, &req, Instant::now());
        assert!(matches!(result, Err(Error::WorkdirDeadline)));
        assert_eq!(names(&fx.workdir), vec!["sub"], "no file and no temp file");

        // The same write with a deadline still ahead goes through.
        let outcome = fx.run(&req);
        assert!(matches!(outcome, WriteOutcome::Wrote { size, .. } if size == bytes.len() as u64));
    }

    #[test]
    fn a_past_deadline_stops_hashing_an_existing_file() {
        let fx = Fixture::new();
        let target = fx.workdir.join("a.txt");
        let bytes = vec![3u8; COPY_CHUNK * 2 + 1];
        fs::write(&target, &bytes).unwrap();
        let content = fx.stage(b"new");
        let req = Fixture::request("a.txt", content, None);
        let result = write(&fx.workdir, &fx.staging, &req, Instant::now());
        assert!(matches!(result, Err(Error::WorkdirDeadline)));
        assert_eq!(
            fs::read(&target).unwrap(),
            bytes,
            "the occupant is untouched"
        );

        // The same occupant with the deadline still ahead is answered in full.
        let exists = WriteFileResult::Exists {
            sha256: ContentHash::of(&bytes),
            size: bytes.len() as u64,
        };
        assert_eq!(fx.run(&req), answer(exists));
    }

    #[test]
    fn a_name_that_only_contains_git_is_an_ordinary_file() {
        let fx = Fixture::new();
        let content = fx.stage(b"x");
        let outcome = fx.run(&Fixture::request("x.git", content, None));
        assert!(matches!(outcome, WriteOutcome::Wrote { .. }));
    }
}
