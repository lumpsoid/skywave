//! The git worktrees of the repositories sessions' worktrees were made from:
//! `GET` [`skywave_proto::path::WORKTREES`].

use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

use chrono::Utc;
use skywave_proto::{RepoListing, RepoWorktrees, WorktreeEntry, WorktreeList, limits};

use super::changes::neutralise_filters;
use super::git::{GitError, Repo};
use super::{Git, Snapshot};
use crate::dirs;
use crate::error::Result;
use crate::event_log;

impl Snapshot<'_> {
    /// `GET /v1/worktrees`, always answered. Recorded as nothing: the answer
    /// names no blob.
    pub fn worktrees(&mut self) -> Result<WorktreeList> {
        let at = Utc::now();
        let mut repos = Vec::new();
        for repo in event_log::worktree_repos(self.db.conn())? {
            let listing = if self.is_repo_dir(&repo) {
                match read(self.git, Path::new(&repo), self.roots, self.deadline) {
                    Ok(listing) => listing,
                    Err(GitError::Failed(reason)) => RepoListing::Failed { reason },
                    Err(GitError::Fault(e)) => return Err(e),
                }
            } else {
                RepoListing::Gone
            };
            repos.push(RepoWorktrees { repo, listing });
        }
        Ok(WorktreeList { at, repos })
    }

    /// Whether `repo` is still a directory that canonicalises under the roots
    /// to exactly itself.
    fn is_repo_dir(&self, repo: &str) -> bool {
        under_roots_as_is(self.roots, repo)
    }
}

/// Whether `dir` canonicalises under `roots` to exactly itself: the check
/// [`Snapshot::workdir`] makes of a session, and the one a worktree's status
/// is read under.
fn under_roots_as_is(roots: &[PathBuf], dir: &str) -> bool {
    matches!(
        dirs::canonical_under_roots(roots, dir),
        Ok(canonical) if canonical == dir
    )
}

/// The git side of `GET /v1/worktrees` for one repository: its worktrees
/// from `git worktree list --porcelain -z`, each with its status and merge
/// check, at most [`limits::WORKTREES_MAX`] of them in git's order. `Gone`
/// when `repo` is not its own top level: its `.git` is gone, so git finds the
/// repository above it.
fn read(
    git: &Git,
    repo: &Path,
    roots: &[PathBuf],
    deadline: Instant,
) -> Result<RepoListing, GitError> {
    let run = Repo::new(git, repo, deadline);
    let top = run.output(&["rev-parse", "--show-toplevel"])?;
    let printed = top.out.strip_suffix(b"\n").unwrap_or(top.out.as_slice());
    if !top.status.success() || printed != repo.as_os_str().as_bytes() {
        return Ok(RepoListing::Gone);
    }
    let out = run
        .output(&["worktree", "list", "--porcelain", "-z"])?
        .ok()?;
    let blocks = parse(&out)?;
    // The first block is the main working tree, bare or not.
    let main_head = blocks.first().and_then(|block| block.head.clone());
    let more = blocks.iter().filter(|block| !block.bare).count() > limits::WORKTREES_MAX;
    let mut worktrees = Vec::new();
    for (index, block) in blocks
        .iter()
        .enumerate()
        .filter(|(_, block)| !block.bare)
        .take(limits::WORKTREES_MAX)
    {
        let main = index == 0;
        let missing = block.missing || !Path::new(&block.path).is_dir();
        // A status is read only where a workdir read would be allowed.
        let dirty = if missing || !under_roots_as_is(roots, &block.path) {
            None
        } else {
            Some(has_changes(git, Path::new(&block.path), deadline)?)
        };
        let merged = match (&block.head, &main_head) {
            _ if main => true,
            (Some(head), Some(target)) => is_ancestor(&run, head, target)?,
            _ => false,
        };
        worktrees.push(WorktreeEntry {
            path: block.path.clone(),
            branch: block.branch.clone(),
            head: block.head.clone(),
            main,
            missing,
            dirty,
            merged,
        });
    }
    Ok(RepoListing::Worktrees { worktrees, more })
}

