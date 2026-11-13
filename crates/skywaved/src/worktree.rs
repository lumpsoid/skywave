//! Git worktrees for sessions that ask for one (`CreateSession.worktree`):
//! created next to the repository, under `<repo>.worktrees/<branch>`.
//!
//! The repository is found from the workdir with `--git-common-dir`, so a
//! subdirectory resolves to the repository's root and a worktree resolves to
//! the main repository. A bare repository is refused. The main working tree's
//! parent must lie under a configured root (D14), and so must every directory
//! between `<repo>.worktrees` and the new worktree that already exists: a
//! symlink there would send git's writes outside the roots. A new branch starts
//! at the workdir's `HEAD`, which is the main tree's only when the workdir is
//! the main tree. [`create`] and [`remove`] run one at a time. Outcomes come from git's exit
//! statuses; a timeout is a failure.
//!
//! [`discard`] is rollback only: the caller uses it for a worktree [`create`]
//! made for a session that was not committed ([`Prepared`]). `--force` is
//! safe there because only a worktree just made is ever discarded.
//!
//! [`remove`] is the explicit removal of a session's worktree. It never
//! forces: a dirty worktree or an unmerged branch is refused, and a removal
//! that is already done is accepted, so a resend is safe.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use skywave_proto::{RejectReason, Worktree};

use crate::dirs;
use crate::workdir::Git;
use crate::workdir::git::{Finished, Repo};

/// A worktree this module created, for the caller to keep or discard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Created {
    /// Canonical path of the new worktree: the session's workdir.
    pub path: String,
    /// Canonical path of the repository's main working tree.
    pub repo: String,
    pub branch: String,
}

/// How long git may take to create or discard a worktree; git is killed past it.
pub const WORKTREE_DEADLINE: Duration = Duration::from_secs(20);

/// Serialises [`create`] and [`remove`]. A create's checks and its `git
/// worktree add` are one step:
/// two creates naming one branch would both pass [`branch_exists`] and the
/// target check, and the loser would fail as `WorktreeFailed` instead of
/// `BranchExists`. It also keeps a failed add's cleanup to what this create
/// made: the branch and target did not exist before its add, and no other
/// create can have made them since.
static CREATING: Mutex<()> = Mutex::new(());

/// Creates a worktree of `workdir`'s repository on a new `branch`, starting
/// at the workdir's `HEAD`. Refusals: `WorkdirNotAllowed` or
/// `WorkdirNotFound` from the roots check, `NotARepository`, `BranchExists`;
/// anything else that stops the create is `WorktreeFailed`.
pub fn create(
    git: &Git,
    roots: &[PathBuf],
    workdir: &str,
    branch: &str,
    deadline: Instant,
) -> Result<Created, RejectReason> {
    let _creating = CREATING.lock().unwrap_or_else(PoisonError::into_inner);
    check_name(branch)?;
    let workdir = dirs::canonical_under_roots(roots, workdir)?;
    let main = main_working_tree(git, Path::new(&workdir), deadline)?;
    let (repo, worktrees) = layout(roots, &main)?;
    let (repo_text, target) = (text(&repo)?.to_owned(), worktrees.join(branch));
    let target_text = text(&target)?.to_owned();
    check_ref_format(git, &repo, branch, deadline)?;
    if branch_exists(git, &repo, branch, deadline)? {
        return Err(RejectReason::BranchExists);
    }
    if fs::symlink_metadata(&target).is_ok() || !real_dirs_below(&worktrees, &target) {
        return Err(RejectReason::WorktreeFailed);
    }
    let head = head_of(git, Path::new(&workdir), deadline)?;
    let add = [
        "worktree",
        "add",
        "-b",
        branch,
        target_text.as_str(),
        head.as_str(),
    ];
    if !succeeded(
        git,
        &repo,
        deadline,
        &add,
        branch,
        "git worktree add failed",
    ) {
        undo_failed_add(git, &repo, branch, &target_text);
        return Err(RejectReason::WorktreeFailed);
    }
    let made = Created {
        path: target_text,
        repo: repo_text,
        branch: branch.to_owned(),
    };
    finish(git, made, &target)
}

