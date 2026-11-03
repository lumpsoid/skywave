//! Shared harness for the end-to-end tests: `skywaved` in-process (HTTPS API
//! plus supervisor with the fake agent) on 127.0.0.1 over real TLS, cores
//! with their engines, bounded waits, and ground-truth checks against the
//! server's own database. The link between them is a
//! [`skywave_testkit::LinkProxy`].
//!
//! Waits are bounded and driven by `ViewState` snapshots or the server log's
//! notifier; nothing relies on a fixed sleep for correctness.

#![allow(dead_code)] // each test crate uses a different part

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use skywave_core::{
    Action, ApprovalStatus, Backoff, Core, DeviceId, DeviceRef, EngineConfig, Entry, Jitter, Link,
    ListKind, OutboxEntry, OutboxStatus, Screen, Seam, SessionRow, SessionView, TimelineItem,
    TransportConfig, ViewState,
};
use skywave_proto::{
    CommandId, CommandKind, CommandResult, ContextRef, Event, EventKind, Seq, SessionId,
    SessionOrigin,
};
use skywaved::agent::fake::{FakeAgent, FakeOutside};
use skywaved::agent::{AdapterEvent, AgentCallId, HistoryItem, ResultDisplay};
use skywaved::identity::ServerIdentity;
use skywaved::pairing;
use skywaved::server::{Server, ServerOptions};
use skywaved::workdir::Git;
use skywaved::{EventLog, Storage, Supervisor, SupervisorConfig, SupervisorHandle};
use tempfile::TempDir;
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;

/// Upper bound for any wait in the fast tests; a passing test never gets
/// near it.
pub const SOON: Duration = Duration::from_secs(20);

// ---- server -----------------------------------------------------------------

/// `skywaved` in-process: API server plus supervisor with the fake agent.
pub struct Daemon {
    pub tmp: TempDir,
    pub storage: Storage,
    pub identity: Arc<ServerIdentity>,
    pub log: EventLog,
    pub addr: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<skywaved::Result<()>>,
    _supervisor: SupervisorHandle,
}

impl Daemon {
    pub async fn start(agent: FakeAgent) -> Daemon {
        Daemon::start_with_shell(agent, false).await
    }

    /// As [`Daemon::start`], running shell commands from devices if `shell`.
    pub async fn start_with_shell(agent: FakeAgent, shell: bool) -> Daemon {
        Daemon::start_with(agent, shell, FakeOutside::new()).await
    }

    /// As [`Daemon::start`], an adopting `CreateSession` reading its
    /// conversation from `outside` (keyed by [`Daemon::canonical_workdir`]).
    pub async fn start_with_outside(agent: FakeAgent, outside: FakeOutside) -> Daemon {
        Daemon::start_with(agent, false, outside).await
    }

    async fn start_with(agent: FakeAgent, shell: bool, outside: FakeOutside) -> Daemon {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("root/project")).unwrap();
        let data = tmp.path().join("server");
        let storage = Storage::open(&data).unwrap();
        let identity = Arc::new(
            ServerIdentity::bootstrap(&storage.dir, &["localhost".into(), "127.0.0.1".into()])
                .unwrap(),
        );
        let log = EventLog::open(&storage.db, vec![tmp.path().join("root")])
            .unwrap()
            .with_shell(shell);
        // The daemon's git (workdir changes), immune to the user's config.
        let git = Git::default()
            .with_env("GIT_CONFIG_GLOBAL", "/dev/null")
            .with_env("GIT_CONFIG_NOSYSTEM", "1");
        let options = ServerOptions {
            git,
            outside: Arc::new(outside),
            ..ServerOptions::default()
        };
        let server = Server::bind_with(
            "127.0.0.1:0".parse().unwrap(),
            &storage.dir,
            identity.clone(),
            log.clone(),
            options,
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(server.serve(async {
            let _ = stopped.await;
        }));
        let supervisor = Supervisor::start(agent, &data, log.clone(), SupervisorConfig::default())
            .await
            .unwrap();
        Daemon {
            tmp,
            storage,
            identity,
            log,
            addr,
            stop: Some(stop),
            task,
            _supervisor: supervisor,
        }
    }

