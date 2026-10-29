//! The TUI against a real core, without a terminal or a network: keys go in
//! through `App::on_key`, actions go to `Core::dispatch`, engine reports go
//! in through `core.seam()`, and frames are checked on ratatui's
//! `TestBackend`.

use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use chrono::{DateTime, TimeZone, Utc};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use skywave_core::{
    Action, ApprovalId, BlobId, BlobPolicy, BlobStatus, CommandId, Core, Decision, DeviceId,
    FailReason, Link, LinkRejection, PairedCredentials, Request, Route, Screen, Seam, Seq,
    SessionId, ViewState,
};
use skywave_proto::{
    BlobRef, CommandResult, Event, EventBatch, EventKind, SessionState as LogState,
};
use skywave_tui::App;
use skywave_tui::app::PairUri;
use skywave_tui::ui;

const FP: &str = "ab:cd:ef";

/// A core over a temp dir, the TUI over it, and the engine's side.
struct Harness {
    _dir: tempfile::TempDir,
    core: Core,
    seam: Seam,
    requests: Receiver<Request>,
    views: Receiver<ViewState>,
    app: App,
    t0: Instant,
}

impl Harness {
    fn new() -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let core = Core::open_detached(dir.path()).unwrap();
        let seam = core.seam();
        let requests = seam.take_requests().unwrap();
        let (tx, views) = mpsc::channel();
        core.subscribe(move |view| tx.send(view).unwrap());
        let t0 = Instant::now();
        let app = App::new(views.recv().unwrap(), t0);
        Harness {
            _dir: dir,
            core,
            seam,
            requests,
            views,
            app,
            t0,
        }
    }

    fn paired() -> Harness {
        let mut h = Harness::new();
        h.type_pairing("srv:7443", "ABCDE12345", FP);
        h.seam.pairing_succeeded("srv:7443", credentials()).unwrap();
        h.sync();
        h.drain_requests();
        h
    }

    /// Takes the snapshots the core pushed, as the event loop does.
    fn sync_at(&mut self, now: Instant) {
        loop {
            match self.views.try_recv() {
                Ok(view) => self.app.set_view(view, now),
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => panic!("core gone"),
            }
        }
    }

    fn sync(&mut self) {
        self.sync_at(self.t0);
    }

    /// A key press; any resulting action goes to the core.
    fn key(&mut self, code: KeyCode) -> Option<Action> {
        self.key_with(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn key_with(&mut self, key: KeyEvent) -> Option<Action> {
        let action = self.app.on_key(key);
        if let Some(action) = &action {
            let result = self.core.dispatch(action.clone());
            self.app.dispatched(result);
        }
        self.sync();
        action
    }

    fn type_text(&mut self, text: &str) {
        for c in text.chars() {
            assert_eq!(self.key(KeyCode::Char(c)), None);
        }
    }

    fn type_pairing(&mut self, address: &str, code: &str, fingerprint: &str) -> Option<Action> {
        self.type_text(address);
        self.key(KeyCode::Tab);
        self.type_text(code);
        self.key(KeyCode::Tab);
        self.type_text(fingerprint);
        self.key(KeyCode::Enter)
    }

    fn drain_requests(&self) -> Vec<Request> {
        self.requests.try_iter().collect()
    }

    fn screen_at(&self, now: Instant) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal
            .draw(|frame| ui::render(frame, &self.app, now))
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

    fn screen(&self) -> String {
        self.screen_at(self.t0)
    }

    fn banner_at(&self, now: Instant) -> String {
        self.screen_at(now)
            .lines()
            .next()
            .unwrap()
            .trim_end()
            .to_owned()
    }

    fn banner(&self) -> String {
        self.banner_at(self.t0)
    }
}

fn credentials() -> PairedCredentials {
    PairedCredentials {
        device_id: DeviceId::new(),
        device_key_pem: "KEY".into(),
        device_cert_pem: "CERT".into(),
        ca_cert_pem: "CA".into(),
    }
}

fn assert_contains(screen: &str, needle: &str) {
    assert!(
        screen.contains(needle),
        "{needle:?} not on screen:\n{screen}"
    );
}

/// The one screen line containing `needle`.
fn line_with(screen: &str, needle: &str) -> String {
    let lines: Vec<&str> = screen.lines().filter(|l| l.contains(needle)).collect();
    assert_eq!(
        lines.len(),
        1,
        "{needle:?} on {} lines:\n{screen}",
        lines.len()
    );
    lines[0].to_owned()
}

fn at(minute: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 29, 12, minute, 0).unwrap()
}

/// Plays the sync engine: hands the core the next pulled batch, as if the
/// server's log grew, e.g. because another device acted.
fn pull(h: &mut Harness, events: Vec<Event>) {
    let from = Seq(h.seam.cursor().unwrap().0 + 1);
    h.seam.apply_events(&EventBatch { from, events }).unwrap();
    h.sync();
}

fn event(session_id: SessionId, minute: u32, kind: EventKind) -> Event {
    Event {
        session_id,
        at: at(minute),
        kind,
    }
}

fn created(session_id: SessionId, minute: u32, workdir: &str) -> Event {
    event(
        session_id,
        minute,
        EventKind::SessionCreated {
            workdir: workdir.into(),
        },
    )
}

fn selected_workdir(h: &Harness) -> Option<String> {
    let Screen::SessionList(list) = &h.app.view().screen else {
        panic!("not on the session list");
    };
    let i = h.app.selected_index(list)?;
    list.sessions[i].workdir.clone()
}

/// The id of the newest command in the outbox.
fn last_command(h: &Harness) -> CommandId {
    h.seam.pending_outbox().unwrap().last().unwrap().command.id
}