/// Rolls back a worktree [`create`] made: removes it with `--force` and
/// deletes its branch, both in the main working tree. Failures are logged
/// with git's stderr; nothing here panics or returns an error.
pub fn discard(git: &Git, created: &Created, deadline: Instant) {
    let repo = Path::new(&created.repo);
    let remove = ["worktree", "remove", "--force", created.path.as_str()];
    succeeded(
        git,
        repo,
        deadline,
        &remove,
        &created.branch,
        "git worktree remove failed",
    );
    let delete = ["branch", "-D", created.branch.as_str()];
    succeeded(
        git,
        repo,
        deadline,
        &delete,
        &created.branch,
        "git branch -D failed",
    );
}

/// Removes a session's worktree at `path` and deletes its branch, both in the
/// repository's main working tree. Refused: `BranchUnmerged` when the branch,
/// or the worktree's own `HEAD`, is not merged into the repository's `HEAD`;
/// `WorktreeDirty` when the worktree has changes; anything else git cannot do
/// is `WorktreeFailed`. It never forces a dirty or unmerged removal. The branch
/// is deleted with `-D`, but only after [`check_merged`] passed in this same
/// call: `-d` checks the upstream rather than `HEAD`, so it could fail after
/// the worktree is gone and never recover. Each step is skipped when already
/// done, so a removal whose worktree and branch are both gone is `Ok`: a resend
/// after a failed transaction is accepted. A directory deleted by hand is
/// pruned from the repository. The checks run again on every call.
pub fn remove(
    git: &Git,
    worktree: &Worktree,
    path: &str,
    deadline: Instant,
) -> Result<(), RejectReason> {
    let _creating = CREATING.lock().unwrap_or_else(PoisonError::into_inner);
    let repo = Path::new(&worktree.repo);
    let registered = is_registered(git, repo, path, deadline)?;
    let branch = branch_exists(git, repo, &worktree.branch, deadline)?;
    if !registered && !branch {
        return Ok(());
    }
    if branch {
        check_merged(git, repo, &worktree.branch, deadline)?;
    }
    if registered {
        take_off(git, repo, &worktree.branch, path, deadline)?;
    }
    if branch {
        let delete = ["branch", "-D", worktree.branch.as_str()];
        let what = "git branch -D failed";
        if !succeeded(git, repo, deadline, &delete, &worktree.branch, what) {
            return Err(RejectReason::WorktreeFailed);
        }
    }
    Ok(())
}

/// Takes a registered worktree off `repo`. A directory still there is checked
/// (its own `HEAD` merged, no changes) and removed with `git worktree remove`,
/// never with `--force`. A directory deleted by hand has nothing to check: git
/// prunes its registration instead.
fn take_off(
    git: &Git,
    repo: &Path,
    branch: &str,
    path: &str,
    deadline: Instant,
) -> Result<(), RejectReason> {
    let dir = Path::new(path);
    if is_gone(dir) {
        let prune = ["worktree", "prune"];
        let what = "git worktree prune failed";
        return if succeeded(git, repo, deadline, &prune, branch, what) {
            Ok(())
        } else {
            Err(RejectReason::WorktreeFailed)
        };
    }
    let head = head_of(git, dir, deadline)?;
    check_merged(git, repo, &head, deadline)?;
    check_clean(git, dir, deadline)?;
    let remove = ["worktree", "remove", path];
    let what = "git worktree remove failed";
    if succeeded(git, repo, deadline, &remove, branch, what) {
        Ok(())
    } else {
        Err(RejectReason::WorktreeFailed)
    }
}

/// Whether nothing is at `dir`: a directory deleted by hand. A file or a link
/// in its place is something, and is not pruned.
fn is_gone(dir: &Path) -> bool {
    matches!(fs::symlink_metadata(dir), Err(e) if e.kind() == io::ErrorKind::NotFound)
}