/// Whether the worktree at `path` has uncommitted or untracked changes
/// (`status --porcelain`, untracked files included), the check
/// `RemoveWorktree` makes too, except that `--ignore-submodules=dirty`
/// (as in Changes) leaves a submodule's own content unread: its dirty files
/// are not counted, and no filter in its config runs.
fn has_changes(git: &Git, path: &Path, deadline: Instant) -> Result<bool, GitError> {
    let mut repo = Repo::new(git, path, deadline);
    neutralise_filters(&mut repo)?;
    let status = repo
        .output(&[
            "status",
            "--porcelain",
            "--untracked-files=normal",
            "--ignore-submodules=dirty",
        ])?
        .ok()?;
    Ok(!status.is_empty())
}

/// Whether `head` is an ancestor of `target` in the repository `run` runs
/// in: exit 0 is yes, exit 1 no, any other exit git's failure.
fn is_ancestor(run: &Repo<'_>, head: &str, target: &str) -> Result<bool, GitError> {
    let out = run.output(&["merge-base", "--is-ancestor", head, target])?;
    match out.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(GitError::Failed(out.reason())),
    }
}

/// One block of `git worktree list --porcelain -z`.
#[derive(Debug, PartialEq)]
struct Listed {
    path: String,
    /// `None` when the block has no `HEAD` line, or it is all zeros (unborn).
    head: Option<String>,
    /// The branch with `refs/heads/` stripped; `None` when detached.
    branch: Option<String>,
    /// Its `prunable` line: git found its directory gone. `read` also counts
    /// a directory that is not there.
    missing: bool,
    bare: bool,
}

/// The blocks of `git worktree list --porcelain -z`, in git's order. Every
/// field ends in a NUL and a block ends with an empty field, so a path (which
/// git does not quote here) cannot start a block or a field of its own.
/// Unknown fields (`locked`, `detached`, a reason) are ignored. A path that is
/// not UTF-8, or a `HEAD` that is not a full sha, fails the whole list.
fn parse(porcelain: &[u8]) -> Result<Vec<Listed>, GitError> {
    let fields: Vec<&[u8]> = porcelain.split(|&b| b == 0).collect();
    fields
        .split(|field| field.is_empty())
        .filter(|block| !block.is_empty())
        .map(parse_block)
        .collect()
}

/// One block, from its fields in git's order.
fn parse_block(fields: &[&[u8]]) -> Result<Listed, GitError> {
    let mut listed = Listed {
        path: String::new(),
        head: None,
        branch: None,
        missing: false,
        bare: false,
    };
    for &field in fields {
        if let Some(path) = field.strip_prefix(b"worktree ") {
            listed.path = std::str::from_utf8(path)
                .map_err(|_| GitError::Failed("a worktree path is not UTF-8".into()))?
                .to_owned();
        } else if let Some(sha) = field.strip_prefix(b"HEAD ") {
            listed.head = head(sha)?;
        } else if let Some(name) = field.strip_prefix(b"branch refs/heads/") {
            listed.branch = Some(String::from_utf8_lossy(name).into_owned());
        } else if field == b"bare" {
            listed.bare = true;
        } else if field == b"prunable" || field.starts_with(b"prunable ") {
            listed.missing = true;
        }
    }
    Ok(listed)
}

