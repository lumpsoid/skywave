//! The TUI against a real core, without a terminal or a network: keys go in
//! through `handle_key`, actions go to `Core::dispatch`, engine reports go
//! in through `core.seam()`, and frames are checked on ratatui's
//! `TestBackend`.

use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use chrono::{DateTime, TimeZone, Utc};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::Color;
use skywave_core::{
    Action, ApprovalId, BlobId, BlobPolicy, BlobStatus, CommandId, Core, Decision, Delivery,
    DeviceId, Draft, Entry, FailReason, Link, LinkRejection, PairedCredentials, Request, Route,
    Screen, Seam, Seq, SessionId, TimelineItem, ViewState,
};
use skywave_proto::{
    BlobRef, Body, BodyFormat, CallId, CommandResult, Event, EventBatch, EventKind,
    SessionState as LogState, ToolCall, ToolDisplay, ToolKind, ToolResult,
};
use skywave_tui::App;
use skywave_tui::app::{DRAFT_SAVE_AFTER, PairUri};
use skywave_tui::timeline::{ItemKey, delivery, find};
use skywave_tui::ui;

const FP: &str = "ab:cd:ef";

/// A core over a temp dir, the TUI over it, and the engine's side.
struct Harness {
    dir: tempfile::TempDir,
    core: Core,
    seam: Seam,
    requests: Receiver<Request>,
    views: Receiver<ViewState>,
    app: App,
    t0: Instant,
}

impl Harness {
    fn new() -> Harness {
        Harness::open(tempfile::tempdir().unwrap())
    }

    fn open(dir: tempfile::TempDir) -> Harness {
        let core = Core::open_detached(dir.path()).unwrap();
        let seam = core.seam();
        let requests = seam.take_requests().unwrap();
        let (tx, views) = mpsc::channel();
        core.subscribe(move |view| tx.send(view).unwrap());
        let t0 = Instant::now();
        let app = App::new(views.recv().unwrap(), t0);
        Harness {
            dir,
            core,
            seam,
            requests,
            views,
            app,
            t0,
        }
    }

    /// Drops the core and the TUI without quitting, as a crash does, and
    /// starts both again over the same data.
    fn restart(self) -> Harness {
        let Harness { dir, core, app, .. } = self;
        drop((core, app));
        Harness::open(dir)
    }

    /// The idle save the event loop makes once the first unsaved edit is
    /// old enough.
    fn idle_save(&mut self) {
        skywave_tui::save_draft(
            &mut self.app,
            &self.core,
            Instant::now() + DRAFT_SAVE_AFTER,
            false,
        );
        self.sync();
    }

    /// Makes every enqueue fail (`true`), as a full disk would, or work
    /// again.
    fn fail_enqueue(&self, fail: bool) {
        let db = self.dir.path().join(skywave_core::store::DB_FILE);
        let conn = rusqlite::Connection::open(db).unwrap();
        conn.execute_batch(if fail {
            "CREATE TRIGGER full BEFORE INSERT ON outbox BEGIN SELECT RAISE(ABORT, 'disk full'); END;"
        } else {
            "DROP TRIGGER full"
        })
        .unwrap();
    }

