//! The TUI end to end: a real `skywaved` (HTTPS API plus supervisor with the
//! scripted fake agent) in-process on 127.0.0.1 over real TLS, real cores
//! with their sync engines, and the TUI's `App` driven by key events, with
//! every check made on rendered frames (ratatui's `TestBackend`). Covers
//! flows F5–F9 and F11 (spec §3). The link runs through
//! `skywave_testkit::LinkProxy` where a test takes it down.
//!
//! Waits are bounded and driven by the core's snapshots; nothing relies on a
//! fixed sleep.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::json;
use skywave_core::{
    Backoff, Core, DeviceId, EngineConfig, Jitter, Link, Screen, Store, TransportConfig, ViewState,
};
use skywave_proto::{Event, EventKind, Seq, SessionState};
use skywave_testkit::{LinkProfile, LinkProxy, seed};
use skywave_tui::{App, ui};
use skywaved::agent::fake::{FakeAgent, FakeScript, Gate, Step};
use skywaved::identity::ServerIdentity;
use skywaved::pairing;
use skywaved::server::Server;
use skywaved::{EventLog, Storage, Supervisor, SupervisorConfig, SupervisorHandle};
use tempfile::TempDir;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

/// Upper bound for any wait; a passing test never gets near it.
const SOON: Duration = Duration::from_secs(20);

// ---- server -----------------------------------------------------------------

/// `skywaved` in-process: API server plus supervisor with the fake agent.
struct Daemon {
    tmp: TempDir,
    storage: Storage,
    identity: Arc<ServerIdentity>,
    log: EventLog,
    addr: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<skywaved::Result<()>>,
    _supervisor: SupervisorHandle,
}

