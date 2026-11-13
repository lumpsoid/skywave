//! How a session list groups its rows by repo: a session in a git worktree
//! sits under the repo its worktree was made from, and a plain session run
//! in a worktree's directory sits there too. A head shows a header where a
//! group starts.

use skywave_core::SessionRow;

/// One group of a session list: the rows sharing a repo (or directory).
#[derive(Debug, Clone, PartialEq)]
pub struct Group<'a> {
    /// The repo's main working tree, else the directory; empty for rows with no workdir.
    pub key: &'a str,
    /// In the list's order.
    pub rows: Vec<&'a SessionRow>,
}

/// The rows grouped by repo. A row's key is, in the first rule that applies:
/// its worktree's repo; else the repo of a worktree row of `rows` in the same
/// directory (a plain session started inside a worktree belongs with its
/// repo); else its directory; else the empty key. Groups come in the order
/// of their first row, and rows keep their relative order within a group.
pub fn by_repo(rows: &[SessionRow]) -> Vec<Group<'_>> {
    let mut groups: Vec<Group<'_>> = Vec::new();
    for row in rows {
        let key = key_of(row, rows);
        match groups.iter_mut().find(|group| group.key == key) {
            Some(group) => group.rows.push(row),
            None => groups.push(Group {
                key,
                rows: vec![row],
            }),
        }
    }
    groups
}

/// The key `row` groups under among `rows` (see [`by_repo`]).
fn key_of<'a>(row: &'a SessionRow, rows: &'a [SessionRow]) -> &'a str {
    if let Some(worktree) = &row.worktree {
        return &worktree.repo;
    }
    let Some(workdir) = row.workdir.as_deref() else {
        return "";
    };
    rows.iter()
        .find(|other| other.worktree.is_some() && other.workdir.as_deref() == Some(workdir))
        .and_then(|other| other.worktree.as_ref())
        .map_or(workdir, |worktree| worktree.repo.as_str())
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use skywave_core::{SessionId, SessionRow, SessionState};
    use skywave_proto::Worktree;

    use super::*;

    /// A row titled `title`, in `workdir`, on `worktree` as (repo, branch).
    fn row(title: &str, workdir: Option<&str>, worktree: Option<(&str, &str)>) -> SessionRow {
        SessionRow {
            id: SessionId::new(),
            title: title.into(),
            named: false,
            rename: None,
            workdir: workdir.map(str::to_owned),
            state: SessionState::Idle,
            pending_approvals: 0,
            last_activity: Utc.with_ymd_and_hms(2026, 11, 13, 9, 0, 0).unwrap(),
            has_draft: false,
            unread: 0,
            cost_usd: None,
            deletable: false,
            importing: false,
            worktree: worktree.map(|(repo, branch)| Worktree {
                repo: repo.into(),
                branch: branch.into(),
            }),
            worktree_removed: false,
        }
    }

    /// The groups as their key and their rows' titles, for comparing.
    fn titles<'a>(groups: &[Group<'a>]) -> Vec<(&'a str, Vec<&'a str>)> {
        groups
            .iter()
            .map(|group| {
                (
                    group.key,
                    group.rows.iter().map(|row| row.title.as_str()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn rows_in_their_own_dirs_keep_their_order() {
        let rows = [
            row("a", Some("/a"), None),
            row("b", Some("/b"), None),
            row("c", Some("/c"), None),
        ];
        assert_eq!(
            titles(&by_repo(&rows)),
            vec![("/a", vec!["a"]), ("/b", vec!["b"]), ("/c", vec!["c"])]
        );
    }

    #[test]
    fn worktrees_group_under_their_repo() {
        let rows = [
            row("W1", Some("/srv/app.worktrees/x"), Some(("/srv/app", "x"))),
            row("P", Some("/srv/other"), None),
            row("M", Some("/srv/app"), None),
            row("W2", Some("/srv/app.worktrees/y"), Some(("/srv/app", "y"))),
            row("H", Some("/srv/app.worktrees/x"), None),
        ];
        assert_eq!(
            titles(&by_repo(&rows)),
            vec![
                ("/srv/app", vec!["W1", "M", "W2", "H"]),
                ("/srv/other", vec!["P"]),
            ]
        );
    }

    #[test]
    fn rows_without_a_workdir_share_the_empty_group() {
        let rows = [
            row("N1", None, None),
            row("X", Some("/x"), None),
            row("N2", None, None),
        ];
        assert_eq!(
            titles(&by_repo(&rows)),
            vec![("", vec!["N1", "N2"]), ("/x", vec!["X"])]
        );
    }
}