/// Whether `repo` has a worktree at `path`: some line of `git worktree list
/// --porcelain` is `worktree <path>`, exactly.
fn is_registered(
    git: &Git,
    repo: &Path,
    path: &str,
    deadline: Instant,
) -> Result<bool, RejectReason> {
    let done = run(git, repo, deadline, &["worktree", "list", "--porcelain"])?;
    if !done.status.success() {
        let stderr = String::from_utf8_lossy(&done.stderr);
        tracing::warn!(stderr = %stderr.trim_end(), "git worktree list failed");
        return Err(RejectReason::WorktreeFailed);
    }
    let wanted = format!("worktree {path}");
    let out = String::from_utf8_lossy(&done.out);
    Ok(out.lines().any(|line| line == wanted))
}

/// `rev` (a branch or a commit) is merged into `repo`'s `HEAD`: `git
/// merge-base --is-ancestor` exits 0 for that and 1 for not. Any other exit is
/// a failure.
fn check_merged(git: &Git, repo: &Path, rev: &str, deadline: Instant) -> Result<(), RejectReason> {
    let done = run(
        git,
        repo,
        deadline,
        &["merge-base", "--is-ancestor", rev, "HEAD"],
    )?;
    match done.status.code() {
        Some(0) => Ok(()),
        Some(1) => Err(RejectReason::BranchUnmerged),
        _ => {
            let stderr = String::from_utf8_lossy(&done.stderr);
            tracing::warn!(rev = %rev, stderr = %stderr.trim_end(), "git merge-base failed");
            Err(RejectReason::WorktreeFailed)
        }
    }
}

/// The worktree at `path` has no changes: `git status --porcelain` prints
/// nothing. Untracked files are listed whatever `status.showUntrackedFiles`
/// says, so a config cannot hide them.
fn check_clean(git: &Git, path: &Path, deadline: Instant) -> Result<(), RejectReason> {
    let args = ["status", "--porcelain", "--untracked-files=normal"];
    let done = run(git, path, deadline, &args)?;
    if !done.status.success() {
        tracing::warn!(dir = %path.display(), "git status failed");
        return Err(RejectReason::WorktreeFailed);
    }
    if done.out.is_empty() {
        Ok(())
    } else {
        Err(RejectReason::WorktreeDirty)
    }
}

/// A worktree [`create`] made for a session, or the refusal it came to, held
/// until the command's transaction is known, as
/// [`crate::agent::import::Prepared`] holds its blobs. `handle_with` calls
/// [`Prepared::kept`] for a rejected command too, so a worktree is kept only
/// when apply logged its session ([`Prepared::logged`]). Dropped otherwise,
/// it is discarded.
#[derive(Debug)]
pub struct Prepared {
    git: Git,
    result: Result<Created, RejectReason>,
    /// Set once apply has checked the worktree and logs its session.
    logged: bool,
    /// Set by [`Prepared::kept`] when `logged`.
    kept: bool,
}

impl Prepared {
    pub(crate) fn new(git: Git, result: Result<Created, RejectReason>) -> Prepared {
        Prepared {
            git,
            result,
            logged: false,
            kept: false,
        }
    }

    /// The worktree, or the refusal it came to.
    pub(crate) fn created(&self) -> Result<Created, RejectReason> {
        self.result.clone()
    }

    /// Marks the worktree as one apply logs a session for.
    pub(crate) fn logged(&mut self) {
        self.logged = true;
    }

    /// Called once the command's transaction is committed: a logged worktree
    /// is the session's from now on. One never logged is still discarded.
    pub(crate) fn kept(mut self) {
        if self.logged {
            self.kept = true;
        }
    }
}

impl Drop for Prepared {
    fn drop(&mut self) {
        if self.kept {
            return;
        }
        if let Ok(created) = &self.result {
            discard(&self.git, created, Instant::now() + WORKTREE_DEADLINE);
        }
    }
}

/// Refuses, before git runs, a name that would read as an option or climb
/// out of the worktrees directory. `check_ref_format` refuses the rest.
fn check_name(branch: &str) -> Result<(), RejectReason> {
    if branch.starts_with('-') || branch.split('/').any(|part| part == "..") {
        return Err(RejectReason::WorktreeFailed);
    }
    Ok(())
}

