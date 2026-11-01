//! The link harness (T16, spec §9, D11): real cores against a real
//! `skywaved` (HTTPS/mTLS, supervisor with the fake agent) through
//! [`LinkProxy`]s that add latency, jitter, bandwidth caps and connection
//! drops. Ground truth is the server's own database: its log and its
//! per-command results.
//!
//! The fast tests run in `cargo test`. The long variants are `#[ignore]`d:
//!
//! ```text
//! cargo test -p skywave-core --test link -- --ignored --nocapture
//! SKYWAVE_SOAK_SECS=600 cargo test -p skywave-core --test link soak -- --ignored
//! ```
//!
//! Every proxy draws from the seed printed at the start of each test (shown
//! when it fails); `SKYWAVE_LINK_SEED=<seed>` repeats a run's link
//! decisions. Pairing always runs on a clean link (a one-time code is
//! consumed on the server even if the reply is lost), then the bad profile
//! is switched on.

mod support;

use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant};

use serde_json::json;
use skywave_core::{
    Action, ApprovalStatus, Decision, DeviceRef, EngineConfig, Entry, FailReason, Link,
    OutboxStatus, Outgoing, RejectReason, SessionState as ViewSessionState, TimelineItem,
    TransportConfig,
};
use skywave_proto::{
    ApprovalId, CommandKind, CommandResult, Event, EventKind, SessionId, SessionState,
};
use skywave_testkit::{LinkProfile, LinkProxy, LinkStats, Rng, seed};
use skywaved::agent::fake::{FakeAgent, FakeInput, FakeScript, Gate, Step};
use support::{
    Client, Daemon, SOON, assert_exactly_once, assert_same_log, assert_same_view, config, count,
    is_reconnecting, user_messages,
};

const KIB: u64 = 1024;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// Jittery, capped, and dropping connections by size and by age.
fn lossy() -> LinkProfile {
    LinkProfile::clean()
        .latency(ms(10))
        .jitter(ms(30))
        .bandwidth(512 * KIB)
        .drop_after_bytes(12 * KIB..=64 * KIB)
        .drop_after(ms(400)..=ms(1500))
}

/// A slow, narrow link with long, uneven delays that drops by size.
fn slow() -> LinkProfile {
    LinkProfile::clean()
        .latency(ms(60))
        .jitter(ms(40))
        .bandwidth(96 * KIB)
        .drop_after_bytes(16 * KIB..=96 * KIB)
}

async fn proxy(daemon: &Daemon, seed: u64) -> LinkProxy {
    LinkProxy::start(daemon.addr, LinkProfile::clean(), seed).await
}

/// A paired client behind its own proxy, switched to `profile` once paired.
async fn device(
    daemon: &Daemon,
    seed: u64,
    name: &str,
    profile: LinkProfile,
) -> (Client, LinkProxy) {
    let proxy = proxy(daemon, seed).await;
    let client = Client::open(EngineConfig {
        device_name: Some(name.into()),
        ..config()
    });
    daemon.pair(&client, &proxy.address()).await;
    proxy.set_profile(profile);
    (client, proxy)
}

