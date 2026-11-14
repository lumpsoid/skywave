//! The worktrees screen: what the server says of its git worktrees, the
//! repositories sessions' worktrees came from, and the sessions in each,
//! worded once for every head.
//!
//! Paths, branches and failure reasons are text from the server: heads
//! sanitise them before writing them anywhere raw.

use skywave_core::{Outgoing, WorktreesStatus, WorktreesUnavailable, WorktreesView};
use skywave_proto::{RepoListing, WorktreeEntry};

use crate::format::{fail_reason, outgoing_label};

/// What the screen says above the list while it has no fresh one; `None`
/// once the list is the server's latest answer.
pub fn status_line(view: &WorktreesView) -> Option<String> {
    let have = view.list.is_some();
    match &view.status {
        WorktreesStatus::Ready => None,
        WorktreesStatus::Loading if have => Some("Refreshing…".to_owned()),
        WorktreesStatus::Loading => Some("Asking the server…".to_owned()),
        WorktreesStatus::Unavailable(WorktreesUnavailable::Offline) => Some(if have {
            "Offline: this list may be out of date.".to_owned()
        } else {
            "Offline: the worktrees are read only when connected.".to_owned()
        }),
        WorktreesStatus::Unavailable(WorktreesUnavailable::Failed(reason)) => Some(if have {
            format!("Refresh failed ({reason}); this list may be out of date.")
        } else {
            format!("Could not read the worktrees: {reason}")
        }),
    }
}

/// What an empty list says: `None` once it has repositories, or while
/// there is no list yet.
pub fn empty_label(view: &WorktreesView) -> Option<String> {
    let list = view.list.as_ref()?;
    list.repos
        .is_empty()
        .then(|| "No session runs in a worktree yet.".to_owned())
}

/// Why a repository's list is short or missing; `None` when it is whole.
pub fn listing_note(listing: &RepoListing) -> Option<String> {
    match listing {
        RepoListing::Worktrees { more: true, .. } => Some("More worktrees than shown.".to_owned()),
        RepoListing::Worktrees { more: false, .. } => None,
        RepoListing::Gone => Some("Not a repository any more.".to_owned()),
        RepoListing::Failed { reason } => Some(format!("git failed: {reason}")),
    }
}

/// The checked-out branch; `detached at` the first seven characters of
/// `HEAD` when no branch is checked out, else `detached`.
pub fn branch_label(entry: &WorktreeEntry) -> String {
    match (&entry.branch, &entry.head) {
        (Some(branch), _) => branch.clone(),
        (None, Some(head)) => format!("detached at {}", head.chars().take(7).collect::<String>()),
        (None, None) => "detached".to_owned(),
    }
}

/// What is true of a worktree, in the order a head shows it: `main`,
/// `missing`, the status (`uncommitted changes`, or `status unknown` when
/// it was not read for a directory that is there), then `merged` or
/// `not merged` for every worktree but the main one.
pub fn tags(entry: &WorktreeEntry) -> Vec<String> {
    let mut tags = Vec::new();
    if entry.main {
        tags.push("main");
    }
    if entry.missing {
        tags.push("missing");
    }
    match entry.dirty {
        Some(true) => tags.push("uncommitted changes"),
        None if !entry.missing => tags.push("status unknown"),
        Some(false) | None => {}
    }
    if !entry.main {
        tags.push(if entry.merged { "merged" } else { "not merged" });
    }
    tags.into_iter().map(str::to_owned).collect()
}

