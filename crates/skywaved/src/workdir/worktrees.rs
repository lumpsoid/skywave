//! The git worktrees of the repositories sessions' worktrees were made from:
//! `GET` [`skywave_proto::path::WORKTREES`].

use std::path::Path;
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
                match read(self.git, Path::new(&repo), self.deadline) {
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
    /// to exactly itself: the check [`Snapshot::workdir`] makes of a session.
    fn is_repo_dir(&self, repo: &str) -> bool {
        matches!(
            dirs::canonical_under_roots(self.roots, repo),
            Ok(canonical) if canonical == repo
        )
    }
}

/// The git side of `GET /v1/worktrees` for one repository: its worktrees
/// from `git worktree list --porcelain`, each with its status and merge
/// check, at most [`limits::WORKTREES_MAX`] of them in git's order.
fn read(git: &Git, repo: &Path, deadline: Instant) -> Result<RepoListing, GitError> {
    let run = Repo::new(git, repo, deadline);
    let out = run.output(&["worktree", "list", "--porcelain"])?.ok()?;
    let blocks = parse(&String::from_utf8_lossy(&out));
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
        let dirty = !block.missing && has_changes(git, Path::new(&block.path), deadline)?;
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
            missing: block.missing,
            dirty,
            merged,
        });
    }
    Ok(RepoListing::Worktrees { worktrees, more })
}

/// Whether the worktree at `path` has uncommitted or untracked changes
/// (`status --porcelain`, untracked files included): what `RemoveWorktree`
/// refuses on.
fn has_changes(git: &Git, path: &Path, deadline: Instant) -> Result<bool, GitError> {
    let mut repo = Repo::new(git, path, deadline);
    neutralise_filters(&mut repo)?;
    let status = repo
        .output(&["status", "--porcelain", "--untracked-files=normal"])?
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

/// One block of `git worktree list --porcelain`.
#[derive(Debug, PartialEq)]
struct Listed {
    path: String,
    /// `None` when the block has no `HEAD` line, or it is all zeros (unborn).
    head: Option<String>,
    /// The branch with `refs/heads/` stripped; `None` when detached.
    branch: Option<String>,
    /// Its directory is gone (a `prunable` line).
    missing: bool,
    bare: bool,
}

/// The blocks of `git worktree list --porcelain`, in git's order. Blocks are
/// separated by a blank line; unknown lines (`locked`, a reason) are ignored.
fn parse(porcelain: &str) -> Vec<Listed> {
    porcelain
        .split("\n\n")
        .filter(|block| !block.trim().is_empty())
        .map(|block| {
            let mut listed = Listed {
                path: String::new(),
                head: None,
                branch: None,
                missing: false,
                bare: false,
            };
            for line in block.lines() {
                if let Some(path) = line.strip_prefix("worktree ") {
                    listed.path = path.to_owned();
                } else if let Some(sha) = line.strip_prefix("HEAD ") {
                    listed.head = (!sha.bytes().all(|b| b == b'0')).then(|| sha.to_owned());
                } else if let Some(name) = line.strip_prefix("branch refs/heads/") {
                    listed.branch = Some(name.to_owned());
                } else if line == "bare" {
                    listed.bare = true;
                } else if line == "prunable" || line.starts_with("prunable ") {
                    listed.missing = true;
                }
            }
            listed
        })
        .collect()
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

    const PORCELAIN: &str = "worktree /r\n\
        HEAD 1111111111111111111111111111111111111111\n\
        branch refs/heads/main\n\
        \n\
        worktree /r.worktrees/feat-x\n\
        HEAD 2222222222222222222222222222222222222222\n\
        branch refs/heads/feat/x\n\
        \n\
        worktree /r.worktrees/detached\n\
        HEAD 3333333333333333333333333333333333333333\n\
        detached\n\
        \n\
        worktree /r.worktrees/gone\n\
        HEAD 4444444444444444444444444444444444444444\n\
        branch refs/heads/gone\n\
        prunable gitdir file points to non-existent location\n\
        \n\
        worktree /r.bare\n\
        bare\n\
        \n\
        worktree /r.worktrees/unborn\n\
        HEAD 0000000000000000000000000000000000000000\n\
        branch refs/heads/unborn\n\
        locked a reason\n\
        \n";

    fn listed(path: &str, head: Option<&str>, branch: Option<&str>) -> Listed {
        Listed {
            path: path.into(),
            head: head.map(Into::into),
            branch: branch.map(Into::into),
            missing: false,
            bare: false,
        }
    }

    #[test]
    fn parse_reads_every_kind_of_block() {
        assert_eq!(
            parse(PORCELAIN),
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

    /// The listing of the repository at `repo`, which must be listed.
    fn listing(t: &Tree, repo: &Path) -> Vec<WorktreeEntry> {
        match read(&isolated(t), repo, Tree::later()) {
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
        assert!(main.main && main.merged && !main.dirty && !main.missing);
        assert_eq!(main.branch.as_deref(), Some("main"));
        assert!(main.head.is_some());
        assert_eq!(feat.path, linked_arg);
        assert_eq!(feat.branch.as_deref(), Some("feat/x"));
        assert!(!feat.main && feat.merged && !feat.dirty && !feat.missing);

        // Untracked is dirty.
        std::fs::write(linked.join("scratch"), "x").unwrap();
        let entries = listing(&t, &repo);
        assert!(entries[1].dirty, "an untracked file is a change");
        assert!(!entries[0].dirty);

        // Committed, it is clean again, and no longer merged: it is ahead.
        git(&linked, &["add", "-A"]);
        git(&linked, &["commit", "-q", "-m", "two"]);
        let entries = listing(&t, &repo);
        assert!(!entries[1].merged, "its head is ahead of main");
        assert!(!entries[1].dirty);
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
        assert!(entries[1].dirty, "the changed file is a change");
        assert!(!mark.exists(), "the listing ran the clean filter");
        // Positive control: a plain status there runs it.
        run_git(&linked, &["status", "--porcelain"]);
        assert!(mark.exists(), "the clean filter did not run");
    }
}
