//! The storage screen: what the server's data takes on its disk, and what
//! this device keeps, asked for when the screen opens and on every refresh.
//!
//! Only the latest request's answer is kept; an older one is dropped. A
//! failed refresh keeps the report before it. Offline nothing is asked of the
//! server and the screen says so at once; this device's report is asked
//! either way. Everything here is in memory: the report goes stale as the
//! server works, and opening the screen asks again.

use skywave_proto::StorageReport;

use crate::seam::Request;
use crate::store::StoreError;
use crate::transport::TransportError;
use crate::view::{DeviceStorage, StorageStatus, StorageUnavailable};

#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Storage {
    /// Id of the latest [`Request::Storage`] or [`Request::ReclaimStorage`];
    /// never reused.
    req: u64,
    /// The request the screen waits on; `None` when nothing is in flight.
    waiting: Option<u64>,
    status: Option<StorageStatus>,
    report: Option<StorageReport>,
    /// Id of the latest [`Request::DeviceStorage`]; never reused.
    device_req: u64,
    /// The device request the screen waits on; `None` when nothing is in
    /// flight.
    device_waiting: Option<u64>,
    device: Option<DeviceStorage>,
}

impl Storage {
    /// The screen opened: forget the last reports and any reclaim in flight,
    /// and ask again.
    pub(crate) fn open(&mut self, online: bool) -> Vec<Request> {
        self.report = None;
        self.device = None;
        // Cleared so that `refresh` is not held back by a reclaim.
        self.status = None;
        self.refresh(online)
    }

