//! The usage limits page: the limits of the account the server's agent runs
//! as, read when the user asks.
//!
//! The replica keeps the latest answer ([`Store::put_usage_limits`]), so the
//! page reads offline with the time the agent reported it. Opening the page
//! asks only when nothing is stored; a refresh is the user's to ask for.
//! Only the latest request's answer is taken; an older one is dropped. A
//! transport error or a `Failed` answer stays in memory and never replaces
//! the stored answer. Offline nothing is asked and the page says so at once.
//!
//! [`Store::put_usage_limits`]: crate::Store::put_usage_limits

use skywave_proto::UsageLimitsResult;

use crate::seam::Request;
use crate::transport::TransportError;
use crate::view::{UsageLimitsStatus, UsageLimitsUnavailable, UsageLimitsView};

#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct UsageLimits {
    /// Id of the latest [`Request::UsageLimits`]; never reused.
    req: u64,
    /// The request the page waits on; `None` when nothing is in flight.
    waiting: Option<u64>,
    /// Why the latest read gave no answer; cleared by the next ask.
    failure: Option<UsageLimitsUnavailable>,
    /// The latest stored answer, as stored.
    pub(crate) stored: Option<UsageLimitsResult>,
}

impl UsageLimits {
    /// The page opened: asks once when no answer is stored and none is on
    /// its way, else shows the stored one.
    pub(crate) fn open(&mut self, online: bool) -> Option<Request> {
        if self.stored.is_some() || self.waiting.is_some() {
            return None;
        }
        self.refresh(online)
    }

    /// Asks the server again, keeping the stored answer shown until the new
    /// one. Offline it is unavailable at once.
    pub(crate) fn refresh(&mut self, online: bool) -> Option<Request> {
        if !online {
            self.waiting = None;
            self.failure = Some(UsageLimitsUnavailable::Offline);
            return None;
        }
        self.req += 1;
        self.waiting = Some(self.req);
        self.failure = None;
        Some(Request::UsageLimits { req: self.req })
    }

    /// Whether `req` is the request the page waits on: only its answer is
    /// taken.
    pub(crate) fn awaited(&self, req: u64) -> bool {
        self.waiting == Some(req)
    }

    /// The read ended without an answer to keep.
    pub(crate) fn failed(&mut self, why: UsageLimitsUnavailable) {
        self.waiting = None;
        self.failure = Some(why);
    }

    /// The read's answer, stored.
    pub(crate) fn answered(&mut self, answer: UsageLimitsResult) {
        self.waiting = None;
        self.failure = None;
        self.stored = Some(answer);
    }

    pub(crate) fn view(&self) -> UsageLimitsView {
        let status = match (&self.waiting, &self.failure) {
            (Some(_), _) => UsageLimitsStatus::Loading,
            (None, Some(why)) => UsageLimitsStatus::Unavailable(why.clone()),
            (None, None) => UsageLimitsStatus::Idle,
        };
        UsageLimitsView {
            status,
            answer: self.stored.clone(),
        }
    }
}

impl From<TransportError> for UsageLimitsUnavailable {
    fn from(error: TransportError) -> Self {
        match error {
            // The engine's answer while the link is down, or a server that
            // could not be reached.
            TransportError::Unreachable(_) => UsageLimitsUnavailable::Offline,
            error => {
                tracing::debug!(%error, "usage limits read failed");
                UsageLimitsUnavailable::Failed(error.to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use skywave_proto::{LimitWindow, UsageLimit};

    use super::*;

    fn read(percent: u32) -> UsageLimitsResult {
        let at: DateTime<Utc> = "2026-11-08T12:00:00Z".parse().unwrap();
        UsageLimitsResult::Read {
            at,
            limits: vec![UsageLimit {
                window: LimitWindow::Session,
                scope: None,
                percent: Some(percent),
                severity: None,
                resets_at: None,
            }],
        }
    }

    fn req(request: Option<Request>) -> u64 {
        match request {
            Some(Request::UsageLimits { req }) => req,
            other => panic!("expected a usage limits request, got {other:?}"),
        }
    }

    #[test]
    fn opening_asks_only_while_nothing_is_stored() {
        let mut limits = UsageLimits::default();
        let first = req(limits.open(true));
        assert_eq!(limits.view().status, UsageLimitsStatus::Loading);
        assert_eq!(limits.open(true), None, "one read at a time");
        assert!(limits.awaited(first));
        limits.answered(read(10));
        assert_eq!(limits.open(true), None, "stored: shown, not asked");
        let view = limits.view();
        assert_eq!(view.status, UsageLimitsStatus::Idle);
        assert_eq!(view.answer, Some(read(10)));
    }

    #[test]
    fn only_the_latest_request_is_awaited() {
        let mut limits = UsageLimits::default();
        let first = req(limits.refresh(true));
        let second = req(limits.refresh(true));
        assert!(!limits.awaited(first));
        assert!(limits.awaited(second));
        limits.answered(read(20));
        assert!(!limits.awaited(second), "answered");
    }

    #[test]
    fn a_failed_refresh_keeps_the_stored_answer() {
        let mut limits = UsageLimits {
            stored: Some(read(30)),
            ..UsageLimits::default()
        };
        req(limits.refresh(true));
        assert_eq!(limits.view().answer, Some(read(30)));
        limits.failed(TransportError::Timeout.into());
        let view = limits.view();
        assert!(matches!(
            view.status,
            UsageLimitsStatus::Unavailable(UsageLimitsUnavailable::Failed(_))
        ));
        assert_eq!(view.answer, Some(read(30)));
        req(limits.refresh(true));
        assert_eq!(limits.view().status, UsageLimitsStatus::Loading);
    }

    #[test]
    fn offline_nothing_is_asked() {
        let mut limits = UsageLimits::default();
        assert_eq!(limits.open(false), None);
        assert_eq!(
            limits.view(),
            UsageLimitsView {
                status: UsageLimitsStatus::Unavailable(UsageLimitsUnavailable::Offline),
                answer: None,
            }
        );
        assert_eq!(
            UsageLimitsUnavailable::from(TransportError::Unreachable("down".into())),
            UsageLimitsUnavailable::Offline
        );
    }
}
