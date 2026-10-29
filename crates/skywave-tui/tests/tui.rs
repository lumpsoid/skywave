//! The TUI against a real core, without a terminal or a network: keys go in
//! through `App::on_key`, actions go to `Core::dispatch`, engine reports go
//! in through `core.seam()`, and frames are checked on ratatui's
//! `TestBackend`.

use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use skywave_core::{
    Action, BlobPolicy, Core, DeviceId, Link, LinkRejection, PairedCredentials, Request, Route,
    Screen, Seam, Seq, ViewState,
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
        let core = Core::open(dir.path()).unwrap();
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
    assert_contains(&h.screen(), "/srv/a  [queued]");

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