/// The main working tree of the repository `dir` is in: the parent of its
/// common git dir, which must be a `.git` directory (a bare repository is
/// not one, and neither is a directory outside any repository).
fn main_working_tree(git: &Git, dir: &Path, deadline: Instant) -> Result<PathBuf, RejectReason> {
    let args = ["rev-parse", "--path-format=absolute", "--git-common-dir"];
    let done = run(git, dir, deadline, &args)?;
    if !done.status.success() {
        return Err(RejectReason::NotARepository);
    }
    let out = String::from_utf8(done.out).map_err(|_| RejectReason::WorktreeFailed)?;
    let common = Path::new(out.strip_suffix('\n').unwrap_or(&out));
    if common.file_name() != Some(OsStr::new(".git")) {
        return Err(RejectReason::NotARepository);
    }
    common
        .parent()
        .map(Path::to_path_buf)
        .ok_or(RejectReason::NotARepository)
}

/// The repository's canonical main working tree, and the directory its
/// worktrees go in: `<repo>.worktrees`, a sibling of it. The main working
/// tree's parent must lie under a root, which is what keeps the worktrees
/// under one.
fn layout(roots: &[PathBuf], main: &Path) -> Result<(PathBuf, PathBuf), RejectReason> {
    let name = main.file_name().ok_or(RejectReason::NotARepository)?;
    let parent = main.parent().ok_or(RejectReason::NotARepository)?;
    let parent = PathBuf::from(dirs::canonical_under_roots(roots, text(parent)?)?);
    let mut worktrees = name.to_os_string();
    worktrees.push(".worktrees");
    Ok((parent.join(name), parent.join(worktrees)))
}

/// `branch` must be a name git takes as it is. `check-ref-format --branch`
/// prints the name it checked, which differs for an `@{-N}` expansion.
fn check_ref_format(
    git: &Git,
    repo: &Path,
    branch: &str,
    deadline: Instant,
) -> Result<(), RejectReason> {
    let done = run(
        git,
        repo,
        deadline,
        &["check-ref-format", "--branch", branch],
    )?;
    let printed = String::from_utf8_lossy(&done.out);
    if done.status.success() && printed.trim_end() == branch {
        return Ok(());
    }
    Err(RejectReason::WorktreeFailed)
}

fn branch_exists(
    git: &Git,
    repo: &Path,
    branch: &str,
    deadline: Instant,
) -> Result<bool, RejectReason> {
    let full = format!("refs/heads/{branch}");
    let args = ["rev-parse", "--verify", "--quiet", full.as_str()];
    Ok(run(git, repo, deadline, &args)?.status.success())
}

/// Whether every directory from `worktrees` down to `target`, the target
/// itself excluded, that exists is a real directory. `symlink_metadata` does
/// not follow a link, so a symlink or a file in the way is refused: git would
/// otherwise write through the link, outside the roots, or fail after it has
/// made the branch.
fn real_dirs_below(worktrees: &Path, target: &Path) -> bool {
    target
        .ancestors()
        .skip(1)
        .take_while(|dir| dir.starts_with(worktrees))
        .all(|dir| match fs::symlink_metadata(dir) {
            Ok(meta) => meta.is_dir(),
            Err(e) => e.kind() == io::ErrorKind::NotFound,
        })
}

/// The commit the workdir's `HEAD` names, where the new branch starts. A
/// repository with no commit has none, so it cannot have a worktree branch.
fn head_of(git: &Git, dir: &Path, deadline: Instant) -> Result<String, RejectReason> {
    let done = run(
        git,
        dir,
        deadline,
        &["rev-parse", "--verify", "HEAD^{commit}"],
    )?;
    if !done.status.success() {
        return Err(RejectReason::WorktreeFailed);
    }
    let printed = String::from_utf8_lossy(&done.out);
    Ok(printed.trim_end().to_owned())
}