    /// The draft the core keeps for the open session.
    fn core_draft(&self) -> Option<Draft> {
        match self.core.view().screen {
            Screen::Session(view) => view.draft,
            other => panic!("expected the session screen, got {other:?}"),
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

    /// A key press, handled as the event loop does.
    fn key_with(&mut self, key: KeyEvent) -> Option<Action> {
        let action = skywave_tui::handle_key(&mut self.app, &self.core, key);
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
            .draw(|frame| ui::render_at(frame, &self.app, now, mockup_now().into()))
            .unwrap();
        text(terminal.backend().buffer())
    }

    /// A `width` × `height` frame with the wall clock at `wall`, so running
    /// calls' clocks are fixed.
    fn frame(&self, width: u16, height: u16, wall: DateTime<Utc>) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| ui::render_at(frame, &self.app, self.t0, wall.into()))
            .unwrap();
        terminal.backend().buffer().clone()
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

/// A frame's text, one line per row, trailing blanks kept.
fn text(buffer: &Buffer) -> String {
    let mut out = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            out.push_str(buffer[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
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
        "offline · 2 queued · retry in 05s · Ctrl-R now (attempt 3)"
    );
    assert_eq!(
        h.banner_at(t0 + Duration::from_millis(2500)),
        "offline · 2 queued · retry in 03s · Ctrl-R now (attempt 3)"
    );
    assert_eq!(
        h.banner_at(t0 + Duration::from_secs(6)),
        "offline · 2 queued · retrying… (attempt 3)"
    );
    // Local data still renders under the banner.
    assert_contains(&line_with(&h.screen(), "/srv/a"), "[queued]");

    // Ctrl-R asks the engine to stop waiting.
    h.drain_requests();
    assert_eq!(h.key_with(ctrl('r')), Some(Action::RetryNow));
    assert_eq!(h.drain_requests(), vec![Request::RetryNow]);

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
        "offline · 2 queued · retry in 09s · Ctrl-R now (attempt 4)"
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
                    call: None,
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
                EventKind::ToolCall(ToolCall {
                    call: None,
                    name: "shell".into(),
                    display: ToolDisplay {
                        kind: ToolKind::Other,
                        title: "{\"command\":\"ls\"}".into(),
                        detail: None,
                        body: None,
                    },
                    input: None,
                }),
            ),
            event(
                id,
                4,
                EventKind::ToolResult(ToolResult {
                    call: None,
                    is_error: false,
                    status: None,
                    preview: "file-000".into(),
                    lines: 1,
                    output: Some(output),
                }),
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
    // Standalone (v1) calls and results, in the tool block style.
    assert_contains(&screen, "12:03 • shell {\"command\":\"ls\"}");
    assert_contains(&screen, "12:04 ↳ result ");
    assert_contains(&screen, "        │ file-000 ");
    assert_contains(&screen, "        │ … full 9.8 KiB · not downloaded ");
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
    assert_eq!(h.app.composer().unwrap().text(), "", "sent drafts clear");
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
    assert_eq!(h.app.composer().unwrap().text(), "pasted\nlines");
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
                call: None,
                tool: "write".into(),
                summary: "a.txt".into(),
                input: Some(input),
            },
        )],
    );
    let screen = h.screen();
    assert_contains(&screen, "Approval needed");
    assert_contains(&screen, "│? write a.txt ");
    assert_contains(&screen, "Esc, then a approve · x deny");
    assert_contains(&screen, "full input 5.9 KiB · v to see it");
    assert_contains(&line_with(&screen, "needs approval"), "12:01 ? write a.txt");
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
    assert_contains(&line_with(&screen, "? write a.txt"), "approved here");

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
                call: None,
                tool: "shell".into(),
                summary: "rm -rf build".into(),
                input: None,
            },
        )],
    );
    assert_contains(&h.screen(), "│? shell rm -rf build ");
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
        &line_with(&screen, "? shell rm -rf build"),
        &format!("denied on device …{}", tail(other)),
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
                    call: None,
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
    assert_contains(&line_with(&screen, "? net curl"), "cancelled: interrupted");
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
fn stop_key_and_its_delivery_state() {
    let (mut h, id, _) = open_session();
    // A capital S in the composer is text, not a stop.
    assert_eq!(h.key(KeyCode::Char('S')), None);
    h.key(KeyCode::Esc);
    assert_eq!(
        h.key(KeyCode::Char('S')),
        Some(Action::StopSession { session_id: id })
    );
    assert_contains(&h.screen(), "stop queued");
    let command = last_command(&h);
    h.seam
        .command_result(command, &CommandResult::Accepted { seq: Seq(1) })
        .unwrap();
    pull(&mut h, vec![event(id, 1, EventKind::SessionStopped)]);
    let screen = h.screen();
    assert_contains(&screen, "stopped · the agent was shut down");
    assert_contains(&screen, "agent stopped · a message restarts it");
    assert!(!screen.contains("stop accepted"), "{screen}");
    h.key(KeyCode::Esc);
    assert_contains(&h.screen(), "[stopped]");
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
                EventKind::ToolCall(ToolCall {
                    call: None,
                    name: "shell".into(),
                    display: ToolDisplay {
                        kind: ToolKind::Other,
                        title: "line a\nline b\nline c".into(),
                        detail: None,
                        body: None,
                    },
                    input: None,
                }),
            ),
            event(
                id,
                2,
                EventKind::ToolResult(ToolResult {
                    call: None,
                    is_error: false,
                    status: None,
                    preview: "file-000".into(),
                    lines: 1,
                    output: Some(output),
                }),
            ),
        ],
    );
    // A v1 call's further summary lines sit under the gutter.
    let screen = h.screen();
    assert_contains(&screen, "12:01 • shell line a ");
    assert_contains(&screen, "        │ line c ");
    assert_contains(&screen, "        │ … full 9.8 KiB · not downloaded ");
    h.key(KeyCode::Esc);
    // Selection starts at the newest item.
    h.key(KeyCode::Up);
    assert_contains(&h.screen(), "> 12:02 ↳ result");
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
    assert_contains(&screen, "│ downloading · 4.0 KiB of 9.8 KiB (40%)");
    // Collapsed, the footer follows the download.
    h.key(KeyCode::Enter);
    assert_contains(&h.screen(), "│ … downloading 40%");
    h.key(KeyCode::Enter);

    // Content arrives; invalid UTF-8 and control bytes are shown safely.
    let path = dir.path().join("blob");
    let content = b"file-000\nfile-001\tx\n\xff\xfebroken\x07\n".to_vec();
    std::fs::write(&path, &content).unwrap();
    h.seam
        .blob_status(blob_id, BlobStatus::Available { path })
        .unwrap();
    h.sync();
    let screen = h.screen();
    assert_contains(&screen, "        │ file-000 ");
    assert_contains(&screen, "        │ file-001    x ");
    assert_contains(&screen, "        │ ��broken ");
    assert!(!screen.contains("not downloaded"), "{screen}");
    assert_eq!(h.key(KeyCode::Char('d')), None, "already here");

    // Collapse: the preview and that the whole is here.
    h.key(KeyCode::Enter);
    let screen = h.screen();
    assert!(!screen.contains("file-001"));
    assert_contains(&screen, "│ … downloaded ");
    // A call with nothing behind its summary has nothing to expand.
    h.key(KeyCode::Char('k'));
    assert_eq!(h.key(KeyCode::Char(' ')), None);
    assert_contains(&h.screen(), "        │ line c ");
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
            EventKind::ToolResult(ToolResult {
                call: None,
                is_error: false,
                status: None,
                preview: "row 0".into(),
                lines: 1,
                output: Some(output),
            }),
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
        EventKind::ToolCall(ToolCall {
            call: None,
            name: "early".into(),
            display: ToolDisplay {
                kind: ToolKind::Other,
                title: "first".into(),
                detail: None,
                body: None,
            },
            input: None,
        }),
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
        EventKind::ToolCall(ToolCall {
            call: None,
            name: "late".into(),
            display: ToolDisplay {
                kind: ToolKind::Other,
                title: "second".into(),
                detail: None,
                body: None,
            },
            input: None,
        }),
    ));
    pull(&mut h, events);
    assert!(!h.screen().contains("early first"));
    h.key(KeyCode::Esc);
    h.key(KeyCode::Up);
    assert_contains(&h.screen(), "> 12:03 • late second");
    // Agent messages are selectable too: 40 of them in between.
    for _ in 0..41 {
        h.key(KeyCode::Up);
    }
    let screen = h.screen();
    assert_contains(&screen, "> 12:01 • early first");
    assert!(!screen.contains("late second"));
    h.key(KeyCode::Down);
    assert_contains(&h.screen(), "> 12:02 agent: chatter 0");
    for _ in 0..40 {
        h.key(KeyCode::Down);
    }
    assert_contains(&h.screen(), "> 12:03 • late second");
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
    h.type_text("half a thöught");
    h.key(KeyCode::Left);
    h.key(KeyCode::Esc);
    // Leaving saves the draft first; the list marks the session.
    assert_eq!(
        h.key(KeyCode::Esc),
        Some(Action::Navigate(Route::SessionList))
    );
    assert_contains(&line_with(&h.screen(), "/srv/work"), "✎ draft");

    // The TUI kept no copy: the composer comes back from the core.
    assert_eq!(h.key(KeyCode::Enter), Some(Action::OpenSession(id)));
    let composer = h.app.composer().unwrap();
    assert_eq!(composer.text(), "half a thöught");
    assert_eq!(composer.cursor_chars(), 13);
    assert_contains(&h.screen(), "│half a thöught");

    // Sending clears it everywhere.
    h.key(KeyCode::Enter);
    assert_eq!(h.core_draft(), None);
    h.key(KeyCode::Esc);
    h.key(KeyCode::Esc);
    assert!(!line_with(&h.screen(), "/srv/work").contains("draft"));
    h.key(KeyCode::Enter);
    assert_eq!(h.app.composer().unwrap().text(), "");
}

#[test]
fn drafts_survive_a_restart() {
    let (mut h, id, _) = open_session();
    h.type_text("before the crash");
    for _ in 0..3 {
        h.key(KeyCode::Left);
    }
    h.idle_save();

    let mut h = h.restart();
    assert_contains(&line_with(&h.screen(), "/srv/work"), "✎ draft");
    assert_eq!(h.key(KeyCode::Enter), Some(Action::OpenSession(id)));
    let composer = h.app.composer().unwrap();
    assert_eq!(composer.text(), "before the crash");
    assert_eq!(composer.cursor_chars(), 13);

    // Quitting saves what the idle timer has not yet.
    h.type_text("X");
    h.key_with(ctrl('c'));
    assert!(h.app.ui().quit);
    let mut h = h.restart();
    h.key(KeyCode::Enter);
    assert_eq!(h.app.composer().unwrap().text(), "before the crXash");
}

#[test]
fn idle_draft_is_saved_once() {
    let (mut h, _, _) = open_session();
    let before = Instant::now();
    h.type_text("abc");
    // Typing writes nothing.
    assert_eq!(h.core_draft(), None);
    assert!(h.app.draft_due_in(Instant::now()).unwrap() <= DRAFT_SAVE_AFTER);
    assert_eq!(
        h.app.draft_save(before, false),
        None,
        "not idle long enough"
    );

    h.idle_save();
    assert_eq!(
        h.core_draft(),
        Some(Draft {
            text: "abc".into(),
            cursor: 3
        })
    );
    assert_eq!(h.app.draft_due_in(before), None);
    let later = Instant::now() + 10 * DRAFT_SAVE_AFTER;
    assert_eq!(h.app.draft_save(later, false), None, "saved once");
    assert_eq!(h.app.draft_save(later, true), None, "even when forced");

    h.type_text("d");
    assert!(h.app.draft_save(later, false).is_some());
}

