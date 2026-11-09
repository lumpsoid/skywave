//! Logging the server's agent in from this device, on the settings screen.
//!
//! [`Action::StartAgentLogin`] asks the server to start a login
//! ([`Request::LoginStart`]); its `Open` answer names the flow and the URL
//! the user logs in at. [`Action::FinishAgentLogin`] sends the code that
//! page shows, naming the flow ([`Request::LoginFinish`]); with no open
//! flow nothing is sent. Only the latest request's answer is taken; an
//! older one is dropped.
//!
//! The flow, its URL and the code live here, in model memory only: never
//! in the store, a draft or the outbox, and the code never in a `Debug`
//! ([`LoginCode`], [`LoginFinish`]). A restart, [`Action::DismissAgentLogin`]
//! or pairing with another server forgets them.
//!
//! A finish that gets no answer (offline, or a retryable transport error:
//! the server's `503`, the engine's timeout, a broken connection) keeps
//! the flow and the code, and the view says so
//! ([`LoginFlowView::NoAnswer`]); [`Action::RetryAgentLogin`] sends the
//! very same flow and code again, and the server answers a repeated finish
//! with the first one's outcome. A finish refused for good (a `4xx`, an
//! unreadable answer) ends the login: resending the same body could only
//! be refused again.
//!
//! A `Failed` answer to a finish ends the login on this device too. The
//! server answers `Failed` both for a code shaped wrong, after which its
//! flow stays open, and for a code the agent refused or an attempt that
//! stopped, after which the flow is done and every later finish of it
//! gets the same `Failed` without asking the agent. The wire does not
//! tell them apart, so keeping the flow open here would loop a refused
//! user on the old refusal. The way on is [`Action::StartAgentLogin`]:
//! the server joins a flow still open (the same flow and URL) and opens a
//! new one after a terminal failure. Only a code this device does not
//! send (blank, or over [`LOGIN_CODE_MAX_BYTES`]) keeps the flow `Open`,
//! with the reason ([`LoginCodeRefused`]).
//!
//! A start that gets no answer simply failed: a new start joins whatever
//! the server opened meanwhile.
//!
//! [`Action::StartAgentLogin`]: crate::Action::StartAgentLogin
//! [`Action::FinishAgentLogin`]: crate::Action::FinishAgentLogin
//! [`Action::RetryAgentLogin`]: crate::Action::RetryAgentLogin
//! [`Action::DismissAgentLogin`]: crate::Action::DismissAgentLogin

use std::fmt;

use skywave_proto::limits::LOGIN_CODE_MAX_BYTES;
use skywave_proto::{LoginFinish, LoginFinishResult, LoginFlowId, LoginStartResult};

use crate::seam::Request;
use crate::transport::TransportError;
use crate::view::{LoginCodeRefused, LoginFlowView, LoginNoAnswer, LoginOutcome};

/// A code pasted from the login page. A secret: `Debug` prints
/// `<redacted>`, so an action logged whole does not leak it.
#[derive(Clone, PartialEq, Eq)]
pub struct LoginCode(String);

