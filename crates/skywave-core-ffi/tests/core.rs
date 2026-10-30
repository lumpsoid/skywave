//! `SkywaveCore` driven only through its exported surface, the way Kotlin
//! drives it, against `skywaved` in-process (HTTPS API plus supervisor
//! with the fake agent) on 127.0.0.1 over real TLS.
//!
//! Waits are bounded and driven by `ViewState` snapshots from a
//! channel-backed listener; nothing relies on a fixed sleep.

use std::net::SocketAddr;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Utc;
use skywave_core_ffi::action::Action;
use skywave_core_ffi::client::{SkywaveCore, ViewListener};
use skywave_core_ffi::error::SkywaveError;
use skywave_core_ffi::helpers::parse_pair_uri;
use skywave_core_ffi::view::{Entry, Link, PairingStatus, Screen, TimelineItem, ViewState};
use skywaved::agent::fake::{FakeAgent, FakeScript, Step};
use skywaved::identity::ServerIdentity;
use skywaved::pairing;
use skywaved::server::Server;
use skywaved::{EventLog, Storage, Supervisor, SupervisorConfig, SupervisorHandle};
use tempfile::TempDir;
use tokio::runtime::Runtime;
use tokio::sync::oneshot;

/// Upper bound for any wait; a passing test never gets near it.
const SOON: Duration = Duration::from_secs(20);

const REPLY: &str = "hello from the fake agent";

/// `skywaved` in-process, on its own runtime so the test thread is free to
/// block on snapshots.
struct Daemon {
    tmp: TempDir,
    storage: Storage,
    identity: Arc<ServerIdentity>,
    addr: SocketAddr,
    _stop: oneshot::Sender<()>,
    _supervisor: SupervisorHandle,
    _runtime: Runtime,
}

impl Daemon {
    fn start(agent: FakeAgent) -> Daemon {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("root/project")).unwrap();
        let data = tmp.path().join("server");
        let storage = Storage::open(&data).unwrap();
        let identity = Arc::new(
            ServerIdentity::bootstrap(&storage.dir, &["localhost".into(), "127.0.0.1".into()])
                .unwrap(),
        );
        let log = EventLog::open(&storage.db, vec![tmp.path().join("root")]).unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let (addr, supervisor) = runtime.block_on(async {
            let server = Server::bind(
                "127.0.0.1:0".parse().unwrap(),
                &storage.dir,
                identity.clone(),
                log.clone(),
            )
            .await
            .unwrap();
            let addr = server.local_addr();
            tokio::spawn(server.serve(async {
                let _ = stopped.await;
            }));
            let supervisor =
                Supervisor::start(agent, &data, log.clone(), SupervisorConfig::default())
                    .await
                    .unwrap();
            (addr, supervisor)
        });
        Daemon {
            tmp,
            storage,
            identity,
            addr,
            _stop: stop,
            _supervisor: supervisor,
            _runtime: runtime,
        }
    }

    fn workdir(&self) -> String {
        self.tmp.path().join("root/project").display().to_string()
    }

    /// What the pairing QR code of `skywaved pair` would hold.
    fn pair_uri(&self) -> String {
        let code = pairing::issue_code(self.storage.db.conn(), Utc::now(), pairing::DEFAULT_TTL)
            .unwrap()
            .code;
        format!(
            "skywave://pair?addr={}&code={}&fp={}",
            self.addr,
            code,
            self.identity.ca_fingerprint()
        )
    }
}

/// A listener that forwards every snapshot to a channel.
struct Forward(Mutex<Sender<ViewState>>);

impl ViewListener for Forward {
    fn on_view(&self, view: ViewState) {
        let _ = self.0.lock().unwrap().send(view);
    }
}

struct Views(Receiver<ViewState>);

impl Views {
    fn listener() -> (Arc<dyn ViewListener>, Views) {
        let (tx, rx) = mpsc::channel();
        (Arc::new(Forward(Mutex::new(tx))), Views(rx))
    }

    /// The first snapshot from now on that satisfies `done`.
    fn wait(&self, what: &str, done: impl Fn(&ViewState) -> bool) -> ViewState {
        let deadline = Instant::now() + SOON;
        let mut last = None;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.0.recv_timeout(left) {
                Ok(view) if done(&view) => return view,
                Ok(view) => last = Some(view),
                Err(_) => panic!("timed out waiting for {what}; last view: {last:?}"),
            }
        }
    }
}