#[test]
fn failed_send_keeps_the_text() {
    let (mut h, id, _) = open_session();
    h.type_text("keep");
    h.idle_save();
    h.type_text(" me");
    h.fail_enqueue(true);
    assert_eq!(
        h.key(KeyCode::Enter),
        Some(Action::SendMessage {
            session_id: id,
            text: "keep me".into(),
        })
    );
    assert!(h.app.ui().error.is_some());
    assert_eq!(h.app.composer().unwrap().text(), "keep me");
    assert_eq!(
        h.core_draft().map(|d| d.text),
        Some("keep".into()),
        "the core's draft is kept too"
    );

    // A save right after is not a sent message: the text stays.
    h.idle_save();
    assert_eq!(h.app.composer().unwrap().text(), "keep me");
    assert_eq!(h.core_draft().map(|d| d.text), Some("keep me".into()));

    h.fail_enqueue(false);
    h.key(KeyCode::Enter);
    assert_eq!(h.app.composer().unwrap().text(), "");
    assert_eq!(h.core_draft(), None);
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

#[test]
fn a_grouped_tool_call_shows_its_approval_and_result() {
    let (mut h, id, me) = open_session();
    let (call, approval) = (CallId::new(), ApprovalId::new());
    pull(
        &mut h,
        vec![
            event(
                id,
                1,
                EventKind::ToolCall(ToolCall {
                    call: Some(call),
                    name: "Write".into(),
                    display: ToolDisplay {
                        kind: ToolKind::FileWrite,
                        title: "a.txt".into(),
                        detail: None,
                        body: None,
                    },
                    input: None,
                }),
            ),
            event(
                id,
                2,
                EventKind::ApprovalRequested {
                    id: approval,
                    call: Some(call),
                    tool: "Write".into(),
                    summary: "a.txt".into(),
                    input: None,
                },
            ),
            event(
                id,
                3,
                EventKind::ApprovalResolved {
                    id: approval,
                    decision: Decision::Approve,
                    by: me,
                },
            ),
            event(
                id,
                4,
                EventKind::ToolResult(ToolResult {
                    call: Some(call),
                    is_error: false,
                    status: Some("created".into()),
                    preview: "wrote 1 line".into(),
                    lines: 1,
                    output: None,
                }),
            ),
        ],
    );
    let screen = h.screen();
    assert_contains(
        &line_with(&screen, "12:01 ✎ Write a.txt"),
        "created · approved here · 3m00s",
    );
    assert_contains(&screen, "        │ wrote 1 line ");
    assert!(!screen.contains("12:02"), "folded into the call: {screen}");
    assert!(!screen.contains("12:04"), "folded into the call: {screen}");
}

// ---- the tool timeline mock-up (review "tool timeline redesign") ----------

fn at_s(minute: u32, second: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 29, 12, minute, second)
        .unwrap()
}

fn call(call: CallId, name: &str, kind: ToolKind, title: &str) -> ToolCall {
    ToolCall {
        call: Some(call),
        name: name.into(),
        display: ToolDisplay {
            kind,
            title: title.into(),
            detail: None,
            body: None,
        },
        input: None,
    }
}

fn body(format: BodyFormat, text: &str) -> Option<Body> {
    Some(Body {
        format,
        preview: text.into(),
        lines: text.lines().count() as u32,
        full: None,
    })
}

/// A linked result; `lines` counts the preview's, as when it holds all.
fn result(call: CallId, is_error: bool, status: &str, preview: &str) -> ToolResult {
    ToolResult {
        call: Some(call),
        is_error,
        status: (!status.is_empty()).then(|| status.into()),
        preview: preview.into(),
        lines: preview.lines().count() as u32,
        output: None,
    }
}

fn user(device: DeviceId, text: &str) -> EventKind {
    EventKind::UserMessage {
        command_id: CommandId::new(),
        device_id: device,
        text: text.into(),
    }
}

fn agent(text: &str) -> EventKind {
    EventKind::AgentMessage { text: text.into() }
}

/// What the wall clock reads in the mock-up: 42 s into the running call.
fn mockup_now() -> DateTime<Utc> {
    at_s(8, 42)
}

/// The review's mock-up session: a Write creating a file, a Bash with its
/// exit status, an Edit with a diff, a failed call, a denied one, a
/// cancelled one, and a running call beside one waiting for its approval.
/// Returns the other device, the Write's file blob and the failed call's
/// output blob.
fn mockup() -> (Harness, DeviceId, BlobId, BlobId) {
    let (mut h, id, me) = open_session();
    // Fixed, so the snapshot names it.
    let other: DeviceId = serde_json::from_str("\"0192c8a0-0000-7000-8000-0000c0ffee42\"").unwrap();
    let ev = |minute, second, kind| Event {
        session_id: id,
        at: at_s(minute, second),
        kind,
    };
    let ids: Vec<CallId> = (0..8).map(|_| CallId::new()).collect();
    let approvals: Vec<ApprovalId> = (0..5).map(|_| ApprovalId::new()).collect();
    let (file_blob, file) = blob(400);
    let (output_blob, output) = blob(2048);

    let mut write = call(ids[0], "Write", ToolKind::FileWrite, "src/bin/hello.rs");
    write.display.body = Some(Body {
        format: BodyFormat::Text,
        preview: "//! Says hello.\n\nuse std::io::Write;\n\nfn main() {\n    let mut out = std::io::stdout();\n    writeln!(out, \"Hello, world!\").unwrap();\n    out.flush().unwrap();\n".into(),
        lines: 10,
        full: Some(file),
    });
    let mut bash = call(ids[1], "Bash", ToolKind::Shell, "$ cargo run --bin hello");
    bash.display.detail = Some("Build and run the new binary".into());
    let mut edit = call(ids[2], "Edit", ToolKind::FileEdit, "src/main.rs");
    edit.display.body = body(
        BodyFormat::Diff,
        " fn main() {\n-    run();\n+    let code = run();\n+    std::process::exit(code);\n }\n",
    );
    let failing = call(ids[3], "Bash", ToolKind::Shell, "$ cargo test");
    let mut denied = call(ids[4], "Bash", ToolKind::Shell, "$ rm -rf target");
    denied.display.detail = Some("Clean the build directory".into());
    let cancelled = call(ids[5], "Write", ToolKind::FileWrite, "NOTES.md");
    let running = call(
        ids[6],
        "Bash",
        ToolKind::Shell,
        "$ cargo test -- --nocapture",
    );
    let mut pending = call(ids[7], "Edit", ToolKind::FileEdit, "src/lib.rs");
    pending.display.body = body(
        BodyFormat::Diff,
        "-    assert_eq!(add(2, 2), 5);\n+    assert_eq!(add(2, 2), 4);\n",
    );
    let request = |approval: ApprovalId, call: CallId, tool: &str, summary: &str| {
        EventKind::ApprovalRequested {
            id: approval,
            call: Some(call),
            tool: tool.into(),
            summary: summary.into(),
            input: None,
        }
    };
    let resolve =
        |approval: ApprovalId, decision: Decision, by: DeviceId| EventKind::ApprovalResolved {
            id: approval,
            decision,
            by,
        };

    pull(
        &mut h,
        vec![
            // Turn 1: a file written, run, edited.
            ev(1, 0, user(me, "add a hello binary and wire it into main")),
            ev(1, 2, EventKind::SessionStateChanged(LogState::AgentWorking)),
            ev(
                1,
                5,
                agent("I'll add the binary, run it, then use it from main."),
            ),
            ev(1, 6, EventKind::ToolCall(write)),
            ev(
                1,
                6,
                request(approvals[0], ids[0], "Write", "src/bin/hello.rs"),
            ),
            ev(1, 9, resolve(approvals[0], Decision::Approve, me)),
            ev(
                1,
                9,
                EventKind::ToolResult(ToolResult {
                    lines: 1,
                    ..result(ids[0], false, "created", "")
                }),
            ),
            ev(1, 10, EventKind::ToolCall(bash)),
            ev(
                1,
                13,
                EventKind::ToolResult(result(
                    ids[1],
                    false,
                    "exit 0",
                    "   Compiling hello v0.1.0 (/srv/work)\n    Finished `dev` profile in 2.98s\n     Running `target/debug/hello`\nHello, world!\n",
                )),
            ),
            ev(2, 1, EventKind::ToolCall(edit)),
            ev(2, 1, request(approvals[1], ids[2], "Edit", "src/main.rs")),
            ev(2, 20, resolve(approvals[1], Decision::Approve, other)),
            ev(
                2,
                21,
                EventKind::ToolResult(ToolResult {
                    lines: 1,
                    ..result(ids[2], false, "updated +2 -1", "")
                }),
            ),
            ev(
                2,
                25,
                agent(
                    "Done: main now exits with run()'s code. A long reply wraps under its own column instead of falling back to the left edge.",
                ),
            ),
            ev(2, 26, EventKind::SessionStateChanged(LogState::Idle)),
            // Turn 2: a failure and a denial.
            ev(4, 0, user(other, "run the tests and clean up")),
            ev(4, 1, EventKind::SessionStateChanged(LogState::AgentWorking)),
            ev(4, 2, EventKind::ToolCall(failing)),
            ev(
                4,
                14,
                EventKind::ToolResult(ToolResult {
                    lines: 23,
                    output: Some(output),
                    ..result(
                        ids[3],
                        true,
                        "exit 101",
                        "running 3 tests\ntest tests::adds ... FAILED\ntest tests::greets ... ok\n",
                    )
                }),
            ),
            ev(4, 15, EventKind::ToolCall(denied)),
            ev(
                4,
                15,
                request(approvals[2], ids[4], "Bash", "$ rm -rf target"),
            ),
            ev(4, 30, resolve(approvals[2], Decision::Deny, me)),
            ev(
                4,
                30,
                EventKind::ToolResult(result(ids[4], true, "denied", "The user denied this call.")),
            ),
            ev(4, 32, agent("One test fails; I left target/ alone.")),
            ev(4, 33, EventKind::SessionStateChanged(LogState::Idle)),
            // Turn 3: interrupted while an approval waited.
            ev(6, 0, user(me, "write it up in NOTES.md")),
            ev(6, 1, EventKind::SessionStateChanged(LogState::AgentWorking)),
            ev(6, 3, EventKind::ToolCall(cancelled)),
            ev(6, 3, request(approvals[3], ids[5], "Write", "NOTES.md")),
            ev(
                6,
                9,
                EventKind::ApprovalCancelled {
                    id: approvals[3],
                    reason: "interrupted".into(),
                },
            ),
            ev(6, 9, EventKind::TurnInterrupted),
            ev(6, 10, EventKind::SessionStateChanged(LogState::Idle)),
            // Turn 4: still going.
            ev(8, 0, user(me, "fix the failing test")),
            ev(8, 0, EventKind::SessionStateChanged(LogState::AgentWorking)),
            ev(8, 0, EventKind::ToolCall(running)),
            ev(8, 5, EventKind::ToolCall(pending)),
            ev(8, 5, request(approvals[4], ids[7], "Edit", "src/lib.rs")),
        ],
    );
    (h, other, file_blob, output_blob)
}