/// Deletes what a killed `git worktree add` leaves at `target` once git has
/// had its turn: a real directory holding nothing but a `.git` file, or
/// nothing at all. Git refuses to remove such a directory because it is not a
/// registered worktree, so the caller cannot leave it to `git worktree remove`.
/// Any other entry is the user's: the directory is kept with a warning.
fn remove_half_made(target: &Path, branch: &str) {
    if !fs::symlink_metadata(target).is_ok_and(|meta| meta.is_dir()) {
        return;
    }
    let git_file = target.join(".git");
    let listed = fs::read_dir(target).and_then(|dir| {
        dir.map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<io::Result<Vec<_>>>()
    });
    let only_git = listed.is_ok_and(|names| names.iter().all(|name| name == OsStr::new(".git")));
    let git_is_file = match fs::symlink_metadata(&git_file) {
        Ok(meta) => meta.is_file(),
        Err(e) => e.kind() == io::ErrorKind::NotFound,
    };
    if !only_git || !git_is_file {
        tracing::warn!(branch = %branch, dir = %target.display(), "a failed worktree add left a directory with files; keeping it");
        return;
    }
    let deleted = match fs::remove_file(&git_file) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => fs::remove_dir(target),
    };
    if let Err(e) = deleted {
        tracing::warn!(branch = %branch, dir = %target.display(), error = %e, "cannot delete a failed worktree add's directory");
    }
}

/// Undoes a `git worktree add` that did not succeed: git may have made the
/// branch, the target or both before it failed or was killed. Under
/// [`CREATING`] neither existed before the add, so what exists now is this
/// create's, and nothing else is touched. Its own deadline: the create's may
/// be spent.
///
/// A killed add can leave the target as a plain directory that git never
/// registered, which `git worktree remove` refuses. [`remove_half_made`]
/// deletes it, but only when it holds nothing but a `.git` file. A lock the
/// killed add left is why the remove is given `--force` twice.
fn undo_failed_add(git: &Git, repo: &Path, branch: &str, target: &str) {
    let deadline = Instant::now() + WORKTREE_DEADLINE;
    let target_made = fs::symlink_metadata(target).is_ok();
    let branch_made = matches!(branch_exists(git, repo, branch, deadline), Ok(true));
    if !target_made && !branch_made {
        return;
    }
    if target_made {
        let remove = ["worktree", "remove", "--force", "--force", target];
        succeeded(
            git,
            repo,
            deadline,
            &remove,
            branch,
            "git worktree remove failed",
        );
    }
    let prune = ["worktree", "prune"];
    succeeded(
        git,
        repo,
        deadline,
        &prune,
        branch,
        "git worktree prune failed",
    );
    if fs::symlink_metadata(target).is_ok() {
        remove_half_made(Path::new(target), branch);
    }
    if branch_made {
        let delete = ["branch", "-D", branch];
        succeeded(git, repo, deadline, &delete, branch, "git branch -D failed");
    }
}

/// The created worktree with its path canonical. If that cannot be done the
/// worktree is discarded, so a failed create leaves nothing unreported. The
/// discard gets its own deadline: the create's may be spent.
fn finish(git: &Git, made: Created, target: &Path) -> Result<Created, RejectReason> {
    let canonical = fs::canonicalize(target)
        .ok()
        .and_then(|path| path.into_os_string().into_string().ok());
    match canonical {
        Some(path) => Ok(Created { path, ..made }),
        None => {
            tracing::warn!(branch = %made.branch, "cannot canonicalise the new worktree");
            discard(git, &made, Instant::now() + WORKTREE_DEADLINE);
            Err(RejectReason::WorktreeFailed)
        }
    }
}

/// Runs `git args` in `dir`. A run that gave no answer (git missing, a
/// fault, the deadline) is logged here and is `WorktreeFailed`.
fn run(
    git: &Git,
    dir: &Path,
    deadline: Instant,
    args: &[&str],
) -> Result<Finished<Vec<u8>>, RejectReason> {
    Repo::new(git, dir, deadline).output(args).map_err(|e| {
        tracing::warn!(dir = %dir.display(), error = ?e, "git could not run");
        RejectReason::WorktreeFailed
    })
}

/// Whether `git args` in `repo` exits 0. A non-zero exit is logged with
/// git's stderr under the message `what`.
fn succeeded(
    git: &Git,
    repo: &Path,
    deadline: Instant,
    args: &[&str],
    branch: &str,
    what: &str,
) -> bool {
    match run(git, repo, deadline, args) {
        Ok(done) if done.status.success() => true,
        Ok(done) => {
            let stderr = String::from_utf8_lossy(&done.stderr);
            tracing::warn!(branch = %branch, stderr = %stderr.trim_end(), "{}", what);
            false
        }
        Err(_) => false,
    }
}