impl Daemon {
    async fn start(script: FakeScript) -> Daemon {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("root/project")).unwrap();
        let data = tmp.path().join("server");
        let storage = Storage::open(&data).unwrap();
        let identity = Arc::new(
            ServerIdentity::bootstrap(&storage.dir, &["localhost".into(), "127.0.0.1".into()])
                .unwrap(),
        );
        let log = EventLog::open(&storage.db, vec![tmp.path().join("root")]).unwrap();
        let server = Server::bind(
            "127.0.0.1:0".parse().unwrap(),
            &storage.dir,
            identity.clone(),
            log.clone(),
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(server.serve(async {
            let _ = stopped.await;
        }));
        let supervisor = Supervisor::start(
            FakeAgent::new(script),
            &data,
            log.clone(),
            SupervisorConfig::default(),
        )
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

    fn workdir(&self) -> String {
        self.tmp.path().join("root/project").display().to_string()
    }

    /// The whole log, seq 1 first.
    fn events(&self) -> Vec<Event> {
        self.log
            .read(&self.storage.db, Seq::ZERO, 5000)
            .unwrap()
            .events
    }

    /// What `skywaved pair` prints: a fresh code and the CA fingerprint.
    fn pairing(&self) -> (String, String) {
        let code = pairing::issue_code(self.storage.db.conn(), Utc::now(), pairing::DEFAULT_TTL)
            .unwrap()
            .code;
        (code.to_string(), self.identity.ca_fingerprint().to_string())
    }

    /// Waits until the log satisfies `done`.
    async fn wait_log(&self, what: &str, done: impl Fn(&[Event]) -> bool) {
        let mut head = self.log.subscribe();
        tokio::time::timeout(SOON, async {
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
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.task.abort();
    }
}

// ---- the TUI ----------------------------------------------------------------

/// A core with its engine and the TUI's `App` over it, as `run` wires them,
/// minus the terminal.
struct Tui {
    dir: TempDir,
    core: Core,
    views: mpsc::UnboundedReceiver<ViewState>,
    app: App,
}

impl Tui {
    fn open() -> Tui {
        let dir = tempfile::tempdir().unwrap();
        let core = Core::open_with(dir.path(), config()).unwrap();
        let (tx, mut views) = mpsc::unbounded_channel();
        core.subscribe(move |view| {
            let _ = tx.send(view);
        });
        let app = App::new(views.try_recv().unwrap(), Instant::now());
        Tui {
            dir,
            core,
            views,
            app,
        }
    }

    fn device(&self) -> DeviceId {
        self.core.seam().identity().unwrap().device_id()
    }

    /// Takes every snapshot already pushed.
    fn pump(&mut self) {
        while let Ok(view) = self.views.try_recv() {
            self.app.set_view(view, Instant::now());
        }
    }

    /// A key press; its action goes to the core, as the event loop does.
    fn key_event(&mut self, key: KeyEvent) {
        self.pump();
        if let Some(action) = self.app.on_key(key) {
            let result = self.core.dispatch(action);
            self.app.dispatched(result);
        }
        self.pump();
    }

    fn key(&mut self, code: KeyCode) {
        self.key_event(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn type_text(&mut self, text: &str) {
        for c in text.chars() {
            self.key(KeyCode::Char(c));
        }
    }

    /// Types `text` into the composer and sends it.
    fn send(&mut self, text: &str) {
        self.type_text(text);
        self.key(KeyCode::Enter);
    }

    fn screen(&self) -> String {
        let mut terminal = Terminal::new(TestBackend::new(110, 32)).unwrap();
        terminal
            .draw(|frame| ui::render(frame, &self.app, Instant::now()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    /// Waits until the rendered frame satisfies `done`, re-rendering on
    /// every snapshot.
    async fn wait(&mut self, what: &str, done: impl Fn(&App, &str) -> bool) -> String {
        let deadline = tokio::time::Instant::now() + SOON;
        loop {
            self.pump();
            let screen = self.screen();
            if done(&self.app, &screen) {
                return screen;
            }
            match tokio::time::timeout_at(deadline, self.views.recv()).await {
                Ok(Some(view)) => self.app.set_view(view, Instant::now()),
                Ok(None) => panic!("core gone"),
                Err(_) => panic!("timed out waiting for {what}; screen:\n{screen}"),
            }
        }
    }

    async fn wait_for(&mut self, needle: &str) -> String {
        self.wait(needle, |_, screen| screen.contains(needle)).await
    }

    /// F2 through the pairing form, connecting through `address`.
    async fn pair(&mut self, daemon: &Daemon, address: &str) {
        let (code, fingerprint) = daemon.pairing();
        self.type_text(address);
        self.key(KeyCode::Tab);
        self.type_text(&code);
        self.key(KeyCode::Tab);
        self.type_text(&fingerprint);
        self.key(KeyCode::Enter);
        self.wait("paired and online", |app, _| {
            matches!(app.view().screen, Screen::SessionList(_))
                && matches!(app.view().link, Link::Online { .. })
        })
        .await;
    }

    /// F4 through the new-session prompt, then opens the session.
    async fn create_session(&mut self, daemon: &Daemon) {
        self.key(KeyCode::Char('n'));
        self.type_text(&daemon.workdir());
        self.key(KeyCode::Enter);
        self.open_session().await;
    }

    /// Opens the first session once it is in the list.
    async fn open_session(&mut self) {
        self.wait_for("[idle]").await;
        self.key(KeyCode::Enter);
        self.wait_for("agent idle").await;
    }
}

/// Fast retries, short long-polls, small batches.
fn config() -> EngineConfig {
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
        device_name: Some("tui test".into()),
        ..EngineConfig::default()
    }
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

/// The last 8 hex digits of a device id, as the timeline names devices.
fn tail(device: DeviceId) -> String {
    device.0.simple().to_string()[24..].to_owned()
}

/// A tool output well over the 4 KiB blob threshold: 400 numbered lines.
fn listing() -> String {
    (0..400).map(|i| format!("file-{i:03}.txt\n")).collect()
}

fn lines_with(screen: &str, needle: &str) -> usize {
    screen.lines().filter(|l| l.contains(needle)).count()
}

fn line_with<'a>(screen: &'a str, needle: &str) -> &'a str {
    screen
        .lines()
        .find(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("{needle:?} not on screen:\n{screen}"))
}

/// The agent said `text` and the turn is over.
fn turn_done(events: &[Event], text: &str) -> bool {
    let said = events
        .iter()
        .any(|e| matches!(&e.kind, EventKind::AgentMessage { text: t } if t == text));
    let idle = events
        .last()
        .is_some_and(|e| matches!(e.kind, EventKind::SessionStateChanged(SessionState::Idle)));
    said && idle
}

// ---- flows ------------------------------------------------------------------

/// F5: a message goes out; agent text and a tool call arrive; the tool's
/// large output shows as a summary with its blob not downloaded (OnDemand).
#[tokio::test(flavor = "multi_thread")]
async fn f5_converse() {
    let daemon = Daemon::start(FakeScript::new().turn([
        Step::text("Listing files"),
        Step::tool_call("shell", json!({ "command": "ls" })),
        Step::tool_result(listing()),
    ]))
    .await;
    let mut tui = Tui::open();
    tui.pair(&daemon, &daemon.addr.to_string()).await;
    tui.create_session(&daemon).await;

    tui.send("hello agent");
    let screen = tui
        .wait("the turn", |_, s| {
            s.contains("result: file-000.txt") && s.contains("agent idle")
        })
        .await;
    assert!(screen.contains("you [accepted]: hello agent"), "{screen}");
    assert!(screen.contains("agent: Listing files"), "{screen}");
    assert!(screen.contains("tool shell: ls"), "{screen}");
    let result = line_with(&screen, "result: file-000.txt");
    assert!(
        result.contains("[full 5.1 KiB · not downloaded]"),
        "{result}"
    );
    assert!(
        !screen.contains("file-001.txt"),
        "only the summary: {screen}"
    );
}

/// F6: approve and deny from the TUI; an answer from a second device shows
/// there as "on device …" with the controls gone, on both devices.
#[tokio::test(flavor = "multi_thread")]
async fn f6_approvals() {
    let approval = |request: &str| {
        Step::approval(request, "write", json!({ "path": "a.txt" }))
            .on_approve([Step::text("written")])
            .on_deny([Step::text("not writing")])
    };
    let daemon = Daemon::start(
        FakeScript::new()
            .turn([approval("req-1")])
            .turn([approval("req-2")])
            .turn([approval("req-3")]),
    )
    .await;
    let address = daemon.addr.to_string();
    let mut a = Tui::open();
    a.pair(&daemon, &address).await;
    let mut b = Tui::open();
    b.pair(&daemon, &address).await;
    a.create_session(&daemon).await;
    b.open_session().await;

    // Approve here; the other device sees who answered.
    a.send("one");
    a.wait_for("The agent wants to use write").await;
    b.wait_for("The agent wants to use write").await;
    a.key(KeyCode::Esc);
    a.key(KeyCode::Char('a'));
    let screen = a
        .wait("approved", |_, s| {
            s.contains("approval write [approved here]") && s.contains("agent: written")
        })
        .await;
    assert!(!screen.contains("Approval needed"), "{screen}");
    let on_a = format!("approval write [approved on device …{}]", tail(a.device()));
    let screen = b.wait_for(&on_a).await;
    assert!(!screen.contains("a approve"), "{screen}");

    // Deny here.
    a.key(KeyCode::Char('i'));
    a.send("two");
    a.wait("the second prompt", |_, s| {
        s.contains("Approval needed") && s.contains("Esc, then a approve")
    })
    .await;
    a.key(KeyCode::Esc);
    a.key(KeyCode::Char('x'));
    a.wait("denied", |_, s| {
        s.contains("approval write [denied here]") && s.contains("agent: not writing")
    })
    .await;

    // Answered on the other device: controls go, "on device …" shows.
    a.key(KeyCode::Char('i'));
    a.send("three");
    b.wait_for("Approval needed").await;
    a.wait_for("Approval needed").await;
    b.key(KeyCode::Esc);
    b.key(KeyCode::Char('a'));
    let on_b = format!("approval write [approved on device …{}]", tail(b.device()));
    let screen = a
        .wait("resolved elsewhere", |_, s| {
            s.contains(&on_b)
                && !s.contains("Approval needed")
                && lines_with(s, "agent: written") == 2
        })
        .await;
    assert!(!screen.contains("a approve"), "{screen}");
    assert_eq!(lines_with(&screen, "agent: written"), 2, "{screen}");
}

/// F7: Ctrl-X stops a hanging turn; the timeline says so and the session is
/// idle again.
#[tokio::test(flavor = "multi_thread")]
async fn f7_interrupt() {
    let daemon =
        Daemon::start(FakeScript::new().turn([Step::text("working on it"), Step::Hang])).await;
    let mut tui = Tui::open();
    tui.pair(&daemon, &daemon.addr.to_string()).await;
    tui.create_session(&daemon).await;

    tui.send("go");
    tui.wait("the agent at work", |_, s| {
        s.contains("agent: working on it") && s.contains("agent working")
    })
    .await;
    tui.key_event(ctrl('x'));
    let screen = tui
        .wait("the interrupt", |_, s| {
            s.contains("interrupted · the agent stopped its turn") && s.contains("agent idle")
        })
        .await;
    assert!(!screen.contains("interrupt failed"), "{screen}");
}

/// F8: a message composed while the server is unreachable shows `queued`
/// right away and `accepted` after the reconnect; the log has one copy.
#[tokio::test(flavor = "multi_thread")]
async fn f8_offline_compose() {
    let daemon = Daemon::start(FakeScript::new()).await;
    let proxy = LinkProxy::start(daemon.addr, LinkProfile::clean(), seed()).await;
    let mut tui = Tui::open();
    tui.pair(&daemon, &proxy.address()).await;
    tui.create_session(&daemon).await;

    proxy.down();
    tui.wait("offline", |app, _| {
        matches!(app.view().link, Link::Reconnecting { .. })
    })
    .await;
    tui.send("sent while offline");
    let screen = tui.screen();
    assert!(
        screen.contains("you [queued]: sent while offline"),
        "{screen}"
    );
    // The banner counts it (and possibly the session's creation, whose
    // result the cut may have swallowed; it is resent).
    assert!(
        screen.lines().next().unwrap().contains(" queued"),
        "banner: {screen}"
    );
    assert!(
        screen.contains("offline, sends when back online"),
        "{screen}"
    );

    proxy.up();
    let screen = tui
        .wait("delivered and answered", |_, s| {
            s.contains("you [accepted]: sent while offline")
                && s.contains("agent: echo: sent while offline")
        })
        .await;
    assert_eq!(lines_with(&screen, "you ["), 1, "one copy: {screen}");
    let copies = daemon
        .events()
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::UserMessage { text, .. } if text == "sent while offline"))
        .count();
    assert_eq!(copies, 1, "exactly one copy in the log");
}

/// F9: the link dies while the agent works; after reconnecting the
/// timeline catches up from the cursor, complete, nothing doubled.
#[tokio::test(flavor = "multi_thread")]
async fn f9_drop_mid_stream() {
    let gate = Gate::new();
    let daemon = Daemon::start(FakeScript::new().turn([
        Step::text("part one"),
        Step::Wait(gate.clone()),
        Step::text("part two"),
        Step::tool_call("shell", json!({ "command": "ls" })),
        Step::tool_result(listing()),
        Step::text("part three"),
    ]))
    .await;
    let proxy = LinkProxy::start(daemon.addr, LinkProfile::clean(), seed()).await;
    let mut tui = Tui::open();
    tui.pair(&daemon, &proxy.address()).await;
    tui.create_session(&daemon).await;

    tui.send("go");
    tui.wait_for("agent: part one").await;
    proxy.down();
    tui.wait("offline", |app, _| {
        matches!(app.view().link, Link::Reconnecting { .. })
    })
    .await;
    gate.open();
    daemon
        .wait_log("the turn's end", |events| turn_done(events, "part three"))
        .await;
    assert!(
        !tui.screen().contains("part two"),
        "nothing arrives offline"
    );

    proxy.up();
    let screen = tui
        .wait("caught up", |app, s| {
            s.contains("agent: part three")
                && s.contains("agent idle")
                && matches!(app.view().link, Link::Online { .. })
        })
        .await;
    for needle in [
        "you [accepted]: go",
        "agent: part one",
        "agent: part two",
        "tool shell",
        "result: file-000.txt",
        "agent: part three",
    ] {
        assert_eq!(lines_with(&screen, needle), 1, "{needle:?} once: {screen}");
    }

    // The replica holds the server's log: no gaps, no duplicates.
    let server = daemon.events();
    let store = Store::open(tui.dir.path()).unwrap();
    let mut local: Vec<(Seq, Event)> = store
        .sessions()
        .unwrap()
        .into_iter()
        .flat_map(|s| store.session_events(s.id).unwrap())
        .collect();
    local.sort_by_key(|(seq, _)| *seq);
    let seqs: Vec<u64> = local.iter().map(|(seq, _)| seq.0).collect();
    assert_eq!(seqs, (1..=server.len() as u64).collect::<Vec<_>>());
    for ((_, mine), theirs) in local.iter().zip(&server) {
        assert_eq!(mine, theirs);
    }
}

/// F11: with the default OnDemand policy a tool output's blob stays
/// remote until the user expands it; then it downloads and renders.
#[tokio::test(flavor = "multi_thread")]
async fn f11_blob_on_demand() {
    let daemon = Daemon::start(FakeScript::new().turn([Step::tool_result(listing())])).await;
    let mut tui = Tui::open();
    tui.pair(&daemon, &daemon.addr.to_string()).await;
    tui.key(KeyCode::Char('s'));
    tui.wait_for("(•) On demand").await;
    tui.key(KeyCode::Esc);
    tui.create_session(&daemon).await;

    tui.send("list");
    let screen = tui
        .wait("the result", |_, s| {
            s.contains("result: file-000.txt") && s.contains("agent idle")
        })
        .await;
    assert!(
        line_with(&screen, "result: file-000.txt").contains("· not downloaded]"),
        "{screen}"
    );

    tui.key(KeyCode::Esc);
    tui.key(KeyCode::Up);
    tui.key(KeyCode::Enter);
    // Following the bottom: the end of the content.
    tui.wait("the content", |_, s| {
        s.contains("file-399.txt") && s.contains("file-398.txt")
    })
    .await;
    tui.key(KeyCode::Home);
    let screen = tui.screen();
    let head = line_with(&screen, "result: file-000.txt");
    assert!(
        head.contains("▾") && head.contains("[downloaded]"),
        "{screen}"
    );
    assert!(screen.contains("          file-001.txt"), "{screen}");
    assert!(!screen.contains("cut short"), "{screen}");
}