/// Polls `done` until it holds; for conditions with no notifier (proxy
/// counters). Bounded like every other wait.
async fn poll(what: &str, limit: Duration, done: impl Fn() -> bool) {
    tokio::time::timeout(limit, async {
        while !done() {
            tokio::time::sleep(ms(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

fn agent_messages(events: &[Event], session: SessionId) -> usize {
    events
        .iter()
        .filter(|e| e.session_id == session && matches!(e.kind, EventKind::AgentMessage { .. }))
        .count()
}

/// The session's newest event says the agent is idle.
fn idle(events: &[Event], session: SessionId) -> bool {
    events
        .iter()
        .rev()
        .find(|e| e.session_id == session)
        .map(|e| &e.kind)
        == Some(&EventKind::SessionStateChanged(SessionState::Idle))
}

fn delivered_messages(agent: &FakeAgent, session: SessionId) -> Vec<String> {
    agent
        .inputs(session)
        .into_iter()
        .filter_map(|input| match input {
            FakeInput::Message(text) => Some(text),
            _ => None,
        })
        .collect()
}

/// Messages logged for `session`, in log order.
fn logged_messages(events: &[Event], session: SessionId) -> Vec<String> {
    events
        .iter()
        .filter(|e| e.session_id == session)
        .filter_map(|e| match &e.kind {
            EventKind::UserMessage { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// A message of about `len` bytes, unique by `i`.
fn message(tag: &str, i: usize, len: usize) -> String {
    let mut text = format!("{tag} {i} ");
    while text.len() < len {
        text.push_str("lorem ipsum dolor sit amet ");
    }
    text
}

fn report(name: &str, stats: LinkStats) {
    eprintln!("{name}: {stats:?}");
}

// ---- 1. commands ------------------------------------------------------------

/// D7 under a bad link: `n` commands made back to back while connections
/// die mid-request and mid-response; each lands in the log exactly once,
/// in the order made, and reaches the agent exactly once.
async fn commands_survive(n: usize, profile: LinkProfile, limit: Duration) {
    let seed = seed();
    let agent = FakeAgent::new(FakeScript::new());
    let daemon = Daemon::start(agent.clone()).await;
    let (client, proxy) = device(&daemon, seed, "lossy", LinkProfile::clean()).await;
    let session = client.create_session(&daemon).await;
    proxy.set_profile(profile);

    let mut texts = Vec::new();
    for i in 0..n {
        let text = message("message", i, (i * 397) % 6000);
        client.dispatch(Action::SendMessage {
            session_id: session,
            text: text.clone(),
        });
        texts.push(text);
        if i % 10 == 9 {
            client.dispatch(Action::CreateSession {
                conversation: None,
                workdir: daemon.workdir(),
            });
        }
    }
    daemon
        .wait_log_within(limit, "every message echoed", |events| {
            agent_messages(events, session) == n && idle(events, session)
        })
        .await;
    daemon.settle(&[&client], limit).await;
    report("proxy", proxy.stats());

    let events = daemon.events();
    assert_exactly_once(&daemon, &[&client]);
    for entry in client.outbox() {
        assert!(
            matches!(
                entry.status,
                OutboxStatus::Done(CommandResult::Accepted { .. })
            ),
            "accepted: {entry:?}"
        );
    }
    assert_eq!(
        logged_messages(&events, session),
        texts,
        "in the order made"
    );
    assert_eq!(
        delivered_messages(&agent, session),
        texts,
        "each once to the agent"
    );
    assert_eq!(
        count(&events, |k| matches!(k, EventKind::SessionCreated { .. })),
        1 + n / 10
    );
    assert_same_log(&events, &client.local_events());
    assert!(
        proxy.stats().dropped() > 0,
        "the link dropped: {:?}",
        proxy.stats()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn lossy_link_loses_and_duplicates_no_command() {
    commands_survive(30, lossy(), SOON).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "long: 500 commands over a bad link"]
async fn long_lossy_link_many_commands() {
    let profile = lossy().bandwidth(128 * KIB);
    commands_survive(500, profile, Duration::from_secs(600)).await;
}

// ---- 2. events --------------------------------------------------------------

/// Spec §7 pull loop under a bad link: the link dies mid-long-poll (the
/// agent is quiet behind a gate) and mid-batch (large events, big
/// batches), and the replica still ends equal to the server log — seq
/// 1..=head, no gap, no duplicate — with the cursor at the head.
#[tokio::test(flavor = "multi_thread")]
async fn lossy_link_delivers_every_event_once() {
    let seed = seed();
    let gate = Gate::new();
    let mut turn: Vec<Step> = (0..40)
        .map(|i| Step::text(message("early", i, 600)))
        .collect();
    turn.push(Step::Wait(gate.clone()));
    turn.extend((0..160).map(|i| Step::text(message("late", i, 1200))));
    let agent = FakeAgent::new(FakeScript::new().turn(turn));
    let daemon = Daemon::start(agent).await;
    let proxy = proxy(&daemon, seed).await;
    let client = Client::open(EngineConfig {
        limit: Some(25),
        ..config()
    });
    daemon.pair(&client, &proxy.address()).await;
    let session = client.create_session(&daemon).await;
    proxy.set_profile(
        LinkProfile::clean()
            .latency(ms(5))
            .jitter(ms(20))
            .bandwidth(384 * KIB)
            .drop_after_bytes(16 * KIB..=48 * KIB)
            .drop_after(ms(300)..=ms(900)),
    );

    client.dispatch(Action::SendMessage {
        session_id: session,
        text: "go".into(),
    });
    daemon
        .wait_log("the early messages", |events| {
            agent_messages(events, session) == 40
        })
        .await;
    // Quiet log: the client sits in long-polls the link keeps killing.
    let quiet = proxy.stats().dropped_time;
    poll("drops mid-long-poll", SOON, || {
        proxy.stats().dropped_time >= quiet + 2
    })
    .await;
    gate.open();
    daemon
        .wait_log("the turn to finish", |events| {
            agent_messages(events, session) == 200 && idle(events, session)
        })
        .await;
    daemon.settle(&[&client], SOON).await;
    let stats = proxy.stats();
    report("proxy", stats);

    let events = daemon.events();
    assert_eq!(client.cursor(), daemon.head(), "cursor converged");
    assert_same_log(&events, &client.local_events());
    assert!(stats.dropped_bytes >= 1, "dropped mid-transfer: {stats:?}");

    // The rendered timeline shows each logged event of the session once.
    let view = client.open_session(session, |_| true).await;
    let seqs: Vec<u64> = view
        .timeline
        .iter()
        .map(|item| match item {
            TimelineItem::Logged { seq, .. } => seq.0,
            pending => panic!("nothing pending: {pending:?}"),
        })
        .collect();
    assert!(
        seqs.windows(2).all(|w| w[0] < w[1]),
        "ascending, no repeats"
    );
    let texts = view
        .timeline
        .iter()
        .filter(|item| {
            matches!(
                item,
                TimelineItem::Logged {
                    entry: Entry::AgentMessage { .. },
                    ..
                }
            )
        })
        .count();
    assert_eq!(texts, 200);
}

// ---- 3. two devices ---------------------------------------------------------

/// Spec §9: two devices, each behind its own bad link with a different
/// profile, both creating sessions and talking in each other's sessions,
/// converge on the same replica, session list and timelines.
#[tokio::test(flavor = "multi_thread")]
async fn two_devices_converge_over_different_links() {
    let seed = seed();
    let agent = FakeAgent::new(FakeScript::new().turn([
        Step::text("working"),
        Step::tool_call("shell", json!({ "command": "ls" })),
        Step::tool_result("a.txt"),
        Step::text("done"),
    ]));
    let daemon = Daemon::start(agent.clone()).await;
    let (a, proxy_a) = device(&daemon, seed, "device a", lossy()).await;
    let (b, proxy_b) = device(&daemon, seed ^ 0xB, "device b", slow()).await;

    // Sessions made on each side; A's appear on B and B's on A.
    a.dispatch(Action::CreateSession {
        conversation: None,
        workdir: daemon.workdir(),
    });
    b.create_session(&daemon).await;
    a.create_session(&daemon).await;
    for client in [&a, &b] {
        client
            .wait("both devices' sessions", |c, _| c.sessions().len() == 3)
            .await;
    }
    let all: Vec<SessionId> = a.sessions();
    assert_eq!(
        all.iter().collect::<BTreeSet<_>>(),
        b.sessions().iter().collect::<BTreeSet<_>>()
    );

    // Interleaved messages from both into every session.
    let mut sent: HashMap<SessionId, usize> = HashMap::new();
    for i in 0..3 {
        for &session in &all {
            for (who, client) in [("a", &a), ("b", &b)] {
                client.dispatch(Action::SendMessage {
                    session_id: session,
                    text: message(who, i, 200 + i * 900),
                });
                *sent.entry(session).or_default() += 1;
            }
        }
    }
    // The first message of a session runs the scripted turn (two agent
    // messages), every later one an echo turn.
    daemon
        .wait_log("every turn done", |events| {
            all.iter()
                .all(|&s| agent_messages(events, s) == 2 + sent[&s] - 1 && idle(events, s))
        })
        .await;
    daemon.settle(&[&a, &b], SOON).await;
    report("proxy a", proxy_a.stats());
    report("proxy b", proxy_b.stats());

    let events = daemon.events();
    assert_exactly_once(&daemon, &[&a, &b]);
    assert_same_log(&events, &a.local_events());
    assert_same_log(&events, &b.local_events());
    assert_eq!(a.cursor(), daemon.head());
    assert_eq!(b.cursor(), daemon.head());
    for session in &all {
        assert_eq!(
            delivered_messages(&agent, *session),
            logged_messages(&events, *session),
            "the agent got the log's messages, in log order"
        );
    }
    assert_same_view(&a, &b).await;
}

// ---- 4. first answer wins ---------------------------------------------------

/// D5 under bad links: the agent asks for approval, A approves and B denies
/// at the same moment over different links. Exactly one answer wins —
/// whichever reaches the server first — the other is refused with
/// `ApprovalAlreadyResolved`, the agent hears exactly one answer, and both
/// devices show the same resolution by the winner.
#[tokio::test(flavor = "multi_thread")]
async fn first_answer_wins_across_devices() {
    let seed = seed();
    let agent = FakeAgent::new(
        FakeScript::new().turn(
            [Step::approval("req-1", "write", json!({ "path": "a.txt" }))
                .on_approve([Step::tool_result("written")])
                .on_deny([Step::text("Not writing it.")])],
        ),
    );
    let daemon = Daemon::start(agent.clone()).await;
    let (a, proxy_a) = device(&daemon, seed, "device a", lossy()).await;
    let (b, proxy_b) = device(&daemon, seed ^ 0xB, "device b", slow()).await;

    let session = a.create_session(&daemon).await;
    a.dispatch(Action::SendMessage {
        session_id: session,
        text: "write a.txt".into(),
    });
    let mut approval: Option<ApprovalId> = None;
    for client in [&a, &b] {
        client
            .wait("the session on both devices", |c, _| {
                c.sessions().contains(&session)
            })
            .await;
        let view = client
            .open_session(session, |s| s.pending_approval.is_some())
            .await;
        let id = view.pending_approval.unwrap().id;
        assert_eq!(*approval.get_or_insert(id), id, "the same approval");
    }
    let approval = approval.unwrap();

    a.dispatch(Action::ResolveApproval {
        id: approval,
        decision: Decision::Approve,
        rule: None,
    });
    b.dispatch(Action::ResolveApproval {
        id: approval,
        decision: Decision::Deny,
        rule: None,
    });
    for client in [&a, &b] {
        assert!(
            matches!(
                client.outbox().last().unwrap().command.kind,
                CommandKind::ResolveApproval { id, .. } if id == approval
            ),
            "both answers were queued"
        );
    }

    daemon
        .wait_log("the turn after the answer", |events| {
            count(events, |k| matches!(k, EventKind::ApprovalResolved { .. })) == 1
                && idle(events, session)
        })
        .await;
    daemon.settle(&[&a, &b], SOON).await;
    report("proxy a", proxy_a.stats());
    report("proxy b", proxy_b.stats());

    let events = daemon.events();
    let resolutions: Vec<(Decision, skywave_core::DeviceId)> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::ApprovalResolved {
                id, decision, by, ..
            } if *id == approval => Some((*decision, *by)),
            _ => None,
        })
        .collect();
    assert_eq!(
        resolutions.len(),
        1,
        "exactly one resolution: {resolutions:?}"
    );
    let (decision, by) = resolutions[0];
    let (winner, loser) = if by == a.device() { (&a, &b) } else { (&b, &a) };
    assert_eq!(by, winner.device());
    assert_eq!(
        decision,
        if by == a.device() {
            Decision::Approve
        } else {
            Decision::Deny
        },
        "the winner's decision"
    );
    eprintln!("winner: {}", if by == a.device() { "a" } else { "b" });
    assert_eq!(
        count(&events, |k| matches!(
            k,
            EventKind::ApprovalCancelled { .. }
        )),
        0
    );

    // The loser's answer was refused for good.
    let answer = |client: &Client| {
        client
            .outbox()
            .into_iter()
            .find(|e| matches!(e.command.kind, CommandKind::ResolveApproval { .. }))
            .unwrap()
    };
    assert!(matches!(
        Outgoing::from(&answer(winner).status),
        Outgoing::Accepted(_)
    ));
    assert_eq!(
        Outgoing::from(&answer(loser).status),
        Outgoing::Failed(FailReason::Rejected(RejectReason::ApprovalAlreadyResolved))
    );
    assert_exactly_once(&daemon, &[&a, &b]);

    // The agent heard one answer, the winner's, and went on accordingly.
    let answers: Vec<FakeInput> = agent
        .inputs(session)
        .into_iter()
        .filter(|input| matches!(input, FakeInput::Answer(..)))
        .collect();
    assert_eq!(
        answers,
        vec![FakeInput::Answer("req-1".into(), decision, None)]
    );
    let followed = match decision {
        Decision::Approve => count(&events, |k| matches!(k, EventKind::ToolResult { .. })),
        Decision::Deny => count(
            &events,
            |k| matches!(k, EventKind::AgentMessage { text } if text == "Not writing it."),
        ),
    };
    assert_eq!(followed, 1);

    // Both devices show the same resolution, by the winner, named as it
    // paired: the loser may first see it a `hello` before its name.
    let winner_name = if by == a.device() {
        "device a"
    } else {
        "device b"
    };
    let named = |s: &skywave_core::SessionView| {
        s.timeline.iter().any(|item| {
            matches!(
                item,
                TimelineItem::Logged {
                    entry: Entry::Approval {
                        status: ApprovalStatus::Resolved { by, .. },
                        ..
                    },
                    ..
                } if by.name.is_some()
            )
        })
    };
    for client in [&a, &b] {
        let view = client
            .open_session(session, |s| s.pending_approval.is_none() && named(s))
            .await;
        assert_eq!(view.state, ViewSessionState::Idle);
        let statuses: Vec<&ApprovalStatus> = view
            .timeline
            .iter()
            .filter_map(|item| match item {
                TimelineItem::Logged {
                    entry: Entry::Approval { id, status, .. },
                    ..
                } if *id == approval => Some(status),
                _ => None,
            })
            .collect();
        assert_eq!(
            statuses,
            vec![&ApprovalStatus::Resolved {
                decision,
                by: DeviceRef {
                    id: by,
                    name: Some(winner_name.into()),
                    me: client.device() == by,
                },
                rule: None,
            }]
        );
    }
    assert_same_log(&events, &a.local_events());
    assert_same_log(&events, &b.local_events());
    assert_same_view(&a, &b).await;
}

// ---- 5. offline compose -----------------------------------------------------

/// F8 on a bad link that then goes dark: the route blackholes (connects
/// hang until the client's own timeout), work is composed meanwhile and
/// stays `Queued`, and once the route is back it drains exactly once, in
/// order, over the still-lossy link.
#[tokio::test(flavor = "multi_thread")]
async fn offline_compose_drains_over_a_lossy_link() {
    let seed = seed();
    let agent = FakeAgent::new(FakeScript::new());
    let daemon = Daemon::start(agent.clone()).await;
    let proxy = proxy(&daemon, seed).await;
    let client = Client::open(EngineConfig {
        transport: TransportConfig {
            connect_timeout: ms(800),
            request_timeout: Duration::from_secs(5),
            ..TransportConfig::default()
        },
        ..config()
    });
    daemon.pair(&client, &proxy.address()).await;
    let session = client.create_session(&daemon).await;
    proxy.set_profile(lossy());

    proxy.blackhole();
    client
        .wait("the link to drop", |_, view| is_reconnecting(&view.link))
        .await;
    let texts: Vec<String> = (0..5)
        .map(|i| message("offline", i, 100 + i * 1500))
        .collect();
    for text in &texts {
        client.dispatch(Action::SendMessage {
            session_id: session,
            text: text.clone(),
        });
    }
    client.dispatch(Action::CreateSession {
        conversation: None,
        workdir: daemon.workdir(),
    });
    let queued = client.seam.pending_outbox().unwrap();
    assert_eq!(queued.len(), 6);
    assert!(
        queued.iter().all(|e| e.status == OutboxStatus::Queued),
        "{queued:?}"
    );
    // A reconnect attempt that hung on the dead route and timed out.
    client
        .wait(
            "a timed-out reconnect",
            |_, view| matches!(view.link, Link::Reconnecting { attempt, .. } if attempt >= 2),
        )
        .await;
    assert!(proxy.stats().held >= 1, "{:?}", proxy.stats());
    assert!(user_messages(&daemon.events()).is_empty());

    proxy.up();
    daemon
        .wait_log("every message echoed", |events| {
            agent_messages(events, session) == texts.len() && idle(events, session)
        })
        .await;
    daemon.settle(&[&client], SOON).await;
    report("proxy", proxy.stats());

    let events = daemon.events();
    assert_exactly_once(&daemon, &[&client]);
    assert_eq!(logged_messages(&events, session), texts);
    assert_eq!(delivered_messages(&agent, session), texts);
    assert_same_log(&events, &client.local_events());
}

// ---- soak -------------------------------------------------------------------

/// Minutes of random bad links for two devices: random profiles, cuts and
/// outages; many sessions, each starting with an approval both devices race
/// to answer; payloads up to 64 KiB. Then the links heal and everything
/// must have converged: exactly once, same replicas, same views, one
/// resolution per approval and one answer to the agent per approval.
///
/// `SKYWAVE_SOAK_SECS` sets the duration (default 120).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "long: soak with random link profiles"]
async fn soak_two_devices_random_links() {
    let secs: u64 = std::env::var("SKYWAVE_SOAK_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(120);
    let seed = seed();
    let mut rng = Rng::new(seed).fork(u64::MAX);
    let agent = FakeAgent::new(
        FakeScript::new().turn([
            Step::text("starting"),
            Step::approval("req-1", "write", json!({ "path": "out.txt" }))
                .on_approve([Step::tool_result("written")])
                .on_deny([Step::text("skipped")]),
            Step::text("finished"),
        ]),
    );
    let daemon = Daemon::start(agent.clone()).await;
    let (a, proxy_a) = device(&daemon, seed, "device a", LinkProfile::random(&mut rng)).await;
    let (b, proxy_b) = device(
        &daemon,
        seed ^ 0xB,
        "device b",
        LinkProfile::random(&mut rng),
    )
    .await;
    let devices = [(&a, &proxy_a), (&b, &proxy_b)];
    let long = Duration::from_secs(300);

    let until = Instant::now() + Duration::from_secs(secs);
    let mut messages = 0usize;
    while Instant::now() < until {
        for (client, proxy) in devices {
            let sessions = client.sessions();
            match rng.below(100) {
                0..=4 if sessions.len() < 12 => client.dispatch(Action::CreateSession {
                    conversation: None,
                    workdir: daemon.workdir(),
                }),
                5..=7 => proxy.set_profile(LinkProfile::random(&mut rng)),
                8 => proxy.cut(),
                9 => {
                    proxy.down();
                    tokio::time::sleep(ms(rng.in_range(100..=2000))).await;
                    proxy.up();
                }
                10..=29 => {
                    // Race the other device to every open approval.
                    for approval in open_approvals(&daemon.events()) {
                        let decision = *rng.pick(&[Decision::Approve, Decision::Deny]);
                        client.dispatch(Action::ResolveApproval {
                            id: approval,
                            decision,
                            rule: None,
                        });
                    }
                }
                _ if !sessions.is_empty() => {
                    let session = *rng.pick(&sessions);
                    let len = if rng.chance(0.1) {
                        rng.in_range(16 * KIB..=64 * KIB)
                    } else {
                        rng.in_range(1..=2 * KIB)
                    };
                    client.dispatch(Action::SendMessage {
                        session_id: session,
                        text: message("soak", messages, len as usize),
                    });
                    messages += 1;
                }
                _ => {}
            }
        }
        tokio::time::sleep(ms(rng.in_range(20..=300))).await;
    }

    // Heal and finish: clean links, answer what is still open, wait idle.
    for (_, proxy) in devices {
        proxy.up();
        proxy.set_profile(LinkProfile::clean());
    }
    let deadline = Instant::now() + long;
    loop {
        assert!(Instant::now() < deadline, "the soak did not settle");
        daemon.settle(&[&a, &b], long).await;
        let events = daemon.events();
        let open = open_approvals(&events);
        let sessions: BTreeSet<SessionId> = events.iter().map(|e| e.session_id).collect();
        if open.is_empty()
            && sessions
                .iter()
                .all(|&s| idle(&events, s) || !worked(&events, s))
        {
            break;
        }
        for approval in open {
            a.dispatch(Action::ResolveApproval {
                id: approval,
                decision: Decision::Approve,
                rule: None,
            });
        }
        // Wait for the log to move before checking again.
        let head = daemon.head();
        daemon
            .wait_log_within(long, "progress", |events| events.len() as u64 > head.0)
            .await;
    }
    daemon.settle(&[&a, &b], long).await;
    report("proxy a", proxy_a.stats());
    report("proxy b", proxy_b.stats());
    eprintln!("{messages} messages, {} events", daemon.head().0);

    let events = daemon.events();
    assert_exactly_once(&daemon, &[&a, &b]);
    assert_same_log(&events, &a.local_events());
    assert_same_log(&events, &b.local_events());
    let requested: Vec<(SessionId, ApprovalId)> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::ApprovalRequested { id, .. } => Some((e.session_id, *id)),
            _ => None,
        })
        .collect();
    for (session, approval) in requested {
        assert_eq!(
            count(
                &events,
                |k| matches!(k, EventKind::ApprovalResolved { id, .. } if *id == approval)
            ),
            1,
            "one resolution for {approval:?}"
        );
        let answers = agent
            .inputs(session)
            .into_iter()
            .filter(|i| matches!(i, FakeInput::Answer(..)))
            .count();
        assert_eq!(answers, 1, "one answer to the agent in {session:?}");
        assert_eq!(
            delivered_messages(&agent, session),
            logged_messages(&events, session)
        );
    }
    assert_same_view(&a, &b).await;
}

/// Approvals requested and not yet resolved or cancelled.
fn open_approvals(events: &[Event]) -> Vec<ApprovalId> {
    let mut open: Vec<ApprovalId> = Vec::new();
    for event in events {
        match &event.kind {
            EventKind::ApprovalRequested { id, .. } => open.push(*id),
            EventKind::ApprovalResolved { id, .. } | EventKind::ApprovalCancelled { id, .. } => {
                open.retain(|open| open != id)
            }
            _ => {}
        }
    }
    open
}

/// The session ever ran a turn.
fn worked(events: &[Event], session: SessionId) -> bool {
    events.iter().any(|e| {
        e.session_id == session
            && e.kind == EventKind::SessionStateChanged(SessionState::AgentWorking)
    })
}