fn open_core() -> (TempDir, Arc<SkywaveCore>) {
    let dir = tempfile::tempdir().unwrap();
    let core =
        SkywaveCore::open(dir.path().display().to_string(), Some("ffi test".into())).unwrap();
    (dir, core)
}

fn is_session_list(view: &ViewState) -> bool {
    matches!(view.screen, Screen::SessionList { .. })
}

/// Pair → create a session → send a message → the agent's reply, all
/// through the exported API.
#[test]
fn pairs_creates_a_session_and_sees_the_agent_reply() {
    let daemon = Daemon::start(FakeAgent::new(FakeScript::new().turn([Step::text(REPLY)])));
    let (_dir, core) = open_core();
    let (listener, views) = Views::listener();
    core.subscribe(listener).unwrap();
    let first = views.wait("the first snapshot", |_| true);
    assert!(matches!(first.screen, Screen::Pairing { .. }), "{first:?}");

    let pair = parse_pair_uri(daemon.pair_uri()).unwrap();
    core.dispatch(Action::Pair {
        address: pair.address,
        code: pair.code,
        fingerprint: pair.fingerprint,
    })
    .unwrap();
    views.wait("paired and online", |view| {
        is_session_list(view) && matches!(view.link, Link::Online { .. })
    });

    core.dispatch(Action::CreateSession {
        workdir: daemon.workdir(),
    })
    .unwrap();
    let workdir = daemon.workdir();
    let view = views.wait("the new session in the list", |view| {
        matches!(&view.screen, Screen::SessionList { view }
            if view.sessions.iter().any(|row| row.workdir.as_deref() == Some(workdir.as_str())))
    });
    let Screen::SessionList { view: list } = view.screen else {
        unreachable!()
    };
    let session_id = list.sessions[0].id.clone();

    core.dispatch(Action::OpenSession {
        session_id: session_id.clone(),
    })
    .unwrap();
    core.dispatch(Action::SendMessage {
        session_id: session_id.clone(),
        text: "hi agent".into(),
    })
    .unwrap();
    let view = views.wait("the agent's reply", |view| {
        matches!(&view.screen, Screen::Session { view } if view.timeline.iter().any(|item| {
            matches!(item, TimelineItem::Logged { entry: Entry::AgentMessage { text }, .. }
                if text == REPLY)
        }))
    });
    let Screen::Session { view: session } = view.screen else {
        unreachable!()
    };
    assert_eq!(session.id, session_id);
    assert_eq!(session.prompts, ["hi agent"]);
    assert_eq!(
        core.view().unwrap().screen,
        Screen::Session { view: session }
    );

    // What is sent while suspended reaches the server after resume.
    core.suspend();
    core.dispatch(Action::SendMessage {
        session_id: session_id.clone(),
        text: "while away".into(),
    })
    .unwrap();
    core.resume();
    views.wait("the message sent while suspended, logged", |view| {
        view.queued == 0
            && matches!(&view.screen, Screen::Session { view } if view.timeline.iter().any(|item| {
                matches!(item, TimelineItem::Logged { entry: Entry::UserMessage { text, .. }, .. }
                    if text == "while away")
            }))
    });

    core.shutdown().unwrap();
    assert_eq!(core.view(), Err(SkywaveError::ShutDown));
}

/// A listener that panics (as a Kotlin exception does) on every snapshot
/// neither stops the others nor later snapshots.
#[test]
fn a_panicking_listener_does_not_wedge_the_core() {
    struct Panics;
    impl ViewListener for Panics {
        fn on_view(&self, _: ViewState) {
            panic!("the listener threw");
        }
    }
    let (_dir, core) = open_core();
    core.subscribe(Arc::new(Panics)).unwrap();
    let (listener, views) = Views::listener();
    core.subscribe(listener).unwrap();
    views.wait("the first snapshot", |_| true);
    // Nothing listens there: the engine reports the failure from its own
    // thread, after the dispatch's snapshot from this one.
    core.dispatch(Action::Pair {
        address: "127.0.0.1:1".into(),
        code: "ABCDE-12345".into(),
        fingerprint: ["AB"; 32].join(":"),
    })
    .unwrap();
    views.wait("the failed pairing", |view| {
        matches!(&view.screen, Screen::Pairing { view }
            if matches!(view.status, PairingStatus::Failed { .. }))
    });
    core.shutdown().unwrap();
}

