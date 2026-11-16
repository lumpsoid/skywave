//! The storage screen: what the server's data takes on its disk, asked for
//! when the screen opens and on every refresh.
//!
//! Only the latest request's answer is kept; an older one is dropped. A
//! failed refresh keeps the report before it. Offline nothing is asked and
//! the screen says so at once. Everything here is in memory: the report
//! goes stale as the server works, and opening the screen asks again.

use skywave_proto::StorageReport;

use crate::seam::Request;
use crate::transport::TransportError;
use crate::view::{StorageStatus, StorageUnavailable};

#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Storage {
    /// Id of the latest [`Request::Storage`] or [`Request::ReclaimStorage`];
    /// never reused.
    req: u64,
    /// The request the screen waits on; `None` when nothing is in flight.
    waiting: Option<u64>,
    status: Option<StorageStatus>,
    report: Option<StorageReport>,
}

impl Storage {
    /// The screen opened: forget the last report and any reclaim in flight,
    /// and ask again.
    pub(crate) fn open(&mut self, online: bool) -> Option<Request> {
        self.report = None;
        // Cleared so that `refresh` is not held back by a reclaim.
        self.status = None;
        self.refresh(online)
    }

    /// Asks the server again, keeping the report shown until the answer.
    /// Offline it is unavailable at once. Nothing while a reclaim runs.
    pub(crate) fn refresh(&mut self, online: bool) -> Option<Request> {
        if self.reclaiming() {
            return None;
        }
        if !online {
            self.waiting = None;
            self.status = Some(StorageStatus::Unavailable(StorageUnavailable::Offline));
            return None;
        }
        self.req += 1;
        self.waiting = Some(self.req);
        self.status = Some(StorageStatus::Loading);
        Some(Request::Storage { req: self.req })
    }

    /// Asks the server to reclaim its space, keeping the report shown until
    /// the answer. Nothing while a reclaim runs. Offline it is unavailable
    /// at once.
    pub(crate) fn reclaim(&mut self, online: bool) -> Option<Request> {
        if self.reclaiming() {
            return None;
        }
        if !online {
            self.waiting = None;
            self.status = Some(StorageStatus::Unavailable(StorageUnavailable::Offline));
            return None;
        }
        self.req += 1;
        self.waiting = Some(self.req);
        self.status = Some(StorageStatus::Reclaiming);
        Some(Request::ReclaimStorage { req: self.req })
    }

    /// The server's answer to request `req`; a stale one is ignored.
    pub(crate) fn reported(&mut self, req: u64, result: Result<StorageReport, TransportError>) {
        if self.waiting != Some(req) {
            return;
        }
        self.waiting = None;
        self.status = Some(match result {
            Ok(report) => {
                self.report = Some(report);
                StorageStatus::Ready
            }
            // The engine's answer while the link is down, or a server that
            // could not be reached.
            Err(TransportError::Unreachable(_)) => {
                StorageStatus::Unavailable(StorageUnavailable::Offline)
            }
            Err(error) => {
                tracing::debug!(%error, "storage report failed");
                StorageStatus::Unavailable(StorageUnavailable::Failed(error.to_string()))
            }
        });
    }

    /// The server's answer to reclaim request `req`; a stale one is ignored.
    /// A `507` means its free disk is below what a reclaim needs: nothing
    /// was done, and the report shown stays.
    pub(crate) fn reclaimed(&mut self, req: u64, result: Result<StorageReport, TransportError>) {
        if matches!(result, Err(TransportError::BadStatus(507))) && self.waiting == Some(req) {
            self.waiting = None;
            self.status = Some(StorageStatus::Unavailable(
                StorageUnavailable::NoRoomToReclaim,
            ));
        } else {
            self.reported(req, result);
        }
    }

    /// What the screen shows: its status and the latest report.
    pub(crate) fn shown(&self) -> (StorageStatus, Option<StorageReport>) {
        (
            self.status.clone().unwrap_or(StorageStatus::Loading),
            self.report.clone(),
        )
    }