    pub fn workdir(&self) -> String {
        self.tmp.path().join("root/project").display().to_string()
    }

    /// The workdir as the server validates it: what an outside source is
    /// asked for.
    pub fn canonical_workdir(&self) -> std::path::PathBuf {
        std::fs::canonicalize(self.workdir()).unwrap()
    }

    pub fn head(&self) -> Seq {
        self.log.head()
    }

    /// The whole log, seq 1 first.
    pub fn events(&self) -> Vec<Event> {
        let mut events = Vec::new();
        loop {
            let batch = self
                .log
                .read(&self.storage.db, Seq(events.len() as u64), 5000)
                .unwrap();
            if batch.events.is_empty() {
                return events;
            }
            events.extend(batch.events);
        }
    }

    /// Every command result the server stored (one row per command id),
    /// with the device that sent it.
    pub fn command_results(&self) -> HashMap<CommandId, (DeviceId, CommandResult)> {
        let conn = self.storage.db.conn();
        let mut stmt = conn
            .prepare("SELECT command_id, device_id, result FROM commands")
            .unwrap();
        stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .unwrap()
        .map(|row| {
            let (command, device, result) = row.unwrap();
            (
                CommandId(command.parse().unwrap()),
                (
                    DeviceId(device.parse().unwrap()),
                    serde_json::from_str(&result).unwrap(),
                ),
            )
        })
        .collect()
    }

    /// Pairs `client` through its engine: the head's `Pair` action, with
    /// what `skywaved pair` would print, connecting to `address`.
    pub async fn pair(&self, client: &Client, address: &str) {
        let code = pairing::issue_code(self.storage.db.conn(), Utc::now(), pairing::DEFAULT_TTL)
            .unwrap()
            .code;
        client
            .core
            .dispatch(Action::Pair {
                address: address.to_owned(),
                code: code.to_string(),
                fingerprint: self.identity.ca_fingerprint().to_string(),
            })
            .unwrap();
        client
            .wait("paired and online", |_, view| {
                matches!(view.screen, Screen::SessionList(_))
                    && matches!(view.link, Link::Online { .. })
            })
            .await;
    }

    /// Waits until the log satisfies `done`.
    pub async fn wait_log(&self, what: &str, done: impl Fn(&[Event]) -> bool) {
        self.wait_log_within(SOON, what, done).await
    }

