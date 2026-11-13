//! Git worktrees for sessions that ask for one (`CreateSession.worktree`):
//! created next to the repository, under `<repo>.worktrees/<branch>`.
//!
//! The repository is found from the workdir with `--git-common-dir`, so a
//! subdirectory resolves to the repository's root and a worktree resolves to
//! the main repository. A bare repository is refused. The main working tree's
//! parent must lie under a configured root (D14). Outcomes come from git's
//! exit statuses; a timeout is a failure.
//!
//! [`discard`] is rollback only: the caller uses it for a worktree [`create`]
//! made for a session that was not committed ([`Prepared`]). `--force` is
//! safe there because only a worktree just made is ever discarded.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use skywave_proto::RejectReason;

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

/// Creates a worktree of `workdir`'s repository on a new `branch`, starting
/// at the repository's current `HEAD`. Refusals: `WorkdirNotAllowed` or
/// `WorkdirNotFound` from the roots check, `NotARepository`, `BranchExists`;
/// anything else that stops the create is `WorktreeFailed`.
pub fn create(
    git: &Git,
    roots: &[PathBuf],
    workdir: &str,
    branch: &str,
    deadline: Instant,
) -> Result<Created, RejectReason> {
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
    if fs::symlink_metadata(&target).is_ok() {
        return Err(RejectReason::WorktreeFailed);
    }
    let add = ["worktree", "add", "-b", branch, target_text.as_str()];
    if !succeeded(
        git,
        &repo,
        deadline,
        &add,
        branch,
        "git worktree add failed",
    ) {
        return Err(RejectReason::WorktreeFailed);
    }
    let made = Created {
        path: target_text,
        repo: repo_text,
        branch: branch.to_owned(),
    };
    finish(git, made, &target, deadline)
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

/// The created worktree with its path canonical. If that cannot be done the
/// worktree is discarded, so a failed create leaves nothing unreported.
fn finish(
    git: &Git,
    made: Created,
    target: &Path,
    deadline: Instant,
) -> Result<Created, RejectReason> {
    let canonical = fs::canonicalize(target)
        .ok()
        .and_then(|path| path.into_os_string().into_string().ok());
    match canonical {
        Some(path) => Ok(Created { path, ..made }),
        None => {
            tracing::warn!(branch = %made.branch, "cannot canonicalise the new worktree");
            discard(git, &made, deadline);
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
}