    fn reclaiming(&self) -> bool {
        matches!(self.status, Some(StorageStatus::Reclaiming))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(database: u64) -> StorageReport {
        StorageReport {
            database,
            reclaimable: 0,
            event_blobs: 0,
            workdir_blobs: 0,
            shell_running: 0,
            log_file: None,
            free: None,
            min_free: 0,
            sessions: Vec::new(),
            blobs: Vec::new(),
            shell_runs: Vec::new(),
        }
    }

    fn req(request: Option<Request>) -> u64 {
        match request {
            Some(Request::Storage { req }) => req,
            other => panic!("expected a storage request, got {other:?}"),
        }
    }

    fn reclaim_req(request: Option<Request>) -> u64 {
        match request {
            Some(Request::ReclaimStorage { req }) => req,
            other => panic!("expected a reclaim request, got {other:?}"),
        }
    }

    #[test]
    fn opening_asks_and_shows_the_answer() {
        let mut storage = Storage::default();
        let first = req(storage.open(true));
        assert_eq!(storage.shown(), (StorageStatus::Loading, None));
        storage.reported(first, Ok(report(1)));
        assert_eq!(storage.shown(), (StorageStatus::Ready, Some(report(1))));
    }

    #[test]
    fn only_the_latest_request_is_answered() {
        let mut storage = Storage::default();
        let first = req(storage.open(true));
        let second = req(storage.refresh(true));
        storage.reported(first, Ok(report(1)));
        assert_eq!(storage.shown(), (StorageStatus::Loading, None));
        storage.reported(second, Ok(report(2)));
        assert_eq!(storage.shown(), (StorageStatus::Ready, Some(report(2))));
        // Answered: a repeat of it changes nothing.
        storage.reported(second, Ok(report(3)));
        assert_eq!(storage.shown().1, Some(report(2)));
    }

    #[test]
    fn a_failed_refresh_keeps_the_report_and_reopening_drops_it() {
        let mut storage = Storage::default();
        let first = req(storage.open(true));
        storage.reported(first, Ok(report(1)));
        let second = req(storage.refresh(true));
        assert_eq!(storage.shown(), (StorageStatus::Loading, Some(report(1))));
        storage.reported(second, Err(TransportError::Timeout));
        let (status, kept) = storage.shown();
        assert!(matches!(
            status,
            StorageStatus::Unavailable(StorageUnavailable::Failed(_))
        ));
        assert_eq!(kept, Some(report(1)));
        req(storage.open(true));
        assert_eq!(storage.shown(), (StorageStatus::Loading, None));
    }

    #[test]
    fn offline_nothing_is_asked() {
        let mut storage = Storage::default();
        assert_eq!(storage.open(false), None);
        assert_eq!(
            storage.shown(),
            (
                StorageStatus::Unavailable(StorageUnavailable::Offline),
                None
            )
        );
    }

    #[test]
    fn reclaiming_shows_the_fresh_report() {
        let mut storage = Storage::default();
        let first = req(storage.open(true));
        storage.reported(first, Ok(report(1)));
        let reclaim = reclaim_req(storage.reclaim(true));
        assert_eq!(
            storage.shown(),
            (StorageStatus::Reclaiming, Some(report(1)))
        );
        // Nothing else is asked while it runs.
        assert_eq!(storage.refresh(true), None);
        assert_eq!(storage.reclaim(true), None);
        assert_eq!(storage.shown().0, StorageStatus::Reclaiming);
        storage.reclaimed(reclaim, Ok(report(2)));
        assert_eq!(storage.shown(), (StorageStatus::Ready, Some(report(2))));
    }

    #[test]
    fn opening_the_screen_forgets_a_reclaim_in_flight() {
        let mut storage = Storage::default();
        let first = req(storage.open(true));
        let reclaim = reclaim_req(storage.reclaim(true));
        let reopened = req(storage.open(true));
        assert_ne!(reopened, first);
        assert_eq!(storage.shown(), (StorageStatus::Loading, None));
        // The reclaim's answer is no longer waited on.
        storage.reclaimed(reclaim, Ok(report(1)));
        assert_eq!(storage.shown(), (StorageStatus::Loading, None));
        storage.reported(reopened, Ok(report(2)));
        assert_eq!(storage.shown(), (StorageStatus::Ready, Some(report(2))));
    }

    #[test]
    fn a_reclaim_with_no_room_says_so_and_keeps_the_report() {
        let mut storage = Storage::default();
        let first = req(storage.open(true));
        storage.reported(first, Ok(report(1)));
        let reclaim = reclaim_req(storage.reclaim(true));
        storage.reclaimed(reclaim, Err(TransportError::BadStatus(507)));
        assert_eq!(
            storage.shown(),
            (
                StorageStatus::Unavailable(StorageUnavailable::NoRoomToReclaim),
                Some(report(1))
            )
        );
        // Any other failure of a reclaim is a plain failure.
        let busy = reclaim_req(storage.reclaim(true));
        storage.reclaimed(busy, Err(TransportError::BadStatus(503)));
        assert!(matches!(
            storage.shown().0,
            StorageStatus::Unavailable(StorageUnavailable::Failed(_))
        ));
    }

    #[test]
    fn offline_nothing_is_reclaimed() {
        let mut storage = Storage::default();
        let first = req(storage.open(true));
        storage.reported(first, Ok(report(1)));
        assert_eq!(storage.reclaim(false), None);
        assert_eq!(
            storage.shown(),
            (
                StorageStatus::Unavailable(StorageUnavailable::Offline),
                Some(report(1))
            )
        );
        reclaim_req(storage.reclaim(true));
    }
}
