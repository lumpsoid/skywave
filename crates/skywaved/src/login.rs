//! Logging the agent in from a device (architecture.md, "The agent's
//! login"; D49): the one login flow `POST /v1/agent-login/start` and
//! `/finish` drive, kept in memory.
//!
//! [`LoginFlows`] holds one slot. A start in an idle slot (or one whose
//! last flow is done) mints a [`LoginFlowId`] and asks the [`AgentLogin`]
//! for an attempt; a start while that runs waits for the same answer, and
//! one while the flow is open answers the same flow and URL, so racing
//! starts run one login. A start while a finish runs is `Busy`.
//!
//! A finish names its flow; any other is `NoFlow`. The first finish of an
//! open flow checks the code's shape (a refusal leaves the flow open), then
//! hands it to the attempt on a task of its own and waits for the outcome.
//! Every later finish of that flow gets the first one's outcome, whatever
//! code it carries: a device retrying after a lost reply never sends the
//! agent a second code. A success tells the [`LoginState`].
//!
//! An open flow at least [`LOGIN_FLOW_MAX`] old is dropped (its attempt
//! with it, which stops the agent's process) the next time anything looks
//! at the slot: a start, a finish or a `hello`. There is no timer.
//!
//! The slot's lock is never held across an `.await`: `hello` looks at it
//! on a blocking thread, and a lock held while the agent answers would
//! stall every reconnect. Starting and finishing run on spawned tasks, so a
//! request that gives up never leaves the slot starting or finishing. The
//! URL and the code are never logged.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use skywave_proto::{LoginFinishResult, LoginFlowId, LoginStartResult, limits};
use tokio::sync::watch;

use crate::agent::{AgentLogin, LoginAttempt, LoginError, LoginState};
use crate::lock;
use crate::server::JoinedHook;

/// How long an open flow waits for its code before the next look at the
/// slot drops it (the default of `ServerOptions::login_flow_max`).
pub const LOGIN_FLOW_MAX: Duration = Duration::from_secs(15 * 60);

/// The failure of a start or finish whose task stopped without an answer.
pub const LOGIN_STOPPED: &str = "the agent's login stopped";

/// The failure of a start whose URL is longer than the wire carries.
pub const URL_TOO_LONG: &str = "the agent's sign-in URL is too long";

/// The device login flow: one slot, shared by every request.
#[derive(Clone)]
pub struct LoginFlows(Arc<Flows>);

struct Flows {
    login: Arc<dyn AgentLogin>,
    state: LoginState,
    flow_max: Duration,
    /// Run when a start waits for another start's answer (tests).
    joined_hook: Option<JoinedHook>,
    slot: Mutex<Slot>,
}

/// What a finished flow came to: logged in, or the agent's refusal.
type Outcome = std::result::Result<(), String>;

enum Slot {
    Idle,
    /// The agent is being asked for an attempt; `ready` gets the answer.
    Starting {
        flow: LoginFlowId,
        ready: watch::Receiver<Option<LoginStartResult>>,
    },
    /// The user signs in at `url` and pastes the code back.
    Open {
        flow: LoginFlowId,
        url: String,
        attempt: Box<dyn LoginAttempt>,
        opened_at: Instant,
    },
    /// The first finish's code is with the agent; `outcome` gets its answer.
    Finishing {
        flow: LoginFlowId,
        outcome: watch::Receiver<Option<Outcome>>,
    },
    /// The flow is over; later finishes of it get `outcome`.
    Done {
        flow: LoginFlowId,
        outcome: Outcome,
    },
}

/// What a start does once it has looked at the slot.
enum StartPlan {
    Answer(LoginStartResult),
    Wait {
        ready: watch::Receiver<Option<LoginStartResult>>,
        joined: bool,
        /// The begin to spawn, once the lock is released.
        begin: Option<Starting>,
    },
}

