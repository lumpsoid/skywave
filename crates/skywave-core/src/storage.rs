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
    /// Id of the latest [`Request::Storage`]; never reused.
    req: u64,
    /// The request the screen waits on; `None` when nothing is in flight.
    waiting: Option<u64>,
    status: Option<StorageStatus>,
    report: Option<StorageReport>,
}

impl Storage {
    /// The screen opened: forget the last report and ask again.
    pub(crate) fn open(&mut self, online: bool) -> Option<Request> {
        self.report = None;
        self.refresh(online)
    }

    /// Asks the server again, keeping the report shown until the answer.
    /// Offline it is unavailable at once.
    pub(crate) fn refresh(&mut self, online: bool) -> Option<Request> {
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

    /// What the screen shows: its status and the latest report.
    pub(crate) fn shown(&self) -> (StorageStatus, Option<StorageReport>) {
        (
            self.status.clone().unwrap_or(StorageStatus::Loading),
            self.report.clone(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(database: u64) -> StorageReport {
        StorageReport {
            database,
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
}