/// A `HEAD` value: a full sha in lowercase hex, 40 or 64 digits. All zeros
/// is unborn (`None`); anything else is not git's list.
fn head(sha: &[u8]) -> Result<Option<String>, GitError> {
    let hex = sha
        .iter()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b));
    if !matches!(sha.len(), 40 | 64) || !hex {
        return Err(GitError::Failed("unexpected worktree list".into()));
    }
    if sha.iter().all(|&b| b == b'0') {
        return Ok(None);
    }
    Ok(Some(sha.iter().map(|&b| char::from(b)).collect()))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::super::changes::tests::{git, init, isolated, run_git, write};
    use super::super::tests::Tree;
    use super::*;

    const MAIN: &str = "1111111111111111111111111111111111111111";
    const FEAT: &str = "2222222222222222222222222222222222222222";
    const DETACHED: &str = "3333333333333333333333333333333333333333";
    const GONE: &str = "4444444444444444444444444444444444444444";

    const PORCELAIN: &[u8] = b"worktree /r\0\
        HEAD 1111111111111111111111111111111111111111\0\
        branch refs/heads/main\0\
        \0\
        worktree /r.worktrees/feat-x\0\
        HEAD 2222222222222222222222222222222222222222\0\
        branch refs/heads/feat/x\0\
        \0\
        worktree /r.worktrees/detached\0\
        HEAD 3333333333333333333333333333333333333333\0\
        detached\0\
        \0\
        worktree /r.worktrees/gone\0\
        HEAD 4444444444444444444444444444444444444444\0\
        branch refs/heads/gone\0\
        prunable gitdir file points to non-existent location\0\
        \0\
        worktree /r.bare\0\
        bare\0\
        \0\
        worktree /r.worktrees/unborn\0\
        HEAD 0000000000000000000000000000000000000000\0\
        branch refs/heads/unborn\0\
        locked a reason\0\
        \0";

    fn listed(path: &str, head: Option<&str>, branch: Option<&str>) -> Listed {
        Listed {
            path: path.into(),
            head: head.map(Into::into),
            branch: branch.map(Into::into),
            missing: false,
            bare: false,
        }
    }

    /// A list of one block: a worktree at `path` on `head`, NUL-terminated.
    fn one_block(path: &[u8], head: &[u8]) -> Vec<u8> {
        let mut out = b"worktree ".to_vec();
        out.extend_from_slice(path);
        out.extend_from_slice(b"\0HEAD ");
        out.extend_from_slice(head);
        out.extend_from_slice(b"\0\0");
        out
    }

    #[test]
    fn parse_reads_every_kind_of_block() {
        assert_eq!(
            parse(PORCELAIN).unwrap(),
            vec![
                listed("/r", Some(MAIN), Some("main")),
                listed("/r.worktrees/feat-x", Some(FEAT), Some("feat/x")),
                listed("/r.worktrees/detached", Some(DETACHED), None),
                Listed {
                    missing: true,
                    ..listed("/r.worktrees/gone", Some(GONE), Some("gone"))
                },
                Listed {
                    bare: true,
                    ..listed("/r.bare", None, None)
                },
                listed("/r.worktrees/unborn", None, Some("unborn")),
            ]
        );
    }

    #[test]
    fn a_newline_in_a_path_cannot_forge_a_block() {
        let forged = one_block(b"/r\n\nworktree /etc", MAIN.as_bytes());
        assert_eq!(
            parse(&forged).unwrap(),
            vec![listed("/r\n\nworktree /etc", Some(MAIN), None)]
        );
    }

    #[test]
    fn a_bad_head_fails_the_list() {
        assert!(matches!(
            parse(&one_block(b"/r", b"--upload-pack=x")),
            Err(GitError::Failed(_))
        ));
        // Positive control: a full sha parses.
        assert!(parse(&one_block(b"/r", MAIN.as_bytes())).is_ok());
    }

    #[test]
    fn a_path_that_is_not_utf8_fails_the_list() {
        assert!(matches!(
            parse(&one_block(b"/r\xff", MAIN.as_bytes())),
            Err(GitError::Failed(_))
        ));
        // Positive control: the same block with a UTF-8 path parses.
        assert!(parse(&one_block(b"/r", MAIN.as_bytes())).is_ok());
    }

    /// The listing of the repository at `repo`, which must be listed, under
    /// the roots `t.root`.
    fn listing(t: &Tree, repo: &Path) -> Vec<WorktreeEntry> {
        match read(
            &isolated(t),
            repo,
            std::slice::from_ref(&t.root),
            Tree::later(),
        ) {
            Ok(RepoListing::Worktrees { worktrees, more }) => {
                assert!(!more);
                worktrees
            }
            Ok(other) => panic!("expected worktrees, got {other:?}"),
            Err(_) => panic!("git failed for {}", repo.display()),
        }
    }

    #[test]
    fn a_repo_lists_its_worktrees_with_their_status() {
        let t = Tree::new();
        let repo = t.workdir.clone();
        init(&repo);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "one"]);
        let linked = t.root.join("project.worktrees/feat-x");
        std::fs::create_dir_all(linked.parent().unwrap()).unwrap();
        let linked_arg = linked.to_str().unwrap();
        git(
            &repo,
            &["worktree", "add", "-q", "-b", "feat/x", linked_arg],
        );

        let entries = listing(&t, &repo);
        assert_eq!(entries.len(), 2);
        let (main, feat) = (&entries[0], &entries[1]);
        assert!(main.main && main.merged && main.dirty == Some(false) && !main.missing);
        assert_eq!(main.branch.as_deref(), Some("main"));
        assert!(main.head.is_some());
        assert_eq!(feat.path, linked_arg);
        assert_eq!(feat.branch.as_deref(), Some("feat/x"));
        assert!(!feat.main && feat.merged && feat.dirty == Some(false) && !feat.missing);

        // Untracked is dirty.
        std::fs::write(linked.join("scratch"), "x").unwrap();
        let entries = listing(&t, &repo);
        assert_eq!(
            entries[1].dirty,
            Some(true),
            "an untracked file is a change"
        );
        assert_eq!(entries[0].dirty, Some(false));

        // Committed, it is clean again, and no longer merged: it is ahead.
        git(&linked, &["add", "-A"]);
        git(&linked, &["commit", "-q", "-m", "two"]);
        let entries = listing(&t, &repo);
        assert!(!entries[1].merged, "its head is ahead of main");
        assert_eq!(entries[1].dirty, Some(false));
        assert!(entries[0].merged);
    }

    #[test]
    fn a_listing_runs_no_filter_driver() {
        let t = Tree::new();
        let repo = t.workdir.clone();
        init(&repo);
        write(repo.join("f.x"), "old\n");
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "one"]);
        let linked = t.root.join("project.worktrees/feat-x");
        std::fs::create_dir_all(linked.parent().unwrap()).unwrap();
        let linked_arg = linked.to_str().unwrap();
        git(
            &repo,
            &["worktree", "add", "-q", "-b", "feat/x", linked_arg],
        );
        // The repo's own clean filter, shared by its linked worktrees, and
        // a tracked file changed in the linked one, so `status` must run
        // the filter to compare it.
        let mark = t.outside.join("clean-ran");
        let script = t.outside.join("clean.sh");
        write(
            script.clone(),
            format!("#!/bin/sh\ntouch '{}'\ncat\n", mark.display()),
        );
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        git(
            &repo,
            &["config", "filter.x.clean", script.to_str().unwrap()],
        );
        write(repo.join(".git/info/attributes"), "*.x filter=x\n");
        write(linked.join("f.x"), "new\n");

        let entries = listing(&t, &repo);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].dirty, Some(true), "the changed file is a change");
        assert!(!mark.exists(), "the listing ran the clean filter");
        // Positive control: a plain status there runs it.
        run_git(&linked, &["status", "--porcelain"]);
        assert!(mark.exists(), "the clean filter did not run");
    }

    #[test]
    fn a_submodules_filter_driver_does_not_run() {
        let t = Tree::new();
        let repo = t.workdir.clone();
        init(&repo);
        // The repository added as the submodule, with a tracked file the
        // filter will see changed in the linked worktree's copy.
        let src = t.outside.join("src");
        std::fs::create_dir_all(&src).unwrap();
        init(&src);
        write(src.join("f.x"), "old\n");
        git(&src, &["add", "-A"]);
        git(&src, &["commit", "-q", "-m", "one"]);
        git(&repo, &["submodule", "add", src.to_str().unwrap(), "sub"]);
        git(&repo, &["commit", "-q", "-m", "submodule"]);
        let linked = t.root.join("project.worktrees/feat-x");
        std::fs::create_dir_all(linked.parent().unwrap()).unwrap();
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feat/x",
                linked.to_str().unwrap(),
            ],
        );
        git(&linked, &["submodule", "update", "--init"]);
        // The filter lives in the submodule's own config and attributes,
        // which the superproject's neutralising does not reach.
        let sub = linked.join("sub");
        let mark = t.outside.join("clean-ran");
        let script = t.outside.join("clean.sh");
        write(
            script.clone(),
            format!("#!/bin/sh\ntouch '{}'\ncat\n", mark.display()),
        );
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        git(
            &sub,
            &["config", "filter.x.clean", script.to_str().unwrap()],
        );
        let attributes = git(&sub, &["rev-parse", "--git-path", "info/attributes"]);
        let attributes = String::from_utf8(attributes).unwrap();
        write(sub.join(attributes.trim_end()), "*.x filter=x\n");
        write(sub.join("f.x"), "new\n");

        let entries = listing(&t, &repo);
        assert_eq!(entries.len(), 2);
        assert!(
            !mark.exists(),
            "the listing ran the submodule's clean filter"
        );
        assert_eq!(
            entries[1].dirty,
            Some(false),
            "a submodule's own dirty content is not counted"
        );
        // Positive control: a plain status there runs it.
        run_git(&linked, &["status", "--porcelain"]);
        assert!(mark.exists(), "the clean filter did not run");
    }

    #[test]
    fn a_locked_worktree_whose_directory_is_gone_is_missing() {
        let t = Tree::new();
        let repo = t.workdir.clone();
        init(&repo);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "one"]);
        let linked = t.root.join("project.worktrees/feat-x");
        std::fs::create_dir_all(linked.parent().unwrap()).unwrap();
        let linked_arg = linked.to_str().unwrap();
        git(
            &repo,
            &["worktree", "add", "-q", "-b", "feat/x", linked_arg],
        );
        // Locked, git prints no `prunable` line for it once its directory is
        // gone.
        git(&repo, &["worktree", "lock", linked_arg]);
        std::fs::remove_dir_all(&linked).unwrap();

        let entries = listing(&t, &repo);
        assert_eq!(entries.len(), 2);
        assert!(entries[1].missing, "{:?}", entries[1]);
        assert_eq!(entries[1].dirty, None, "{:?}", entries[1]);
        assert_eq!(entries[0].dirty, Some(false));
    }

    #[test]
    fn a_directory_inside_another_repo_is_gone() {
        let t = Tree::new();
        init(&t.workdir);
        git(&t.workdir, &["commit", "-q", "--allow-empty", "-m", "one"]);
        let plain = t.workdir.join("proj");
        std::fs::create_dir_all(&plain).unwrap();
        let roots = [t.root.clone()];

        // Git would list the repository above it.
        let answer = read(&isolated(&t), &plain, &roots, Tree::later());
        assert!(matches!(answer, Ok(RepoListing::Gone)), "{answer:?}");
        // Positive control: the repository itself lists.
        let answer = read(&isolated(&t), &t.workdir, &roots, Tree::later());
        assert!(
            matches!(answer, Ok(RepoListing::Worktrees { .. })),
            "{answer:?}"
        );
    }

    #[test]
    fn a_worktree_outside_the_roots_has_no_status() {
        let t = Tree::new();
        let repo = t.workdir.clone();
        init(&repo);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "one"]);
        let linked = t.root.join("project.worktrees/feat-x");
        std::fs::create_dir_all(linked.parent().unwrap()).unwrap();
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feat/x",
                linked.to_str().unwrap(),
            ],
        );
        // Untracked, so it would be dirty if its status were read.
        write(linked.join("scratch"), "x\n");

        // Only the repository's own directory is under the roots.
        let roots = [repo.clone()];
        let answer = read(&isolated(&t), &repo, &roots, Tree::later());
        let Ok(RepoListing::Worktrees { worktrees, .. }) = answer else {
            panic!("expected worktrees, got {answer:?}");
        };
        assert_eq!(worktrees[0].dirty, Some(false));
        assert_eq!(worktrees[1].dirty, None, "{:?}", worktrees[1]);
        // Positive control: with the whole tree under the roots, it is read.
        let entries = listing(&t, &repo);
        assert_eq!(entries[1].dirty, Some(true));
    }
}