/// What a finish does once it has looked at the slot.
enum FinishPlan {
    Answer(LoginFinishResult),
    Outcome(Outcome),
    Wait {
        outcome: watch::Receiver<Option<Outcome>>,
        /// The finish to spawn, once the lock is released.
        run: Option<(Finishing, BoxFuture<'static, Outcome>)>,
    },
}

impl LoginFlows {
    /// Flows that log in through `login` and tell `state` of a success; an
    /// open flow lives `flow_max`.
    pub fn new(
        login: Arc<dyn AgentLogin>,
        state: LoginState,
        flow_max: Duration,
        joined_hook: Option<JoinedHook>,
    ) -> LoginFlows {
        LoginFlows(Arc::new(Flows {
            login,
            state,
            flow_max,
            joined_hook,
            slot: Mutex::new(Slot::Idle),
        }))
    }

    /// Drops an abandoned open flow; what a `hello` does.
    pub fn touch(&self) {
        self.0.touch(&mut lock(&self.0.slot));
    }

    /// Starts a login, or joins the one starting or open. The agent gets
    /// until `deadline` to answer with a URL. A failure's reason is uncut.
    pub async fn start(&self, deadline: Instant) -> LoginStartResult {
        let plan = {
            let mut slot = lock(&self.0.slot);
            self.0.touch(&mut slot);
            match &*slot {
                Slot::Idle | Slot::Done { .. } => {
                    let flow = LoginFlowId::random();
                    let (publish, ready) = watch::channel(None);
                    *slot = Slot::Starting {
                        flow,
                        ready: ready.clone(),
                    };
                    StartPlan::Wait {
                        ready,
                        joined: false,
                        begin: Some(Starting {
                            flows: self.clone(),
                            flow,
                            publish: Some(publish),
                        }),
                    }
                }
                Slot::Starting { ready, .. } => StartPlan::Wait {
                    ready: ready.clone(),
                    joined: true,
                    begin: None,
                },
                Slot::Open { flow, url, .. } => StartPlan::Answer(LoginStartResult::Open {
                    flow: *flow,
                    url: url.clone(),
                }),
                Slot::Finishing { .. } => StartPlan::Answer(LoginStartResult::Busy),
            }
        };
        let (mut ready, joined) = match plan {
            StartPlan::Answer(answer) => return answer,
            StartPlan::Wait {
                ready,
                joined,
                begin,
            } => {
                // Outside the lock: a runtime shutting down drops the task
                // here, on this thread, and its drop settles the slot,
                // which takes the lock.
                if let Some(begin) = begin {
                    tokio::spawn(begin.begin(deadline));
                }
                (ready, joined)
            }
        };
        if joined && let Some(hook) = &self.0.joined_hook {
            hook();
        }
        let answer = ready
            .wait_for(Option::is_some)
            .await
            .ok()
            .and_then(|answer| answer.clone());
        answer.unwrap_or_else(|| LoginStartResult::Failed {
            reason: LOGIN_STOPPED.to_owned(),
        })
    }