/// A path as the UTF-8 text git and the wire take; any other path is
/// `WorktreeFailed`.
fn text(path: &Path) -> Result<&str, RejectReason> {
    path.to_str().ok_or(RejectReason::WorktreeFailed)
}

#[cfg(test)]
mod tests {
    use std::process::Output;
    use std::time::Duration;

    use super::*;

    fn setup_git(dir: &std::path::Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap_or_else(|e| panic!("this test needs `git` on PATH: {e}"));
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Runs `git args` in `dir` and returns its output, whatever the status.
    fn git_in(dir: &Path, args: &[&str]) -> Output {
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap_or_else(|e| panic!("this test needs `git` on PATH: {e}"))
    }

    fn git() -> Git {
        Git::default()
            .with_env("GIT_CONFIG_GLOBAL", "/dev/null")
            .with_env("GIT_CONFIG_NOSYSTEM", "1")
    }

    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(10)
    }

    /// `tmp/root/project`: a repository with one commit, under the one root
    /// `tmp/root`. Paths are canonical, as the module returns them.
    struct Fixture {
        _tmp: tempfile::TempDir,
        root: PathBuf,
    }

    impl Fixture {
        fn new() -> Fixture {
            let tmp = tempfile::tempdir().unwrap();
            let root = fs::canonicalize(tmp.path()).unwrap().join("root");
            let project = root.join("project");
            fs::create_dir_all(&project).unwrap();
            setup_git(&project, &["init"]);
            setup_git(&project, &["commit", "--allow-empty", "-m", "init"]);
            Fixture { _tmp: tmp, root }
        }

        fn roots(&self) -> Vec<PathBuf> {
            vec![self.root.clone()]
        }

        fn project(&self) -> PathBuf {
            self.root.join("project")
        }

        fn create(&self, workdir: &Path, branch: &str) -> Result<Created, RejectReason> {
            create(
                &git(),
                &self.roots(),
                text(workdir).unwrap(),
                branch,
                deadline(),
            )
        }
    }

    #[test]
    fn creates_a_worktree_next_to_the_repository() {
        let f = Fixture::new();
        let created = f.create(&f.project(), "feat/login").unwrap();
        let path = f.root.join("project.worktrees/feat/login");
        assert_eq!(
            created,
            Created {
                path: text(&path).unwrap().to_owned(),
                repo: text(&f.project()).unwrap().to_owned(),
                branch: "feat/login".to_owned(),
            }
        );
        assert!(path.is_dir());
        let head = git_in(&path, &["rev-parse", "--abbrev-ref", "HEAD"]);
        assert_eq!(String::from_utf8_lossy(&head.stdout), "feat/login\n");
    }

    #[test]
    fn a_subdirectory_resolves_to_the_repository() {
        let f = Fixture::new();
        let sub = f.project().join("sub");
        fs::create_dir(&sub).unwrap();
        let created = f.create(&sub, "sub-work").unwrap();
        assert_eq!(created.repo, text(&f.project()).unwrap());
        assert_eq!(
            created.path,
            text(&f.root.join("project.worktrees/sub-work")).unwrap()
        );
    }

    #[test]
    fn a_worktree_workdir_resolves_to_the_main_repository() {
        let f = Fixture::new();
        let a = f.create(&f.project(), "a").unwrap();
        let b = f.create(Path::new(&a.path), "b").unwrap();
        assert_eq!(b.repo, text(&f.project()).unwrap());
        assert_eq!(b.path, text(&f.root.join("project.worktrees/b")).unwrap());
    }

    #[test]
    fn an_existing_branch_is_refused() {
        let f = Fixture::new();
        assert_eq!(
            f.create(&f.project(), "main"),
            Err(RejectReason::BranchExists)
        );
    }