#[test]
fn pairing_form_dispatches_pair_then_shows_progress_and_success() {
    let mut h = Harness::new();
    let screen = h.screen();
    assert_contains(&screen, "Pair this device");
    assert_contains(&screen, "Address:");
    assert_contains(&screen, "Code:");
    assert_contains(&screen, "Fingerprint:");

    // `q` and `?` are text in the form, not commands.
    h.type_text("q?");
    h.key(KeyCode::Backspace);
    h.key(KeyCode::Backspace);
    assert!(!h.app.ui().quit);
    assert!(!h.app.ui().help);

    let action = h.type_pairing("srv:7443", "ABCDE12345", FP);
    let expected = Action::Pair {
        address: "srv:7443".into(),
        code: "ABCDE12345".into(),
        fingerprint: FP.into(),
    };
    assert_eq!(action, Some(expected));
    assert_eq!(
        h.drain_requests(),
        vec![Request::Pair {
            address: "srv:7443".into(),
            code: "ABCDE12345".into(),
            fingerprint: FP.into(),
        }]
    );
    assert_contains(&h.screen(), "Pairing with srv:7443…");

    h.seam.pairing_succeeded("srv:7443", credentials()).unwrap();
    h.sync();
    assert!(matches!(h.app.view().screen, Screen::SessionList(_)));
    let screen = h.screen();
    assert_contains(&screen, "Sessions");
    assert_contains(&screen, "No sessions yet.");
    assert_eq!(h.banner(), "connecting…");
}