    /// Finishes flow `flow` with the pasted `code`, or answers what its
    /// first finish came to. A failure's reason is uncut.
    pub async fn finish(&self, flow: LoginFlowId, code: String) -> LoginFinishResult {
        let plan = {
            let mut slot = lock(&self.0.slot);
            self.0.touch(&mut slot);
            match std::mem::replace(&mut *slot, Slot::Idle) {
                Slot::Open {
                    flow: open,
                    url,
                    attempt,
                    opened_at,
                } if open == flow => match attempt.check(&code) {
                    Err(reason) => {
                        *slot = Slot::Open {
                            flow: open,
                            url,
                            attempt,
                            opened_at,
                        };
                        FinishPlan::Answer(LoginFinishResult::Failed { reason })
                    }
                    Ok(()) => {
                        let (publish, outcome) = watch::channel(None);
                        *slot = Slot::Finishing {
                            flow,
                            outcome: outcome.clone(),
                        };
                        let finishing = Finishing {
                            flows: self.clone(),
                            flow,
                            publish: Some(publish),
                        };
                        FinishPlan::Wait {
                            outcome,
                            run: Some((finishing, attempt.finish(code))),
                        }
                    }
                },
                Slot::Finishing {
                    flow: finishing,
                    outcome,
                } if finishing == flow => {
                    *slot = Slot::Finishing {
                        flow,
                        outcome: outcome.clone(),
                    };
                    FinishPlan::Wait { outcome, run: None }
                }
                Slot::Done {
                    flow: done,
                    outcome,
                } if done == flow => {
                    *slot = Slot::Done {
                        flow,
                        outcome: outcome.clone(),
                    };
                    FinishPlan::Outcome(outcome)
                }
                other => {
                    *slot = other;
                    FinishPlan::Answer(LoginFinishResult::NoFlow)
                }
            }
        };
        let outcome = match plan {
            FinishPlan::Answer(answer) => return answer,
            FinishPlan::Outcome(outcome) => outcome,
            FinishPlan::Wait { mut outcome, run } => {
                // Outside the lock, as a start's begin.
                if let Some((finishing, finish)) = run {
                    tokio::spawn(finishing.run(finish));
                }
                let outcome = outcome
                    .wait_for(Option::is_some)
                    .await
                    .ok()
                    .and_then(|outcome| outcome.clone());
                outcome.unwrap_or_else(|| Err(LOGIN_STOPPED.to_owned()))
            }
        };
        match outcome {
            // Read after the agent answered: the new login's.
            Ok(()) => LoginFinishResult::LoggedIn {
                expires: self.0.login.expires(),
            },
            Err(reason) => LoginFinishResult::Failed { reason },
        }
    }
}

impl Flows {
    /// Drops the open flow if it is at least `flow_max` old.
    fn touch(&self, slot: &mut Slot) {
        if let Slot::Open { opened_at, .. } = slot
            && opened_at.elapsed() >= self.flow_max
        {
            *slot = Slot::Idle;
        }
    }
}

/// A start's request to the agent, on a task of its own. Dropped without
/// an answer (the task panicked), it answers [`LOGIN_STOPPED`].
struct Starting {
    flows: LoginFlows,
    flow: LoginFlowId,
    publish: Option<watch::Sender<Option<LoginStartResult>>>,
}

impl Starting {
    async fn begin(mut self, deadline: Instant) {
        let login = self.flows.0.login.clone();
        let begun = login.begin(deadline).await;
        self.settle(begun);
    }

    /// Opens the flow with the agent's attempt, or empties the slot, and
    /// answers every start waiting.
    fn settle(&mut self, begun: std::result::Result<Box<dyn LoginAttempt>, LoginError>) {
        let Some(publish) = self.publish.take() else {
            return;
        };
        let mut slot = lock(&self.flows.0.slot);
        let ours = matches!(&*slot, Slot::Starting { flow, .. } if *flow == self.flow);
        let answer = match begun {
            // Nothing else ends a starting flow; never so, but never Open
            // without the slot holding the attempt.
            Ok(_) if !ours => LoginStartResult::Failed {
                reason: LOGIN_STOPPED.to_owned(),
            },
            // Never cut: a cut URL signs in nowhere. Dropped, the attempt
            // stops.
            Ok(attempt) if attempt.url().len() > limits::LOGIN_URL_MAX_BYTES => {
                LoginStartResult::Failed {
                    reason: URL_TOO_LONG.to_owned(),
                }
            }
            Ok(attempt) => {
                let url = attempt.url().to_owned();
                *slot = Slot::Open {
                    flow: self.flow,
                    url: url.clone(),
                    attempt,
                    opened_at: Instant::now(),
                };
                LoginStartResult::Open {
                    flow: self.flow,
                    url,
                }
            }
            Err(LoginError::Unavailable) => LoginStartResult::Unavailable,
            Err(LoginError::Failed(reason)) => LoginStartResult::Failed { reason },
        };
        if ours && !matches!(answer, LoginStartResult::Open { .. }) {
            *slot = Slot::Idle;
        }
        publish.send_replace(Some(answer));
    }
}

impl Drop for Starting {
    fn drop(&mut self) {
        self.settle(Err(LoginError::Failed(LOGIN_STOPPED.to_owned())));
    }
}

/// The first finish's wait for the agent, on a task of its own. Dropped
/// without an outcome (the task panicked), the flow fails with
/// [`LOGIN_STOPPED`].
struct Finishing {
    flows: LoginFlows,
    flow: LoginFlowId,
    publish: Option<watch::Sender<Option<Outcome>>>,
}

impl Finishing {
    async fn run(mut self, finish: impl Future<Output = Outcome>) {
        let outcome = finish.await;
        self.settle(outcome);
    }