    pub async fn wait_log_within(
        &self,
        limit: Duration,
        what: &str,
        done: impl Fn(&[Event]) -> bool,
    ) {
        let mut head = self.log.subscribe();
        tokio::time::timeout(limit, async {
            loop {
                head.borrow_and_update();
                if done(&self.events()) {
                    return;
                }
                head.changed().await.unwrap();
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for the log: {what}"));
    }

    /// Waits until every client has applied the whole log and has no
    /// command left without a result, with the head unchanged meanwhile.
    pub async fn settle(&self, clients: &[&Client], limit: Duration) {
        loop {
            let head = self.head();
            for client in clients {
                client
                    .wait_within(limit, "the replica to reach the head", |c, view| {
                        view.queued == 0 && c.cursor() >= self.head()
                    })
                    .await;
            }
            if self.head() == head {
                return;
            }
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.task.abort();
    }
}

// ---- client -----------------------------------------------------------------

/// A core with its engine, its snapshots on a watch channel.
pub struct Client {
    pub dir: TempDir,
    pub core: Core,
    pub seam: Seam,
    pub views: watch::Receiver<ViewState>,
}

impl Client {
    pub fn open(config: EngineConfig) -> Client {
        Client::open_in(tempfile::tempdir().unwrap(), config)
    }

    /// A core over the data dir a [`stop`](Client::stop)ped one left: a
    /// cold start.
    pub fn open_in(dir: TempDir, config: EngineConfig) -> Client {
        let core = Core::open_with(dir.path(), config).unwrap();
        let (tx, views) = watch::channel(core.view());
        core.subscribe(move |view| {
            tx.send_replace(view);
        });
        Client {
            dir,
            seam: core.seam(),
            core,
            views,
        }
    }

    /// Waits until `done` holds for the latest snapshot (and whatever else
    /// it checks), re-checking on every new snapshot. Snapshots older than
    /// the call — the delivery thread may not have caught up with a
    /// dispatch yet — do not count.
    pub async fn wait(&self, what: &str, done: impl Fn(&Client, &ViewState) -> bool) -> ViewState {
        self.wait_within(SOON, what, done).await
    }

    pub async fn wait_within(
        &self,
        limit: Duration,
        what: &str,
        done: impl Fn(&Client, &ViewState) -> bool,
    ) -> ViewState {
        let mut views = self.views.clone();
        tokio::time::timeout(limit, async {
            let mut caught_up = false;
            loop {
                let view = views.borrow_and_update().clone();
                caught_up = caught_up || view == self.core.view();
                if caught_up && done(self, &view) {
                    return view;
                }
                views.changed().await.unwrap();
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "timed out waiting for {what}; last view: {:?}",
                self.core.view()
            )
        })
    }

    /// Ends the core and its engine, as the process dying would, and hands
    /// back its data dir.
    pub fn stop(self) -> TempDir {
        let Client {
            dir,
            core,
            seam,
            views,
        } = self;
        drop((seam, views));
        // Off the runtime: a shutdown refuses to block a runtime thread.
        std::thread::spawn(move || core.shutdown(SOON))
            .join()
            .unwrap()
            .unwrap();
        dir
    }

    pub fn dispatch(&self, action: Action) {
        self.core.dispatch(action).unwrap();
    }

    pub fn device(&self) -> DeviceId {
        self.seam.identity().unwrap().device_id()
    }

    pub fn cursor(&self) -> Seq {
        self.seam.cursor().unwrap()
    }

    /// Every command this device made, in the order made.
    pub fn outbox(&self) -> Vec<OutboxEntry> {
        let db = replica(self.dir.path());
        let mut stmt = db
            .prepare(
                "SELECT command, sending, result, tries, first_sent_at, answered_at
                 FROM outbox ORDER BY position",
            )
            .unwrap();
        let time = |text: Option<String>| {
            text.map(|t| {
                chrono::DateTime::parse_from_rfc3339(&t)
                    .unwrap()
                    .with_timezone(&Utc)
            })
        };
        stmt.query_map([], |row| {
            let command: String = row.get(0)?;
            let sending: bool = row.get(1)?;
            let result: Option<String> = row.get(2)?;
            let status = match (result, sending) {
                // The core's own marker for a command it gave up on, or
                // the server's answer.
                (Some(result), _) => {
                    let value: serde_json::Value = serde_json::from_str(&result).unwrap();
                    if value["result"] == "undeliverable" {
                        OutboxStatus::Undeliverable
                    } else {
                        OutboxStatus::Done(serde_json::from_value(value).unwrap())
                    }
                }
                (None, true) => OutboxStatus::Sending,
                (None, false) => OutboxStatus::Queued,
            };
            Ok(OutboxEntry {
                command: serde_json::from_str(&command).unwrap(),
                status,
                tries: row.get(3)?,
                first_sent_at: time(row.get(4)?),
                answered_at: time(row.get(5)?),
            })
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
    }

    /// Every event in the local replica, seq order.
    pub fn local_events(&self) -> Vec<(Seq, Event)> {
        local_events(self.dir.path())
    }

    /// Creates a session and waits until the replica has it and the
    /// command has its result.
    pub async fn create_session(&self, daemon: &Daemon) -> SessionId {
        self.dispatch(Action::CreateSession {
            conversation: None,
            workdir: daemon.workdir(),
        });
        let command = self.outbox().last().unwrap().command.id;
        self.wait("the session in the replica", |c, _| {
            c.created_session(command).is_some()
        })
        .await;
        self.created_session(command).unwrap()
    }

    /// The session a `CreateSession` command made, once it is in the
    /// replica.
    pub fn created_session(&self, command: CommandId) -> Option<SessionId> {
        let entry = self
            .outbox()
            .into_iter()
            .find(|e| e.command.id == command)?;
        let OutboxStatus::Done(CommandResult::Accepted { seq }) = entry.status else {
            return None;
        };
        self.local_events()
            .into_iter()
            .find(|(s, _)| *s == seq)
            .map(|(_, e)| e.session_id)
    }

    /// Session ids in the local replica.
    pub fn sessions(&self) -> Vec<SessionId> {
        let db = replica(self.dir.path());
        let mut stmt = db.prepare("SELECT id FROM sessions ORDER BY id").unwrap();
        stmt.query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(|id| SessionId(id.unwrap().parse().unwrap()))
            .collect()
    }

    /// The session list as the head renders it.
    pub async fn session_list(&self) -> Vec<SessionRow> {
        self.list(skywave_core::Route::SessionList, ListKind::Active)
            .await
    }

    /// The archived sessions as the head renders them.
    pub async fn archived_list(&self) -> Vec<SessionRow> {
        self.list(skywave_core::Route::ArchivedSessions, ListKind::Archived)
            .await
    }

    async fn list(&self, route: skywave_core::Route, kind: ListKind) -> Vec<SessionRow> {
        self.dispatch(Action::Navigate(route));
        let view = self
            .wait(
                "the session list",
                |_, view| matches!(&view.screen, Screen::SessionList(list) if list.kind == kind),
            )
            .await;
        let Screen::SessionList(list) = view.screen else {
            unreachable!()
        };
        list.sessions
    }

    /// Opens `id` and returns the first snapshot that satisfies `done`.
    pub async fn open_session(
        &self,
        id: SessionId,
        done: impl Fn(&SessionView) -> bool,
    ) -> SessionView {
        self.dispatch(Action::OpenSession(id));
        let view = self
            .wait(
                "the session screen",
                |_, view| matches!(&view.screen, Screen::Session(s) if s.id == id && done(s)),
            )
            .await;
        let Screen::Session(session) = view.screen else {
            unreachable!()
        };
        *session
    }
}

/// A read-only connection to a running core's replica. `Store::open` is
/// for the replica's one owner: it migrates and puts `Sending` commands back
/// to `Queued`, which would meddle with the engine under test.
fn replica(dir: &Path) -> rusqlite::Connection {
    let db = rusqlite::Connection::open_with_flags(
        dir.join(skywave_core::store::DB_FILE),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    db.busy_timeout(SOON).unwrap();
    db
}

pub fn local_events(dir: &Path) -> Vec<(Seq, Event)> {
    let db = replica(dir);
    let mut stmt = db
        .prepare("SELECT seq, event FROM events ORDER BY seq")
        .unwrap();
    stmt.query_map([], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })
    .unwrap()
    .map(|row| {
        let (seq, event) = row.unwrap();
        (Seq(seq as u64), serde_json::from_str(&event).unwrap())
    })
    .collect()
}

/// Fast retries, short long-polls, small batches.
pub fn config() -> EngineConfig {
    EngineConfig {
        transport: TransportConfig {
            connect_timeout: SOON,
            request_timeout: SOON,
            ..TransportConfig::default()
        },
        wait_secs: 1,
        limit: Some(7),
        backoff: Backoff {
            base: Duration::from_millis(20),
            cap: Duration::from_millis(200),
            jitter: Jitter::random(),
        },
        device_name: Some("sync test".into()),
        ..EngineConfig::default()
    }
}

/// An outside conversation of `turns` turns, each a prompt, a call with a
/// result of `output_len` bytes (a blob once long enough) and the agent's
/// answer; then a last call that never got a result. Times are a minute
/// per turn on 2026-11-01, the result's left unsaid.
pub fn adopted_history(turns: usize, output_len: usize) -> Vec<HistoryItem> {
    let at = |turn: usize, second: u32| {
        let start: chrono::DateTime<Utc> = "2026-11-01T10:00:00Z".parse().unwrap();
        Some(
            start
                + chrono::TimeDelta::minutes(turn as i64)
                + chrono::TimeDelta::seconds(second.into()),
        )
    };
    let call = |id: String, command: String, at| HistoryItem::Agent {
        at,
        event: AdapterEvent::ToolCall {
            call: Some(AgentCallId::from(id.as_str())),
            parent: None,
            name: "Bash".into(),
            input: serde_json::json!({ "command": command }),
            display: None,
        },
    };
    let mut items = Vec::new();
    for turn in 0..turns {
        let id = format!("toolu_{turn}");
        items.push(HistoryItem::User {
            at: at(turn, 0),
            text: format!("imported prompt {turn}"),
        });
        items.push(call(id.clone(), format!("ls {turn}"), at(turn, 1)));
        items.push(HistoryItem::Agent {
            at: None,
            event: AdapterEvent::ToolResult {
                call: Some(AgentCallId::from(id.as_str())),
                result: ResultDisplay {
                    is_error: false,
                    status: None,
                    preview: None,
                    output: format!("{turn}\n").repeat(output_len / 2 + 1),
                },
            },
        });
        items.push(HistoryItem::Agent {
            at: at(turn, 2),
            event: AdapterEvent::Text {
                text: format!("imported answer {turn}"),
                parent: None,
            },
        });
    }
    items.push(call("toolu_open".into(), "sleep 100".into(), at(turns, 0)));
    items
}

pub fn is_reconnecting(link: &Link) -> bool {
    matches!(link, Link::Reconnecting { .. })
}

pub fn user_messages(events: &[Event]) -> Vec<(CommandId, String)> {
    events
        .iter()
        .filter_map(|e| match (&e.kind, e.command_id) {
            (EventKind::UserMessage { text, .. }, Some(command_id)) => {
                Some((command_id, text.clone()))
            }
            _ => None,
        })
        .collect()
}

pub fn count(events: &[Event], what: impl Fn(&EventKind) -> bool) -> usize {
    events.iter().filter(|e| what(&e.kind)).count()
}

/// The server log and the replica hold the same events at the same seqs.
pub fn assert_same_log(server: &[Event], local: &[(Seq, Event)]) {
    let seqs: Vec<u64> = local.iter().map(|(seq, _)| seq.0).collect();
    let expected: Vec<u64> = (1..=server.len() as u64).collect();
    assert_eq!(seqs, expected, "no gaps, no duplicates");
    for ((seq, mine), theirs) in local.iter().zip(server) {
        assert_eq!(mine, theirs, "event {seq:?}");
    }
}

/// Ground truth for D7 across `clients`: the server stored exactly one
/// result per command made, the one each device recorded; every accepted
/// command's seq names its own event; nothing else reached the log but the
/// new session's `SessionCreated` right before each move's `ContextMoved`,
/// and an adopted session's `SessionCreated` and imported history before
/// its `ImportFinished`.
pub fn assert_exactly_once(daemon: &Daemon, clients: &[&Client]) {
    let events = daemon.events();
    let results = daemon.command_results();
    let event_at = |seq: Seq| &events[(seq.0 - 1) as usize].kind;
    let mut made = 0;
    let mut accepted: HashMap<&'static str, usize> = HashMap::new();
    for client in clients {
        let device = client.device();
        for entry in client.outbox() {
            made += 1;
            let id = entry.command.id;
            let OutboxStatus::Done(result) = &entry.status else {
                panic!("command without a result: {entry:?}");
            };
            assert_eq!(
                results.get(&id),
                Some(&(device, result.clone())),
                "the server's stored result for {entry:?}"
            );
            let CommandResult::Accepted { seq } = result else {
                continue;
            };
            let kind = event_at(*seq);
            assert_eq!(
                events[(seq.0 - 1) as usize].command_id,
                Some(id),
                "seq {seq:?} names the command it acknowledges"
            );
            let name = match (&entry.command.kind, kind) {
                (
                    CommandKind::CreateSession {
                        conversation: None, ..
                    },
                    EventKind::SessionCreated { from: None, .. },
                ) => "SessionCreated",
                // An adopting create names itself on its `ImportFinished`,
                // after its session's `SessionCreated` and imported history.
                (
                    CommandKind::CreateSession {
                        conversation: Some(_),
                        ..
                    },
                    EventKind::ImportFinished { .. },
                ) => {
                    let session = events[(seq.0 - 1) as usize].session_id;
                    let created = events
                        .iter()
                        .find(|e| e.session_id == session)
                        .expect("the adopted session's events");
                    assert_eq!(created.command_id, None);
                    assert!(
                        matches!(
                            &created.kind,
                            EventKind::SessionCreated {
                                from: Some(SessionOrigin::Agent { .. }),
                                ..
                            }
                        ),
                        "{created:?} is not an adopted session's first event"
                    );
                    *accepted.entry("AdoptedSessionCreated").or_default() += 1;
                    "ImportFinished"
                }
                (
                    CommandKind::SendMessage {
                        session_id,
                        context,
                        text,
                    },
                    EventKind::UserMessage {
                        device_id,
                        text: logged,
                    },
                ) => {
                    assert_eq!(*device_id, device);
                    assert_eq!(logged, text);
                    assert_eq!(events[(seq.0 - 1) as usize].session_id, *session_id);
                    assert_eq!(events[(seq.0 - 1) as usize].context, *context);
                    "UserMessage"
                }
                (
                    CommandKind::ResolveApproval {
                        id: approval,
                        decision,
                        rule: None,
                    },
                    EventKind::ApprovalResolved {
                        id: resolved,
                        decision: logged,
                        by,
                        rule: None,
                    },
                ) => {
                    assert_eq!((resolved, logged, by), (approval, decision, &device));
                    "ApprovalResolved"
                }
                (CommandKind::Interrupt { .. }, EventKind::TurnInterrupted) => "TurnInterrupted",
                (
                    CommandKind::ClearContext {
                        session_id,
                        context,
                        name,
                    },
                    EventKind::ContextStarted { by, name: logged },
                ) => {
                    assert_eq!((by, logged), (&device, name));
                    assert_eq!(events[(seq.0 - 1) as usize].session_id, *session_id);
                    assert_eq!(events[(seq.0 - 1) as usize].context, *context);
                    "ContextStarted"
                }
                (
                    CommandKind::SwitchContext {
                        session_id,
                        context,
                    },
                    EventKind::ContextSwitched { by },
                ) => {
                    assert_eq!(by, &device);
                    assert_eq!(events[(seq.0 - 1) as usize].session_id, *session_id);
                    assert_eq!(events[(seq.0 - 1) as usize].context, *context);
                    "ContextSwitched"
                }
                (
                    CommandKind::MoveContext {
                        session_id,
                        context,
                        ..
                    },
                    EventKind::ContextMoved { to, by },
                ) => {
                    assert_eq!(by, &device);
                    assert_eq!(events[(seq.0 - 1) as usize].session_id, *session_id);
                    assert_eq!(events[(seq.0 - 1) as usize].context, *context);
                    // The new session comes right before, naming no command.
                    let created = &events[(seq.0 - 2) as usize];
                    assert_eq!(created.session_id, *to);
                    assert_eq!(created.command_id, None);
                    let from = ContextRef {
                        session: *session_id,
                        context: *context,
                    };
                    assert!(
                        matches!(&created.kind, EventKind::SessionCreated { from: Some(SessionOrigin::Context(r)), .. } if *r == from),
                        "seq {} holds {created:?}, not the moved context's session",
                        seq.0 - 1
                    );
                    *accepted.entry("MovedSessionCreated").or_default() += 1;
                    "ContextMoved"
                }
                (command, event) => panic!("seq {seq:?} holds {event:?}, not {command:?}"),
            };
            *accepted.entry(name).or_default() += 1;
        }
    }
    assert_eq!(results.len(), made, "the server knows no other commands");
    let logged = |name: &str| {
        count(&events, |k| {
            name == match k {
                // A fresh one is its `CreateSession`'s own event; an adopting
                // one's is the `ImportFinished` after it.
                EventKind::SessionCreated { from: None, .. } => "SessionCreated",
                EventKind::SessionCreated {
                    from: Some(SessionOrigin::Agent { .. }),
                    ..
                } => "AdoptedSessionCreated",
                EventKind::ImportFinished { .. } => "ImportFinished",
                EventKind::SessionCreated {
                    from: Some(SessionOrigin::Context(_)),
                    ..
                } => "MovedSessionCreated",
                EventKind::UserMessage { .. } => "UserMessage",
                EventKind::ApprovalResolved { .. } => "ApprovalResolved",
                EventKind::TurnInterrupted => "TurnInterrupted",
                EventKind::ContextStarted { .. } => "ContextStarted",
                EventKind::ContextSwitched { .. } => "ContextSwitched",
                EventKind::ContextMoved { .. } => "ContextMoved",
                _ => "",
            }
        })
    };
    for name in [
        "SessionCreated",
        "UserMessage",
        "ApprovalResolved",
        "TurnInterrupted",
        "ContextStarted",
        "ContextSwitched",
        "ContextMoved",
        "MovedSessionCreated",
        "ImportFinished",
        "AdoptedSessionCreated",
    ] {
        assert_eq!(
            logged(name),
            accepted.get(name).copied().unwrap_or(0),
            "each {name} in the log comes from exactly one accepted command"
        );
    }
    let mut ids: Vec<CommandId> = user_messages(&events)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    ids.sort_by_key(|id| id.0);
    ids.dedup();
    assert_eq!(ids.len(), logged("UserMessage"), "no message twice");
}

/// A device as every device shows it: who is `me` differs, and a name
/// may come a `hello` later on one than on another.
fn shared_device(device: DeviceRef) -> DeviceRef {
    DeviceRef {
        name: None,
        me: false,
        ..device
    }
}

/// A timeline with what differs per device (who is `me`, delivery state)
/// blanked, for comparing devices.
pub fn shared_timeline(timeline: &[TimelineItem]) -> Vec<TimelineItem> {
    timeline
        .iter()
        .cloned()
        .map(|item| match item {
            TimelineItem::Logged {
                seq,
                at,
                context,
                parent,
                entry,
            } => TimelineItem::Logged {
                seq,
                at,
                context,
                parent,
                entry: match entry {
                    Entry::UserMessage {
                        command_id,
                        text,
                        from,
                        ..
                    } => Entry::UserMessage {
                        command_id,
                        text,
                        from: shared_device(from),
                        outgoing: None,
                        delivery: None,
                    },
                    Entry::Approval {
                        id,
                        tool,
                        summary,
                        input,
                        status,
                        answer: _,
                    } => Entry::Approval {
                        id,
                        tool,
                        summary,
                        input,
                        status: match status {
                            ApprovalStatus::Resolved { decision, by, .. } => {
                                ApprovalStatus::Resolved {
                                    decision,
                                    by: shared_device(by),
                                    rule: None,
                                }
                            }
                            other => other,
                        },
                        answer: None,
                    },
                    Entry::Renamed { name, by, .. } => Entry::Renamed {
                        name,
                        by: shared_device(by),
                    },
                    Entry::Archived { archived, by, .. } => Entry::Archived {
                        archived,
                        by: shared_device(by),
                    },
                    other => other,
                },
            },
            pending => pending,
        })
        .collect()
}

/// Two devices show the same session list and, session by session, the
/// same timeline.
pub async fn assert_same_view(a: &Client, b: &Client) {
    let (list_a, list_b) = (a.session_list().await, b.session_list().await);
    assert_eq!(list_a, list_b, "session lists");
    for row in &list_a {
        let view_a = a.open_session(row.id, |_| true).await;
        let view_b = b.open_session(row.id, |_| true).await;
        assert!(
            !view_a
                .timeline
                .iter()
                .any(|i| matches!(i, TimelineItem::Pending { .. })),
            "nothing pending: {view_a:?}"
        );
        assert_eq!(view_a.state, view_b.state);
        assert_eq!(view_a.workdir, view_b.workdir);
        assert_eq!(
            shared_timeline(&view_a.timeline),
            shared_timeline(&view_b.timeline),
            "timeline of {:?}",
            row.id
        );
    }
}