/// A removal of a session's worktree on its way, or why it failed; `None`
/// when there is none or it is settled.
pub fn removal_label(removal: Option<&Outgoing>) -> Option<String> {
    match removal? {
        Outgoing::Accepted(_) => None,
        Outgoing::Failed(reason) => Some(format!("remove failed: {}", fail_reason(*reason))),
        pending => Some(outgoing_label(pending)),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use skywave_core::{FailReason, Seq, WorktreesStatus, WorktreesUnavailable, WorktreesView};
    use skywave_proto::{RepoWorktrees, WorktreeList};

    use super::*;

    const HEAD: &str = "1a2b3c4d5e6f708192a3b4c5d6e7f80910111213";

    fn entry() -> WorktreeEntry {
        WorktreeEntry {
            path: "/srv/app.worktrees/feat-x".into(),
            branch: Some("feat-x".into()),
            head: Some(HEAD.into()),
            main: false,
            missing: false,
            dirty: Some(false),
            merged: false,
        }
    }

    fn view(list: Option<WorktreeList>, status: WorktreesStatus) -> WorktreesView {
        WorktreesView {
            status,
            list,
            sessions: HashMap::new(),
        }
    }

    fn list(repos: Vec<RepoWorktrees>) -> WorktreeList {
        WorktreeList {
            at: Default::default(),
            repos,
        }
    }

    #[test]
    fn tags_say_what_a_worktree_is_and_how_its_branch_stands() {
        let main = WorktreeEntry {
            main: true,
            merged: true,
            ..entry()
        };
        assert_eq!(tags(&main), ["main"]);
        assert_eq!(tags(&entry()), ["not merged"]);
        let merged = WorktreeEntry {
            merged: true,
            ..entry()
        };
        assert_eq!(tags(&merged), ["merged"]);
        let dirty = WorktreeEntry {
            dirty: Some(true),
            ..entry()
        };
        assert_eq!(tags(&dirty), ["uncommitted changes", "not merged"]);
        let unknown = WorktreeEntry {
            dirty: None,
            ..entry()
        };
        assert_eq!(tags(&unknown), ["status unknown", "not merged"]);
        // A missing directory was never read, so its status is not unknown.
        let missing = WorktreeEntry {
            missing: true,
            dirty: None,
            merged: true,
            ..entry()
        };
        assert_eq!(tags(&missing), ["missing", "merged"]);
    }

    #[test]
    fn the_branch_label_names_the_branch_or_the_detached_head() {
        assert_eq!(branch_label(&entry()), "feat-x");
        let detached = WorktreeEntry {
            branch: None,
            ..entry()
        };
        assert_eq!(branch_label(&detached), "detached at 1a2b3c4");
        let unborn = WorktreeEntry {
            branch: None,
            head: None,
            ..entry()
        };
        assert_eq!(branch_label(&unborn), "detached");
    }

    #[test]
    fn the_status_line_says_whether_the_list_is_fresh_in_every_state() {
        let offline = WorktreesStatus::Unavailable(WorktreesUnavailable::Offline);
        let failed = WorktreesStatus::Unavailable(WorktreesUnavailable::Failed("timed out".into()));
        assert_eq!(
            status_line(&view(Some(list(Vec::new())), WorktreesStatus::Ready)),
            None
        );
        assert_eq!(
            status_line(&view(None, WorktreesStatus::Loading)).unwrap(),
            "Asking the server…"
        );
        assert_eq!(
            status_line(&view(Some(list(Vec::new())), WorktreesStatus::Loading)).unwrap(),
            "Refreshing…"
        );
        assert_eq!(
            status_line(&view(None, offline.clone())).unwrap(),
            "Offline: the worktrees are read only when connected."
        );
        assert_eq!(
            status_line(&view(Some(list(Vec::new())), offline)).unwrap(),
            "Offline: this list may be out of date."
        );
        assert_eq!(
            status_line(&view(None, failed.clone())).unwrap(),
            "Could not read the worktrees: timed out"
        );
        assert_eq!(
            status_line(&view(Some(list(Vec::new())), failed)).unwrap(),
            "Refresh failed (timed out); this list may be out of date."
        );
    }

    #[test]
    fn the_empty_label_says_no_session_runs_in_a_worktree() {
        assert_eq!(empty_label(&view(None, WorktreesStatus::Loading)), None);
        assert_eq!(
            empty_label(&view(Some(list(Vec::new())), WorktreesStatus::Ready)).unwrap(),
            "No session runs in a worktree yet."
        );
        let repo = RepoWorktrees {
            repo: "/srv/app".into(),
            listing: RepoListing::Gone,
        };
        assert_eq!(
            empty_label(&view(Some(list(vec![repo])), WorktreesStatus::Ready)),
            None
        );
    }

    #[test]
    fn a_repository_note_says_why_its_list_is_short_or_missing() {
        assert_eq!(
            listing_note(&RepoListing::Worktrees {
                worktrees: Vec::new(),
                more: true,
            })
            .unwrap(),
            "More worktrees than shown."
        );
        assert_eq!(
            listing_note(&RepoListing::Worktrees {
                worktrees: Vec::new(),
                more: false,
            }),
            None
        );
        assert_eq!(
            listing_note(&RepoListing::Gone).unwrap(),
            "Not a repository any more."
        );
        assert_eq!(
            listing_note(&RepoListing::Failed {
                reason: "exit 128".into(),
            })
            .unwrap(),
            "git failed: exit 128"
        );
    }

    #[test]
    fn a_removal_says_what_is_on_its_way_or_why_it_failed() {
        assert_eq!(removal_label(None), None);
        assert_eq!(removal_label(Some(&Outgoing::Accepted(Seq(4)))), None);
        assert_eq!(removal_label(Some(&Outgoing::Queued)).unwrap(), "queued");
        assert_eq!(removal_label(Some(&Outgoing::Sending)).unwrap(), "sending");
        assert_eq!(
            removal_label(Some(&Outgoing::Failed(FailReason::Undeliverable))).unwrap(),
            "remove failed: the server could not take it"
        );
    }
}