    /// Ends the flow with `outcome` and answers every finish waiting.
    fn settle(&mut self, outcome: Outcome) {
        let Some(publish) = self.publish.take() else {
            return;
        };
        let mut slot = lock(&self.flows.0.slot);
        if matches!(&*slot, Slot::Finishing { flow, .. } if *flow == self.flow) {
            *slot = Slot::Done {
                flow: self.flow,
                outcome: outcome.clone(),
            };
        }
        // Before anyone hears of it: a hello after the answer says logged
        // in.
        if outcome.is_ok() {
            self.flows.0.state.login_succeeded();
        }
        publish.send_replace(Some(outcome));
    }
}

impl Drop for Finishing {
    fn drop(&mut self) {
        self.settle(Err(LOGIN_STOPPED.to_owned()));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use chrono::{DateTime, Utc};
    use futures_util::FutureExt;

    use super::*;
    use crate::agent::LoginStatus;
    use crate::agent::fake::Gate;

    const URL: &str = "https://example.invalid/sign-in";
    const SOON: Duration = Duration::from_secs(10);

    /// An agent login whose begins and finishes wait for gates the test
    /// opens, and count themselves.
    struct Scripted {
        begin_gate: Gate,
        finish_gate: Gate,
        begins: AtomicUsize,
        finishes: Arc<AtomicUsize>,
        /// Attempts dropped, finished or not.
        dropped: Arc<AtomicUsize>,
        /// What the next begin answers; a URL by default.
        begin_answer: Mutex<Option<std::result::Result<String, LoginError>>>,
        finish_answer: Mutex<Outcome>,
    }

    impl Scripted {
        fn new() -> Arc<Scripted> {
            Arc::new(Scripted {
                begin_gate: Gate::new(),
                finish_gate: Gate::new(),
                begins: AtomicUsize::new(0),
                finishes: Arc::new(AtomicUsize::new(0)),
                dropped: Arc::new(AtomicUsize::new(0)),
                begin_answer: Mutex::new(None),
                finish_answer: Mutex::new(Ok(())),
            })
        }

        /// Begins and finishes answer at once.
        fn open() -> Arc<Scripted> {
            let scripted = Scripted::new();
            scripted.begin_gate.open();
            scripted.finish_gate.open();
            scripted
        }
    }

    impl AgentLogin for Scripted {
        fn expires(&self) -> Option<DateTime<Utc>> {
            DateTime::from_timestamp(1_800_000_000, 0)
        }

        fn offered(&self) -> bool {
            true
        }

        fn begin(
            &self,
            _deadline: Instant,
        ) -> BoxFuture<'_, std::result::Result<Box<dyn LoginAttempt>, LoginError>> {
            self.begins.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                self.begin_gate.opened().await;
                let url = lock(&self.begin_answer)
                    .take()
                    .unwrap_or_else(|| Ok(URL.to_owned()))?;
                Ok(Box::new(Attempt {
                    url,
                    gate: self.finish_gate.clone(),
                    finishes: self.finishes.clone(),
                    dropped: self.dropped.clone(),
                    answer: lock(&self.finish_answer).clone(),
                }) as Box<dyn LoginAttempt>)
            })
        }
    }

    struct Attempt {
        url: String,
        gate: Gate,
        finishes: Arc<AtomicUsize>,
        dropped: Arc<AtomicUsize>,
        answer: Outcome,
    }

    impl LoginAttempt for Attempt {
        fn url(&self) -> &str {
            &self.url
        }

        fn check(&self, code: &str) -> std::result::Result<(), String> {
            if code.contains('#') {
                Ok(())
            } else {
                Err("paste it whole".to_owned())
            }
        }

        fn finish(self: Box<Self>, _code: String) -> BoxFuture<'static, Outcome> {
            self.finishes.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                self.gate.opened().await;
                self.answer.clone()
            })
        }
    }

    impl Drop for Attempt {
        fn drop(&mut self) {
            self.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn flows(scripted: &Arc<Scripted>, state: &LoginState) -> LoginFlows {
        LoginFlows::new(scripted.clone(), state.clone(), LOGIN_FLOW_MAX, None)
    }

    fn deadline() -> Instant {
        Instant::now() + SOON
    }

    async fn open(flows: &LoginFlows) -> LoginFlowId {
        let started = tokio::time::timeout(SOON, flows.start(deadline()))
            .await
            .expect("the start never answered");
        match started {
            LoginStartResult::Open { flow, url } => {
                assert_eq!(url, URL);
                flow
            }
            other => panic!("not open: {other:?}"),
        }
    }

    fn logged_in() -> LoginFinishResult {
        LoginFinishResult::LoggedIn {
            expires: DateTime::from_timestamp(1_800_000_000, 0),
        }
    }

    #[tokio::test]
    async fn a_start_while_a_flow_is_open_joins_it() {
        let scripted = Scripted::open();
        let flows = flows(&scripted, &LoginState::new());
        let flow = open(&flows).await;
        assert_eq!(open(&flows).await, flow);
        assert_eq!(scripted.begins.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn racing_starts_wait_for_one_begin() {
        let scripted = Scripted::new();
        scripted.finish_gate.open();
        let joined = Gate::new();
        let hook: JoinedHook = Arc::new({
            let joined = joined.clone();
            move || joined.open()
        });
        let flows = LoginFlows::new(
            scripted.clone(),
            LoginState::new(),
            LOGIN_FLOW_MAX,
            Some(hook),
        );
        let first = tokio::spawn({
            let flows = flows.clone();
            async move { flows.start(deadline()).await }
        });
        let second = tokio::spawn({
            let flows = flows.clone();
            async move { flows.start(deadline()).await }
        });
        tokio::time::timeout(SOON, joined.opened()).await.unwrap();
        // The slot is free to look at while the agent is being asked.
        flows.touch();
        assert!(!first.is_finished() && !second.is_finished());
        scripted.begin_gate.open();
        let (first, second) = (first.await.unwrap(), second.await.unwrap());
        assert!(matches!(first, LoginStartResult::Open { .. }), "{first:?}");
        assert_eq!(first, second);
        assert_eq!(scripted.begins.load(Ordering::SeqCst), 1);
    }

    /// The request that started the agent gave up (its future dropped):
    /// the begin goes on, and the next start gets its flow.
    #[tokio::test]
    async fn a_start_given_up_on_does_not_wedge_the_slot() {
        let scripted = Scripted::new();
        scripted.finish_gate.open();
        let flows = flows(&scripted, &LoginState::new());
        let mut start = Box::pin(flows.start(deadline()));
        assert!((&mut start).now_or_never().is_none());
        drop(start);
        scripted.begin_gate.open();
        open(&flows).await;
        assert_eq!(scripted.begins.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_failed_or_unavailable_start_leaves_the_slot_idle() {
        let scripted = Scripted::open();
        let flows = flows(&scripted, &LoginState::new());
        *lock(&scripted.begin_answer) = Some(Err(LoginError::Failed("refused".into())));
        assert_eq!(
            flows.start(deadline()).await,
            LoginStartResult::Failed {
                reason: "refused".into()
            }
        );
        *lock(&scripted.begin_answer) = Some(Err(LoginError::Unavailable));
        assert_eq!(flows.start(deadline()).await, LoginStartResult::Unavailable);
        open(&flows).await;
        assert_eq!(scripted.begins.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_url_longer_than_the_wire_carries_fails_the_start() {
        let scripted = Scripted::open();
        let flows = flows(&scripted, &LoginState::new());
        let long = "x".repeat(limits::LOGIN_URL_MAX_BYTES + 1);
        *lock(&scripted.begin_answer) = Some(Ok(long));
        assert_eq!(
            flows.start(deadline()).await,
            LoginStartResult::Failed {
                reason: URL_TOO_LONG.into()
            }
        );
        assert_eq!(scripted.dropped.load(Ordering::SeqCst), 1);
        // Exactly the cap is carried.
        *lock(&scripted.begin_answer) = Some(Ok("x".repeat(limits::LOGIN_URL_MAX_BYTES)));
        assert!(matches!(
            flows.start(deadline()).await,
            LoginStartResult::Open { .. }
        ));
    }

    /// The first finish's outcome is every later finish's, whatever code
    /// it carries; the agent gets one code. A start while it runs is busy.
    #[tokio::test]
    async fn a_flow_is_finished_once() {
        let scripted = Scripted::new();
        scripted.begin_gate.open();
        let state = LoginState::new();
        let flows = flows(&scripted, &state);
        let flow = open(&flows).await;

        let mut first = Box::pin(flows.finish(flow, "good#state".into()));
        assert!((&mut first).now_or_never().is_none());
        // Given up on: the agent's answer is still collected.
        drop(first);
        assert_eq!(flows.start(deadline()).await, LoginStartResult::Busy);
        let second = tokio::spawn({
            let flows = flows.clone();
            async move { flows.finish(flow, "other#code".into()).await }
        });
        assert_eq!(state.successes(), 0);
        scripted.finish_gate.open();
        assert_eq!(second.await.unwrap(), logged_in());
        assert_eq!(state.status(), LoginStatus::LoggedIn);
        assert_eq!(state.successes(), 1);
        assert_eq!(flows.finish(flow, "nohash".into()).await, logged_in());
        assert_eq!(scripted.finishes.load(Ordering::SeqCst), 1);
        assert_eq!(state.successes(), 1);

        // A done flow does not keep the next from starting.
        let next = open(&flows).await;
        assert_ne!(next, flow);
        assert_eq!(
            flows.finish(flow, "good#state".into()).await,
            LoginFinishResult::NoFlow
        );
    }

    #[tokio::test]
    async fn a_refused_code_is_the_flows_outcome_and_not_a_login() {
        let scripted = Scripted::open();
        *lock(&scripted.finish_answer) = Err("bad code".into());
        let state = LoginState::new();
        let flows = flows(&scripted, &state);
        let flow = open(&flows).await;
        let failed = LoginFinishResult::Failed {
            reason: "bad code".into(),
        };
        assert_eq!(flows.finish(flow, "bad#state".into()).await, failed);
        assert_eq!(flows.finish(flow, "good#state".into()).await, failed);
        assert_eq!(scripted.finishes.load(Ordering::SeqCst), 1);
        assert_eq!(state.status(), LoginStatus::Unknown);
        assert_eq!(state.successes(), 0);
    }

    #[tokio::test]
    async fn a_misshapen_code_leaves_the_flow_open() {
        let scripted = Scripted::open();
        let flows = flows(&scripted, &LoginState::new());
        let flow = open(&flows).await;
        assert_eq!(
            flows.finish(flow, "nohash".into()).await,
            LoginFinishResult::Failed {
                reason: "paste it whole".into()
            }
        );
        assert_eq!(scripted.finishes.load(Ordering::SeqCst), 0);
        assert_eq!(open(&flows).await, flow);
        assert_eq!(flows.finish(flow, "good#state".into()).await, logged_in());
    }

    #[tokio::test]
    async fn a_finish_of_another_flow_finds_none() {
        let scripted = Scripted::new();
        scripted.finish_gate.open();
        let flows = flows(&scripted, &LoginState::new());
        assert_eq!(
            flows.finish(LoginFlowId::random(), "a#b".into()).await,
            LoginFinishResult::NoFlow
        );
        // Not even while starting: no device knows the flow yet.
        let start = tokio::spawn({
            let flows = flows.clone();
            async move { flows.start(deadline()).await }
        });
        tokio::time::timeout(SOON, async {
            while scripted.begins.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            flows.finish(LoginFlowId::random(), "a#b".into()).await,
            LoginFinishResult::NoFlow
        );
        scripted.begin_gate.open();
        let LoginStartResult::Open { flow, .. } = start.await.unwrap() else {
            panic!("not open");
        };
        assert_eq!(
            flows.finish(LoginFlowId::random(), "a#b".into()).await,
            LoginFinishResult::NoFlow
        );
        assert_eq!(flows.finish(flow, "a#b".into()).await, logged_in());
    }

    /// With no time to live, the next look at the slot drops the open
    /// flow and its attempt; its finish then finds none.
    #[tokio::test]
    async fn an_abandoned_flow_goes_at_the_next_look() {
        let scripted = Scripted::open();
        let flows = LoginFlows::new(scripted.clone(), LoginState::new(), Duration::ZERO, None);
        let flow = open(&flows).await;
        assert_eq!(scripted.dropped.load(Ordering::SeqCst), 0);
        flows.touch();
        assert_eq!(scripted.dropped.load(Ordering::SeqCst), 1);
        assert_eq!(
            flows.finish(flow, "good#state".into()).await,
            LoginFinishResult::NoFlow
        );
        assert_eq!(scripted.finishes.load(Ordering::SeqCst), 0);

        // A start drops it too, and opens a new one.
        let again = open(&flows).await;
        assert_ne!(open(&flows).await, again);
        assert_eq!(scripted.begins.load(Ordering::SeqCst), 3);
    }

    /// Held open well within its time, a flow stays.
    #[tokio::test]
    async fn a_flow_within_its_time_stays() {
        let scripted = Scripted::open();
        let flows = flows(&scripted, &LoginState::new());
        let flow = open(&flows).await;
        flows.touch();
        assert_eq!(scripted.dropped.load(Ordering::SeqCst), 0);
        assert_eq!(flows.finish(flow, "good#state".into()).await, logged_in());
    }

    /// Runs `f` on a thread of its own inside a runtime already shut down,
    /// where a spawned task is dropped at once on the spawning thread; a
    /// hang (a task settling the slot under the spawner's own lock) fails
    /// instead of wedging the suite.
    fn after_shutdown<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        let (done, answer) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            let handle = runtime.handle().clone();
            drop(runtime);
            let _entered = handle.enter();
            let _ = done.send(f());
        });
        answer
            .recv_timeout(SOON)
            .expect("the slot's lock was held while a task was spawned")
    }

    /// A task the runtime refuses (it is shutting down) settles the slot
    /// as it drops, which needs the lock: spawning must come after it is
    /// released.
    #[test]
    fn a_task_refused_at_shutdown_settles_the_slot() {
        let scripted = Scripted::open();
        let flows = flows(&scripted, &LoginState::new());
        let started = after_shutdown({
            let flows = flows.clone();
            // One poll: the refused task settles as it is spawned.
            move || flows.start(deadline()).now_or_never()
        });
        assert_eq!(
            started.expect("the start did not settle at once"),
            LoginStartResult::Failed {
                reason: LOGIN_STOPPED.into()
            }
        );
        assert_eq!(scripted.begins.load(Ordering::SeqCst), 0);

        // An open flow whose finish is refused fails, and is done.
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let flow = runtime.block_on(open(&flows));
        let finished = after_shutdown({
            let flows = flows.clone();
            move || flows.finish(flow, "good#state".into()).now_or_never()
        });
        let failed = LoginFinishResult::Failed {
            reason: LOGIN_STOPPED.into(),
        };
        assert_eq!(finished, Some(failed.clone()));
        assert_eq!(
            runtime.block_on(flows.finish(flow, "good#state".into())),
            failed
        );
    }
}
