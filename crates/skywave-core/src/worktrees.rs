//! The worktrees screen's list: in memory only; opening the screen asks again.
//!
//! Only the latest request's answer is kept; an older one is dropped. A
//! failed refresh keeps the list before it. Offline nothing is asked and the
//! screen says so at once.

use skywave_proto::WorktreeList;

use crate::seam::Request;
use crate::transport::TransportError;
use crate::view::{WorktreesStatus, WorktreesUnavailable};

#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Worktrees {
    /// Id of the latest [`Request::Worktrees`]; never reused.
    req: u64,
    /// The request the screen waits on; `None` when nothing is in flight.
    waiting: Option<u64>,
    status: Option<WorktreesStatus>,
    list: Option<WorktreeList>,
}

impl Worktrees {
    /// The screen opened: forget the last list and ask again.
    pub(crate) fn open(&mut self, online: bool) -> Option<Request> {
        self.list = None;
        self.refresh(online)
    }

    /// Asks the server again, keeping the list shown until the answer.
    /// Offline it is unavailable at once.
    pub(crate) fn refresh(&mut self, online: bool) -> Option<Request> {
        if !online {
            self.waiting = None;
            self.status = Some(WorktreesStatus::Unavailable(WorktreesUnavailable::Offline));
            return None;
        }
        self.req += 1;
        self.waiting = Some(self.req);
        self.status = Some(WorktreesStatus::Loading);
        Some(Request::Worktrees { req: self.req })
    }

    /// The server's answer to request `req`; a stale one is ignored.
    pub(crate) fn listed(&mut self, req: u64, result: Result<WorktreeList, TransportError>) {
        if self.waiting != Some(req) {
            return;
        }
        self.waiting = None;
        self.status = Some(match result {
            Ok(list) => {
                self.list = Some(list);
                WorktreesStatus::Ready
            }
            // The engine's answer while the link is down, or a server that
            // could not be reached.
            Err(TransportError::Unreachable(_)) => {
                WorktreesStatus::Unavailable(WorktreesUnavailable::Offline)
            }
            Err(error) => {
                tracing::debug!(%error, "worktree list failed");
                WorktreesStatus::Unavailable(WorktreesUnavailable::Failed(error.to_string()))
            }
        });
    }

    /// What the screen shows: its status and the latest list.
    pub(crate) fn shown(&self) -> (WorktreesStatus, Option<WorktreeList>) {
        (
            self.status.clone().unwrap_or(WorktreesStatus::Loading),
            self.list.clone(),
        )
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use skywave_proto::{RepoListing, RepoWorktrees};

    use super::*;

    fn list(repo: &str) -> WorktreeList {
        WorktreeList {
            at: Utc.timestamp_opt(0, 0).unwrap(),
            repos: vec![RepoWorktrees {
                repo: repo.into(),
                listing: RepoListing::Gone,
            }],
        }
    }

    fn req(request: Option<Request>) -> u64 {
        match request {
            Some(Request::Worktrees { req }) => req,
            other => panic!("expected a worktrees request, got {other:?}"),
        }
    }

    #[test]
    fn opening_asks_and_shows_the_answer() {
        let mut worktrees = Worktrees::default();
        let first = req(worktrees.open(true));
        assert_eq!(worktrees.shown(), (WorktreesStatus::Loading, None));
        worktrees.listed(first, Ok(list("/repos/one")));
        assert_eq!(
            worktrees.shown(),
            (WorktreesStatus::Ready, Some(list("/repos/one")))
        );
    }

    #[test]
    fn only_the_latest_request_is_answered() {
        let mut worktrees = Worktrees::default();
        let first = req(worktrees.open(true));
        let second = req(worktrees.refresh(true));
        worktrees.listed(first, Ok(list("/repos/one")));
        assert_eq!(worktrees.shown(), (WorktreesStatus::Loading, None));
        worktrees.listed(second, Ok(list("/repos/two")));
        assert_eq!(
            worktrees.shown(),
            (WorktreesStatus::Ready, Some(list("/repos/two")))
        );
        // Answered: a repeat of it changes nothing.
        worktrees.listed(second, Ok(list("/repos/three")));
        assert_eq!(worktrees.shown().1, Some(list("/repos/two")));
    }

    #[test]
    fn a_failed_refresh_keeps_the_list_and_reopening_drops_it() {
        let mut worktrees = Worktrees::default();
        let first = req(worktrees.open(true));
        worktrees.listed(first, Ok(list("/repos/one")));
        let second = req(worktrees.refresh(true));
        assert_eq!(
            worktrees.shown(),
            (WorktreesStatus::Loading, Some(list("/repos/one")))
        );
        worktrees.listed(second, Err(TransportError::Timeout));
        let (status, kept) = worktrees.shown();
        assert!(matches!(
            status,
            WorktreesStatus::Unavailable(WorktreesUnavailable::Failed(_))
        ));
        assert_eq!(kept, Some(list("/repos/one")));
        req(worktrees.open(true));
        assert_eq!(worktrees.shown(), (WorktreesStatus::Loading, None));
    }

    #[test]
    fn offline_nothing_is_asked() {
        let mut worktrees = Worktrees::default();
        assert_eq!(worktrees.open(false), None);
        assert_eq!(
            worktrees.shown(),
            (
                WorktreesStatus::Unavailable(WorktreesUnavailable::Offline),
                None
            )
        );
    }
}