#[test]
fn a_bad_id_is_a_typed_error_and_shutdown_is_final() {
    let (_dir, core) = open_core();
    assert_eq!(
        core.dispatch(Action::SendMessage {
            session_id: "42".into(),
            text: "hi".into(),
        }),
        Err(SkywaveError::InvalidId {
            name: "session_id".into(),
            input: "42".into()
        })
    );
    core.shutdown().unwrap();
    core.shutdown().unwrap();
    core.suspend();
    core.resume();
    core.unsubscribe(1).unwrap();
    assert_eq!(core.dispatch(Action::RetryNow), Err(SkywaveError::ShutDown));
    let (listener, _views) = Views::listener();
    assert_eq!(core.subscribe(listener), Err(SkywaveError::ShutDown));
    // The object outliving shutdown (Kotlin GC) is harmless.
    drop(core);
}

/// SQLite removes the write-ahead log when its last connection closes.
fn wal_exists(dir: &TempDir) -> bool {
    dir.path()
        .join(format!("{}-wal", skywave_core::store::DB_FILE))
        .exists()
}

fn wait_online(views: &Views) {
    views.wait("paired and online", |view| {
        is_session_list(view) && matches!(view.link, Link::Online { .. })
    });
}

/// A paired, online core shuts down completely: the store is closed when
/// `shutdown` returns, and the same dir opens and connects again at once.
#[test]
fn shutdown_closes_the_store_so_the_dir_reopens_at_once() {
    let daemon = Daemon::start(FakeAgent::new(FakeScript::new()));
    let (dir, mut core) = open_core();
    let (listener, views) = Views::listener();
    core.subscribe(listener).unwrap();
    let pair = parse_pair_uri(daemon.pair_uri()).unwrap();
    core.dispatch(Action::Pair {
        address: pair.address,
        code: pair.code,
        fingerprint: pair.fingerprint,
    })
    .unwrap();
    wait_online(&views);
    for _ in 0..3 {
        assert!(wal_exists(&dir));
        core.shutdown().unwrap();
        assert!(!wal_exists(&dir), "the old store is still open");
        core =
            SkywaveCore::open(dir.path().display().to_string(), Some("ffi test".into())).unwrap();
        let (listener, views) = Views::listener();
        core.subscribe(listener).unwrap();
        wait_online(&views);
    }
    core.shutdown().unwrap();
    assert!(!wal_exists(&dir));
}

/// `shutdown` from a listener on an engine thread fails instead of
/// deadlocking or stopping the core from underneath its own engine.
#[test]
fn shutdown_from_a_listener_on_an_engine_thread_is_reentrant() {
    struct ShutsDown {
        core: Mutex<Option<Arc<SkywaveCore>>>,
        results: Mutex<Sender<(ViewState, Result<(), SkywaveError>)>>,
    }
    impl ViewListener for ShutsDown {
        fn on_view(&self, view: ViewState) {
            let core = self.core.lock().unwrap().clone();
            if let Some(core) = core {
                let result = core.shutdown();
                let _ = self.results.lock().unwrap().send((view, result));
            }
        }
    }
    let (dir, core) = open_core();
    let (tx, results) = mpsc::channel();
    let listener = Arc::new(ShutsDown {
        core: Mutex::new(None),
        results: Mutex::new(tx),
    });
    core.subscribe(listener.clone()).unwrap();
    *listener.core.lock().unwrap() = Some(core.clone());
    // Nothing listens there: the engine reports the failure from its own
    // thread.
    core.dispatch(Action::Pair {
        address: "127.0.0.1:1".into(),
        code: "ABCDE-12345".into(),
        fingerprint: ["AB"; 32].join(":"),
    })
    .unwrap();
    let deadline = Instant::now() + SOON;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let (view, result) = results.recv_timeout(left).expect("no failed pairing");
        assert_eq!(
            result,
            Err(SkywaveError::Reentrant {
                call: "shutdown".into()
            })
        );
        if matches!(&view.screen, Screen::Pairing { view }
            if matches!(view.status, PairingStatus::Failed { .. }))
        {
            break;
        }
    }
    core.view().unwrap();
    // Break the listener's cycle back to the core, then stop it for real.
    listener.core.lock().unwrap().take();
    core.shutdown().unwrap();
    assert!(!wal_exists(&dir));
}