#[test]
fn enter_moves_through_the_form_and_submits_on_the_last_field() {
    let mut h = Harness::new();
    h.type_text("srv:7443");
    assert_eq!(h.key(KeyCode::Enter), None, "Enter on the address advances");
    h.type_text("ABCDE12345");
    assert_eq!(h.key(KeyCode::Enter), None, "Enter on the code advances");
    h.type_text(FP);
    assert!(matches!(h.key(KeyCode::Enter), Some(Action::Pair { .. })));

    // Shift-Tab goes back; Ctrl-U clears.
    h.key(KeyCode::BackTab);
    h.key_with(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    assert_eq!(h.app.ui().form.code, "");
    assert_eq!(h.app.ui().form.address, "srv:7443");
}

#[test]
fn pairing_failure_shows_the_reason_and_keeps_the_form() {
    let mut h = Harness::new();
    h.type_pairing("srv:7443", "WRONG00000", FP);
    h.seam.pairing_failed("code expired or already used");
    h.sync();
    assert_contains(
        &h.screen(),
        "Pairing with srv:7443 failed: code expired or already used",
    );
    assert_eq!(h.app.ui().form.code, "WRONG00000");

    // An incomplete form is refused by the core, not the TUI.
    let mut h = Harness::new();
    let action = h.type_pairing("srv:7443", "", FP);
    assert!(matches!(action, Some(Action::Pair { .. })));
    assert!(h.drain_requests().is_empty());
    assert_contains(&h.screen(), "Pairing with srv:7443 failed:");
}

#[test]
fn a_pasted_pairing_uri_fills_the_form() {
    let uri = format!(
        "skywave://pair?addr=%5Bfd00::7%5D:7443&code=ABCDE12345&fp={}",
        "0123456789abcdef".repeat(4)
    );
    let parsed = PairUri::parse(&uri).unwrap();
    assert_eq!(parsed.address, "[fd00::7]:7443");
    assert_eq!(parsed.code, "ABCDE12345");
    assert!(parsed.fingerprint.starts_with("01:23:45:67:89:ab:cd:ef:01"));
    assert_eq!(parsed.fingerprint.len(), 64 + 31);
    assert_eq!(PairUri::parse("skywave://pair?addr=x&code=y"), None);
    assert_eq!(PairUri::parse("srv:7443"), None);

    let mut h = Harness::new();
    h.app.on_paste(&uri);
    let form = &h.app.ui().form;
    assert_eq!(form.address, "[fd00::7]:7443");
    assert_eq!(form.fingerprint, parsed.fingerprint);
    assert!(matches!(h.key(KeyCode::Enter), Some(Action::Pair { .. })));

    // Without bracketed paste the URI arrives as keys in one field.
    let mut h = Harness::new();
    h.type_text(&uri);
    assert_eq!(h.key(KeyCode::Enter), None);
    assert_eq!(h.app.ui().form.address, "[fd00::7]:7443");
    assert_eq!(h.app.ui().form.code, "ABCDE12345");
}

#[test]
fn banner_shows_every_link_state() {
    let mut h = Harness::paired();
    let cases = [
        (Link::Connecting, "connecting…"),
        (Link::Online { latency_ms: 42 }, "online · 42 ms"),
        (
            Link::CatchingUp {
                have: Seq(120),
                server: Seq(450),
            },
            "syncing · 120/450",
        ),
        (
            Link::Rejected(LinkRejection::Revoked),
            "device revoked · re-pair to reconnect",
        ),
        (
            Link::Rejected(LinkRejection::ProtocolMismatch {
                client: 1,
                server: 2,
            }),
            "protocol mismatch · client v1, server v2 · update to reconnect",
        ),
    ];
    for (link, text) in cases {
        h.seam.set_link(link);
        h.sync();
        assert_eq!(h.banner(), text);
    }
}

#[test]
fn reconnecting_banner_counts_queued_commands_and_down_to_the_retry() {
    let mut h = Harness::paired();
    h.core
        .dispatch(Action::CreateSession {
            workdir: "/srv/a".into(),
        })
        .unwrap();
    h.core
        .dispatch(Action::CreateSession {
            workdir: "/srv/b".into(),
        })
        .unwrap();
    // The killed server: the engine reports it is backing off.
    h.seam.set_link(Link::Reconnecting {
        attempt: 3,
        next_in: Duration::from_secs(5),
        offline_since: h.t0,
    });
    h.sync();
    let t0 = h.t0;
    assert_eq!(
        h.banner_at(t0),
        "offline · 2 queued · retry in 5s (attempt 3)"
    );
    assert_eq!(
        h.banner_at(t0 + Duration::from_millis(2500)),
        "offline · 2 queued · retry in 3s (attempt 3)"
    );
    assert_eq!(
        h.banner_at(t0 + Duration::from_secs(6)),
        "offline · 2 queued · retrying… (attempt 3)"
    );
    // Local data still renders under the banner.
    assert_contains(&line_with(&h.screen(), "/srv/a"), "[queued]");

    // A new attempt restarts the countdown from when it arrived.
    let later = t0 + Duration::from_secs(10);
    h.seam.set_link(Link::Reconnecting {
        attempt: 4,
        next_in: Duration::from_secs(10),
        offline_since: t0,
    });
    h.sync_at(later);
    assert_eq!(
        h.banner_at(later + Duration::from_secs(1)),
        "offline · 2 queued · retry in 9s (attempt 4)"
    );

    // Back online, the queue drains; the count stays until results arrive.
    h.seam.set_link(Link::Online { latency_ms: 80 });
    h.sync();
    assert_eq!(h.banner(), "online · 80 ms · 2 queued");
}

#[test]
fn revoked_device_gets_a_full_screen_state_with_re_pair() {
    let mut h = Harness::paired();
    h.seam.set_link(Link::Rejected(LinkRejection::Revoked));
    h.sync();
    let screen = h.screen();
    assert_contains(&screen, "This device was revoked.");
    assert_contains(&screen, "Local data stays readable.");

    // Esc: browse the local replica, the banner keeps saying why.
    assert_eq!(h.key(KeyCode::Esc), None);
    assert_contains(&h.screen(), "No sessions yet.");
    assert_eq!(h.banner(), "device revoked · re-pair to reconnect");

    // Settings → p re-pairs; the form starts from the known address.
    assert_eq!(
        h.key(KeyCode::Char('s')),
        Some(Action::Navigate(Route::Settings))
    );
    assert_eq!(
        h.key(KeyCode::Char('p')),
        Some(Action::Navigate(Route::Pairing))
    );
    assert!(matches!(h.app.view().screen, Screen::Pairing(_)));
    let screen = h.screen();
    assert_contains(&screen, "Currently paired as");
    assert_eq!(h.app.ui().form.address, "srv:7443");
    // Esc cancels a re-pair.
    assert_eq!(
        h.key(KeyCode::Esc),
        Some(Action::Navigate(Route::SessionList))
    );

    // From the full-screen state, p goes straight to pairing.
    h.seam.set_link(Link::Connecting);
    h.sync();
    h.seam.set_link(Link::Rejected(LinkRejection::Revoked));
    h.sync();
    assert_contains(&h.screen(), "This device was revoked.");
    assert_eq!(
        h.key(KeyCode::Char('p')),
        Some(Action::Navigate(Route::Pairing))
    );
    assert!(matches!(h.app.view().screen, Screen::Pairing(_)));

    // Re-pairing succeeds: back to the sessions, connecting again. The
    // address is prefilled and the code field focused.
    assert_eq!(h.app.ui().form.code, "", "the old code is gone");
    h.type_text("NEWCODE123");
    h.key(KeyCode::Tab);
    h.type_text(FP);
    assert!(matches!(h.key(KeyCode::Enter), Some(Action::Pair { .. })));
    h.seam.pairing_succeeded("srv:7443", credentials()).unwrap();
    h.sync();
    assert!(matches!(h.app.view().screen, Screen::SessionList(_)));
    assert_eq!(h.banner(), "connecting…");
}

#[test]
fn help_overlay_lists_keys_and_closes() {
    let mut h = Harness::new();
    // On the pairing screen `?` is text, F1 opens help.
    h.key(KeyCode::F(1));
    let screen = h.screen();
    assert_contains(&screen, "Keys");
    assert_contains(&screen, "previous field");
    h.key(KeyCode::Esc);
    assert!(!h.app.ui().help);

    let mut h = Harness::paired();
    h.key(KeyCode::Char('?'));
    let screen = h.screen();
    assert_contains(&screen, "open session");
    assert_contains(&screen, "new session in a server directory");
    assert_contains(&screen, "agent blocked on an approval");
    assert_contains(&screen, "Ctrl-C");
    // Keys do nothing else while help is open.
    assert_eq!(h.key(KeyCode::Char('s')), None);
    h.key(KeyCode::Char('?'));
    assert!(!h.app.ui().help);
    assert!(!h.screen().contains("open session"));
}

#[test]
fn settings_toggle_the_blob_policy_through_the_core() {
    let mut h = Harness::paired();
    h.key(KeyCode::Char('s'));
    let screen = h.screen();
    assert_contains(&screen, "Blob policy: on demand");
    assert_contains(&screen, "Server:      srv:7443");
    assert_eq!(
        h.key(KeyCode::Char('b')),
        Some(Action::SetBlobPolicy(BlobPolicy::Eager))
    );
    assert_eq!(
        h.drain_requests(),
        vec![Request::BlobPolicyChanged(BlobPolicy::Eager)]
    );
    assert_contains(&h.screen(), "Blob policy: eager");
    assert_eq!(
        h.key(KeyCode::Esc),
        Some(Action::Navigate(Route::SessionList))
    );
}

#[test]
fn quit_keys() {
    let mut h = Harness::paired();
    h.key(KeyCode::Char('q'));
    assert!(h.app.ui().quit);

    let mut h = Harness::new();
    h.key_with(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(h.app.ui().quit, "Ctrl-C quits even while typing");
}

#[test]
fn sessions_created_elsewhere_appear_live_and_keep_the_selection() {
    let mut h = Harness::paired();
    assert_contains(&h.screen(), "No sessions yet. Press n to start one.");

    // Another device created a session: the pulled batch alone, without a
    // key press, puts the row on screen.
    let a = SessionId::new();
    pull(&mut h, vec![created(a, 0, "/srv/a")]);
    let row = line_with(&h.screen(), "/srv/a");
    assert_contains(&row, "> [idle]");
    assert_contains(&row, "2026-10-29 12:00 UTC");
    assert_eq!(selected_workdir(&h).as_deref(), Some("/srv/a"));

    // A newer session sorts above it; the selection stays on /srv/a.
    let b = SessionId::new();
    pull(&mut h, vec![created(b, 5, "/srv/b")]);
    let screen = h.screen();
    assert_contains(&screen, "Sessions (2)");
    assert!(screen.find("/srv/b").unwrap() < screen.find("/srv/a").unwrap());
    assert_eq!(selected_workdir(&h).as_deref(), Some("/srv/a"));
    assert!(!line_with(&screen, "/srv/b").contains("> "));

    // Activity reorders the rows; the selection follows the session.
    h.key(KeyCode::Up);
    assert_eq!(selected_workdir(&h).as_deref(), Some("/srv/b"));
    pull(
        &mut h,
        vec![event(
            a,
            9,
            EventKind::SessionStateChanged(LogState::AgentWorking),
        )],
    );
    let screen = h.screen();
    assert!(screen.find("/srv/a").unwrap() < screen.find("/srv/b").unwrap());
    assert_eq!(selected_workdir(&h).as_deref(), Some("/srv/b"));
    assert_eq!(h.key(KeyCode::Enter), Some(Action::OpenSession(b)));

    // Back on the list (Esc leaves the composer, then the session), the
    // same session is still selected.
    assert_eq!(h.key(KeyCode::Esc), None);
    assert_eq!(
        h.key(KeyCode::Esc),
        Some(Action::Navigate(Route::SessionList))
    );
    assert_eq!(selected_workdir(&h).as_deref(), Some("/srv/b"));
    h.key(KeyCode::Char('j'));
    assert_eq!(selected_workdir(&h).as_deref(), Some("/srv/b"), "last row");
    h.key(KeyCode::Char('k'));
    assert_eq!(selected_workdir(&h).as_deref(), Some("/srv/a"));
}

#[test]
fn every_session_state_has_its_own_badge() {
    let mut h = Harness::paired();
    let [idle, working, approval, errored] = std::array::from_fn(|_| SessionId::new());
    pull(
        &mut h,
        vec![
            created(idle, 0, "/srv/idle"),
            created(working, 1, "/srv/working"),
            created(approval, 2, "/srv/approval"),
            created(errored, 3, "/srv/errored"),
            event(
                working,
                4,
                EventKind::SessionStateChanged(LogState::AgentWorking),
            ),
            event(
                approval,
                5,
                EventKind::ApprovalRequested {
                    id: ApprovalId::new(),
                    tool: "Bash".into(),
                    summary: "rm -rf target".into(),
                    input: None,
                },
            ),
            event(
                errored,
                6,
                EventKind::SessionStateChanged(LogState::Errored {
                    message: "agent exited with status 1".into(),
                }),
            ),
        ],
    );
    let screen = h.screen();
    assert_contains(&line_with(&screen, "/srv/idle"), "[idle]");
    assert_contains(&line_with(&screen, "/srv/working"), "[working]");
    let row = line_with(&screen, "/srv/approval");
    assert_contains(&row, "[approval]");
    assert_contains(&row, "! 1 approval pending");
    let row = line_with(&screen, "/srv/errored");
    assert_contains(&row, "[error]");
    assert_contains(&row, "agent exited with status 1");
    assert_contains(&row, "2026-10-29 12:06 UTC");
    // Badges are padded so the workdirs line up.
    let column = |dir: &str| line_with(&screen, dir).find(dir).unwrap();
    assert_eq!(column("/srv/idle"), column("/srv/approval"));
}

#[test]
fn create_session_goes_from_queued_to_the_real_row() {
    let mut h = Harness::paired();
    assert_eq!(h.key(KeyCode::Char('n')), None);
    assert!(h.app.typing());
    let screen = h.screen();
    assert_contains(&screen, "New session · workdir on the server");
    assert_contains(&screen, "Enter create · Esc cancel");

    // Letters are text in the prompt, not commands.
    h.type_text("/srv/quiet");
    assert!(!h.app.ui().quit);
    for _ in 0.."quiet".len() {
        h.key(KeyCode::Backspace);
    }
    h.type_text("new");
    assert_contains(&h.screen(), "> /srv/new");
    assert_eq!(
        h.key(KeyCode::Enter),
        Some(Action::CreateSession {
            workdir: "/srv/new".into()
        })
    );
    assert_eq!(h.drain_requests(), vec![Request::OutboxChanged]);
    assert!(h.app.ui().create.is_none(), "the prompt closes");
    assert!(!h.app.typing());
    let command = last_command(&h);

    assert_contains(&line_with(&h.screen(), "/srv/new"), "[queued]");
    assert_contains(&h.screen(), "Creating");

    h.seam.command_sending(command).unwrap();
    h.sync();
    assert_contains(&line_with(&h.screen(), "/srv/new"), "[sending]");

    h.seam
        .command_result(command, &CommandResult::Accepted { seq: Seq(1) })
        .unwrap();
    h.sync();
    let row = line_with(&h.screen(), "/srv/new");
    assert_contains(&row, "[accepted]");
    assert_contains(&row, "created, syncing…");

    // The session's event arrives: the pending row becomes the real one.
    let id = SessionId::new();
    pull(&mut h, vec![created(id, 0, "/srv/new")]);
    let screen = h.screen();
    assert!(!screen.contains("Creating"));
    assert_contains(&line_with(&screen, "/srv/new"), "> [idle]");
    assert_eq!(h.key(KeyCode::Enter), Some(Action::OpenSession(id)));
}

#[test]
fn refused_creations_say_why() {
    let mut h = Harness::paired();
    let cases = [
        (
            "/etc",
            FailReason::WorkdirNotAllowed,
            "not under a configured root on the server",
        ),
        (
            "/srv/missing",
            FailReason::WorkdirNotFound,
            "no such directory on the server",
        ),
    ];
    for (workdir, reason, words) in cases {
        h.key(KeyCode::Char('n'));
        h.app.on_paste(&format!(" {workdir}\n"));
        assert!(matches!(
            h.key(KeyCode::Enter),
            Some(Action::CreateSession { .. })
        ));
        let command = last_command(&h);
        h.seam.command_sending(command).unwrap();
        h.seam
            .command_result(command, &CommandResult::Rejected { reason })
            .unwrap();
        h.sync();
        let row = line_with(&h.screen(), workdir);
        assert_contains(&row, "[failed]");
        assert_contains(&row, words);
    }
    assert_contains(&h.screen(), "No sessions yet.");
}

#[test]
fn esc_cancels_the_new_session_prompt() {
    let mut h = Harness::paired();
    h.key(KeyCode::Char('n'));
    h.type_text("/srv/a");
    h.key_with(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    assert_eq!(h.app.ui().create.as_ref().unwrap().workdir, "");
    h.type_text("/srv/b");
    // `?` is text here; F1 still opens help, which lists the prompt's keys.
    h.key(KeyCode::Char('?'));
    assert!(!h.app.ui().help);
    h.key(KeyCode::Backspace);
    h.key(KeyCode::F(1));
    assert_contains(&h.screen(), "create the session");
    h.key(KeyCode::Esc);
    assert!(!h.app.ui().help);
    assert_eq!(h.app.ui().create.as_ref().unwrap().workdir, "/srv/b");

    assert_eq!(h.key(KeyCode::Esc), None);
    assert!(h.app.ui().create.is_none());
    assert!(h.drain_requests().is_empty());
    assert!(h.seam.pending_outbox().unwrap().is_empty());
    assert_contains(&h.screen(), "n new session");
    // Out of the prompt, `q` quits again.
    h.key(KeyCode::Char('q'));
    assert!(h.app.ui().quit);
}

// ---- session view -----------------------------------------------------------

/// A paired harness with one session `/srv/work` open, plus this device's id.
fn open_session() -> (Harness, SessionId, DeviceId) {
    let mut h = Harness::paired();
    let id = SessionId::new();
    pull(&mut h, vec![created(id, 0, "/srv/work")]);
    assert_eq!(h.key(KeyCode::Enter), Some(Action::OpenSession(id)));
    let me = h.seam.identity().unwrap().device_id();
    (h, id, me)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

/// The last 8 hex digits of a device id, as the timeline names devices.
fn tail(device: DeviceId) -> String {
    device.0.simple().to_string()[24..].to_owned()
}

fn blob(size: u64) -> (BlobId, BlobRef) {
    let id = BlobId::new();
    (id, BlobRef { id, size })
}

#[test]
fn every_timeline_entry_renders_distinctly() {
    let (mut h, id, me) = open_session();
    assert_contains(&h.screen(), "Nothing here yet.");
    let other = DeviceId::new();
    let (_, output) = blob(10_000);
    pull(
        &mut h,
        vec![
            event(
                id,
                1,
                EventKind::UserMessage {
                    command_id: CommandId::new(),
                    device_id: other,
                    text: "from the laptop".into(),
                },
            ),
            event(
                id,
                2,
                EventKind::AgentMessage {
                    text: "Looking.\nSecond line".into(),
                },
            ),
            event(
                id,
                3,
                EventKind::ToolCall {
                    name: "shell".into(),
                    summary: "{\"command\":\"ls\"}".into(),
                    input: None,
                },
            ),
            event(
                id,
                4,
                EventKind::ToolResult {
                    summary: "file-000".into(),
                    output: Some(output),
                },
            ),
            event(id, 5, EventKind::TurnInterrupted),
            event(
                id,
                6,
                EventKind::SessionStateChanged(LogState::Errored {
                    message: "agent crashed".into(),
                }),
            ),
        ],
    );
    let screen = h.screen();
    assert_contains(
        &screen,
        &format!("12:01 device …{}: from the laptop", tail(other)),
    );
    assert_contains(&screen, "12:02 agent: Looking.");
    assert_contains(&screen, "        Second line");
    assert_contains(&screen, "12:03 ▸ tool shell: {\"command\":\"ls\"}");
    assert_contains(
        &screen,
        "12:04 ▸ result: file-000 [full 9.8 KiB · not downloaded]",
    );
    assert_contains(&screen, "12:05 interrupted · the agent stopped its turn");
    assert_contains(&screen, "12:06 agent error: agent crashed");
    assert_contains(&screen, "/srv/work · error");
    assert!(!screen.contains(&me.0.simple().to_string()[24..]));
}

#[test]
fn own_messages_show_their_delivery_state() {
    let (mut h, id, me) = open_session();
    assert!(h.app.typing(), "the composer has the focus");
    assert_contains(&h.screen(), "Write to the agent…");
    // `q` and `?` are text here.
    h.type_text("q? line one");
    h.key_with(ctrl('j'));
    h.type_text("line two");
    assert!(!h.app.ui().quit && !h.app.ui().help);
    let screen = h.screen();
    assert_contains(&screen, "│q? line one");
    assert_contains(&screen, "│line two");
    assert_eq!(
        h.key(KeyCode::Enter),
        Some(Action::SendMessage {
            session_id: id,
            text: "q? line one\nline two".into(),
        })
    );
    assert_eq!(h.drain_requests(), vec![Request::OutboxChanged]);
    assert_eq!(h.app.draft(id).unwrap().text(), "", "sent drafts clear");
    let command = last_command(&h);
    let screen = h.screen();
    assert_contains(&screen, "you [queued]: q? line one");
    assert_contains(&screen, "        line two");

    h.seam.command_sending(command).unwrap();
    h.sync();
    assert_contains(&h.screen(), "you [sending]: q? line one");
    h.seam
        .command_result(command, &CommandResult::Accepted { seq: Seq(2) })
        .unwrap();
    h.sync();
    assert_contains(&h.screen(), "you [accepted]: q? line one");
    pull(
        &mut h,
        vec![event(
            id,
            1,
            EventKind::UserMessage {
                command_id: command,
                device_id: me,
                text: "q? line one\nline two".into(),
            },
        )],
    );
    assert_contains(&line_with(&h.screen(), "line one"), "12:01 you [accepted]");

    // A refused message says why, in words.
    h.type_text("again");
    h.key(KeyCode::Enter);
    let command = last_command(&h);
    h.seam
        .command_result(
            command,
            &CommandResult::Rejected {
                reason: FailReason::UnknownSession,
            },
        )
        .unwrap();
    h.sync();
    assert_contains(
        &h.screen(),
        "you [failed: the server does not know this session]: again",
    );

    // A blank draft goes nowhere: the core refuses it.
    h.drain_requests();
    h.key(KeyCode::Enter);
    h.type_text("   ");
    h.key(KeyCode::Enter);
    assert_eq!(h.drain_requests(), vec![]);

    // A paste keeps its line breaks and lands in the composer, also from
    // the timeline.
    h.key(KeyCode::Esc);
    assert!(!h.app.typing());
    h.app.on_paste("pasted\r\nlines");
    assert!(h.app.typing());
    assert_eq!(h.app.draft(id).unwrap().text(), "pasted\nlines");
}

#[test]
fn approval_prompt_answer_and_resolutions() {
    let (mut h, id, me) = open_session();
    let first = ApprovalId::new();
    let (_, input) = blob(6000);
    pull(
        &mut h,
        vec![event(
            id,
            1,
            EventKind::ApprovalRequested {
                id: first,
                tool: "write".into(),
                summary: "{\"path\":\"a.txt\"}".into(),
                input: Some(input),
            },
        )],
    );
    let screen = h.screen();
    assert_contains(&screen, "Approval needed");
    assert_contains(&screen, "The agent wants to use write");
    assert_contains(&screen, "Esc, then a approve · x deny");
    assert_contains(&screen, "[full 5.9 KiB · not downloaded]");
    assert_contains(&screen, "approval write [waiting for an answer]");
    // In the composer `a` is text.
    assert_eq!(h.key(KeyCode::Char('a')), None);
    h.key(KeyCode::Backspace);
    h.key(KeyCode::Esc);
    assert_contains(&h.screen(), "│a approve · x deny");
    assert_eq!(
        h.key(KeyCode::Char('a')),
        Some(Action::ResolveApproval {
            id: first,
            decision: Decision::Approve,
        })
    );
    assert_contains(&h.screen(), "you approved · queued");
    let command = last_command(&h);
    h.seam
        .command_result(command, &CommandResult::Accepted { seq: Seq(3) })
        .unwrap();
    h.sync();
    assert_contains(&h.screen(), "you approved · accepted");
    pull(
        &mut h,
        vec![event(
            id,
            2,
            EventKind::ApprovalResolved {
                id: first,
                decision: Decision::Approve,
                by: me,
            },
        )],
    );
    let screen = h.screen();
    assert!(!screen.contains("Approval needed"), "{screen}");
    assert_contains(&screen, "approval write [approved here]");

    // Answered on another device: named, and the controls are gone.
    let other = DeviceId::new();
    let second = ApprovalId::new();
    pull(
        &mut h,
        vec![event(
            id,
            3,
            EventKind::ApprovalRequested {
                id: second,
                tool: "shell".into(),
                summary: "rm -rf build".into(),
                input: None,
            },
        )],
    );
    assert_contains(&h.screen(), "The agent wants to use shell");
    pull(
        &mut h,
        vec![event(
            id,
            4,
            EventKind::ApprovalResolved {
                id: second,
                decision: Decision::Deny,
                by: other,
            },
        )],
    );
    let screen = h.screen();
    assert!(!screen.contains("a approve"), "{screen}");
    assert_contains(
        &screen,
        &format!("approval shell [denied on device …{}]", tail(other)),
    );
    assert_eq!(h.key(KeyCode::Char('x')), None, "nothing left to answer");

    let third = ApprovalId::new();
    pull(
        &mut h,
        vec![
            event(
                id,
                5,
                EventKind::ApprovalRequested {
                    id: third,
                    tool: "net".into(),
                    summary: "curl".into(),
                    input: None,
                },
            ),
            event(
                id,
                6,
                EventKind::ApprovalCancelled {
                    id: third,
                    reason: "interrupted".into(),
                },
            ),
        ],
    );
    let screen = h.screen();
    assert_contains(&screen, "approval net [cancelled: interrupted]");
    assert!(!screen.contains("Approval needed"));
}

#[test]
fn interrupt_key_and_its_delivery_state() {
    let (mut h, id, _) = open_session();
    pull(
        &mut h,
        vec![event(
            id,
            1,
            EventKind::SessionStateChanged(LogState::AgentWorking),
        )],
    );
    assert_contains(&h.screen(), "agent working · Ctrl-X interrupts");
    // From the composer and from the timeline alike.
    assert_eq!(
        h.key_with(ctrl('x')),
        Some(Action::Interrupt { session_id: id })
    );
    assert_contains(&h.screen(), "interrupt queued");
    let command = last_command(&h);
    h.seam.command_sending(command).unwrap();
    h.sync();
    assert_contains(&h.screen(), "interrupt sending");
    h.seam
        .command_result(command, &CommandResult::Accepted { seq: Seq(2) })
        .unwrap();
    pull(
        &mut h,
        vec![
            event(id, 2, EventKind::TurnInterrupted),
            event(id, 3, EventKind::SessionStateChanged(LogState::Idle)),
        ],
    );
    let screen = h.screen();
    assert_contains(&screen, "interrupted · the agent stopped its turn");
    assert_contains(&screen, "agent idle");
    assert!(!screen.contains("interrupt accepted"), "{screen}");

    h.key(KeyCode::Esc);
    assert_eq!(
        h.key_with(ctrl('x')),
        Some(Action::Interrupt { session_id: id })
    );
    let command = last_command(&h);
    h.seam
        .command_result(
            command,
            &CommandResult::Rejected {
                reason: FailReason::NoActiveTurn,
            },
        )
        .unwrap();
    h.sync();
    assert_contains(&h.screen(), "interrupt failed: the agent was not working");
}

#[test]
fn tool_output_expands_through_every_blob_state() {
    let (mut h, id, _) = open_session();
    let (blob_id, output) = blob(10_000);
    pull(
        &mut h,
        vec![
            event(
                id,
                1,
                EventKind::ToolCall {
                    name: "shell".into(),
                    summary: "line a\nline b\nline c".into(),
                    input: None,
                },
            ),
            event(
                id,
                2,
                EventKind::ToolResult {
                    summary: "file-000".into(),
                    output: Some(output),
                },
            ),
        ],
    );
    assert_contains(&h.screen(), "tool shell: line a (+2 lines)");
    h.key(KeyCode::Esc);
    // Selection starts at the newest tool item.
    h.key(KeyCode::Up);
    assert_contains(&h.screen(), "> 12:02 ▸ result: file-000");
    assert_eq!(
        h.key(KeyCode::Char('d')),
        Some(Action::DownloadBlob(blob_id))
    );
    assert_eq!(h.drain_requests(), vec![Request::DownloadBlob(blob_id)]);
    // Expanding asks for it too.
    assert_eq!(h.key(KeyCode::Enter), Some(Action::DownloadBlob(blob_id)));
    assert_contains(
        &h.screen(),
        "not downloaded · 9.8 KiB · press d to download",
    );

    let dir = tempfile::tempdir().unwrap();
    let part = dir.path().join("blob.part");
    h.seam
        .blob_status(
            blob_id,
            BlobStatus::Downloading {
                got: 4096,
                path: part,
            },
        )
        .unwrap();
    h.sync();
    let screen = h.screen();
    assert_contains(&screen, "[downloading 40%]");
    assert_contains(&screen, "downloading · 4.0 KiB of 9.8 KiB (40%)");

    // Content arrives; invalid UTF-8 and control bytes are shown safely.
    let path = dir.path().join("blob");
    let content = b"file-000\nfile-001\tx\n\xff\xfebroken\x07\n".to_vec();
    std::fs::write(&path, &content).unwrap();
    h.seam
        .blob_status(blob_id, BlobStatus::Available { path })
        .unwrap();
    h.sync();
    let screen = h.screen();
    assert_contains(&screen, "▾ result: file-000 [downloaded]");
    assert_contains(&screen, "          file-000");
    assert_contains(&screen, "          file-001    x");
    assert_contains(&screen, "          ��broken");
    assert_eq!(h.key(KeyCode::Char('d')), None, "already here");

    // Collapse; the inline tool call expands to its whole summary.
    h.key(KeyCode::Enter);
    assert!(!h.screen().contains("file-001"));
    h.key(KeyCode::Char('k'));
    assert_eq!(h.key(KeyCode::Char(' ')), None);
    let screen = h.screen();
    assert_contains(&screen, "▾ tool shell: line a");
    assert_contains(&screen, "          line c");
}

#[test]
fn large_blobs_are_cut_short_when_shown() {
    let (mut h, id, _) = open_session();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big");
    let text: String = (0..1000).map(|i| format!("row {i}\n")).collect();
    std::fs::write(&path, &text).unwrap();
    let (blob_id, output) = blob(text.len() as u64);
    pull(
        &mut h,
        vec![event(
            id,
            1,
            EventKind::ToolResult {
                summary: "row 0".into(),
                output: Some(output),
            },
        )],
    );
    h.seam
        .blob_status(blob_id, BlobStatus::Available { path })
        .unwrap();
    h.sync();
    h.key(KeyCode::Esc);
    h.key(KeyCode::Up);
    assert_eq!(h.key(KeyCode::Enter), None);
    // Following the bottom shows the end of what is shown.
    let screen = h.screen();
    assert_contains(&screen, "row 399");
    assert_contains(&screen, "… cut short: 400 lines of");
    assert!(!screen.contains("row 400"));
}

#[test]
fn timeline_follows_new_items_until_scrolled_back() {
    let (mut h, id, _) = open_session();
    let messages = |from: u32, to: u32| {
        (from..to)
            .map(|i| {
                event(
                    id,
                    1,
                    EventKind::AgentMessage {
                        text: format!("message {i:02}"),
                    },
                )
            })
            .collect::<Vec<_>>()
    };
    pull(&mut h, messages(0, 40));
    let screen = h.screen();
    assert_contains(&screen, "message 39");
    assert!(!screen.contains("message 00"));
    pull(&mut h, messages(40, 41));
    assert_contains(&h.screen(), "message 40");

    // Page back (works from the composer too); new items do not move it.
    h.key(KeyCode::PageUp);
    let screen = h.screen();
    assert!(!screen.contains("message 40"), "{screen}");
    assert_contains(&screen, "scrolled back, End follows");
    let before = screen.clone();
    pull(&mut h, messages(41, 45));
    assert_eq!(h.screen(), before);

    h.key(KeyCode::Esc);
    h.key(KeyCode::Home);
    assert_contains(&h.screen(), "message 00");
    h.key(KeyCode::PageDown);
    assert!(!h.screen().contains("message 00"));
    h.key(KeyCode::End);
    let screen = h.screen();
    assert_contains(&screen, "message 44");
    assert!(!screen.contains("scrolled back"));

    // Paging forward to the bottom follows again.
    h.key(KeyCode::PageUp);
    h.screen();
    h.key(KeyCode::PageDown);
    pull(&mut h, messages(45, 46));
    let screen = h.screen();
    assert_contains(&screen, "message 45");
    assert!(!screen.contains("scrolled back"));
}

#[test]
fn selecting_an_old_tool_item_scrolls_to_it() {
    let (mut h, id, _) = open_session();
    let mut events = vec![event(
        id,
        1,
        EventKind::ToolCall {
            name: "early".into(),
            summary: "first".into(),
            input: None,
        },
    )];
    events.extend((0..40).map(|i| {
        event(
            id,
            2,
            EventKind::AgentMessage {
                text: format!("chatter {i}"),
            },
        )
    }));
    events.push(event(
        id,
        3,
        EventKind::ToolCall {
            name: "late".into(),
            summary: "second".into(),
            input: None,
        },
    ));
    pull(&mut h, events);
    assert!(!h.screen().contains("tool early"));
    h.key(KeyCode::Esc);
    h.key(KeyCode::Up);
    assert_contains(&h.screen(), "> 12:03 ▸ tool late");
    h.key(KeyCode::Up);
    let screen = h.screen();
    assert_contains(&screen, "> 12:01 ▸ tool early");
    assert!(!screen.contains("tool late"));
    h.key(KeyCode::Down);
    assert_contains(&h.screen(), "> 12:03 ▸ tool late");
}

#[test]
fn session_keys_are_in_help_and_footer() {
    let (mut h, _, _) = open_session();
    assert_contains(
        &h.screen(),
        "Enter send · Ctrl-J newline · Ctrl-X interrupt",
    );
    h.key(KeyCode::F(1));
    let screen = h.screen();
    assert_contains(&screen, "interrupt the agent's turn");
    assert_contains(&screen, "new line (also Alt-Enter, Shift-Enter)");
    h.key(KeyCode::Esc);
    h.key(KeyCode::Tab);
    assert!(!h.app.typing());
    assert_contains(&h.screen(), "↑↓ select · Enter expand · d download");
    h.key(KeyCode::Char('?'));
    let screen = h.screen();
    assert_contains(&screen, "expand it; downloads its blob if needed");
    assert_contains(&screen, "approve / deny the pending approval");
    h.key(KeyCode::Char('?'));
    h.key(KeyCode::Char('i'));
    assert!(h.app.typing());
    h.key(KeyCode::Esc);
    assert_eq!(
        h.key(KeyCode::Esc),
        Some(Action::Navigate(Route::SessionList))
    );
}

#[test]
fn drafts_survive_leaving_the_session() {
    let (mut h, id, _) = open_session();
    h.type_text("half a thought");
    h.key(KeyCode::Esc);
    h.key(KeyCode::Esc);
    assert_eq!(h.key(KeyCode::Enter), Some(Action::OpenSession(id)));
    assert_contains(&h.screen(), "│half a thought");
}

#[test]
fn offline_composer_says_messages_wait() {
    let (mut h, _, _) = open_session();
    h.seam.set_link(Link::Reconnecting {
        attempt: 1,
        next_in: Duration::from_secs(5),
        offline_since: h.t0,
    });
    h.sync();
    assert_contains(&h.screen(), "Message · offline, sends when back online");
}

#[test]
fn settings_explain_the_blob_policy() {
    let mut h = Harness::paired();
    h.key(KeyCode::Char('s'));
    let screen = h.screen();
    assert_contains(&screen, "(•) On demand  a blob downloads when you expand");
    assert_contains(&screen, "( ) Eager      every blob downloads as soon");
    h.key(KeyCode::Char('b'));
    let screen = h.screen();
    assert_contains(&screen, "( ) On demand");
    assert_contains(&screen, "(•) Eager");
}