    #[test]
    fn a_plain_directory_is_not_a_repository() {
        let f = Fixture::new();
        let plain = f.root.join("plain");
        fs::create_dir(&plain).unwrap();
        assert_eq!(
            f.create(&plain, "feat/x"),
            Err(RejectReason::NotARepository)
        );
    }

    #[test]
    fn a_repository_that_is_a_root_is_not_allowed() {
        let f = Fixture::new();
        let project = f.project();
        let roots = vec![project.clone()];
        assert_eq!(
            create(
                &git(),
                &roots,
                text(&project).unwrap(),
                "feat/x",
                deadline()
            ),
            Err(RejectReason::WorkdirNotAllowed)
        );
        assert!(!f.root.join("project.worktrees").exists());
    }

    #[test]
    fn bad_branch_names_are_refused() {
        let f = Fixture::new();
        for name in ["-x", "a..b", "../escape", "has space"] {
            assert_eq!(
                f.create(&f.project(), name),
                Err(RejectReason::WorktreeFailed),
                "{name}"
            );
        }
        let entries: Vec<String> = fs::read_dir(&f.root)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(entries, vec!["project"]);
        // Positive control: a good name from the same fixture is created.
        assert!(f.create(&f.project(), "ok").is_ok());
    }

    #[test]
    fn discard_removes_the_worktree_and_its_branch() {
        let f = Fixture::new();
        let created = f.create(&f.project(), "feat/login").unwrap();
        let path = PathBuf::from(&created.path);
        let verify = ["rev-parse", "--verify", "--quiet", "refs/heads/feat/login"];
        assert!(path.is_dir());
        assert!(git_in(&f.project(), &verify).status.success());
        discard(&git(), &created, deadline());
        assert!(!path.exists());
        assert!(!git_in(&f.project(), &verify).status.success());
    }

    /// The commit `dir`'s `HEAD` names.
    fn head_at(dir: &Path) -> String {
        let out = git_in(dir, &["rev-parse", "HEAD"]);
        String::from_utf8_lossy(&out.stdout).trim_end().to_owned()
    }

    fn has_branch(repo: &Path, branch: &str) -> bool {
        let full = format!("refs/heads/{branch}");
        git_in(repo, &["rev-parse", "--verify", "--quiet", &full])
            .status
            .success()
    }

    #[test]
    fn a_worktree_starts_at_the_workdirs_head() {
        let f = Fixture::new();
        let a = f.create(&f.project(), "a").unwrap();
        let a_path = PathBuf::from(&a.path);
        setup_git(&a_path, &["commit", "-q", "--allow-empty", "-m", "on a"]);
        let b = f.create(&a_path, "b").unwrap();
        let b_head = head_at(Path::new(&b.path));
        assert_eq!(b_head, head_at(&a_path));
        assert_ne!(b_head, head_at(&f.project()));
    }

    #[test]
    fn a_symlinked_worktrees_directory_is_refused() {
        let f = Fixture::new();
        let outside = f.root.parent().unwrap().join("outside");
        fs::create_dir(&outside).unwrap();
        let link = f.root.join("project.worktrees");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert_eq!(
            f.create(&f.project(), "feat/x"),
            Err(RejectReason::WorktreeFailed)
        );
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
        assert!(!has_branch(&f.project(), "feat/x"));
        // Positive control: without the link the same branch is created.
        fs::remove_file(&link).unwrap();
        assert!(f.create(&f.project(), "feat/x").is_ok());
    }

    #[test]
    fn a_symlink_for_a_branch_component_is_refused() {
        let f = Fixture::new();
        let outside = f.root.parent().unwrap().join("outside");
        fs::create_dir(&outside).unwrap();
        let worktrees = f.root.join("project.worktrees");
        fs::create_dir(&worktrees).unwrap();
        let link = worktrees.join("feat");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert_eq!(
            f.create(&f.project(), "feat/x"),
            Err(RejectReason::WorktreeFailed)
        );
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
        assert!(!has_branch(&f.project(), "feat/x"));
        // Positive control: without the link the same branch is created.
        fs::remove_file(&link).unwrap();
        assert!(f.create(&f.project(), "feat/x").is_ok());
    }