    /// Asks the server again, and this device for its report, keeping the
    /// reports shown until the answers. Offline the server is unavailable at
    /// once. Nothing while a reclaim runs.
    pub(crate) fn refresh(&mut self, online: bool) -> Vec<Request> {
        if self.reclaiming() {
            return Vec::new();
        }
        let mut asked = Vec::new();
        if online {
            self.req += 1;
            self.waiting = Some(self.req);
            self.status = Some(StorageStatus::Loading);
            asked.push(Request::Storage { req: self.req });
        } else {
            self.waiting = None;
            self.status = Some(StorageStatus::Unavailable(StorageUnavailable::Offline));
        }
        asked.push(self.ask_device());
        asked
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

    /// This device's answer to device request `req`; a stale one is ignored.
    /// A failed read is logged and keeps the report before it.
    pub(crate) fn device_read(&mut self, req: u64, result: Result<DeviceStorage, StoreError>) {
        if self.device_waiting != Some(req) {
            return;
        }
        self.device_waiting = None;
        match result {
            Ok(device) => self.device = Some(device),
            Err(error) => tracing::debug!(%error, "device storage report failed"),
        }
    }

    /// What the screen shows: its status, the latest server report and this
    /// device's.
    pub(crate) fn shown(&self) -> (StorageStatus, Option<StorageReport>, Option<DeviceStorage>) {
        (
            self.status.clone().unwrap_or(StorageStatus::Loading),
            self.report.clone(),
            self.device.clone(),
        )
    }

    /// Asks this device for its report, which the engine reads off the state
    /// lock.
    fn ask_device(&mut self) -> Request {
        self.device_req += 1;
        self.device_waiting = Some(self.device_req);
        Request::DeviceStorage {
            req: self.device_req,
        }
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

    fn device(database: u64) -> DeviceStorage {
        DeviceStorage {
            database,
            reclaimable: 0,
            blobs: 0,
            attachments: 0,
            sessions: Vec::new(),
        }
    }

    /// The id of the server's storage request among `asked`.
    fn req(asked: Vec<Request>) -> u64 {
        asked
            .iter()
            .find_map(|request| match request {
                Request::Storage { req } => Some(*req),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no storage request in {asked:?}"))
    }

    /// The id of this device's storage request among `asked`.
    fn device_req(asked: Vec<Request>) -> u64 {
        asked
            .iter()
            .find_map(|request| match request {
                Request::DeviceStorage { req } => Some(*req),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no device storage request in {asked:?}"))
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
        assert_eq!(storage.shown(), (StorageStatus::Loading, None, None));
        storage.reported(first, Ok(report(1)));
        assert_eq!(
            storage.shown(),
            (StorageStatus::Ready, Some(report(1)), None)
        );
    }

    #[test]
    fn only_the_latest_request_is_answered() {
        let mut storage = Storage::default();
        let first = req(storage.open(true));
        let second = req(storage.refresh(true));
        storage.reported(first, Ok(report(1)));
        assert_eq!(storage.shown(), (StorageStatus::Loading, None, None));
        storage.reported(second, Ok(report(2)));
        assert_eq!(
            storage.shown(),
            (StorageStatus::Ready, Some(report(2)), None)
        );
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
        assert_eq!(
            storage.shown(),
            (StorageStatus::Loading, Some(report(1)), None)
        );
        storage.reported(second, Err(TransportError::Timeout));
        let (status, kept, _) = storage.shown();
        assert!(matches!(
            status,
            StorageStatus::Unavailable(StorageUnavailable::Failed(_))
        ));
        assert_eq!(kept, Some(report(1)));
        req(storage.open(true));
        assert_eq!(storage.shown(), (StorageStatus::Loading, None, None));
    }

    #[test]
    fn offline_no_server_report_is_asked() {
        let mut storage = Storage::default();
        let asked = storage.open(false);
        assert_eq!(asked, [Request::DeviceStorage { req: 1 }]);
        assert_eq!(
            storage.shown(),
            (
                StorageStatus::Unavailable(StorageUnavailable::Offline),
                None,
                None
            )
        );
    }

    #[test]
    fn the_device_report_is_shown_and_a_stale_one_ignored() {
        let mut storage = Storage::default();
        let first = device_req(storage.open(true));
        let second = device_req(storage.refresh(true));
        storage.device_read(first, Ok(device(1)));
        assert_eq!(storage.shown().2, None);
        storage.device_read(second, Ok(device(2)));
        assert_eq!(storage.shown().2, Some(device(2)));
    }

    #[test]
    fn a_failed_device_read_keeps_the_one_before() {
        let mut storage = Storage::default();
        let first = device_req(storage.open(true));
        storage.device_read(first, Ok(device(1)));
        let second = device_req(storage.refresh(true));
        storage.device_read(second, Err(StoreError::Corrupt("test")));
        assert_eq!(storage.shown().2, Some(device(1)));
    }

    #[test]
    fn reclaiming_shows_the_fresh_report() {
        let mut storage = Storage::default();
        let first = req(storage.open(true));
        storage.reported(first, Ok(report(1)));
        let reclaim = reclaim_req(storage.reclaim(true));
        assert_eq!(
            storage.shown(),
            (StorageStatus::Reclaiming, Some(report(1)), None)
        );
        // Nothing else is asked while it runs, the device report included.
        assert!(storage.refresh(true).is_empty());
        assert_eq!(storage.reclaim(true), None);
        assert_eq!(storage.shown().0, StorageStatus::Reclaiming);
        storage.reclaimed(reclaim, Ok(report(2)));
        assert_eq!(
            storage.shown(),
            (StorageStatus::Ready, Some(report(2)), None)
        );
    }

    #[test]
    fn opening_the_screen_forgets_a_reclaim_in_flight() {
        let mut storage = Storage::default();
        let first = req(storage.open(true));
        let reclaim = reclaim_req(storage.reclaim(true));
        let reopened = req(storage.open(true));
        assert_ne!(reopened, first);
        assert_eq!(storage.shown(), (StorageStatus::Loading, None, None));
        // The reclaim's answer is no longer waited on.
        storage.reclaimed(reclaim, Ok(report(1)));
        assert_eq!(storage.shown(), (StorageStatus::Loading, None, None));
        storage.reported(reopened, Ok(report(2)));
        assert_eq!(
            storage.shown(),
            (StorageStatus::Ready, Some(report(2)), None)
        );
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
                Some(report(1)),
                None
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
                Some(report(1)),
                None
            )
        );
        reclaim_req(storage.reclaim(true));
    }
}