/// A frame's text without trailing blanks.
fn trimmed(buffer: &Buffer) -> String {
    text(buffer)
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The foreground colour of the first cell of `needle` in the frame.
fn colour_of(buffer: &Buffer, needle: &str) -> Color {
    let screen = text(buffer);
    let (y, line) = screen
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains(needle))
        .unwrap_or_else(|| panic!("{needle:?} not on screen:\n{screen}"));
    let x = line[..line.find(needle).unwrap()].chars().count();
    buffer[(x as u16, y as u16)].fg
}

/// The mock-up at 90 × 60, trailing blanks trimmed.
const MOCKUP: &str = r#"connecting…
┌ /srv/work · approval ──────────────────────────────────────────────────────────────────┐
│agent waiting for an approval                                                           │
│  12:01 you: add a hello binary and wire it into main                                   │
│        agent: I'll add the binary, run it, then use it from main.                      │
│        ✎ Write src/bin/hello.rs                          created · approved here · 3.0s│
│        │  1 //! Says hello.                                                            │
│        │  2                                                                            │
│        │  3 use std::io::Write;                                                        │
│        │  4                                                                            │
│        │  5 fn main() {                                                                │
│        │  6     let mut out = std::io::stdout();                                       │
│        │  7     writeln!(out, "Hello, world!").unwrap();                               │
│        │  8     out.flush().unwrap();                                                  │
│        │ … +2 lines · full 400 B · not downloaded                                      │
│        ❯ Bash $ cargo run --bin hello                                     exit 0 · 3.0s│
│          Build and run the new binary                                                  │
│        │    Compiling hello v0.1.0 (/srv/work)                                         │
│        │     Finished `dev` profile in 2.98s                                           │
│        │      Running `target/debug/hello`                                             │
│        │ Hello, world!                                                                 │
│  12:02 ± Edit src/main.rs            updated +2 -1 · approved on device …c0ffee42 · 20s│
│        │  fn main() {                                                                  │
│        │ -    run();                                                                   │
│        │ +    let code = run();                                                        │
│        │ +    std::process::exit(code);                                                │
│        │  }                                                                            │
│        agent: Done: main now exits with run()'s code. A long reply wraps under its own │
│        column instead of falling back to the left edge.                                │
│────────────────────────────────────────────────────────────────────────────────────────│
│  12:04 device …c0ffee42: run the tests and clean up                                    │
│        ❯ Bash $ cargo test                                               exit 101 · 12s│
│        │ running 3 tests                                                               │
│        │ test tests::adds ... FAILED                                                   │
│        │ test tests::greets ... ok                                                     │
│        │ … +20 lines · full 2.0 KiB · not downloaded                                   │
│        ❯ Bash $ rm -rf target                                               denied here│
│          Clean the build directory                                                     │
│        agent: One test fails; I left target/ alone.                                    │
│────────────────────────────────────────────────────────────────────────────────────────│
│  12:06 you: write it up in NOTES.md                                                    │
│        ✎ Write NOTES.md                                          cancelled: interrupted│
│        interrupted · the agent stopped its turn                                        │
│────────────────────────────────────────────────────────────────────────────────────────│
│  12:08 you: fix the failing test                                                       │
│        ❯ Bash $ cargo test -- --nocapture                                   running 42s│
│        ± Edit src/lib.rs                                                 needs approval│
│        │ -    assert_eq!(add(2, 2), 5);                                                │
│        │ +    assert_eq!(add(2, 2), 4);                                                │
│┌ Approval needed ─────────────────────────────────────────────────────────────────────┐│
││± Edit src/lib.rs                                                                     ││
│││ -    assert_eq!(add(2, 2), 5);                                                      ││
│││ +    assert_eq!(add(2, 2), 4);                                                      ││
││Esc, then a approve · x deny                                                          ││
│└──────────────────────────────────────────────────────────────────────────────────────┘│
│┌ Message ─────────────────────────────────────────────────────────────────────────────┐│
││Write to the agent…                                                                   ││
│└──────────────────────────────────────────────────────────────────────────────────────┘│
└────────────────────────────────────────────────────────────────────────────────────────┘
Enter send · Ctrl-J newline · Ctrl-X interrupt · PgUp/PgDn · Esc timeline · F1 help"#;

#[test]
fn tool_timeline_mockup_at_90_columns() {
    let (h, other, _, _) = mockup();
    let frame = h.frame(90, 60, mockup_now());
    let screen = trimmed(&frame);
    assert_eq!(screen, MOCKUP, "\n{screen}");
    assert!(screen.contains(&tail(other)));

    // Colour carries the state: glyph, state and gutter alike.
    for (needle, colour) in [
        ("✎ Write src/bin/hello.rs", Color::Green),
        ("created · approved here", Color::Green),
        ("❯ Bash $ cargo test  ", Color::Red),
        ("exit 101", Color::Red),
        ("│ running 3 tests", Color::Red),
        ("denied here", Color::Magenta),
        ("cancelled: interrupted", Color::DarkGray),
        ("running 42s", Color::Cyan),
        ("needs approval", Color::Yellow),
    ] {
        assert_eq!(colour_of(&frame, needle), colour, "{needle}");
    }
    // Diffs are red and green by line; the error's output is red.
    assert_eq!(colour_of(&frame, "+    let code"), Color::Green);
    assert_eq!(colour_of(&frame, "-    run();"), Color::Red);
    assert_eq!(colour_of(&frame, "test tests::adds"), Color::Red);
    assert_eq!(colour_of(&frame, "Hello, world!"), Color::Reset);

    // The approval box names the call and shows its diff; no raw JSON.
    let start = screen.find("Approval needed").unwrap();
    let approval = &screen[start..screen.find(" Message ").unwrap()];
    assert!(approval.contains("± Edit src/lib.rs"), "{approval}");
    assert!(
        approval.contains("│ +    assert_eq!(add(2, 2), 4);"),
        "{approval}"
    );
    assert!(
        !approval.contains('{') && !approval.contains('"'),
        "{approval}"
    );
}

#[test]
fn a_running_call_counts_up() {
    let (h, _, _, _) = mockup();
    let later = mockup_now() + chrono::Duration::seconds(83);
    assert_contains(&trimmed(&h.frame(90, 58, later)), "running 2m05s");
    assert!(h.app.live(), "the event loop ticks for the clock");
}

/// From the newest item, `[` goes to the start of its turn, then back a
/// turn at a time; each lands on the turn's first selectable item with the
/// turn at the top.
fn select_the_write(h: &mut Harness) {
    h.key(KeyCode::Esc);
    h.key(KeyCode::Up);
    assert_contains(&h.screen(), ">       ± Edit src/lib.rs");
    // A turn starts at its message.
    h.key(KeyCode::Char('['));
    assert_contains(&h.screen(), "> 12:08 you: fix the failing test");
    h.key(KeyCode::Char('['));
    assert_contains(&h.screen(), "> 12:06 you: write it up in NOTES.md");
    h.key(KeyCode::Char('['));
    h.key(KeyCode::Char('['));
    let screen = h.screen();
    assert!(
        screen
            .lines()
            .nth(3)
            .unwrap()
            .starts_with("│> 12:01 you: add a hello binary"),
        "the turn is at the top, its message selected:\n{screen}"
    );
    h.key(KeyCode::Down);
    assert_contains(&h.screen(), ">       agent: I'll add the binary");
    h.key(KeyCode::Down);
    assert_contains(&h.screen(), ">       ✎ Write src/bin/hello.rs");
}

#[test]
fn turns_step_and_agent_messages_select() {
    let (mut h, _, _, _) = mockup();
    select_the_write(&mut h);
    h.key(KeyCode::Char(']'));
    let screen = h.screen();
    assert_contains(
        &line_with(&screen, "> 12:04 "),
        "run the tests and clean up",
    );
    assert_contains(screen.lines().nth(3).unwrap(), "────");
    // Agent messages expand to nothing.
    h.key(KeyCode::Up);
    assert_contains(&h.screen(), ">       agent: Done:");
    assert_eq!(h.key(KeyCode::Enter), None);
    assert!(h.app.ui().session.as_ref().unwrap().expanded.is_empty());
}

#[test]
fn an_expanded_file_shows_every_line_numbered() {
    let (mut h, _, file_blob, _) = mockup();
    select_the_write(&mut h);
    assert_eq!(h.key(KeyCode::Enter), Some(Action::DownloadBlob(file_blob)));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hello.rs");
    let content: String = (1..=10).map(|i| format!("line {i}\n")).collect();
    std::fs::write(&path, content).unwrap();
    h.seam
        .blob_status(file_blob, BlobStatus::Available { path })
        .unwrap();
    h.sync();
    let screen = trimmed(&h.frame(90, 58, mockup_now()));
    assert_contains(&screen, "│  1 line 1 ");
    assert_contains(&screen, "│ 10 line 10 ");
    assert!(!screen.contains("+2 lines"), "{screen}");
}

#[test]
fn expanding_above_the_screen_keeps_the_view_still() {
    let (mut h, _, file_blob, _) = mockup();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hello.rs");
    let content: String = (1..=30).map(|i| format!("line {i}\n")).collect();
    std::fs::write(&path, content).unwrap();
    h.seam
        .blob_status(file_blob, BlobStatus::Available { path })
        .unwrap();
    h.sync();
    select_the_write(&mut h);
    h.key(KeyCode::PageDown);
    let before = h.screen();
    assert!(!before.contains("✎ Write src/bin/hello.rs"), "{before}");
    assert_contains(&before, "scrolled back");
    let total = h.app.ui().session.as_ref().unwrap().geometry.borrow().total;
    h.key(KeyCode::Enter);
    let after = h.screen();
    let grown = h.app.ui().session.as_ref().unwrap().geometry.borrow().total;
    assert!(grown > total, "the Write grew: {total} → {grown}");
    // The Write's last line is cut by the top edge: that row now shows its
    // new end; everything from the next item down stays where it was.
    let rows = |screen: &str| {
        screen
            .lines()
            .skip(4)
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    assert_eq!(rows(&after), rows(&before), "{after}");
    assert_contains(after.lines().nth(3).unwrap(), "│ 30 line 30");
}

#[test]
fn wrapped_text_hangs_under_its_column() {
    let (h, _, _, _) = mockup();
    let screen = trimmed(&h.frame(60, 70, mockup_now()));
    // A message wraps under its text, a title under the title, a detail
    // and a preview line under their own columns.
    assert_contains(
        &screen,
        "│        agent: I'll add the binary, run it, then use it ",
    );
    assert_contains(&screen, "│        from main. ");
    // A title too long beside its state wraps under the title.
    assert_contains(
        &screen,
        "│        ✎ Write             created · approved here · 3.0s│",
    );
    assert_contains(&screen, "│          src/bin/hello.rs ");
    assert!(
        !screen
            .lines()
            .any(|line| line.starts_with("│instead") || line.starts_with("│column")),
        "{screen}"
    );
}

// ---- the approval panel shows what the gated call will do ----------------

/// `before` events, then `tool` gated by a pending approval; the open
/// session's harness and id, the approval and the call's seq.
fn gated(before: Vec<EventKind>, tool: ToolCall) -> (Harness, SessionId, ApprovalId, Seq) {
    let (mut h, id, _) = open_session();
    let approval = ApprovalId::new();
    let call = tool.call;
    let summary = tool.display.title.clone();
    let name = tool.name.clone();
    let mut kinds = before;
    kinds.push(EventKind::ToolCall(tool));
    kinds.push(EventKind::ApprovalRequested {
        id: approval,
        call,
        tool: name,
        summary,
        input: None,
    });
    let seq = Seq(h.seam.cursor().unwrap().0 + kinds.len() as u64 - 1);
    let events: Vec<Event> = kinds.into_iter().map(|kind| event(id, 1, kind)).collect();
    pull(&mut h, events);
    (h, id, approval, seq)
}

/// The colour of `needle`'s last occurrence: the approval panel's, when
/// the timeline shows it too.
fn last_colour_of(buffer: &Buffer, needle: &str) -> Color {
    let screen = text(buffer);
    let (y, line) = screen
        .lines()
        .enumerate()
        .filter(|(_, line)| line.contains(needle))
        .last()
        .unwrap_or_else(|| panic!("{needle:?} not on screen:\n{screen}"));
    let x = line[..line.find(needle).unwrap()].chars().count();
    buffer[(x as u16, y as u16)].fg
}

/// The approval panel's rows, borders excluded.
fn panel(screen: &str) -> Vec<String> {
    let lines: Vec<&str> = screen.lines().collect();
    let top = lines
        .iter()
        .position(|l| l.contains("Approval needed"))
        .unwrap_or_else(|| panic!("no approval panel:\n{screen}"));
    lines[top + 1..]
        .iter()
        .take_while(|l| !l.contains('└'))
        .map(|l| l.to_string())
        .collect()
}

fn numbered(n: usize) -> String {
    (1..=n).map(|i| format!("line {i}\n")).collect()
}

#[test]
fn approval_panel_shows_edit_diff() {
    let mut edit = call(CallId::new(), "Edit", ToolKind::FileEdit, "src/lib.rs");
    edit.display.body = body(BodyFormat::Diff, "@@ -1 +1 @@\n-old line\n+new line\n");
    let (h, _, _, _) = gated(vec![], edit);
    let frame = h.frame(90, 40, mockup_now());
    let rows = panel(&text(&frame));
    assert_contains(&rows[0], "± Edit src/lib.rs");
    assert_contains(&rows[1], "│ @@ -1 +1 @@");
    assert_contains(&rows[2], "│ -old line");
    assert_contains(&rows[3], "│ +new line");
    assert_contains(&rows[4], "Esc, then a approve · x deny");
    // It all fits: no note.
    assert_eq!(rows.len(), 5, "{rows:#?}");
    // Coloured as in the timeline.
    assert_eq!(last_colour_of(&frame, "+new line"), Color::Green);
    assert_eq!(last_colour_of(&frame, "-old line"), Color::Red);
    assert_eq!(last_colour_of(&frame, "@@ -1 +1 @@"), Color::Cyan);
}

#[test]
fn approval_panel_caps_rows() {
    let mut write = call(CallId::new(), "Write", ToolKind::FileWrite, "notes.txt");
    write.display.body = body(BodyFormat::Text, &numbered(30));
    let (h, _, _, _) = gated(vec![], write);
    // Ten rows at most, then what was left out.
    let rows = panel(&trimmed(&h.frame(90, 60, mockup_now())));
    assert_contains(&rows[10], "│ 10 line 10");
    assert_contains(&rows[11], "│ … +20 lines · v shows all");
    assert_eq!(rows.len(), 13, "{rows:#?}");
    // A third of the session when that is less: 20 rows inside, so 6.
    let rows = panel(&h.screen());
    assert_contains(&rows[6], "│  6 line 6");
    assert_contains(&rows[7], "│ … +24 lines · v shows all");
}

#[test]
fn v_reveals_gated_call() {
    let mut edit = call(CallId::new(), "Edit", ToolKind::FileEdit, "src/lib.rs");
    let diff: String = (1..=30).map(|i| format!("+added {i}\n")).collect();
    edit.display.body = body(BodyFormat::Diff, &diff);
    let before = (0..30).map(|i| agent(&format!("message {i}"))).collect();
    let (mut h, id, approval, seq) = gated(before, edit);
    h.key(KeyCode::Esc);
    assert_eq!(h.key(KeyCode::Char('v')), None, "nothing is downloaded");
    let ui = h.app.ui().session.as_ref().unwrap();
    assert_eq!(ui.selected, Some(ItemKey::Logged(seq)));
    assert!(ui.expanded.contains(&seq));
    // Taller than the timeline: its top is the first row.
    let screen = h.screen();
    assert_contains(screen.lines().nth(3).unwrap(), ">       ± Edit src/lib.rs");
    assert_contains(&screen, "v show the call");
    // Expanded, it shows every line where it fits.
    let screen = trimmed(&h.frame(100, 60, mockup_now()));
    assert_contains(&screen, ">       ± Edit src/lib.rs");
    assert_contains(&screen, "│ +added 30");

    // Answered elsewhere while reading: the panel goes, the selection stays.
    pull(
        &mut h,
        vec![event(
            id,
            2,
            EventKind::ApprovalResolved {
                id: approval,
                decision: Decision::Approve,
                by: DeviceId::new(),
            },
        )],
    );
    let screen = h.screen();
    assert!(!screen.contains("Approval needed"), "{screen}");
    assert_eq!(
        h.app.ui().session.as_ref().unwrap().selected,
        Some(ItemKey::Logged(seq))
    );
    assert_contains(&screen, ">       ± Edit src/lib.rs");
}

#[test]
fn long_shell_output_stays_visible() {
    let id = CallId::new();
    let mut bash = call(id, "Bash", ToolKind::Shell, "cargo build \\…");
    bash.display.body = body(BodyFormat::Text, "cargo build \\\n  --release");
    let (mut h, session, _, _) = gated(vec![], bash);
    let rows = panel(&h.screen());
    assert_contains(&rows[1], "│ cargo build \\");
    assert_contains(&rows[2], "│   --release");
    pull(
        &mut h,
        vec![event(
            session,
            2,
            EventKind::ToolResult(result(id, false, "exit 0", "Finished release")),
        )],
    );
    let screen = h.screen();
    assert_contains(&screen, "│   --release");
    assert_contains(&screen, "│ Finished release");
}

// ---- messages can be selected -----------------------------------------------

fn selected(h: &Harness) -> Option<ItemKey> {
    h.app.ui().session.as_ref().unwrap().selected
}

#[test]
fn user_messages_are_selectable() {
    let (mut h, id, me) = open_session();
    let phone = DeviceId::new();
    pull(
        &mut h,
        vec![
            event(id, 1, user(phone, "from the phone")),
            event(id, 2, agent("on it")),
            event(id, 3, user(me, "and this")),
        ],
    );
    h.type_text("not sent yet");
    h.key(KeyCode::Enter);
    let command = last_command(&h);
    h.key(KeyCode::Esc);
    h.key(KeyCode::Up);
    assert_eq!(selected(&h), Some(ItemKey::Pending(command)));
    assert_contains(&h.screen(), ">       you [queued]: not sent yet");
    // A message expands to nothing and has nothing to download.
    assert_eq!(h.key(KeyCode::Enter), None);
    assert_eq!(h.key(KeyCode::Char('d')), None);
    assert!(h.app.ui().session.as_ref().unwrap().expanded.is_empty());
    h.key(KeyCode::Up);
    assert_contains(&h.screen(), "> 12:03 you: and this");
    h.key(KeyCode::Up);
    assert_contains(&h.screen(), "> 12:02 agent: on it");
    h.key(KeyCode::Up);
    let line = line_with(&h.screen(), "from the phone");
    assert_contains(&line, "> 12:01 ");
    assert_contains(&line, &tail(phone));
    assert_eq!(h.key(KeyCode::Enter), None);
}

#[test]
fn selection_follows_pending_to_logged() {
    let (mut h, id, me) = open_session();
    pull(&mut h, vec![event(id, 1, agent("ready"))]);
    h.type_text("hello");
    h.key(KeyCode::Enter);
    let command = last_command(&h);
    h.key(KeyCode::Esc);
    h.key(KeyCode::Up);
    assert_eq!(selected(&h), Some(ItemKey::Pending(command)));

    // Accepted but not pulled yet: still pending.
    let seq = Seq(h.seam.cursor().unwrap().0 + 1);
    h.seam
        .command_result(command, &CommandResult::Accepted { seq })
        .unwrap();
    h.sync();
    assert_eq!(selected(&h), Some(ItemKey::Pending(command)));

    pull(
        &mut h,
        vec![event(
            id,
            2,
            EventKind::UserMessage {
                command_id: command,
                device_id: me,
                text: "hello".into(),
            },
        )],
    );
    assert_eq!(selected(&h), Some(ItemKey::Logged(seq)));
    assert_contains(&h.screen(), "> 12:02 you [accepted]: hello");
    // And moves on from there.
    h.key(KeyCode::Up);
    assert_contains(&h.screen(), "> 12:01 agent: ready");
}

/// The app's view with `key` taken out of the open session's timeline, as
/// if the message were discarded.
fn without(h: &Harness, key: ItemKey) -> ViewState {
    let mut view = h.app.view().clone();
    let Screen::Session(session) = &mut view.screen else {
        panic!("no session open");
    };
    session.timeline.retain(|item| ItemKey::of(item) != key);
    view
}

#[test]
fn selection_moves_to_neighbour_when_pending_vanishes() {
    let (mut h, id, _) = open_session();
    pull(&mut h, vec![event(id, 1, agent("ready"))]);
    h.type_text("first");
    h.key(KeyCode::Enter);
    let first = last_command(&h);
    h.type_text("second");
    h.key(KeyCode::Enter);
    let second = last_command(&h);
    h.key(KeyCode::Esc);
    h.key(KeyCode::Up);
    h.key(KeyCode::Up);
    assert_eq!(selected(&h), Some(ItemKey::Pending(first)));

    // The next item takes its place; at the end, the one before.
    let view = without(&h, ItemKey::Pending(first));
    h.app.set_view(view, h.t0);
    assert_eq!(selected(&h), Some(ItemKey::Pending(second)));
    assert_contains(&h.screen(), ">       you [queued]: second");
    let view = without(&h, ItemKey::Pending(second));
    h.app.set_view(view, h.t0);
    assert_eq!(selected(&h), Some(ItemKey::Logged(Seq(2))));
    // Nothing left to select.
    let view = without(&h, ItemKey::Logged(Seq(2)));
    h.app.set_view(view, h.t0);
    assert_eq!(selected(&h), None);
}

#[test]
fn turn_stepping_lands_on_the_user_message() {
    let (mut h, id, me) = open_session();
    let phone = DeviceId::new();
    pull(
        &mut h,
        vec![
            event(id, 1, user(me, "one")),
            event(id, 1, agent("a")),
            event(id, 2, user(phone, "two")),
            event(id, 2, agent("b")),
        ],
    );
    h.type_text("three");
    h.key(KeyCode::Enter);
    let three = last_command(&h);
    h.key(KeyCode::Esc);
    h.key(KeyCode::Up);
    h.key(KeyCode::Up);
    assert_contains(&h.screen(), ">       agent: b");
    // Back from inside a turn: its message; from there, the turn before.
    h.key(KeyCode::Char('['));
    assert_eq!(selected(&h), Some(ItemKey::Logged(Seq(4))));
    h.key(KeyCode::Char('['));
    assert_eq!(selected(&h), Some(ItemKey::Logged(Seq(2))));
    assert_contains(&h.screen(), "> 12:01 you: one");
    h.key(KeyCode::Char(']'));
    assert_eq!(selected(&h), Some(ItemKey::Logged(Seq(4))));
    // A pending message starts a turn too.
    h.key(KeyCode::Char(']'));
    assert_eq!(selected(&h), Some(ItemKey::Pending(three)));
}

/// The footer line of the screen.
fn footer(h: &Harness) -> String {
    h.screen().lines().last().unwrap().trim_end().to_owned()
}

#[test]
fn edit_and_cancel_a_queued_message() {
    let (mut h, _, _) = open_session();
    let mut sent = Vec::new();
    for text in ["first", "second", "third"] {
        h.type_text(text);
        h.key(KeyCode::Enter);
        sent.push(last_command(&h));
    }
    h.type_text("unsaved");
    h.key(KeyCode::Esc);
    h.key(KeyCode::Up);
    assert_eq!(selected(&h), Some(ItemKey::Pending(sent[2])));
    assert!(
        footer(&h).starts_with("e edit · Del cancel"),
        "{}",
        footer(&h)
    );

    // `e` saves the composer, then takes the message back into it, ahead of
    // what was typed, focused, the cursor after the message.
    assert_eq!(h.key(KeyCode::Char('e')), Some(Action::EditQueued(sent[2])));
    let session = h.app.ui().session.as_ref().unwrap();
    assert_eq!(session.focus, skywave_tui::app::Focus::Composer);
    assert_eq!(session.composer.text(), "third\n\nunsaved");
    assert_eq!(session.composer.cursor_chars(), 5);
    assert_eq!(h.core_draft().unwrap().text, "third\n\nunsaved");
    assert!(!h.screen().contains("you [queued]: third"));
    assert_eq!(h.seam.pending_outbox().unwrap().len(), 2);

    // The selection went to the neighbour; Delete cancels it.
    h.key(KeyCode::Esc);
    assert_eq!(selected(&h), Some(ItemKey::Pending(sent[1])));
    assert_eq!(h.key(KeyCode::Delete), Some(Action::Discard(sent[1])));
    assert!(!h.screen().contains("you [queued]: second"));
    assert_eq!(selected(&h), Some(ItemKey::Pending(sent[0])));
    assert_eq!(
        h.app.composer().unwrap().text(),
        "third\n\nunsaved",
        "a discard leaves the composer alone"
    );

    // Once sent it may have reached the server: no hint, and the keys do
    // nothing, also once it is back in the queue.
    h.seam.command_sending(sent[0]).unwrap();
    h.sync();
    assert!(!footer(&h).contains("e edit"), "{}", footer(&h));
    assert_eq!(h.key(KeyCode::Char('e')), None);
    assert_eq!(h.key(KeyCode::Delete), None);
    let mut h = h.restart();
    assert_eq!(h.seam.pending_outbox().unwrap().len(), 1);

    // A refused message can be dismissed.
    h.key(KeyCode::Enter);
    h.key(KeyCode::Esc);
    h.key(KeyCode::Up);
    h.seam
        .command_result(
            sent[0],
            &CommandResult::Rejected {
                reason: FailReason::UnknownSession,
            },
        )
        .unwrap();
    h.sync();
    assert!(
        footer(&h).starts_with("e edit · Del dismiss"),
        "{}",
        footer(&h)
    );
    assert_eq!(h.key(KeyCode::Delete), Some(Action::Discard(sent[0])));
    assert!(h.seam.pending_outbox().unwrap().is_empty());
    assert!(!h.screen().contains("first"));
}

#[test]
fn dismiss_refused_creation() {
    let mut h = Harness::paired();
    for workdir in ["/etc", "/srv/new"] {
        h.key(KeyCode::Char('n'));
        h.app.on_paste(workdir);
        h.key(KeyCode::Enter);
    }
    let outbox = h.seam.pending_outbox().unwrap();
    let (refused, waiting) = (outbox[0].command.id, outbox[1].command.id);
    // Nothing refused yet: Delete does nothing to a queued creation.
    assert!(!h.screen().contains("Del dismiss"));
    assert_eq!(h.key(KeyCode::Delete), None);

    h.seam.command_sending(refused).unwrap();
    h.seam
        .command_result(
            refused,
            &CommandResult::Rejected {
                reason: FailReason::WorkdirNotAllowed,
            },
        )
        .unwrap();
    h.sync();
    assert_contains(&h.screen(), "Creating · Del dismiss");
    assert_eq!(h.key(KeyCode::Delete), Some(Action::Discard(refused)));
    let screen = h.screen();
    assert!(!screen.contains("/etc"), "{screen}");
    assert!(!screen.contains("Del dismiss"));
    assert_contains(&line_with(&screen, "/srv/new"), "[queued]");
    assert_eq!(h.key(KeyCode::Delete), None);
    let left: Vec<CommandId> = h
        .seam
        .pending_outbox()
        .unwrap()
        .iter()
        .map(|e| e.command.id)
        .collect();
    assert_eq!(left, [waiting]);
}

// ---- delivery detail --------------------------------------------------------

/// The delivery of the own message `key` names, as the core shows it.
fn delivery_of(h: &Harness, key: ItemKey) -> Delivery {
    let (view, _) = h.app.session().unwrap();
    let index = find(view, key).unwrap();
    delivery(&view.timeline[index]).unwrap().1
}

/// The app's view with the delivery of `key`'s message replaced, so the
/// times on screen are fixed.
fn with_delivery(h: &Harness, key: ItemKey, new: Delivery) -> ViewState {
    let mut view = h.app.view().clone();
    let Screen::Session(session) = &mut view.screen else {
        panic!("no session open");
    };
    let index = find(session, key).unwrap();
    match &mut session.timeline[index] {
        TimelineItem::Pending { delivery, .. }
        | TimelineItem::Logged {
            entry: Entry::UserMessage { delivery, .. },
            ..
        } => *delivery = Some(new),
        other => panic!("not a message: {other:?}"),
    }
    view
}

/// The screen line after the one line containing `needle`, without the
/// border and indent.
fn line_after(screen: &str, needle: &str) -> String {
    let line = line_with(screen, needle);
    let mut lines = screen.lines().skip_while(|l| *l != line);
    lines.next();
    let next = lines.next().unwrap_or_default();
    next.trim_matches(|c: char| c == '│' || c.is_whitespace())
        .to_owned()
}

fn ms(ms: i64) -> chrono::Duration {
    chrono::Duration::milliseconds(ms)
}

#[test]
fn selected_own_message_shows_delivery_detail() {
    let (mut h, id, me) = open_session();
    let phone = DeviceId::new();
    pull(&mut h, vec![event(id, 1, user(phone, "from the phone"))]);
    h.type_text("hello");
    h.key(KeyCode::Enter);
    let command = last_command(&h);
    h.seam.command_sending(command).unwrap();
    let seq = Seq(h.seam.cursor().unwrap().0 + 1);
    h.seam
        .command_result(command, &CommandResult::Accepted { seq })
        .unwrap();
    pull(
        &mut h,
        vec![event(
            id,
            2,
            EventKind::UserMessage {
                command_id: command,
                device_id: me,
                text: "hello".into(),
            },
        )],
    );
    // Only a selected message shows it.
    assert!(!h.screen().contains("queued"), "{}", h.screen());
    h.key(KeyCode::Esc);
    h.key(KeyCode::Up);
    assert_eq!(selected(&h), Some(ItemKey::Logged(seq)));
    let real = delivery_of(&h, ItemKey::Logged(seq));
    assert_eq!(real.tries, 1);
    assert!(real.first_sent_at.is_some() && real.answered_at.is_some());
    assert!(line_after(&h.screen(), "you [accepted]: hello").starts_with("queued "));

    // What each step took, from the queueing, by this device's clock.
    let queued_at = at_s(2, 11);
    let resent = Delivery {
        queued_at,
        first_sent_at: Some(queued_at + ms(3200)),
        answered_at: Some(queued_at + ms(8100)),
        tries: 2,
    };
    let view = with_delivery(&h, ItemKey::Logged(seq), resent);
    h.app.set_view(view, h.t0);
    assert_eq!(
        line_after(&h.screen(), "you [accepted]: hello"),
        "queued 12:02:11 · sent +3.2s · 2 tries · answered +8.1s"
    );
    // Sent before send times were recorded: no time for it.
    let old = Delivery {
        first_sent_at: None,
        tries: 1,
        ..resent
    };
    let view = with_delivery(&h, ItemKey::Logged(seq), old);
    h.app.set_view(view, h.t0);
    assert_eq!(
        line_after(&h.screen(), "you [accepted]: hello"),
        "queued 12:02:11 · sent · answered +8.1s"
    );

    // Another device's message has none.
    h.key(KeyCode::Up);
    assert_contains(&h.screen(), "> 12:01 ");
    assert!(!h.screen().contains("queued"), "{}", h.screen());

    // A refused message says how long the refusal took.
    h.key(KeyCode::Tab);
    h.type_text("again");
    h.key(KeyCode::Enter);
    let refused = last_command(&h);
    h.seam.command_sending(refused).unwrap();
    h.seam
        .command_result(
            refused,
            &CommandResult::Rejected {
                reason: FailReason::UnknownSession,
            },
        )
        .unwrap();
    h.sync();
    h.key(KeyCode::Esc);
    h.key(KeyCode::Down);
    h.key(KeyCode::Down);
    assert_eq!(selected(&h), Some(ItemKey::Pending(refused)));
    let quick = Delivery {
        queued_at: at_s(3, 0),
        first_sent_at: Some(at_s(3, 0) + ms(500)),
        answered_at: Some(at_s(3, 0) + ms(2100)),
        tries: 1,
    };
    let view = with_delivery(&h, ItemKey::Pending(refused), quick);
    h.app.set_view(view, h.t0);
    assert_eq!(
        line_after(&h.screen(), "]: again"),
        "queued 12:03:00 · sent +0.5s · refused +2.1s"
    );
}

#[test]
fn pending_message_shows_try_count() {
    let (mut h, _, _) = open_session();
    h.type_text("hello");
    h.key(KeyCode::Enter);
    let command = last_command(&h);
    h.key(KeyCode::Esc);
    h.key(KeyCode::Up);
    let key = ItemKey::Pending(command);
    assert_eq!(selected(&h), Some(key));
    let later = delivery_of(&h, key).queued_at + chrono::Duration::seconds(130);
    let screen = trimmed(&h.frame(100, 24, later));
    assert_eq!(
        line_after(&screen, "you [queued]: hello"),
        "queued 2m10s ago"
    );

    for _ in 0..3 {
        assert!(h.seam.command_sending(command).unwrap());
    }
    h.sync();
    let screen = trimmed(&h.frame(100, 24, later));
    assert_eq!(
        line_after(&screen, "you [sending]: hello"),
        "queued 2m10s ago · sending, try 3"
    );
}

#[test]
fn attempted_queued_message_says_sent_once() {
    let (mut h, _, _) = open_session();
    h.type_text("hello");
    h.key(KeyCode::Enter);
    let command = last_command(&h);
    h.seam.command_sending(command).unwrap();
    // A restart puts it back in the queue, sent once.
    let mut h = h.restart();
    h.key(KeyCode::Enter);
    h.key(KeyCode::Esc);
    h.key(KeyCode::Up);
    let key = ItemKey::Pending(command);
    assert_eq!(selected(&h), Some(key));
    let later = delivery_of(&h, key).queued_at + chrono::Duration::seconds(7);
    let screen = trimmed(&h.frame(100, 24, later));
    assert_eq!(
        line_after(&screen, "you [queued]: hello"),
        "queued 7s ago · sent once, waiting for the server"
    );
    // Which is why it can no longer be taken back.
    assert!(!footer(&h).contains("e edit"), "{}", footer(&h));
}

#[test]
fn delivery_line_ticks_while_selected() {
    let (mut h, _, _) = open_session();
    h.type_text("hello");
    h.key(KeyCode::Enter);
    let command = last_command(&h);
    assert!(!h.app.live(), "nothing selected, nothing counts");
    h.key(KeyCode::Esc);
    h.key(KeyCode::Up);
    let key = ItemKey::Pending(command);
    assert_eq!(selected(&h), Some(key));
    assert!(h.app.live(), "the event loop ticks for the count");
    let queued_at = delivery_of(&h, key).queued_at;
    for seconds in [10, 20] {
        let wall = queued_at + chrono::Duration::seconds(seconds);
        assert_eq!(
            line_after(&trimmed(&h.frame(100, 24, wall)), "you [queued]: hello"),
            format!("queued {seconds}s ago")
        );
    }
    // Not shown while typing: nothing to count.
    h.key(KeyCode::Tab);
    assert!(!h.app.live());
    h.key(KeyCode::Tab);
    assert!(h.app.live());
    // Answered: the times are final.
    let seq = Seq(h.seam.cursor().unwrap().0 + 1);
    h.seam
        .command_result(command, &CommandResult::Accepted { seq })
        .unwrap();
    h.sync();
    assert!(!h.app.live());
}

#[test]
fn approval_answer_says_how_long_it_waited() {
    let (mut h, _, _, _) = gated(
        Vec::new(),
        call(CallId::new(), "Bash", ToolKind::Shell, "cargo test"),
    );
    h.key(KeyCode::Esc);
    h.key(KeyCode::Char('a'));
    let command = last_command(&h);
    assert!(h.app.live(), "the answer's wait counts up");
    let answer = |h: &Harness| {
        let (view, _) = h.app.session().unwrap();
        view.pending_approval.as_ref().unwrap().answer.unwrap()
    };
    let queued_at = answer(&h).delivery.unwrap().queued_at;
    let wall = queued_at + chrono::Duration::seconds(40);
    assert_contains(
        &trimmed(&h.frame(100, 30, wall)),
        "you approved · queued 40s ago",
    );
    h.seam.command_sending(command).unwrap();
    h.sync();
    assert_contains(
        &trimmed(&h.frame(100, 30, wall)),
        "you approved · queued 40s ago · sending",
    );
}