impl LoginCode {
    pub fn new(code: impl Into<String>) -> Self {
        Self(code.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for LoginCode {
    fn from(code: String) -> Self {
        Self(code)
    }
}

impl fmt::Debug for LoginCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LoginCode(<redacted>)")
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct AgentLogin {
    /// Id of the latest login request; never reused, so an answer to an
    /// older one (or one sent before a dismissal or a server switch) is
    /// dropped.
    req: u64,
    flow: Flow,
}

/// Where the login stands, with what each step needs to go on.
#[derive(Debug, Clone, PartialEq, Default)]
enum Flow {
    #[default]
    Idle,
    Starting {
        req: u64,
    },
    Open {
        flow: LoginFlowId,
        url: String,
        refused: Option<LoginCodeRefused>,
    },
    Finishing {
        req: u64,
        url: String,
        finish: LoginFinish,
    },
    NoAnswer {
        url: String,
        finish: LoginFinish,
        why: LoginNoAnswer,
    },
    Done(LoginOutcome),
}

impl AgentLogin {
    /// Starts a login, whatever stands now: a later request wins.
    /// Offline it ends at once, asking nothing.
    pub(crate) fn start(&mut self, online: bool) -> Option<Request> {
        if !online {
            self.flow = Flow::Done(LoginOutcome::Offline);
            return None;
        }
        self.req += 1;
        self.flow = Flow::Starting { req: self.req };
        Some(Request::LoginStart { req: self.req })
    }

    /// Sends `code`, trimmed, for the open flow; nothing without one. A
    /// blank code, or one longer than the server takes, is not sent: the
    /// flow stays open saying why.
    pub(crate) fn finish(&mut self, code: LoginCode, online: bool) -> Option<Request> {
        let Flow::Open { flow, url, refused } = &mut self.flow else {
            return None;
        };
        let trimmed = code.as_str().trim();
        if trimmed.is_empty() {
            *refused = Some(LoginCodeRefused::Blank);
            return None;
        }
        if trimmed.len() > LOGIN_CODE_MAX_BYTES {
            *refused = Some(LoginCodeRefused::TooLong {
                max_bytes: LOGIN_CODE_MAX_BYTES,
            });
            return None;
        }
        let finish = LoginFinish {
            flow: *flow,
            code: trimmed.to_owned(),
        };
        let url = url.clone();
        self.send(url, finish, online)
    }

    /// Sends the flow and code that got no answer again, unchanged.
    pub(crate) fn retry(&mut self, online: bool) -> Option<Request> {
        let Flow::NoAnswer { url, finish, .. } = &self.flow else {
            return None;
        };
        let (url, finish) = (url.clone(), finish.clone());
        self.send(url, finish, online)
    }

    fn send(&mut self, url: String, finish: LoginFinish, online: bool) -> Option<Request> {
        if !online {
            self.flow = Flow::NoAnswer {
                url,
                finish,
                why: LoginNoAnswer::Offline,
            };
            return None;
        }
        self.req += 1;
        self.flow = Flow::Finishing {
            req: self.req,
            url,
            finish: finish.clone(),
        };
        Some(Request::LoginFinish {
            req: self.req,
            finish,
        })
    }

    /// Forgets the login, sending nothing: an answer still on its way is
    /// dropped.
    pub(crate) fn forget(&mut self) {
        self.flow = Flow::Idle;
    }

    /// Whether `req` is the request the login waits on: only its answer is
    /// taken.
    pub(crate) fn awaited(&self, req: u64) -> bool {
        match self.flow {
            Flow::Starting { req: waiting } | Flow::Finishing { req: waiting, .. } => {
                waiting == req
            }
            _ => false,
        }
    }

    /// The answer to the awaited start.
    pub(crate) fn started(&mut self, result: Result<LoginStartResult, TransportError>) {
        self.flow = match result {
            Ok(LoginStartResult::Open { flow, url }) => Flow::Open {
                flow,
                url,
                refused: None,
            },
            Ok(LoginStartResult::Busy) => Flow::Done(LoginOutcome::Busy),
            Ok(LoginStartResult::Unavailable) => Flow::Done(LoginOutcome::Unavailable),
            Ok(LoginStartResult::Failed { reason }) => Flow::Done(LoginOutcome::Failed(reason)),
            Err(TransportError::Unreachable(_)) => Flow::Done(LoginOutcome::Offline),
            Err(error) => {
                tracing::debug!(%error, "login start failed");
                Flow::Done(LoginOutcome::Failed(error.to_string()))
            }
        };
    }

    /// The answer to the awaited finish.
    pub(crate) fn finished(&mut self, result: Result<LoginFinishResult, TransportError>) {
        let Flow::Finishing { url, finish, .. } = &self.flow else {
            return;
        };
        let (url, finish) = (url.clone(), finish.clone());
        self.flow = match result {
            Ok(LoginFinishResult::LoggedIn { .. }) => Flow::Done(LoginOutcome::LoggedIn),
            Ok(LoginFinishResult::NoFlow) => Flow::Done(LoginOutcome::NoFlow),
            // Terminal here, whether or not the server's flow is: see the
            // module docs.
            Ok(LoginFinishResult::Failed { reason }) => Flow::Done(LoginOutcome::Failed(reason)),
            Err(TransportError::Unreachable(_)) => Flow::NoAnswer {
                url,
                finish,
                why: LoginNoAnswer::Offline,
            },
            Err(error) if error.is_retryable() => {
                tracing::debug!(%error, "login finish got no answer");
                Flow::NoAnswer {
                    url,
                    finish,
                    why: LoginNoAnswer::Failed(error.to_string()),
                }
            }
            // Refused for good (a 4xx, an unreadable answer): resending
            // the same body cannot help.
            Err(error) => {
                tracing::debug!(%error, "login finish refused");
                Flow::Done(LoginOutcome::Failed(error.to_string()))
            }
        };
    }

    pub(crate) fn view(&self) -> LoginFlowView {
        match &self.flow {
            Flow::Idle => LoginFlowView::Idle,
            Flow::Starting { .. } => LoginFlowView::Starting,
            Flow::Open { url, refused, .. } => LoginFlowView::Open {
                url: url.clone(),
                refused: refused.clone(),
            },
            Flow::Finishing { url, .. } => LoginFlowView::Finishing { url: url.clone() },
            Flow::NoAnswer { url, why, .. } => LoginFlowView::NoAnswer {
                url: url.clone(),
                why: why.clone(),
            },
            Flow::Done(outcome) => LoginFlowView::Done(outcome.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://login.example/authorize?state=abc";

    fn start_req(request: Option<Request>) -> u64 {
        match request {
            Some(Request::LoginStart { req }) => req,
            other => panic!("expected a login start, got {other:?}"),
        }
    }

    fn finish_req(request: Option<Request>) -> (u64, LoginFinish) {
        match request {
            Some(Request::LoginFinish { req, finish }) => (req, finish),
            other => panic!("expected a login finish, got {other:?}"),
        }
    }

    /// A login opened on `flow`.
    fn opened(flow: LoginFlowId) -> AgentLogin {
        let mut login = AgentLogin::default();
        let req = start_req(login.start(true));
        assert!(login.awaited(req));
        login.started(Ok(LoginStartResult::Open {
            flow,
            url: URL.into(),
        }));
        login
    }

    #[test]
    fn only_the_latest_request_is_awaited() {
        let mut login = AgentLogin::default();
        let first = start_req(login.start(true));
        let second = start_req(login.start(true));
        assert!(!login.awaited(first), "superseded");
        assert!(login.awaited(second));
        assert_eq!(login.view(), LoginFlowView::Starting);
        let flow = LoginFlowId::random();
        login.started(Ok(LoginStartResult::Open {
            flow,
            url: URL.into(),
        }));
        assert!(!login.awaited(second), "answered");
        let (finish, _) = finish_req(login.finish(LoginCode::new("a#b"), true));
        assert!(!login.awaited(second));
        assert!(login.awaited(finish));
        login.forget();
        assert!(!login.awaited(finish), "dismissed");
    }

    #[test]
    fn a_finish_needs_an_open_flow_and_a_code() {
        let mut login = AgentLogin::default();
        assert_eq!(login.finish(LoginCode::new("a#b"), true), None, "idle");
        start_req(login.start(true));
        assert_eq!(login.finish(LoginCode::new("a#b"), true), None, "starting");
        let mut login = opened(LoginFlowId::random());
        assert_eq!(login.finish(LoginCode::new(" \n"), true), None, "blank");
        assert_eq!(login.retry(true), None, "nothing to retry");
        assert!(matches!(login.view(), LoginFlowView::Open { .. }));
    }

    #[test]
    fn a_finish_without_an_answer_is_retried_with_the_same_flow_and_code() {
        let flow = LoginFlowId::random();
        for error in [
            TransportError::Timeout,
            TransportError::BadStatus(503),
            TransportError::Io("reset".into()),
        ] {
            let mut login = opened(flow);
            let (req, sent) = finish_req(login.finish(LoginCode::new("code#state"), true));
            assert_eq!(sent.flow, flow);
            assert_eq!(sent.code, "code#state");
            login.finished(Err(error.clone()));
            assert!(!login.awaited(req));
            assert_eq!(
                login.view(),
                LoginFlowView::NoAnswer {
                    url: URL.into(),
                    why: LoginNoAnswer::Failed(error.to_string()),
                },
            );
            // A finish now is refused: the retry is what resends.
            assert_eq!(login.finish(LoginCode::new("other#code"), true), None);
            let (retried, resent) = finish_req(login.retry(true));
            assert_ne!(retried, req);
            assert_eq!(resent, sent, "the same flow and code");
            assert_eq!(login.view(), LoginFlowView::Finishing { url: URL.into() });
            login.finished(Ok(LoginFinishResult::LoggedIn { expires: None }));
            assert_eq!(login.view(), LoginFlowView::Done(LoginOutcome::LoggedIn));
        }
    }

    #[test]
    fn offline_nothing_is_sent_and_the_code_waits() {
        let mut login = AgentLogin::default();
        assert_eq!(login.start(false), None);
        assert_eq!(login.view(), LoginFlowView::Done(LoginOutcome::Offline));
        let flow = LoginFlowId::random();
        let mut login = opened(flow);
        assert_eq!(login.finish(LoginCode::new("code#state"), false), None);
        assert_eq!(
            login.view(),
            LoginFlowView::NoAnswer {
                url: URL.into(),
                why: LoginNoAnswer::Offline,
            }
        );
        assert_eq!(login.retry(false), None);
        let (_, sent) = finish_req(login.retry(true));
        assert_eq!(sent.flow, flow);
        assert_eq!(sent.code, "code#state");
    }

    #[test]
    fn a_code_the_server_refuses_ends_the_login_until_started_again() {
        let mut login = opened(LoginFlowId::random());
        finish_req(login.finish(LoginCode::new("nohash"), true));
        login.finished(Ok(LoginFinishResult::Failed {
            reason: "not a code".into(),
        }));
        assert_eq!(
            login.view(),
            LoginFlowView::Done(LoginOutcome::Failed("not a code".into()))
        );
        assert_eq!(login.finish(LoginCode::new("code#state"), true), None);
        assert_eq!(login.retry(true), None, "no retry offered");
        let req = start_req(login.start(true));
        assert!(login.awaited(req), "started again");
    }

    #[test]
    fn a_retry_that_collects_a_refusal_ends_the_login() {
        let mut login = opened(LoginFlowId::random());
        finish_req(login.finish(LoginCode::new("code#state"), true));
        login.finished(Err(TransportError::BadStatus(503)));
        finish_req(login.retry(true));
        login.finished(Ok(LoginFinishResult::Failed {
            reason: "invalid code".into(),
        }));
        assert_eq!(
            login.view(),
            LoginFlowView::Done(LoginOutcome::Failed("invalid code".into()))
        );
        assert_eq!(login.retry(true), None);
    }

    #[test]
    fn a_finish_refused_for_good_is_not_retried() {
        for error in [
            TransportError::BadStatus(400),
            TransportError::BadStatus(413),
            TransportError::NotFound,
            TransportError::Revoked,
            TransportError::Decode("bad json".into()),
        ] {
            let mut login = opened(LoginFlowId::random());
            finish_req(login.finish(LoginCode::new("code#state"), true));
            login.finished(Err(error.clone()));
            assert_eq!(
                login.view(),
                LoginFlowView::Done(LoginOutcome::Failed(error.to_string())),
                "{error:?}"
            );
            assert_eq!(login.retry(true), None, "{error:?}: no retry offered");
        }
    }

    #[test]
    fn a_code_this_device_does_not_send_keeps_the_flow_open() {
        let flow = LoginFlowId::random();
        let mut login = opened(flow);
        let at_cap = "c".repeat(LOGIN_CODE_MAX_BYTES - 2) + "#s";
        let over = format!("{at_cap}x");
        assert_eq!(login.finish(LoginCode::new(over), true), None);
        assert_eq!(
            login.view(),
            LoginFlowView::Open {
                url: URL.into(),
                refused: Some(LoginCodeRefused::TooLong {
                    max_bytes: LOGIN_CODE_MAX_BYTES
                }),
            }
        );
        assert_eq!(login.finish(LoginCode::new(" \n"), true), None);
        assert_eq!(
            login.view(),
            LoginFlowView::Open {
                url: URL.into(),
                refused: Some(LoginCodeRefused::Blank),
            }
        );
        // Measured trimmed, and sent trimmed: the server caps what it gets.
        let (_, sent) = finish_req(login.finish(LoginCode::new(format!("  {at_cap}\n")), true));
        assert_eq!(sent.flow, flow);
        assert_eq!(sent.code, at_cap);
    }

    #[test]
    fn start_answers_and_failures_end_the_login() {
        let cases = [
            (Ok(LoginStartResult::Busy), LoginOutcome::Busy),
            (Ok(LoginStartResult::Unavailable), LoginOutcome::Unavailable),
            (
                Ok(LoginStartResult::Failed {
                    reason: "no claude".into(),
                }),
                LoginOutcome::Failed("no claude".into()),
            ),
            (
                Err(TransportError::Unreachable("down".into())),
                LoginOutcome::Offline,
            ),
            (
                Err(TransportError::BadStatus(503)),
                LoginOutcome::Failed(TransportError::BadStatus(503).to_string()),
            ),
        ];
        for (answer, outcome) in cases {
            let mut login = AgentLogin::default();
            start_req(login.start(true));
            login.started(answer);
            assert_eq!(login.view(), LoginFlowView::Done(outcome));
            start_req(login.start(true));
            assert_eq!(login.view(), LoginFlowView::Starting, "started again");
        }
    }

    #[test]
    fn the_code_never_shows_in_debug() {
        let mut login = opened(LoginFlowId::random());
        let code = LoginCode::new("secret-code#state");
        assert!(!format!("{code:?}").contains("secret"));
        let request = login.finish(code, true);
        assert!(!format!("{request:?}").contains("secret"));
        assert!(!format!("{login:?}").contains("secret"));
        // Positive control: the URL, which is no secret, does show.
        assert!(format!("{login:?}").contains("login.example"));
    }
}