    #[test]
    fn a_failed_add_leaves_no_branch() {
        let f = Fixture::new();
        let worktrees = f.root.join("project.worktrees");
        fs::write(&worktrees, "not a directory").unwrap();
        assert_eq!(
            f.create(&f.project(), "feat/x"),
            Err(RejectReason::WorktreeFailed)
        );
        assert!(!has_branch(&f.project(), "feat/x"));
        // Positive control: without the file the same branch is created.
        fs::remove_file(&worktrees).unwrap();
        assert!(f.create(&f.project(), "feat/x").is_ok());
    }

    /// A failing `post-checkout` hook makes `git worktree add -b` exit non-zero
    /// after it has made the branch and the worktree. The cleanup removes both,
    /// so a retry is not refused `BranchExists`.
    #[test]
    fn a_worktree_add_failing_after_its_branch_leaves_nothing() {
        use std::os::unix::fs::PermissionsExt;

        let f = Fixture::new();
        let hooks = f.root.parent().unwrap().join("hooks");
        fs::create_dir(&hooks).unwrap();
        let hook = hooks.join("post-checkout");
        fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        setup_git(
            &f.project(),
            &["config", "core.hooksPath", text(&hooks).unwrap()],
        );
        let target = f.root.join("project.worktrees/feat/x");
        assert_eq!(
            f.create(&f.project(), "feat/x"),
            Err(RejectReason::WorktreeFailed)
        );
        assert!(!target.exists());
        assert!(!has_branch(&f.project(), "feat/x"));
        // Positive control: without the hook the same branch is created.
        setup_git(&f.project(), &["config", "--unset", "core.hooksPath"]);
        assert!(f.create(&f.project(), "feat/x").is_ok());
    }

    /// A killed add after its `mkdir` of the target: an empty directory git
    /// never registered, which `git worktree remove` refuses.
    #[test]
    fn a_worktree_add_killed_after_its_mkdir_is_undone() {
        let f = Fixture::new();
        setup_git(&f.project(), &["branch", "feat/x"]);
        let target = f.root.join("project.worktrees/feat/x");
        fs::create_dir_all(&target).unwrap();
        undo_failed_add(&git(), &f.project(), "feat/x", text(&target).unwrap());
        assert!(!target.exists());
        assert!(!has_branch(&f.project(), "feat/x"));
        assert!(f.create(&f.project(), "feat/x").is_ok());
    }

    /// A killed add after it wrote `.git`: the worktree is registered and
    /// locked, and the directory holds only the `.git` file.
    #[test]
    fn a_worktree_add_killed_after_its_git_file_is_undone() {
        let f = Fixture::new();
        let target = f.root.join("project.worktrees/feat/x");
        setup_git(
            &f.project(),
            &["worktree", "add", "-b", "feat/x", text(&target).unwrap()],
        );
        let locked = git_in(&f.project(), &["worktree", "lock", text(&target).unwrap()]);
        assert!(locked.status.success());
        for entry in fs::read_dir(&target).unwrap() {
            let path = entry.unwrap().path();
            if path.file_name() == Some(OsStr::new(".git")) {
                continue;
            }
            if path.is_dir() {
                fs::remove_dir_all(&path).unwrap();
            } else {
                fs::remove_file(&path).unwrap();
            }
        }
        undo_failed_add(&git(), &f.project(), "feat/x", text(&target).unwrap());
        assert!(!target.exists());
        assert!(!has_branch(&f.project(), "feat/x"));
        assert!(f.create(&f.project(), "feat/x").is_ok());
    }

    /// A directory with a user's file in it is never deleted, even when the
    /// add that made it failed. The branch is still undone.
    #[test]
    fn a_failed_add_keeps_a_target_with_files() {
        let f = Fixture::new();
        setup_git(&f.project(), &["branch", "feat/x"]);
        let target = f.root.join("project.worktrees/feat/x");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("keep.txt"), "mine").unwrap();
        undo_failed_add(&git(), &f.project(), "feat/x", text(&target).unwrap());
        assert!(target.join("keep.txt").exists());
        assert!(!has_branch(&f.project(), "feat/x"));
    }
}
