//! The TUI against a real core, without a terminal or a network: keys go in
//! through `handle_key`, actions go to `Core::dispatch`, engine reports go
//! in through `core.seam()`, and frames are checked on ratatui's
//! `TestBackend`.

use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use chrono::{DateTime, TimeDelta, TimeZone, Utc};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::Color;
use skywave_core::workdir::{WorkdirAnswer, WorkdirAsk};
use skywave_core::{
    Action, ApprovalId, BlobId, BlobPolicy, BlobStatus, CommandId, ComposerMode, Core, Decision,
    Delivery, DeviceId, Draft, Effort, Entry, Link, LinkRejection, PairedCredentials, PairedLink,
    Request, Route, RunWhen, Screen, Seam, Seq, SessionId, TimelineItem, ViewState, WorkdirPane,
};
use skywave_present::format::took_label;
use skywave_present::sections::{ItemKey, delivery, find};
use skywave_proto::{
    BlobRef, BlobUse, Body, BodyFormat, CallId, ChangeSet, ChangedFile, ChangesResult,
    CommandResult, DeviceInfo, DiffBody, DirEntry, DirsResult, EntryKind, Event, EventBatch,
    EventKind, FileBody, FileChange, FileResult, Full, HelloResponse, LineSpan, LinkTarget,
    ListResult, ModelChoice, PermissionMode, RejectReason, RuleScope, RuleSuggestion,
    SessionState as LogState, ShellEnd, ToolCall, ToolDisplay, ToolKind, ToolResult, TurnUsage,
    limits,
};
use skywave_tui::App;
use skywave_tui::app::{Anchor, DRAFT_SAVE_AFTER, Field, READ_MARK_EVERY};
use skywave_tui::clipboard::{self, Clip};
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
        let app = App::new(views.recv().unwrap());
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
        let Harness {
            dir,
            core,
            seam,
            app,
            ..
        } = self;
        // The seam holds the core too: only the last handle frees the dir.
        drop((core, seam, app));
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

    /// Makes every draft save fail (`true`), as a full disk would, or work
    /// again.
    fn fail_draft_save(&self, fail: bool) {
        let db = self.dir.path().join(skywave_core::store::DB_FILE);
        let conn = rusqlite::Connection::open(db).unwrap();
        conn.execute_batch(if fail {
            "CREATE TRIGGER full_drafts BEFORE INSERT ON drafts BEGIN SELECT RAISE(ABORT, 'disk full'); END;"
        } else {
            "DROP TRIGGER full_drafts"
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
        h.seam
            .pairing_succeeded(&"srv:7443".parse().unwrap(), credentials())
            .unwrap();
        h.sync();
        h.drain_requests();
        h
    }

    /// Takes the snapshots the core pushed, then asks for what expanded
    /// items need, as the event loop does; until a fetch brings no new
    /// snapshot.
    fn sync(&mut self) {
        let mut fetched = false;
        loop {
            match self.views.try_recv() {
                Ok(view) => {
                    self.app.set_view(view);
                    fetched = false;
                }
                Err(TryRecvError::Empty) if fetched => return,
                Err(TryRecvError::Empty) => {
                    skywave_tui::fetch_expanded(&mut self.app, &self.core);
                    fetched = true;
                }
                Err(TryRecvError::Disconnected) => panic!("core gone"),
            }
        }
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

    /// Types `text` into a workdir prompt: each key asks the core to
    /// complete the input so far.
    fn type_workdir(&mut self, text: &str) {
        for c in text.chars() {
            let mut input = workdir_input(self);
            input.push(c);
            assert_eq!(
                self.key(KeyCode::Char(c)),
                Some(Action::CompleteWorkdir { input })
            );
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

    fn screen_at(&self, wall: DateTime<Utc>) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal
            .draw(|frame| ui::render_at(frame, &self.app, wall.into()))
            .unwrap();
        text(terminal.backend().buffer())
    }

    /// A `width` × `height` frame with the wall clock at `wall`, so running
    /// calls' clocks are fixed.
    fn frame(&self, width: u16, height: u16, wall: DateTime<Utc>) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| ui::render_at(frame, &self.app, wall.into()))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn screen(&self) -> String {
        self.screen_at(mockup_now())
    }

    fn banner_at(&self, wall: DateTime<Utc>) -> String {
        self.screen_at(wall)
            .lines()
            .next()
            .unwrap()
            .trim_end()
            .to_owned()
    }

    fn banner(&self) -> String {
        self.banner_at(mockup_now())
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
        command_id: kind.logged_by_command().then(CommandId::new),
        context: skywave_proto::ContextId::first_of(session_id),
        kind,
    }
}

/// A message `command` sent from `device_id`, as the log holds it.
fn message(
    session_id: SessionId,
    minute: u32,
    command: CommandId,
    device_id: DeviceId,
    text: impl Into<String>,
) -> Event {
    Event {
        command_id: Some(command),
        ..event(
            session_id,
            minute,
            EventKind::UserMessage {
                device_id,
                text: text.into(),
                attachments: Vec::new(),
            },
        )
    }
}

fn created(session_id: SessionId, minute: u32, workdir: &str) -> Event {
    event(
        session_id,
        minute,
        EventKind::SessionCreated {
            workdir: workdir.into(),
            context_name: "start".into(),
            from: None,
            worktree: None,
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
    // Unpaired: nothing to connect to, so the banner claims nothing.
    assert_eq!(h.app.view().link, Link::Unpaired);
    assert_eq!(h.banner(), "");
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
            address: "srv:7443".parse().unwrap(),
            code: "ABCDE12345".into(),
            fingerprint: FP.into(),
        }]
    );
    assert_contains(&h.screen(), "Pairing with srv:7443…");

    h.seam
        .pairing_succeeded(&"srv:7443".parse().unwrap(), credentials())
        .unwrap();
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
        "skywave://pair?addr=%5Bfd00::7%5D:7443&code=abcde-12345&fp={}",
        "0123456789abcdef".repeat(4)
    );
    let mut h = Harness::new();
    h.app.on_paste(&uri);
    let form = &h.app.ui().form;
    assert_eq!(form.address, "[fd00::7]:7443");
    assert_eq!(form.code, "ABCDE12345");
    assert!(form.fingerprint.starts_with("01:23:45:67:89:AB:CD:EF:01"));
    assert_eq!(form.fingerprint.len(), 64 + 31);
    assert!(matches!(h.key(KeyCode::Enter), Some(Action::Pair { .. })));

    // Without bracketed paste the URI arrives as keys in one field.
    let mut h = Harness::new();
    h.type_text(&uri);
    assert_eq!(h.key(KeyCode::Enter), None);
    assert_eq!(h.app.ui().form.address, "[fd00::7]:7443");
    assert_eq!(h.app.ui().form.code, "ABCDE12345");

    // Anything that is not a pairing URI is just text.
    let mut h = Harness::new();
    h.app.on_paste(" srv:7443\n");
    assert_eq!(h.app.ui().form.address, "srv:7443");
}

#[test]
fn a_malformed_pairing_uri_is_reported_not_typed() {
    let uri = "skywave://pair?addr=srv:7443&code=ABCDE12345";
    let mut h = Harness::new();
    h.app.on_paste(uri);
    assert_eq!(h.app.ui().form.address, "");
    assert_contains(&h.screen(), "error: not a pairing link: missing fp");

    // Typed, Enter reports it and keeps the text to fix.
    let mut h = Harness::new();
    h.type_text(uri);
    assert_eq!(h.key(KeyCode::Enter), None);
    assert_eq!(h.app.ui().form.address, uri);
    assert_eq!(h.app.ui().form.focus, Field::Address);
    assert_contains(&h.screen(), "error: not a pairing link: missing fp");
}

#[test]
fn banner_shows_every_link_state() {
    let mut h = Harness::paired();
    let cases = [
        (PairedLink::Connecting, "connecting…"),
        (PairedLink::Online { latency_ms: 42 }, "online · 42 ms"),
        (
            PairedLink::CatchingUp {
                have: Seq(120),
                server: Seq(450),
            },
            "syncing · 120/450",
        ),
        (
            PairedLink::Rejected(LinkRejection::Revoked),
            "device revoked · re-pair to reconnect",
        ),
        (
            PairedLink::Rejected(LinkRejection::ServerChanged),
            "server identity changed · re-pair to reconnect · Ctrl-R retry",
        ),
        (
            PairedLink::Rejected(LinkRejection::NameMismatch),
            "server name mismatch · re-pair to reconnect · Ctrl-R retry",
        ),
        (
            PairedLink::Rejected(LinkRejection::BadIdentity),
            "device identity unusable · re-pair to reconnect",
        ),
        (
            PairedLink::Rejected(LinkRejection::ProtocolMismatch {
                client: 1,
                server: 2,
            }),
            "protocol mismatch · client v1, server v2 · update, then Ctrl-R",
        ),
        (
            PairedLink::Rejected(LinkRejection::HistoryLost {
                have: Seq(9),
                server: Seq(4),
            }),
            "server lost history (had 9, server at 4) · sync stopped · restart with --reset-replica to start over",
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
            conversation: None,
            workdir: "/srv/a".into(),
            worktree: None,
        })
        .unwrap();
    h.core
        .dispatch(Action::CreateSession {
            conversation: None,
            workdir: "/srv/b".into(),
            worktree: None,
        })
        .unwrap();
    // The killed server: the engine reports it is backing off.
    let t0 = mockup_now();
    h.seam.set_link(PairedLink::Reconnecting {
        attempt: 3,
        retry_at: t0 + TimeDelta::seconds(5),
        offline_since: t0,
        reason: skywave_core::LinkFailure::Unreachable,
    });
    h.sync();
    assert_eq!(
        h.banner_at(t0),
        "offline · server unreachable · 2 queued · retry in 05s · Ctrl-R now (attempt 3)"
    );
    assert_eq!(
        h.banner_at(t0 + TimeDelta::milliseconds(2500)),
        "offline · server unreachable · 2 queued · retry in 03s · Ctrl-R now (attempt 3)"
    );
    // Another snapshot in the same wait does not restart the countdown.
    h.core
        .dispatch(Action::CreateSession {
            conversation: None,
            workdir: "/srv/c".into(),
            worktree: None,
        })
        .unwrap();
    h.sync();
    assert_eq!(
        h.banner_at(t0 + TimeDelta::milliseconds(2500)),
        "offline · server unreachable · 3 queued · retry in 03s · Ctrl-R now (attempt 3)"
    );
    assert_eq!(
        h.banner_at(t0 + TimeDelta::seconds(6)),
        "offline · server unreachable · 3 queued · retrying… (attempt 3)"
    );
    // Local data still renders under the banner.
    assert_contains(&line_with(&h.screen(), "/srv/a"), "[queued]");

    // Ctrl-R asks the engine to stop waiting.
    h.drain_requests();
    assert_eq!(h.key_with(ctrl('r')), Some(Action::RetryNow));
    assert_eq!(h.drain_requests(), vec![Request::RetryNow]);

    // A new attempt counts down to its own deadline.
    let later = t0 + TimeDelta::seconds(10);
    h.seam.set_link(PairedLink::Reconnecting {
        attempt: 4,
        retry_at: later + TimeDelta::seconds(10),
        offline_since: t0,
        reason: skywave_core::LinkFailure::ServerError(503),
    });
    h.sync();
    // It says why this attempt failed, not the first one.
    assert_eq!(
        h.banner_at(later + TimeDelta::seconds(1)),
        "offline · server error 503 · 3 queued · retry in 09s · Ctrl-R now (attempt 4)"
    );

    // Back online, the queue drains; the count stays until results arrive.
    h.seam.set_link(PairedLink::Online { latency_ms: 80 });
    h.sync();
    assert_eq!(h.banner(), "online · 80 ms · 3 queued");
}

#[test]
fn settings_change_the_server_address_without_re_pairing() {
    let mut h = Harness::paired();
    h.key(KeyCode::Char('s'));
    assert_contains(&h.screen(), "Press a to change the server address");

    // The prompt starts from the known address; Esc leaves it as it was.
    assert_eq!(h.key(KeyCode::Char('a')), None);
    assert_eq!(h.app.ui().prompt.as_ref().unwrap().text, "srv:7443");
    assert_contains(&h.screen(), "Server address");
    assert_eq!(h.key(KeyCode::Esc), None);
    assert!(h.app.ui().prompt.is_none());

    // Not an address: shown, and the text stays to fix.
    h.key(KeyCode::Char('a'));
    h.key_with(ctrl('u'));
    h.type_text("host:port");
    assert_eq!(h.key(KeyCode::Enter), None);
    assert_contains(&h.screen(), "invalid server address");
    assert_eq!(h.app.ui().prompt.as_ref().unwrap().text, "host:port");

    // A new one reconnects there; the device stays paired.
    h.key_with(ctrl('u'));
    h.drain_requests();
    h.type_text("10.0.0.7:7443");
    assert_eq!(
        h.key(KeyCode::Enter),
        Some(Action::SetServerAddress {
            address: "10.0.0.7:7443".into(),
        })
    );
    assert!(h.app.ui().prompt.is_none(), "the prompt closes");
    assert_eq!(h.drain_requests(), vec![Request::Connect]);
    let screen = h.screen();
    assert_contains(&screen, "Server:      10.0.0.7:7443");
    assert!(matches!(h.app.view().screen, Screen::Settings(_)));
}

#[test]
fn revoked_device_gets_a_full_screen_state_with_re_pair() {
    let mut h = Harness::paired();
    h.seam
        .set_link(PairedLink::Rejected(LinkRejection::Revoked));
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
    assert_eq!(h.key(KeyCode::Esc), Some(Action::Back));

    // From the full-screen state, p goes straight to pairing.
    h.seam.set_link(PairedLink::Connecting);
    h.sync();
    h.seam
        .set_link(PairedLink::Rejected(LinkRejection::Revoked));
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
    h.seam
        .pairing_succeeded(&"srv:7443".parse().unwrap(), credentials())
        .unwrap();
    h.sync();
    assert!(matches!(h.app.view().screen, Screen::SessionList(_)));
    assert_eq!(h.banner(), "connecting…");
}

/// A server with another CA, an address its certificate dropped, or a
/// damaged identity gets the same full-screen re-pair state as a
/// revoke, saying which; where a fix on the server may do, Ctrl-R tries
/// again. A protocol mismatch is no re-pair matter.
#[test]
fn identity_faults_get_the_re_pair_screen_saying_why() {
    let mut h = Harness::paired();
    h.seam
        .set_link(PairedLink::Rejected(LinkRejection::ServerChanged));
    h.sync();
    let screen = h.screen();
    assert_contains(&screen, "The server's identity changed.");
    assert_contains(&screen, "try again");
    h.drain_requests();
    assert_eq!(h.key_with(ctrl('r')), Some(Action::RetryNow));
    assert_eq!(h.drain_requests(), vec![Request::Connect]);

    h.seam
        .set_link(PairedLink::Rejected(LinkRejection::NameMismatch));
    h.sync();
    assert_contains(&h.screen(), "does not cover this address");

    h.seam
        .set_link(PairedLink::Rejected(LinkRejection::BadIdentity));
    h.sync();
    let screen = h.screen();
    assert_contains(&screen, "This device's identity is damaged.");
    assert!(!screen.contains("try again"), "{screen}");
    assert_eq!(
        h.key(KeyCode::Char('p')),
        Some(Action::Navigate(Route::Pairing))
    );

    let mut h = Harness::paired();
    h.seam
        .set_link(PairedLink::Rejected(LinkRejection::ProtocolMismatch {
            client: 1,
            server: 2,
        }));
    h.sync();
    assert_contains(&h.screen(), "No sessions yet.");
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
    assert_contains(&screen, "Ctrl-C");
    // Too long for 24 rows: the rest is a scroll away.
    h.key(KeyCode::Char('G'));
    assert_contains(&h.screen(), "agent blocked on an approval");
    // Keys do nothing else while help is open.
    assert_eq!(h.key(KeyCode::Char('s')), None);
    h.key(KeyCode::Char('?'));
    assert!(!h.app.ui().help);
    assert!(!h.screen().contains("open session"));
}

/// The help overlay in an 80×24 frame: its rows inside the border, and
/// its bottom border.
fn help_box(h: &Harness) -> (Vec<String>, String) {
    let screen = text(&h.frame(80, 24, mockup_now()));
    let lines: Vec<Vec<char>> = screen.lines().map(|l| l.chars().collect()).collect();
    let top = lines
        .iter()
        .position(|l| l.iter().collect::<String>().contains("┌ Keys "))
        .unwrap_or_else(|| panic!("no help overlay:\n{screen}"));
    let left = lines[top].iter().position(|&c| c == '┌').unwrap();
    let right = left + lines[top][left..].iter().position(|&c| c == '┐').unwrap();
    let bottom = top + lines[top..].iter().position(|l| l[left] == '└').unwrap();
    let rows = lines[top + 1..bottom]
        .iter()
        .map(|l| l[left + 1..right].iter().collect::<String>())
        .collect();
    (rows, lines[bottom][left..=right].iter().collect())
}

/// Opens help with F1, scrolls it to its end with `j`, closes it with Esc:
/// every line it showed, in order. Each `j` scrolls one line until the
/// last line is shown, so nothing is skipped, and the border says when
/// there is more below.
fn read_help(h: &mut Harness) -> Vec<String> {
    h.key(KeyCode::F(1));
    assert!(h.app.ui().help);
    let (mut rows, mut border) = help_box(h);
    assert_contains(&rows[0], "Everywhere");
    let mut seen = rows.clone();
    loop {
        assert_eq!(h.key(KeyCode::Char('j')), None);
        let (next, next_border) = help_box(h);
        if next == rows {
            assert!(!border.contains("below"), "{border}");
            break;
        }
        assert_contains(&border, "below · j/k scroll");
        assert_eq!(next[..next.len() - 1], rows[1..], "one line at a time");
        assert_contains(&next_border, "more above");
        seen.push(next.last().unwrap().clone());
        (rows, border) = (next, next_border);
    }
    h.key(KeyCode::Esc);
    assert!(!h.app.ui().help);
    seen
}

/// `keys` each start a line of `help`, so the key is there with its words.
fn assert_keys(help: &[String], keys: &[&str]) {
    for key in keys {
        assert!(
            help.iter().any(|l| l.starts_with(&format!("  {key:<14}"))),
            "{key:?} not in help:\n{}",
            help.join("\n")
        );
    }
}

#[test]
fn every_help_key_is_reachable_on_an_80x24_terminal() {
    let everywhere = ["F1 / ?", "q / Ctrl-C", "Ctrl-R", "Ctrl-G"];

    let mut h = Harness::new();
    let help = read_help(&mut h);
    assert_keys(&help, &everywhere);
    assert_keys(&help, &["Tab / ↓", "paste", "Esc"]);

    let mut h = Harness::paired();
    let id = SessionId::new();
    pull(&mut h, vec![created(id, 0, "/srv/work")]);
    let help = read_help(&mut h);
    assert_keys(&help, &everywhere);
    assert_keys(
        &help,
        &["↑↓ / k j", "D", "Del", "s", "[stopped]", "! N pending"],
    );
    assert_contains(
        &help.join("\n"),
        "command from this device: answer or cancel",
    );

    h.key(KeyCode::Char('n'));
    assert_keys(&read_help(&mut h), &["Tab", "↓ Enter/Tab", "Ctrl-U", "Esc"]);
    h.key(KeyCode::Esc);
    h.key(KeyCode::Esc);
    h.key(KeyCode::Char('r'));
    assert_keys(&read_help(&mut h), &["type / paste", "Enter", "Esc"]);
    h.key(KeyCode::Esc);
    assert!(h.app.ui().prompt.is_none());

    h.key(KeyCode::Char('s'));
    assert_keys(&read_help(&mut h), &["b", "p", "Esc"]);
    h.key(KeyCode::Esc);

    assert_eq!(h.key(KeyCode::Enter), Some(Action::OpenSession(id)));
    let help = read_help(&mut h);
    assert_keys(
        &help,
        &["Ctrl-X", "Ctrl-F", "Tab", "Ctrl-J", "Ctrl-P / N", "Esc"],
    );
    // A long description wraps under itself instead of being cut.
    assert_contains(&help.join("\n"), "first line, ↓ on the last");
    assert_contains(&help.join("\n"), "session cost; model");

    h.key(KeyCode::Tab);
    assert!(!h.app.typing());
    let help = read_help(&mut h);
    assert!(help.len() > 22, "the timeline's help scrolls at 80×24");
    assert_keys(
        &help,
        &[
            "↑↓ / k j",
            "[ / ]",
            "d",
            "y / Y",
            "A",
            "Home g/End G",
            "/",
            "n / N",
            "i",
            "Esc",
        ],
    );
    assert_contains(&help.join("\n"), "command's text before its output");

    let mut h = Harness::paired();
    h.seam
        .set_link(PairedLink::Rejected(LinkRejection::Revoked));
    h.sync();
    let help = read_help(&mut h);
    assert_keys(&help, &everywhere);
    assert_keys(&help, &["p / Enter", "Esc"]);
}

#[test]
fn keys_in_help_scroll_it_and_reach_nothing_beneath() {
    let mut h = Harness::paired();
    let id = SessionId::new();
    let mut events = vec![created(id, 0, "/srv/work")];
    events.extend(agent_messages(id, "news", 5));
    pull(&mut h, events);
    assert_eq!(h.key(KeyCode::Enter), Some(Action::OpenSession(id)));
    h.key(KeyCode::Tab);
    h.key(KeyCode::Up);
    h.key(KeyCode::Up);
    let before = selected(&h);
    assert!(before.is_some());

    h.key(KeyCode::Char('?'));
    let (first, border) = help_box(&h);
    assert_contains(&border, "↓ more below · j/k scroll");
    for code in [
        KeyCode::Char('j'),
        KeyCode::Char('j'),
        KeyCode::Down,
        KeyCode::Char('k'),
        KeyCode::Up,
        KeyCode::Char('i'),
        KeyCode::Char('a'),
        KeyCode::Enter,
        KeyCode::Tab,
        KeyCode::PageUp,
    ] {
        assert_eq!(h.key(code), None, "{code:?}");
        assert!(h.app.ui().help);
    }
    assert_eq!(selected(&h), before);
    assert!(!h.app.typing(), "still on the timeline");
    let (rows, border) = help_box(&h);
    assert_eq!(rows[0], first[1], "scrolled one line");
    assert_contains(&border, "↑↓ more above and below · j/k scroll");

    // G/End to the end, g/Home back to the top; past the ends stays put.
    h.key(KeyCode::Char('G'));
    let (end, border) = help_box(&h);
    assert_contains(&border, "↑ more above · j/k scroll");
    h.key(KeyCode::Char('j'));
    assert_eq!(help_box(&h).0, end);
    h.key(KeyCode::Char('k'));
    assert_eq!(help_box(&h).0[1..], end[..end.len() - 1]);
    h.key(KeyCode::Home);
    assert_eq!(help_box(&h).0, first);
    h.key(KeyCode::Char('k'));
    assert_eq!(help_box(&h).0, first);
    h.key(KeyCode::End);
    assert_eq!(help_box(&h).0, end);

    // Each closing key closes it as before, and does nothing else; it
    // opens again at the top.
    for close in [
        KeyCode::Esc,
        KeyCode::F(1),
        KeyCode::Char('?'),
        KeyCode::Char('q'),
    ] {
        assert_eq!(h.key(close), None);
        assert!(!h.app.ui().help, "{close:?}");
        assert!(!h.app.ui().quit);
        assert_eq!(selected(&h), before);
        assert!(matches!(h.app.view().screen, Screen::Session(_)));
        h.key(KeyCode::Char('?'));
        assert_eq!(help_box(&h).0, first);
    }
    h.key(KeyCode::Esc);

    // With help closed, j moves the selection again.
    h.key(KeyCode::Char('j'));
    assert_ne!(selected(&h), before);
}

#[test]
fn settings_toggle_the_blob_policy_through_the_core() {
    let mut h = Harness::paired();
    h.key(KeyCode::Char('s'));
    let screen = h.screen();
    assert_contains(&screen, "(•) On demand");
    assert_contains(&screen, "Server:      srv:7443");
    assert_eq!(
        h.key(KeyCode::Char('b')),
        Some(Action::SetBlobPolicy(BlobPolicy::Eager))
    );
    assert_eq!(
        h.drain_requests(),
        vec![Request::BlobPolicyChanged(BlobPolicy::Eager)]
    );
    assert_contains(&h.screen(), "(•) Eager");
    assert_eq!(h.key(KeyCode::Esc), Some(Action::Back));
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

    // The agent's activity leaves the rows in place; a message reorders
    // them, and the selection follows the session.
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
    assert!(screen.find("/srv/b").unwrap() < screen.find("/srv/a").unwrap());
    pull(
        &mut h,
        vec![event(
            a,
            10,
            EventKind::UserMessage {
                device_id: DeviceId::new(),
                text: "from the phone".into(),
                attachments: Vec::new(),
            },
        )],
    );
    let screen = h.screen();
    assert!(screen.find("/srv/a").unwrap() < screen.find("/srv/b").unwrap());
    assert_eq!(selected_workdir(&h).as_deref(), Some("/srv/b"));
    assert_eq!(h.key(KeyCode::Enter), Some(Action::OpenSession(b)));

    // Back on the list (Esc leaves the composer, then the session), the
    // same session is still selected.
    assert_eq!(h.key(KeyCode::Esc), None);
    assert_eq!(h.key(KeyCode::Esc), Some(Action::Back));
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
                    suggestions: Vec::new(),
                    shell: None,
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
    // Badges are padded so the titles line up; each is followed by its
    // workdir.
    let column = |title: &str, dir: &str| {
        let row = line_with(&screen, dir);
        row.find(&format!("{title}  {dir}")).unwrap()
    };
    assert_eq!(
        column("idle", "/srv/idle"),
        column("approval", "/srv/approval")
    );
}

#[test]
fn a_session_error_shows_bidi_controls_and_drops_escapes() {
    let mut h = Harness::paired();
    let errored = SessionId::new();
    pull(
        &mut h,
        vec![
            created(errored, 0, "/srv/errored"),
            event(
                errored,
                1,
                EventKind::SessionStateChanged(LogState::Errored {
                    message: "bad \u{202E}gnp.exe\u{1b}[31m red".into(),
                }),
            ),
        ],
    );
    let row = line_with(&h.screen(), "/srv/errored");
    assert_contains(&row, "bad <U+202E>gnp.exe[31m red");
    assert!(!row.contains(['\u{202E}', '\u{1b}']), "{row}");
}

#[test]
fn create_session_goes_from_queued_to_the_real_row() {
    let mut h = Harness::paired();
    // The workdir list (recents, then roots) is asked for at once.
    assert_eq!(
        h.key(KeyCode::Char('n')),
        Some(Action::CompleteWorkdir {
            input: String::new()
        })
    );
    assert!(h.app.typing());
    let screen = h.screen();
    assert_contains(&screen, "New session · workdir on the server");
    assert_contains(&screen, "Enter create · Esc cancel");

    // Letters are text in the prompt, not commands.
    h.type_workdir("/srv/quiet");
    assert!(!h.app.ui().quit);
    for _ in 0.."quiet".len() {
        h.key(KeyCode::Backspace);
    }
    h.type_workdir("new");
    assert_contains(&h.screen(), "> /srv/new");
    // The listings the typing asked for; Enter asks for none.
    h.drain_requests();
    assert_eq!(
        h.key(KeyCode::Enter),
        Some(Action::CreateSession {
            conversation: None,
            workdir: "/srv/new".into(),
            worktree: None,
        })
    );
    assert_eq!(h.drain_requests(), vec![Request::OutboxChanged]);
    assert!(h.app.ui().prompt.is_none(), "the prompt closes");
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
            RejectReason::WorkdirNotAllowed,
            "not under a configured root on the server",
        ),
        (
            "/srv/missing",
            RejectReason::WorkdirNotFound,
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
    h.type_workdir("/srv/a");
    h.key_with(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    assert_eq!(h.app.ui().prompt.as_ref().unwrap().text, "");
    h.type_workdir("/srv/b");
    // `?` is text here; F1 still opens help, which lists the prompt's keys.
    h.key(KeyCode::Char('?'));
    assert!(!h.app.ui().help);
    h.key(KeyCode::Backspace);
    h.key(KeyCode::F(1));
    assert_contains(&h.screen(), "create the session");
    h.key(KeyCode::Esc);
    assert!(!h.app.ui().help);
    assert_eq!(h.app.ui().prompt.as_ref().unwrap().text, "/srv/b");

    // Nothing is highlighted, so Esc cancels, and the list ends. The
    // listings the typing asked for are drained first.
    h.drain_requests();
    assert_eq!(h.key(KeyCode::Esc), Some(Action::EndCompletion));
    assert!(h.app.ui().prompt.is_none());
    assert!(h.drain_requests().is_empty());
    assert!(h.seam.pending_outbox().unwrap().is_empty());
    assert_contains(&h.screen(), "n new");
    // Out of the prompt, `q` quits again.
    h.key(KeyCode::Char('q'));
    assert!(h.app.ui().quit);
}

/// The last listing the core asked the engine for: its id and directory.
/// Typing a path asks for each directory it passes (the roots, then `/`,
/// then `/srv/`), so the last one is the directory typed.
fn list_dirs_request(h: &Harness) -> (u64, String) {
    let mut listings = h
        .drain_requests()
        .into_iter()
        .filter_map(|request| match request {
            Request::ListDirs { req, dir } => Some((req, dir)),
            _ => None,
        });
    match listings.next_back() {
        Some(listing) => listing,
        None => panic!("expected a listing request"),
    }
}

fn answer_listing(h: &mut Harness, req: u64, dir: &str, names: &[&str]) {
    h.seam.dirs_listed(
        req,
        Ok(DirsResult::Listed {
            dir: dir.into(),
            names: names.iter().map(|&n| n.to_owned()).collect(),
            truncated: false,
        }),
    );
    h.sync();
}

/// The completion popup's lines, empty when it is closed.
fn popup(screen: &str) -> String {
    let lines: Vec<&str> = screen
        .lines()
        .skip_while(|line| !line.contains("Directories"))
        .take_while(|line| !line.contains("New session"))
        .collect();
    lines.join(
        "
",
    )
}

fn workdir_input(h: &Harness) -> String {
    h.app.ui().prompt.as_ref().unwrap().text.clone()
}

#[test]
fn tab_fills_in_the_only_directory() {
    let mut h = Harness::paired();
    h.key(KeyCode::Char('n'));
    assert_contains(&h.screen(), "Tab complete");
    h.type_workdir("/srv/a");
    let (req, dir) = list_dirs_request(&h);
    assert_eq!(dir, "/srv/");
    assert_contains(&h.screen(), "listing…");

    // The reply does not rewrite the input; Tab fills in the only match.
    answer_listing(&mut h, req, "/srv", &[".git", "api", "web"]);
    assert_eq!(workdir_input(&h), "/srv/a");
    assert_eq!(
        h.key(KeyCode::Tab),
        Some(Action::CompleteWorkdir {
            input: "/srv/api/".into()
        })
    );
    assert_eq!(workdir_input(&h), "/srv/api/");
    // The list stays, whatever Tab did.
    assert!(!popup(&h.screen()).is_empty());
    // Enter, with no row highlighted, creates the session as usual.
    assert_eq!(
        h.key(KeyCode::Enter),
        Some(Action::CreateSession {
            conversation: None,
            workdir: "/srv/api/".into(),
            worktree: None,
        })
    );
}

#[test]
fn pasting_an_outside_conversation_asks_the_workdir_then_the_id() {
    let mut h = Harness::paired();
    open_conversations(&mut h);
    // `p` in the picker: the two-step prompt instead, and the picker goes.
    assert_eq!(h.key(KeyCode::Char('p')), Some(Action::EndConversations));
    assert!(h.app.conversation_picker().is_none());
    assert!(h.app.typing());
    assert_contains(
        &h.screen(),
        "Continue an outside conversation · workdir on the server",
    );
    let hint = h.screen();
    assert_contains(&hint, "Enter next: the id");
    assert!(!hint.contains("Enter create"), "{hint}");
    h.type_workdir("/srv/a");
    let (req, _) = list_dirs_request(&h);
    answer_listing(&mut h, req, "/srv", &["api"]);
    // Positive control: step one fills in like the create prompt does.
    assert_eq!(
        h.key(KeyCode::Tab),
        Some(Action::CompleteWorkdir {
            input: "/srv/api/".into()
        })
    );
    assert_eq!(workdir_input(&h), "/srv/api/");

    // Enter moves to the id; nothing is dispatched and the prompt stays.
    assert!(!matches!(
        h.key(KeyCode::Enter),
        Some(Action::CreateSession { .. })
    ));
    assert!(h.app.ui().prompt.is_some());
    assert_eq!(workdir_input(&h), "");
    assert_contains(&h.screen(), "agent session id (paste it)");
    // The workdir step's listings are done with; the id step asks for none.
    h.drain_requests();

    // Tab is not completion here; the text is untouched.
    h.type_text("ab");
    assert_eq!(h.key(KeyCode::Tab), None);
    assert_eq!(workdir_input(&h), "ab");
    h.key(KeyCode::Backspace);
    h.key(KeyCode::Backspace);

    // A pasted id, with its trailing newline, is the whole input.
    h.app.on_paste(" 0b6c1a2e-1111-4222-8333-444455556666\n");
    assert_eq!(
        h.key(KeyCode::Enter),
        Some(Action::CreateSession {
            workdir: "/srv/api/".into(),
            conversation: Some("0b6c1a2e-1111-4222-8333-444455556666".into()),
            worktree: None,
        })
    );
    assert_eq!(h.drain_requests(), vec![Request::OutboxChanged]);
    assert!(h.app.ui().prompt.is_none(), "the prompt closes");
    assert!(!h.app.typing());
}

#[test]
fn escape_at_the_id_step_cancels_without_creating() {
    let mut h = Harness::paired();
    open_conversations(&mut h);
    h.key(KeyCode::Char('p'));
    h.type_workdir("/srv/x");
    h.key(KeyCode::Enter);
    h.type_text("some-id");
    assert_eq!(h.key(KeyCode::Esc), None);
    assert!(h.app.ui().prompt.is_none());
    assert!(!h.app.typing());
    assert!(!h.screen().contains("Continue an outside conversation"));
    // The session list takes keys again (`n` opens the workdir list), and
    // nothing was queued.
    assert_eq!(
        h.key(KeyCode::Char('n')),
        Some(Action::CompleteWorkdir {
            input: String::new()
        })
    );
    assert_contains(&h.screen(), "New session");
}

/// `c` on the session list: the core asks the engine for the server's
/// conversations; the id of that request.
fn open_conversations(h: &mut Harness) -> u64 {
    assert_eq!(h.key(KeyCode::Char('c')), Some(Action::ListConversations));
    assert!(h.app.conversation_picker().is_some(), "the picker opens");
    match h.drain_requests().as_slice() {
        [Request::ListConversations { req }] => *req,
        other => panic!("expected one conversations request, got {other:?}"),
    }
}

fn outside(
    id: &str,
    workdir: &str,
    title: Option<&str>,
    hours_ago: i64,
    session: Option<SessionId>,
) -> skywave_proto::OutsideConversation {
    skywave_proto::OutsideConversation {
        id: id.into(),
        workdir: workdir.into(),
        title: title.map(Into::into),
        first_prompt: Some(format!("first prompt of {id}")),
        last_active: mockup_now() - TimeDelta::hours(hours_ago),
        messages: 42,
        session,
    }
}

fn answer_conversations(h: &mut Harness, req: u64, list: Vec<skywave_proto::OutsideConversation>) {
    h.seam.conversations_listed(
        req,
        Ok(skywave_proto::ConversationsResult::Listed {
            conversations: list,
            truncated: false,
        }),
    );
    h.sync();
}

#[test]
fn c_lists_the_servers_conversations_and_enter_continues_one() {
    let mut h = Harness::paired();
    let req = open_conversations(&mut h);
    let screen = h.screen();
    assert_contains(&screen, "Continue a conversation from outside Skywave");
    assert_contains(&screen, "listing…");
    assert_contains(&screen, "p paste an id");

    answer_conversations(
        &mut h,
        req,
        vec![
            outside(
                "c-new",
                "/home/me/src/api",
                Some("Fix the flaky test"),
                3,
                None,
            ),
            outside("c-old", "/srv/web", None, 30, None),
        ],
    );
    let screen = h.screen();
    assert!(!screen.contains("listing…"), "{screen}");
    // The agent's title, else the first prompt; where, how long ago, how long.
    assert_contains(&line_with(&screen, "Fix the flaky test"), "> ");
    assert_contains(&screen, "…/src/api · 3 h ago · 42 messages");
    assert_contains(&screen, "first prompt of c-old");
    assert_contains(&screen, "/srv/web · 1 d ago · 42 messages");
    // The list's own keys are the picker's now: no `o` for a row with no
    // session.
    assert!(!screen.contains("o open its session"), "{screen}");

    assert_eq!(h.key(KeyCode::Down), None);
    assert_contains(&line_with(&h.screen(), "first prompt of c-old"), "> ");
    assert_eq!(
        h.key(KeyCode::Enter),
        Some(Action::CreateSession {
            workdir: "/srv/web".into(),
            conversation: Some("c-old".into()),
            worktree: None,
        })
    );
    assert!(h.app.conversation_picker().is_none(), "the core closes it");
    assert_eq!(h.drain_requests(), vec![Request::OutboxChanged]);

    // The pending creation says it imports.
    let command = last_command(&h);
    assert_contains(
        &line_with(&h.screen(), "/srv/web"),
        "will import when online",
    );
    h.seam.command_sending(command).unwrap();
    h.sync();
    assert_contains(
        &line_with(&h.screen(), "/srv/web"),
        "Importing conversation history…",
    );
    h.seam
        .command_result(command, &CommandResult::Accepted { seq: Seq(1) })
        .unwrap();
    h.sync();
    let row = line_with(&h.screen(), "/srv/web");
    assert_contains(&row, "created, syncing…");
    assert!(!row.contains("import"), "{row}");
}

#[test]
fn a_listed_conversation_already_in_a_session_opens_it() {
    use skywave_proto::SessionOrigin;
    let mut h = Harness::paired();
    let (running, adopted) = (SessionId::new(), SessionId::new());
    pull(
        &mut h,
        vec![
            created(running, 0, "/srv/run"),
            event(
                adopted,
                1,
                EventKind::SessionCreated {
                    workdir: "/srv/twice".into(),
                    context_name: "start".into(),
                    from: Some(SessionOrigin::Agent {
                        conversation: "c-twice".into(),
                    }),
                    worktree: None,
                },
            ),
            event(
                adopted,
                1,
                EventKind::ImportFinished {
                    outcome: skywave_proto::ImportOutcome::NotFound,
                },
            ),
        ],
    );
    let req = open_conversations(&mut h);
    answer_conversations(
        &mut h,
        req,
        vec![
            outside("c-run", "/srv/run", Some("Running here"), 1, Some(running)),
            outside("c-twice", "/srv/twice", Some("Continued before"), 2, None),
        ],
    );
    let screen = h.screen();
    assert_contains(
        &line_with(&screen, "Running here"),
        "already in Skywave: “run”",
    );
    assert_contains(
        &line_with(&screen, "Continued before"),
        "continued in “twice”",
    );
    assert_contains(&screen, "o open its session");

    // Positive control first: the second row runs in no session.
    h.key(KeyCode::Down);
    assert!(!h.screen().contains("o open its session"));
    assert_eq!(h.key(KeyCode::Char('o')), None);
    assert_eq!(
        h.app.ui().notice.as_deref(),
        Some("no session runs this conversation")
    );
    h.key(KeyCode::Up);
    assert_eq!(
        h.key(KeyCode::Char('o')),
        Some(Action::OpenSession(running))
    );
    assert!(matches!(h.app.view().screen, Screen::Session(_)));
}

#[test]
fn the_picker_says_when_it_cannot_list() {
    let mut h = Harness::paired();
    // Offline: nothing is asked for, the picker says why at once.
    h.seam.set_link(PairedLink::Reconnecting {
        attempt: 1,
        retry_at: mockup_now(),
        offline_since: mockup_now(),
        reason: skywave_core::LinkFailure::Unreachable,
    });
    h.sync();
    assert_eq!(h.key(KeyCode::Char('c')), Some(Action::ListConversations));
    assert!(h.drain_requests().is_empty());
    let screen = h.screen();
    assert_contains(
        &screen,
        "offline · the server lists its conversations once connected",
    );
    assert_contains(&screen, "Press p to paste a conversation id instead.");
    assert_eq!(h.key(KeyCode::Esc), Some(Action::EndConversations));
    assert!(h.app.conversation_picker().is_none());
    assert!(!h.screen().contains("offline · the server lists"));

    // An agent that cannot list them.
    h.seam.set_link(PairedLink::Online { latency_ms: 20 });
    h.sync();
    let req = open_conversations(&mut h);
    h.seam
        .conversations_listed(req, Ok(skywave_proto::ConversationsResult::Unavailable));
    h.sync();
    assert_contains(
        &h.screen(),
        "can't list conversations: the server's agent cannot list its conversations",
    );
    // `r` asks again; an empty answer says so.
    assert_eq!(h.key(KeyCode::Char('r')), Some(Action::ListConversations));
    let [Request::ListConversations { req }] = h.drain_requests()[..] else {
        panic!("expected a new conversations request");
    };
    answer_conversations(&mut h, req, Vec::new());
    assert_contains(
        &h.screen(),
        "The server's agent has no conversations under the roots.",
    );
}

#[test]
fn the_list_shows_at_once_and_enter_descends() {
    let mut h = Harness::paired();
    let [a, b] = std::array::from_fn(|_| SessionId::new());
    pull(
        &mut h,
        vec![created(a, 0, "/srv/project-a"), created(b, 1, "/srv/web")],
    );
    h.key(KeyCode::Char('n'));
    // The recent workdirs show before anything is typed.
    let screen = popup(&h.screen());
    assert_contains(&line_with(&screen, "/srv/project-a"), "recent");
    assert_contains(&line_with(&screen, "/srv/web"), "recent");

    h.type_workdir("/srv/p");
    let (req, _) = list_dirs_request(&h);
    // Recent workdirs show at once, while the server is asked.
    let screen = popup(&h.screen());
    assert_contains(&line_with(&screen, "/srv/project-a"), "recent");
    assert_contains(&screen, "listing…");
    assert!(!screen.contains("/srv/web"), "{screen}");

    answer_listing(&mut h, req, "/srv", &["project-a", "project-b", "web"]);
    assert_eq!(
        workdir_input(&h),
        "/srv/p",
        "the reply does not rewrite the input"
    );
    assert_contains(&h.screen(), "↓ choose");
    let screen = popup(&h.screen());
    assert!(!line_with(&screen, "/srv/project-a").contains("recent"));
    assert_contains(&screen, "/srv/project-b");
    assert!(!screen.contains("listing…"));

    // Typing filters the same listing without asking again.
    h.type_workdir("roject-b");
    assert!(h.drain_requests().is_empty());
    assert!(!popup(&h.screen()).contains("/srv/project-a"));

    // ↓ highlights the one row; Enter on it descends and creates nothing.
    h.key(KeyCode::Down);
    assert_contains(&line_with(&popup(&h.screen()), "/srv/project-b"), "> ");
    assert_contains(&h.screen(), "Enter/Tab open");
    assert_eq!(
        h.key(KeyCode::Enter),
        Some(Action::CompleteWorkdir {
            input: "/srv/project-b/".into()
        })
    );
    assert_eq!(workdir_input(&h), "/srv/project-b/");
    assert!(!popup(&h.screen()).is_empty(), "the list is still shown");
    assert!(h.seam.pending_outbox().unwrap().is_empty());
}

#[test]
fn esc_unselects_then_cancels() {
    let mut h = Harness::paired();
    let [a, b] = std::array::from_fn(|_| SessionId::new());
    pull(
        &mut h,
        vec![created(a, 0, "/srv/one"), created(b, 1, "/srv/two")],
    );
    h.seam.set_link(PairedLink::Reconnecting {
        attempt: 1,
        retry_at: Utc::now() + TimeDelta::seconds(5),
        offline_since: Utc::now(),
        reason: skywave_core::LinkFailure::Unreachable,
    });
    h.sync();
    h.key(KeyCode::Char('n'));
    h.type_workdir("/srv/o");
    assert!(h.drain_requests().is_empty(), "nothing is asked offline");
    // Tab offline: recent workdirs are all there is, and the only match fills in.
    assert_eq!(
        h.key(KeyCode::Tab),
        Some(Action::CompleteWorkdir {
            input: "/srv/one/".into()
        })
    );
    assert_eq!(workdir_input(&h), "/srv/one/");

    h.key_with(ctrl('u'));
    h.type_workdir("/srv/");
    let screen = popup(&h.screen());
    assert_contains(&screen, "offline · recent only");
    assert_contains(&line_with(&screen, "/srv/two"), "recent");
    assert_contains(&line_with(&screen, "/srv/one"), "recent");

    // ↓ highlights a row; the first Esc clears that and keeps the prompt.
    h.key(KeyCode::Down);
    assert_contains(&h.screen(), "Esc unselect");
    assert_eq!(h.key(KeyCode::Esc), None);
    assert!(!h.screen().contains("Esc unselect"));
    assert!(!popup(&h.screen()).is_empty(), "the list stays");
    assert_eq!(workdir_input(&h), "/srv/");
    // With nothing highlighted, the next Esc cancels.
    assert_eq!(h.key(KeyCode::Esc), Some(Action::EndCompletion));
    assert!(h.app.ui().prompt.is_none());
}

#[test]
fn stale_rows_cannot_be_chosen_while_a_new_directory_loads() {
    let mut h = Harness::paired();
    h.key(KeyCode::Char('n'));
    h.type_workdir("/srv/");
    let (req, _) = list_dirs_request(&h);
    answer_listing(&mut h, req, "/srv", &["api", "web"]);
    // Positive control: a fresh row is not dimmed, and can be highlighted.
    assert_ne!(
        colour_of(&h.frame(100, 24, mockup_now()), "/srv/api"),
        Color::DarkGray
    );
    h.key(KeyCode::Down);
    assert_contains(&h.screen(), "Esc unselect");
    h.key(KeyCode::Up);

    // Typing `api/` asks for a new listing, which is not answered. The rows
    // still shown are the last answer's, for `/srv/api` (the input before the
    // directory changed): dimmed, and not choosable.
    h.type_workdir("api/");
    let (_, dir) = list_dirs_request(&h);
    assert_eq!(dir, "/srv/api/");
    assert_contains(&popup(&h.screen()), "/srv/api");
    assert_contains(&h.screen(), "listing…");
    assert_eq!(
        colour_of(&h.frame(100, 24, mockup_now()), "/srv/api"),
        Color::DarkGray
    );
    // Down highlights nothing, so Enter creates in the typed directory
    // instead of descending into the stale row.
    h.key(KeyCode::Down);
    assert!(!h.screen().contains("Esc unselect"));
    assert_eq!(
        h.key(KeyCode::Enter),
        Some(Action::CreateSession {
            conversation: None,
            workdir: "/srv/api/".into(),
            worktree: None,
        })
    );
}

// ---- session view -----------------------------------------------------------

/// A paired harness with one session `/srv/work` open, plus this device's id.
fn open_session() -> (Harness, SessionId, DeviceId) {
    let mut h = Harness::paired();
    let id = SessionId::new();
    pull(&mut h, vec![created(id, 0, "/srv/work")]);
    assert_eq!(h.key(KeyCode::Enter), Some(Action::OpenSession(id)));
    let me = h.seam.pairing().unwrap().credentials.device_id;
    (h, id, me)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

/// The last 8 hex digits of a device id, as the timeline names devices
/// whose name it does not know.
fn tail(device: DeviceId) -> String {
    device.0.simple().to_string()[24..].to_owned()
}

#[test]
fn other_devices_show_by_name_once_the_server_lists_them() {
    let (mut h, id, me) = open_session();
    let (phone, approval) = (DeviceId::new(), ApprovalId::new());
    pull(
        &mut h,
        vec![
            event(
                id,
                1,
                EventKind::UserMessage {
                    device_id: phone,
                    text: "from the phone".into(),
                    attachments: Vec::new(),
                },
            ),
            event(
                id,
                2,
                EventKind::ApprovalRequested {
                    id: approval,
                    call: None,
                    tool: "Bash".into(),
                    summary: "rm -rf target".into(),
                    input: None,
                    suggestions: Vec::new(),
                    shell: None,
                },
            ),
            event(
                id,
                3,
                EventKind::ApprovalResolved {
                    id: approval,
                    decision: Decision::Approve,
                    by: phone,
                    rule: None,
                },
            ),
        ],
    );
    let screen = h.screen();
    assert_contains(
        &screen,
        &format!("12:01 ▌ device …{}: from the phone", tail(phone)),
    );
    assert_contains(&screen, &format!("approved on device …{}", tail(phone)));

    // Listed at `hello`; a control character in a name never reaches the grid.
    h.seam
        .hello_received(&HelloResponse {
            protocol_version: skywave_proto::PROTOCOL_VERSION,
            server_id: skywave_proto::ServerId::new(),
            device_id: me,
            head: Seq::ZERO,
            log: any_log(),
            devices: vec![
                DeviceInfo {
                    id: phone,
                    name: "Pixel\u{7} 8".into(),
                },
                DeviceInfo {
                    id: me,
                    name: "laptop".into(),
                },
            ],
            shell: false,
            bypass: false,
            login_expires: None,
            agent_login_offered: false,
            agent_logged_out: false,
        })
        .unwrap();
    h.sync();
    let screen = h.screen();
    assert_contains(&screen, "12:01 ▌ Pixel 8: from the phone");
    assert_contains(&screen, "approved on Pixel 8");
    assert!(!screen.contains("laptop"), "{screen}");
}

fn blob(size: u64) -> (BlobId, BlobRef) {
    let id = BlobId::new();
    (
        id,
        BlobRef {
            id,
            size,
            sha256: skywave_proto::ContentHash::of(b""),
        },
    )
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
                    device_id: other,
                    text: "from the laptop".into(),
                    attachments: Vec::new(),
                },
            ),
            event(
                id,
                2,
                EventKind::AgentMessage {
                    parent: None,
                    text: "Looking.\nSecond line".into(),
                    full: None,
                },
            ),
            event(
                id,
                3,
                EventKind::ToolCall(ToolCall {
                    parent: None,
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
                    output: Some(Full::Blob(output)),
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
        &format!("12:01 ▌ device …{}: from the laptop", tail(other)),
    );
    assert_contains(&screen, "12:02 ▌ agent: Looking.");
    assert_contains(&screen, "        ▌ Second line");
    // Standalone (v1) calls and results, in the tool block style.
    assert_contains(&screen, "12:03 • shell {\"command\":\"ls\"}");
    assert_contains(&screen, "12:04 ↳ result ");
    assert_contains(&screen, "        │ file-000 ");
    assert_contains(&screen, "        │ … full 9.8 KiB · not downloaded ");
    assert_contains(&screen, "12:05 interrupted · the agent stopped its turn");
    assert_contains(&screen, "12:06 ▌ agent error: agent crashed");
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
    assert_contains(&screen, "        ▌ line two");

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
        vec![message(id, 1, command, me, "q? line one\nline two")],
    );
    assert_contains(
        &line_with(&h.screen(), ": q? line one"),
        "12:01 ▌ you [accepted]",
    );

    // A refused message says why, in words.
    h.type_text("again");
    h.key(KeyCode::Enter);
    let command = last_command(&h);
    h.seam
        .command_result(
            command,
            &CommandResult::Rejected {
                reason: RejectReason::UnknownSession,
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

/// An approval whose input the server could not keep says so in its
/// prompt: the summary is all the user decides on.
#[test]
fn an_approval_whose_input_was_lost_warns_in_its_prompt() {
    let approval = |full| EventKind::ApprovalRequested {
        id: ApprovalId::new(),
        call: None,
        tool: "write".into(),
        summary: "a.txt".into(),
        input: Some(full),
        suggestions: Vec::new(),
        shell: None,
    };
    let (mut h, id, _) = open_session();
    pull(&mut h, vec![event(id, 1, approval(Full::Lost))]);
    let screen = h.screen();
    assert_contains(&screen, "Approval needed");
    assert_contains(&screen, "! the server could not store the full input");
    assert!(!screen.contains("full input 5.9 KiB"), "{screen}");

    // Control: a kept input offers itself and warns of nothing.
    let (mut h, id, _) = open_session();
    pull(
        &mut h,
        vec![event(id, 1, approval(Full::Blob(blob(6000).1)))],
    );
    let screen = h.screen();
    assert_contains(&screen, "full input 5.9 KiB · v to see it");
    assert!(!screen.contains("could not store"), "{screen}");
}

/// An approval of a logged call, asking with an input other than the
/// call's, warns when that input was lost, though the call's own was kept;
/// and a call whose body was kept but whose input was lost warns too.
#[test]
fn an_approval_of_a_call_warns_of_its_own_lost_input() {
    let ask = |call_input: Option<Full>, body: Option<Full>, input: Option<Full>| {
        let (mut h, id, _) = open_session();
        let call_id = CallId::new();
        let mut tool = call(call_id, "Edit", ToolKind::FileEdit, "src/lib.rs");
        tool.input = call_input;
        tool.display.body = Some(Body {
            format: BodyFormat::Diff,
            preview: "-a\n+b".into(),
            lines: 400,
            full: body,
        });
        pull(
            &mut h,
            vec![
                event(id, 1, EventKind::ToolCall(tool)),
                event(
                    id,
                    2,
                    EventKind::ApprovalRequested {
                        id: ApprovalId::new(),
                        call: Some(call_id),
                        tool: "Edit".into(),
                        summary: "src/lib.rs".into(),
                        input,
                        suggestions: Vec::new(),
                        shell: None,
                    },
                ),
            ],
        );
        h.screen()
    };
    let kept = || Some(Full::Blob(blob(6000).1));

    // The request's own input differs from the call's and was lost.
    let screen = ask(None, kept(), Some(Full::Lost));
    assert_contains(&screen, "Approval needed");
    assert_contains(&screen, "! the server could not store the full input");

    // The body was stored, the call's input was not.
    let screen = ask(Some(Full::Lost), kept(), None);
    assert_contains(&screen, "! the server could not store the full input");

    // Control: all kept, nothing said.
    let screen = ask(kept(), kept(), kept());
    assert_contains(&screen, "Approval needed");
    assert!(!screen.contains("could not store"), "{screen}");
}

/// An approval asking with an input other than its call's offers that
/// input: the panel names it, and the call, revealed, shows it labelled.
#[test]
fn an_approval_offers_its_own_input_when_it_differs() {
    let ask = |input: Option<Full>| {
        let (mut h, id, _) = open_session();
        let call_id = CallId::new();
        let tool = call(call_id, "Bash", ToolKind::Shell, "make");
        pull(
            &mut h,
            vec![
                event(id, 1, EventKind::ToolCall(tool)),
                event(
                    id,
                    2,
                    EventKind::ApprovalRequested {
                        id: ApprovalId::new(),
                        call: Some(call_id),
                        tool: "Bash".into(),
                        summary: "make".into(),
                        input,
                        suggestions: Vec::new(),
                        shell: None,
                    },
                ),
            ],
        );
        h
    };
    let label = "approval input: differs from the call's";

    let mut h = ask(Some(Full::Blob(blob(4096).1)));
    let screen = h.screen();
    assert_contains(&screen, &format!("{label} · 4.0 KiB · v to see it"));
    // Collapsed, the timeline does not show it.
    assert!(!screen.contains(&format!("│ {label}")), "{screen}");
    h.key(KeyCode::Esc);
    h.key(KeyCode::Char('v'));
    assert_contains(&h.screen(), &format!("│ {label}"));

    // Control: asking what the call holds offers nothing extra.
    let mut h = ask(None);
    h.key(KeyCode::Esc);
    h.key(KeyCode::Char('v'));
    let screen = h.screen();
    assert_contains(&screen, "Approval needed");
    assert!(!screen.contains(label), "{screen}");
}

/// A cut result whose output the server could not keep says so under its
/// preview, where a kept one says where its blob stands.
#[test]
fn a_lost_output_says_so_under_its_preview() {
    let result = |full| {
        EventKind::ToolResult(ToolResult {
            call: None,
            is_error: false,
            status: None,
            preview: "file-000".into(),
            lines: 40,
            output: Some(full),
        })
    };
    let (mut h, id, _) = open_session();
    pull(&mut h, vec![event(id, 1, result(Full::Lost))]);
    let screen = h.screen();
    assert_contains(&screen, "│ file-000 ");
    assert_contains(&screen, "│ … the server could not store the rest");
    assert!(!screen.contains("not downloaded"), "{screen}");

    // Control: a kept output names its blob and warns of nothing.
    let (mut h, id, _) = open_session();
    pull(
        &mut h,
        vec![event(id, 1, result(Full::Blob(blob(10_000).1)))],
    );
    let screen = h.screen();
    assert_contains(&screen, "│ … full 9.8 KiB · not downloaded");
    assert!(!screen.contains("could not store"), "{screen}");
}

#[test]
fn an_inline_output_expands_to_all_of_it_without_a_download() {
    let (mut h, id, _) = open_session();
    let whole: String = (0..12).map(|i| format!("file-{i:03}\n")).collect();
    let c = CallId::new();
    pull(
        &mut h,
        vec![
            event(
                id,
                1,
                EventKind::ToolCall(call(c, "Glob", ToolKind::Search, "*")),
            ),
            event(
                id,
                2,
                EventKind::ToolResult(ToolResult {
                    output: Some(Full::Inline(whole)),
                    lines: 12,
                    ..result(c, false, "12 files", "file-000")
                }),
            ),
        ],
    );
    let screen = h.screen();
    assert_contains(&screen, "│ file-000 ");
    assert_contains(&screen, "│ … +11 lines ");
    assert!(!screen.contains("file-011"), "collapsed: {screen}");
    assert!(!screen.contains("not downloaded"), "{screen}");
    assert!(!screen.contains("not stored"), "{screen}");
    h.key(KeyCode::Esc);
    h.key(KeyCode::Up);
    // Nothing to fetch, and `d` asks for nothing.
    assert_eq!(h.key(KeyCode::Char('d')), None);
    assert_eq!(h.key(KeyCode::Enter), None);
    assert!(h.drain_requests().is_empty());
    let screen = h.screen();
    assert_contains(&screen, "│ file-011 ");
    assert!(!screen.contains("+11 lines"), "expanded: {screen}");
}

/// A long message shows its head and where its whole stands; expanding
/// fetches the whole and shows it beneath the head; `y` copies the whole.
#[test]
fn a_long_message_shows_its_head_and_expands_to_its_whole() {
    let (blob_id, whole) = blob(40_000);
    let long = |full| EventKind::AgentMessage {
        parent: None,
        text: "# Report\nhead line\n".into(),
        full: Some(full),
    };
    let (mut h, id, _) = open_session();
    pull(&mut h, vec![event(id, 1, long(Full::Blob(whole)))]);
    let screen = h.screen();
    assert_contains(&screen, "agent: # Report");
    assert_contains(&screen, "head line");
    assert_contains(&screen, "│ … full 39.1 KiB · not downloaded ");
    h.key(KeyCode::Esc);
    h.key(KeyCode::Up);
    // `y` takes the whole, never the head alone.
    assert_eq!(
        h.key(KeyCode::Char('y')),
        Some(Action::DownloadBlob(blob_id))
    );
    assert_eq!(h.drain_requests(), vec![Request::DownloadBlob(blob_id)]);
    assert_eq!(h.key(KeyCode::Enter), None, "already asked for");
    let dir = tempfile::tempdir().unwrap();
    downloaded(
        &mut h,
        &dir,
        "m",
        blob_id,
        b"# Report\nhead line\nthe deep finding\n",
    );
    assert_contains(&h.screen(), "│ the deep finding ");
    h.key(KeyCode::Char('y'));
    assert!(
        matches!(clip(&h), Some(Clip::File { .. })),
        "{:?}",
        clip(&h)
    );
    // Collapsed again: the head, and that the whole is here.
    h.key(KeyCode::Enter);
    let screen = h.screen();
    assert!(!screen.contains("the deep finding"), "{screen}");
    assert_contains(&screen, "│ … downloaded ");

    // Lost: the head is all there is, and it says so.
    let (mut h, id, _) = open_session();
    pull(&mut h, vec![event(id, 1, long(Full::Lost))]);
    assert_contains(&h.screen(), "│ … the server could not store the rest");

    // Control: a whole message says nothing more.
    let (mut h, id, _) = open_session();
    pull(&mut h, vec![event(id, 1, agent("# Report\nhead line\n"))]);
    let screen = h.screen();
    assert_contains(&screen, "head line");
    assert!(!screen.contains("│ …"), "{screen}");
}

#[test]
fn an_inline_output_expanded_is_cut_short_like_a_blob() {
    let (mut h, id, _) = open_session();
    let whole: String = (0..500).map(|i| format!("l{i:03}\n")).collect();
    let c = CallId::new();
    pull(
        &mut h,
        vec![
            event(
                id,
                1,
                EventKind::ToolCall(call(c, "Glob", ToolKind::Search, "*")),
            ),
            event(
                id,
                2,
                EventKind::ToolResult(ToolResult {
                    output: Some(Full::Inline(whole)),
                    lines: 500,
                    ..result(c, false, "500 files", "l000")
                }),
            ),
        ],
    );
    h.key(KeyCode::Esc);
    h.key(KeyCode::Up);
    h.key(KeyCode::Enter);
    let mut screen = h.screen();
    for _ in 0..100 {
        if screen.contains("cut short") {
            break;
        }
        h.key(KeyCode::PageDown);
        screen = h.screen();
    }
    assert_contains(&screen, "… cut short: 400 lines of 2.4 KiB shown");
    assert!(!screen.contains("l400"), "past the cap: {screen}");
}

fn blob_deleted(id: SessionId, minute: u32, blob: BlobId, by: DeviceId) -> Event {
    event(
        id,
        minute,
        EventKind::BlobDeleted {
            blob,
            holds: BlobUse::ToolOutput,
            by,
        },
    )
}

/// `blob` cropped to 2 of its 40 lines, into `into`.
fn blob_cropped(id: SessionId, minute: u32, blob: BlobId, into: BlobRef, by: DeviceId) -> Event {
    event(
        id,
        minute,
        EventKind::BlobCropped {
            blob,
            holds: BlobUse::ToolOutput,
            into,
            kept: vec![LineSpan { first: 4, count: 2 }],
            lines: 40,
            by,
        },
    )
}

/// A tool output deleted on the server says so under its preview, with
/// nothing to download; a cropped one says what the crop kept beside its
/// new blob's state.
#[test]
fn a_deleted_or_cropped_output_says_so_under_its_preview() {
    let result = |output| {
        EventKind::ToolResult(ToolResult {
            call: None,
            is_error: false,
            status: None,
            preview: "file-000".into(),
            lines: 40,
            output: Some(Full::Blob(output)),
        })
    };
    let (mut h, id, me) = open_session();
    let (gone, kept) = (blob(10_000), blob(9_000));
    pull(&mut h, vec![event(id, 1, result(gone.1))]);
    h.key(KeyCode::Esc);
    h.key(KeyCode::Up);
    // Control: before the delete, its blob shows and downloads.
    assert_contains(&h.screen(), "│ … full 9.8 KiB · not downloaded");
    assert_contains(&footer(&h), "d download (9.8 KiB)");
    pull(&mut h, vec![blob_deleted(id, 2, gone.0, me)]);
    let screen = h.screen();
    assert_contains(&screen, "│ … Deleted by you");
    assert!(!screen.contains("not downloaded"), "{screen}");
    assert!(!footer(&h).contains("d download ("), "{}", footer(&h));
    assert_eq!(h.key(KeyCode::Char('d')), None, "nothing to download");

    let (mut h, id, me) = open_session();
    let (into_id, into) = blob(300);
    pull(
        &mut h,
        vec![
            event(id, 1, result(kept.1)),
            blob_cropped(id, 2, kept.0, into, me),
        ],
    );
    h.key(KeyCode::Esc);
    h.key(KeyCode::Up);
    assert_contains(
        &h.screen(),
        "│ … full 300 B · not downloaded · Cropped by you: kept 2 of 40 lines",
    );
    assert_eq!(
        h.key(KeyCode::Char('d')),
        Some(Action::DownloadBlob(into_id))
    );
}

/// Expanding a cropped output keeps saying what the crop kept, beside
/// where its new blob stands.
#[test]
fn an_expanded_cropped_output_still_says_what_the_crop_kept() {
    let (mut h, id, me) = open_session();
    let (kept, (_, into)) = (blob(9_000), blob(300));
    pull(
        &mut h,
        vec![
            event(id, 1, output_result(40, kept.1)),
            blob_cropped(id, 2, kept.0, into, me),
        ],
    );
    h.key(KeyCode::Esc);
    h.key(KeyCode::Up);
    h.key(KeyCode::Enter);
    let screen = h.screen();
    // Control: expanded, so the collapsed summary is gone.
    assert!(!screen.contains("full 300 B"), "{screen}");
    assert_contains(
        &screen,
        "│ requested · waiting for a slot · 300 B · Cropped by you: kept 2 of 40 lines",
    );
}

/// Expanding an output whose delete is pending keeps saying so under it.
#[test]
fn an_expanded_output_still_says_its_delete_is_pending() {
    let (mut h, id, _) = open_session();
    let (_, output) = blob(10_000);
    pull(&mut h, vec![event(id, 1, output_result(40, output))]);
    h.key(KeyCode::Esc);
    h.key(KeyCode::Up);
    h.key(KeyCode::Char('D'));
    h.key(KeyCode::Char('y'));
    h.key(KeyCode::Enter);
    let screen = h.screen();
    assert!(!screen.contains("full 9.8 KiB"), "{screen}");
    assert_contains(
        &screen,
        "│ requested · waiting for a slot · 9.8 KiB · Delete queued",
    );
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
                input: Some(Full::Blob(input)),
                suggestions: Vec::new(),
                shell: None,
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
            rule: None,
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
                rule: None,
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
                suggestions: Vec::new(),
                shell: None,
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
                rule: None,
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
                    suggestions: Vec::new(),
                    shell: None,
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
                reason: RejectReason::NoActiveTurn,
            },
        )
        .unwrap();
    h.sync();
    // Nothing was working: the agent already does what was asked.
    assert!(!h.screen().contains("interrupt failed"), "{}", h.screen());
}

#[test]
fn rename_from_the_list_shows_at_once_and_empty_restores_the_default() {
    let mut h = Harness::paired();
    let id = SessionId::new();
    let phone = DeviceId::new();
    pull(
        &mut h,
        vec![
            created(id, 0, "/srv/work"),
            event(id, 1, user(phone, "fix the flaky test\nin ci")),
        ],
    );
    // The title is the first prompt's first line, the workdir follows.
    let row = line_with(&h.screen(), "/srv/work");
    assert_contains(&row, "fix the flaky test  /srv/work");

    // Not named yet: the prompt starts empty.
    assert_eq!(h.key(KeyCode::Char('r')), None);
    assert_eq!(h.app.ui().prompt.as_ref().unwrap().text, "");
    assert_contains(&h.screen(), "Rename session");
    h.type_text("flaky");
    assert_eq!(
        h.key(KeyCode::Enter),
        Some(Action::RenameSession {
            session_id: id,
            name: "flaky".into(),
        })
    );
    assert!(h.app.ui().prompt.is_none(), "the prompt closes");
    assert_contains(
        &line_with(&h.screen(), "/srv/work"),
        "flaky queued  /srv/work",
    );

    // Logged: the badge goes, the name stays; the next prompt starts with it.
    let command = last_command(&h);
    h.seam
        .command_result(command, &CommandResult::Accepted { seq: Seq(3) })
        .unwrap();
    let me = h.seam.pairing().unwrap().credentials.device_id;
    pull(
        &mut h,
        vec![event(
            id,
            2,
            EventKind::SessionRenamed {
                name: Some("flaky".into()),
                by: me,
            },
        )],
    );
    let row = line_with(&h.screen(), "/srv/work");
    assert_contains(&row, "flaky  /srv/work");
    assert!(!row.contains("queued"), "{row}");
    h.key(KeyCode::Char('r'));
    assert_eq!(h.app.ui().prompt.as_ref().unwrap().text, "flaky");

    // Emptied, it restores the default title.
    h.key_with(ctrl('u'));
    assert_eq!(
        h.key(KeyCode::Enter),
        Some(Action::RenameSession {
            session_id: id,
            name: String::new(),
        })
    );
    assert_contains(
        &line_with(&h.screen(), "/srv/work"),
        "fix the flaky test queued  /srv/work",
    );

    // Esc cancels without renaming.
    h.key(KeyCode::Char('r'));
    h.type_text("x");
    assert_eq!(h.key(KeyCode::Esc), None);
    assert!(h.app.ui().prompt.is_none());
}

#[test]
fn rename_from_the_timeline_titles_the_header() {
    let (mut h, id, me) = open_session();
    assert_contains(&h.screen(), "┌ work · /srv/work · idle");
    // A capital R in the composer is text, not a rename.
    assert_eq!(h.key(KeyCode::Char('R')), None);
    h.key_with(ctrl('u'));
    h.key(KeyCode::Esc);
    assert_eq!(h.key(KeyCode::Char('R')), None);
    assert_contains(&h.screen(), "Rename session");
    h.type_text("deploy");
    assert_eq!(
        h.key(KeyCode::Enter),
        Some(Action::RenameSession {
            session_id: id,
            name: "deploy".into(),
        })
    );
    assert_contains(&h.screen(), "┌ deploy queued · /srv/work · idle");
    let command = last_command(&h);
    h.seam
        .command_result(command, &CommandResult::Accepted { seq: Seq(1) })
        .unwrap();
    pull(
        &mut h,
        vec![event(
            id,
            1,
            EventKind::SessionRenamed {
                name: Some("deploy".into()),
                by: me,
            },
        )],
    );
    let screen = h.screen();
    assert_contains(&screen, "renamed to “deploy” by you");
    let other = DeviceId::new();
    pull(
        &mut h,
        vec![event(
            id,
            2,
            EventKind::SessionRenamed {
                name: None,
                by: other,
            },
        )],
    );
    let screen = h.screen();
    assert_contains(&screen, "┌ work · /srv/work · idle");
    assert_contains(
        &screen,
        &format!("default title restored by device …{}", tail(other)),
    );
}

#[test]
fn context_dividers_show_where_the_context_changed() {
    let (mut h, id, me) = open_session();
    let walrus = skywave_proto::ContextId::new();
    let other = DeviceId::new();
    pull(
        &mut h,
        vec![
            Event {
                context: walrus,
                ..event(
                    id,
                    1,
                    EventKind::ContextStarted {
                        by: other,
                        name: "velvet walrus".into(),
                    },
                )
            },
            Event {
                context: skywave_proto::ContextId::first_of(id),
                ..event(id, 2, EventKind::ContextSwitched { by: me })
            },
        ],
    );
    let screen = h.screen();
    assert_contains(
        &screen,
        &format!(
            "── New context · velvet walrus · by device …{} ──",
            tail(other)
        ),
    );
    assert_contains(&screen, "── Back to start · by you ──");
}

#[test]
fn a_moved_context_is_a_divider_in_both_sessions() {
    use skywave_proto::{ContextId, ContextRef, SessionOrigin};
    let (mut h, id, me) = open_session();
    let (walrus, new) = (ContextId::new(), SessionId::new());
    // As the server logs moving the current context: the clear, then the
    // new session, then the move in the source.
    pull(
        &mut h,
        vec![
            Event {
                context: walrus,
                ..event(
                    id,
                    1,
                    EventKind::ContextStarted {
                        by: me,
                        name: "velvet walrus".into(),
                    },
                )
            },
            event(
                new,
                2,
                EventKind::SessionCreated {
                    workdir: "/srv/work".into(),
                    context_name: "start".into(),
                    from: Some(SessionOrigin::Context(ContextRef {
                        session: id,
                        context: ContextId::first_of(id),
                    })),
                    worktree: None,
                },
            ),
            event(
                new,
                3,
                EventKind::SessionRenamed {
                    name: Some("forked".into()),
                    by: me,
                },
            ),
            event(id, 4, EventKind::ContextMoved { to: new, by: me }),
        ],
    );
    let screen = h.screen();
    assert_contains(&screen, "── New context · velvet walrus · by you ──");
    assert_contains(&screen, "── start went on in “forked” · by you ──");

    h.key(KeyCode::Esc);
    h.key(KeyCode::Esc);
    let Screen::SessionList(list) = &h.app.view().screen else {
        panic!("not on the session list");
    };
    let ids: Vec<SessionId> = list.sessions.iter().map(|s| s.id).collect();
    assert_eq!(ids, [new, id], "the new session sorts first");
    h.key(KeyCode::Up);
    assert_eq!(h.key(KeyCode::Enter), Some(Action::OpenSession(new)));
    assert_contains(&h.screen(), "── Continues start from “work” ──");
}

#[test]
fn an_adopted_session_starts_with_a_divider_naming_the_conversation() {
    use skywave_proto::SessionOrigin;
    let mut h = Harness::paired();
    let new = SessionId::new();
    pull(
        &mut h,
        vec![event(
            new,
            0,
            EventKind::SessionCreated {
                workdir: "/srv/work".into(),
                context_name: "start".into(),
                from: Some(SessionOrigin::Agent {
                    conversation: "3f2a-91bc".into(),
                }),
                worktree: None,
            },
        )],
    );
    assert_eq!(h.key(KeyCode::Enter), Some(Action::OpenSession(new)));
    let screen = h.screen();
    assert_contains(
        &screen,
        "Continues agent conversation 3f2a-91bc from outside Skywave",
    );
    assert!(!screen.contains("not shown"), "{screen}");
}

/// An adopted session `/srv/work` whose import brought a prompt, an answer,
/// a shell call with its result and a result of a call not on the
/// branch, all at 09:1x by the agent's clock; then `outcome`, then a live
/// message at 12:05. Its row on the session list is selected.
fn imported_session(outcome: skywave_proto::ImportOutcome) -> (Harness, SessionId, DeviceId) {
    use skywave_proto::{ImportedItem, SessionOrigin};
    let mut h = Harness::paired();
    let me = h.seam.pairing().unwrap().credentials.device_id;
    let id = SessionId::new();
    let agent_at = |minute| Some(Utc.with_ymd_and_hms(2026, 10, 29, 9, minute, 0).unwrap());
    let imported = |minute, item| {
        event(
            id,
            1,
            EventKind::Imported {
                at: agent_at(minute),
                item,
            },
        )
    };
    let (shell, stray) = (CallId::new(), CallId::new());
    pull(
        &mut h,
        vec![
            event(
                id,
                1,
                EventKind::SessionCreated {
                    workdir: "/srv/work".into(),
                    context_name: "start".into(),
                    from: Some(SessionOrigin::Agent {
                        conversation: "3f2a".into(),
                    }),
                    worktree: None,
                },
            ),
            imported(
                10,
                ImportedItem::UserMessage {
                    text: "why is the build red".into(),
                    full: None,
                },
            ),
            imported(
                11,
                ImportedItem::ToolCall(call(shell, "Bash", ToolKind::Shell, "$ cargo build")),
            ),
            imported(
                12,
                ImportedItem::ToolResult(result(shell, false, "exit 0", "Finished dev")),
            ),
            imported(
                13,
                ImportedItem::ToolResult(result(stray, false, "", "stray output")),
            ),
            imported(
                14,
                ImportedItem::AgentMessage {
                    text: "it builds now".into(),
                    full: None,
                },
            ),
            event(id, 1, EventKind::ImportFinished { outcome }),
            message(id, 5, CommandId::new(), me, "live question"),
        ],
    );
    (h, id, me)
}

/// Whether every cell of the screen row holding `needle` is dimmed.
fn dimmed(h: &Harness, needle: &str) -> bool {
    use ratatui::style::Modifier;
    let buffer = h.frame(100, 24, mockup_now());
    let screen = text(&buffer);
    let y = screen
        .lines()
        .position(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("{needle:?} not on screen:\n{screen}"));
    let line = screen.lines().nth(y).unwrap();
    // One char per cell: `text` joins the cells' symbols.
    let x = line[..line.find(needle).unwrap()].chars().count();
    let cell = &buffer[(x as u16, y as u16)];
    cell.modifier.contains(Modifier::DIM)
}

#[test]
fn imported_history_is_marked_dimmed_and_ends_at_a_divider() {
    let (mut h, id, _) = imported_session(skywave_proto::ImportOutcome::Imported { items: 5 });
    assert_eq!(h.key(KeyCode::Enter), Some(Action::OpenSession(id)));
    let screen = h.screen();
    // By the agent's clock, marked, in the order the agent had them.
    assert_contains(&screen, "09:10 imported ▌ you: why is the build red");
    assert_contains(
        &line_with(&screen, "cargo build"),
        "09:11 imported ❯ Bash $ cargo build",
    );
    assert_contains(&line_with(&screen, "cargo build"), "exit 0");
    assert_contains(&screen, "Finished dev");
    assert_contains(&screen, "09:13 imported ↳ result");
    assert_contains(&screen, "stray output");
    assert_contains(&screen, "09:14 imported ▌ agent: it builds now");
    assert_contains(&screen, "── Continued in Skywave from here ──");
    // The session's own items are not imported.
    let live = line_with(&screen, "live question");
    assert!(!live.contains("imported"), "{live}");
    assert_contains(&live, "12:05");
    // The header is titled by the imported prompt, so look past it.
    assert!(dimmed(&h, "you: why is the build red"));
    assert!(dimmed(&h, "it builds now"));
    assert!(dimmed(&h, "Bash $ cargo build"));
    assert!(!dimmed(&h, "live question"), "positive control");

    // Imported rows select and copy like live ones.
    h.key(KeyCode::Esc);
    h.key(KeyCode::Up);
    h.key(KeyCode::Up);
    assert_eq!(h.key(KeyCode::Char('y')), None);
    assert_eq!(clip(&h), Some(Clip::Text("it builds now".into())));
    for _ in 0..2 {
        h.key(KeyCode::Up);
    }
    assert_eq!(h.key(KeyCode::Char('Y')), None);
    assert_eq!(clip(&h), Some(Clip::Text("cargo build".into())));
}

#[test]
fn every_import_outcome_draws_its_divider() {
    use skywave_proto::ImportOutcome;
    let cases = [
        (
            ImportOutcome::NotFound,
            "── history not found · continued in Skywave from here ──",
        ),
        (
            ImportOutcome::Failed {
                reason: "bad line 3".into(),
            },
            "── history could not be read: bad line 3 · continued in Skywave from here ──",
        ),
        (
            ImportOutcome::Unavailable,
            "history not imported: the server's agent cannot read it",
        ),
    ];
    for (outcome, divider) in cases {
        let (mut h, id, _) = imported_session(outcome);
        h.key(KeyCode::Enter);
        assert_eq!(h.app.session().map(|(view, _)| view.id), Some(id));
        assert_contains(&h.screen(), divider);
    }
}

#[test]
fn an_adopted_session_says_it_imports_until_its_history_is_in() {
    use skywave_proto::SessionOrigin;
    let mut h = Harness::paired();
    let id = SessionId::new();
    pull(
        &mut h,
        vec![event(
            id,
            0,
            EventKind::SessionCreated {
                workdir: "/srv/work".into(),
                context_name: "start".into(),
                from: Some(SessionOrigin::Agent {
                    conversation: "3f2a".into(),
                }),
                worktree: None,
            },
        )],
    );
    assert_contains(&line_with(&h.screen(), "/srv/work"), "importing history…");
    pull(
        &mut h,
        vec![event(
            id,
            0,
            EventKind::ImportFinished {
                outcome: skywave_proto::ImportOutcome::Imported { items: 0 },
            },
        )],
    );
    let row = line_with(&h.screen(), "/srv/work");
    assert!(!row.contains("importing"), "{row}");
}

/// A session whose first context ("start") holds "alpha question" and
/// whose current one, "velvet walrus", holds "beta question".
fn two_contexts() -> (Harness, SessionId, DeviceId) {
    let (mut h, id, me) = open_session();
    h.key(KeyCode::Esc);
    let walrus = skywave_proto::ContextId::new();
    pull(
        &mut h,
        vec![
            message(id, 1, CommandId::new(), me, "alpha question"),
            Event {
                context: walrus,
                ..event(
                    id,
                    2,
                    EventKind::ContextStarted {
                        by: me,
                        name: "velvet walrus".into(),
                    },
                )
            },
            Event {
                context: walrus,
                ..message(id, 3, CommandId::new(), me, "beta question")
            },
        ],
    );
    (h, id, me)
}

#[test]
fn contexts_menu_offers_a_new_context_only_once_there_is_a_message() {
    let (mut h, id, me) = open_session();
    h.key(KeyCode::Esc);
    // An empty context: the entry is shown but inert.
    assert_eq!(h.key(KeyCode::Char('C')), None);
    let screen = h.screen();
    assert_contains(&screen, " Contexts · Enter switches");
    assert_contains(&screen, "New context (this one is empty)");
    h.key(KeyCode::Up);
    assert_eq!(h.key(KeyCode::Enter), None);
    assert_contains(&h.screen(), " Contexts · Enter switches");
    h.key(KeyCode::Esc);
    assert!(!h.screen().contains(" Contexts · Enter switches"));

    // Positive control: with a message in it, Enter starts a new one.
    pull(&mut h, vec![message(id, 1, CommandId::new(), me, "hello")]);
    h.key(KeyCode::Char('C'));
    let screen = h.screen();
    assert_contains(&screen, "New context");
    assert!(!screen.contains("this one is empty"), "{screen}");
    assert_contains(&screen, "start · hello");
    assert_contains(&screen, "current");
    h.key(KeyCode::Up);
    assert_eq!(
        h.key(KeyCode::Enter),
        Some(Action::ClearContext { session_id: id })
    );
    assert!(!h.screen().contains(" Contexts · Enter switches"));
    assert_contains(&status_line(&h, 100), "new context queued");
}

#[test]
fn contexts_picker_switches_to_an_earlier_context() {
    let (mut h, id, _) = two_contexts();
    h.key(KeyCode::Char('C'));
    let screen = h.screen();
    assert_contains(&screen, "start · alpha question");
    assert_contains(&screen, "› velvet walrus · beta question · current");
    // Enter on the current one has nothing to switch.
    assert_eq!(h.key(KeyCode::Enter), None);
    assert!(!h.screen().contains(" Contexts · Enter switches"));

    h.key(KeyCode::Char('C'));
    h.key(KeyCode::Up);
    assert_eq!(
        h.key(KeyCode::Enter),
        Some(Action::SwitchContext {
            session_id: id,
            context: skywave_proto::ContextId::first_of(id),
        })
    );
    assert_contains(&status_line(&h, 100), "switch queued");
}

/// A session whose first context ("start") the agent answered in, and
/// whose current one, "velvet walrus", is just begun: the first is movable.
fn movable_context() -> (Harness, SessionId, DeviceId) {
    let (mut h, id, me) = open_session();
    h.key(KeyCode::Esc);
    let walrus = skywave_proto::ContextId::new();
    pull(
        &mut h,
        vec![
            message(id, 1, CommandId::new(), me, "alpha question"),
            event(
                id,
                2,
                EventKind::SessionStateChanged(LogState::AgentWorking),
            ),
            event(
                id,
                3,
                EventKind::AgentMessage {
                    parent: None,
                    text: "alpha answer".into(),
                    full: None,
                },
            ),
            Event {
                context: walrus,
                ..event(
                    id,
                    4,
                    EventKind::ContextStarted {
                        by: me,
                        name: "velvet walrus".into(),
                    },
                )
            },
            Event {
                context: walrus,
                ..message(id, 5, CommandId::new(), me, "beta question")
            },
        ],
    );
    (h, id, me)
}

#[test]
fn moving_a_context_asks_first_and_only_y_sends_it() {
    let (mut h, id, _) = movable_context();
    let start = skywave_proto::ContextId::first_of(id);
    h.key(KeyCode::Char('C'));
    h.key(KeyCode::Up);
    assert_eq!(h.key(KeyCode::Char('M')), None);
    let screen = h.screen();
    assert_contains(&screen, "Move start · alpha question to a new session?");
    assert_contains(&screen, "y move · any other key back");

    // Anything but `y` goes back to the list, and nothing is sent.
    assert_eq!(h.key(KeyCode::Char('n')), None);
    let screen = h.screen();
    assert_contains(&screen, " Contexts · Enter switches");
    assert!(!screen.contains("to a new session?"), "{screen}");
    // The confirmation does not survive: `y` on the list is nothing.
    assert_eq!(h.key(KeyCode::Char('y')), None);

    // Positive control: asked again, `y` sends it.
    h.key(KeyCode::Char('M'));
    assert_eq!(
        h.key(KeyCode::Char('y')),
        Some(Action::MoveContext {
            session_id: id,
            context: start,
        })
    );
    assert!(!h.screen().contains(" Contexts · Enter switches"));
}

#[test]
fn moving_a_context_the_core_does_not_offer_says_why_and_sends_nothing() {
    let (mut h, _, _) = two_contexts();
    // No agent answered in either context.
    h.key(KeyCode::Char('C'));
    assert_eq!(h.key(KeyCode::Char('M')), None);
    let screen = h.screen();
    assert_contains(&screen, "can't move yet: it needs an answered turn");
    assert!(!screen.contains("to a new session?"), "{screen}");
    // The reason goes with the next key, and `y` does not move it.
    assert_eq!(h.key(KeyCode::Char('y')), None);
    assert!(!h.screen().contains("can't move yet"));
    assert_contains(&h.screen(), " Contexts · Enter switches");
}

#[test]
fn a_moved_context_shows_where_it_went_and_enter_opens_that_session() {
    use skywave_proto::{ContextId, ContextRef, SessionOrigin};
    let (mut h, id, me) = movable_context();
    let new = SessionId::new();
    pull(
        &mut h,
        vec![
            event(
                new,
                6,
                EventKind::SessionCreated {
                    workdir: "/srv/work".into(),
                    context_name: "start".into(),
                    from: Some(SessionOrigin::Context(ContextRef {
                        session: id,
                        context: ContextId::first_of(id),
                    })),
                    worktree: None,
                },
            ),
            event(
                new,
                7,
                EventKind::SessionRenamed {
                    name: Some("forked".into()),
                    by: me,
                },
            ),
            event(id, 8, EventKind::ContextMoved { to: new, by: me }),
        ],
    );
    h.key(KeyCode::Char('C'));
    let screen = h.screen();
    assert_contains(
        &line_with(&screen, "start · alpha question"),
        "moved → “forked”",
    );
    assert!(
        !line_with(&screen, "velvet walrus · beta").contains("moved"),
        "{screen}"
    );
    // A moved context cannot move again, and says so.
    h.key(KeyCode::Up);
    assert_eq!(h.key(KeyCode::Char('M')), None);
    assert_contains(&h.screen(), "already moved to another session");
    assert_eq!(h.key(KeyCode::Enter), Some(Action::OpenSession(new)));
}

#[test]
fn x_drops_a_refused_move_from_the_contexts_picker() {
    let (mut h, _, _) = movable_context();
    h.key(KeyCode::Char('C'));
    h.key(KeyCode::Up);
    h.key(KeyCode::Char('M'));
    let Some(Action::MoveContext { .. }) = h.key(KeyCode::Char('y')) else {
        panic!("not sent");
    };
    let moved = last_command(&h);

    h.seam.command_sending(moved).unwrap();
    h.seam
        .command_result(
            moved,
            &CommandResult::Rejected {
                reason: RejectReason::WorkdirNotAllowed,
            },
        )
        .unwrap();
    h.sync();
    h.key(KeyCode::Char('C'));
    // Another row (the current context) has no move to drop.
    assert_eq!(h.key(KeyCode::Char('x')), None);
    h.key(KeyCode::Up);
    assert_eq!(h.key(KeyCode::Char('x')), Some(Action::Discard(moved)));
    assert!(h.seam.pending_outbox().unwrap().is_empty());
}

#[test]
fn contexts_picker_views_one_context_and_back() {
    let (mut h, id, _) = two_contexts();
    let start = skywave_proto::ContextId::first_of(id);
    let screen = h.screen();
    assert_contains(&screen, "alpha question");
    assert_contains(&screen, "beta question");
    assert!(!screen.contains("Viewing"), "{screen}");

    h.key(KeyCode::Char('C'));
    h.key(KeyCode::Up);
    assert_eq!(
        h.key(KeyCode::Char('v')),
        Some(Action::ViewContext {
            session_id: id,
            context: Some(start),
        })
    );
    let screen = h.screen();
    assert_contains(&screen, "alpha question");
    assert!(!screen.contains("beta question"), "{screen}");
    assert_contains(
        &status_line(&h, 140),
        "Viewing start — messages go to velvet walrus",
    );

    // On the current context, `v` shows the whole timeline again.
    h.key(KeyCode::Char('C'));
    assert_eq!(
        h.key(KeyCode::Char('v')),
        Some(Action::ViewContext {
            session_id: id,
            context: None,
        })
    );
    let screen = h.screen();
    assert_contains(&screen, "alpha question");
    assert_contains(&screen, "beta question");
    assert!(!screen.contains("Viewing"), "{screen}");
}

#[test]
fn a_message_refused_for_a_changed_context_is_edited_and_resent() {
    let (mut h, _, _) = open_session();
    h.type_text("written before");
    h.key(KeyCode::Enter);
    let stale = last_command(&h);
    h.key(KeyCode::Esc);
    h.key(KeyCode::Up);
    h.seam
        .command_result(
            stale,
            &CommandResult::Rejected {
                reason: RejectReason::ContextChanged,
            },
        )
        .unwrap();
    h.sync();
    assert_contains(
        &h.screen(),
        "[failed: the session moved to another context first]: written before",
    );
    assert_eq!(h.key(KeyCode::Char('e')), Some(Action::EditQueued(stale)));
    assert_eq!(h.app.composer().unwrap().text(), "written before");
    h.key(KeyCode::Enter);
    let resent = last_command(&h);
    assert_ne!(resent, stale);
    assert_contains(&h.screen(), "you [queued]: written before");
    assert!(!h.screen().contains("failed"));
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
                    parent: None,
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
                    output: Some(Full::Blob(output)),
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
    // Until a worker takes it, it says it is waiting; neither expanding nor
    // `d` asks again.
    assert_contains(&h.screen(), "│ … full 9.8 KiB · requested ");
    assert_eq!(h.key(KeyCode::Enter), None);
    assert_contains(&h.screen(), "│ requested · waiting for a slot · 9.8 KiB");
    assert_eq!(h.key(KeyCode::Char('d')), None);

    // A given-up download says why, and `d` retries it.
    h.seam
        .blob_failed(blob_id, "blob size mismatch: expected 10000 bytes")
        .unwrap();
    h.sync();
    assert_contains(
        &h.screen(),
        "│ download failed: blob size mismatch: expected 10000 bytes · 9.8 KiB · d retries",
    );
    assert_eq!(
        h.key(KeyCode::Char('d')),
        Some(Action::DownloadBlob(blob_id))
    );
    assert_eq!(h.drain_requests(), vec![Request::DownloadBlob(blob_id)]);
    assert_contains(&h.screen(), "│ requested · waiting for a slot · 9.8 KiB");

    h.seam.set_link(PairedLink::Online { latency_ms: 20 });
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
    assert_contains(&screen, "│ downloading · 4.0 KiB of 9.8 KiB (40%) ");
    // Collapsed, the footer follows the download.
    h.key(KeyCode::Enter);
    assert_contains(&h.screen(), "│ … downloading 40% ");
    // With the link down it is paused, not frozen; back online it goes on.
    h.seam.set_link(PairedLink::Reconnecting {
        attempt: 1,
        retry_at: mockup_now() + TimeDelta::seconds(5),
        offline_since: mockup_now(),
        reason: skywave_core::LinkFailure::Unreachable,
    });
    h.sync();
    assert_contains(&h.screen(), "│ … downloading 40% · paused, offline");
    h.key(KeyCode::Enter);
    assert_contains(
        &h.screen(),
        "│ downloading · 4.0 KiB of 9.8 KiB (40%) · paused, offline",
    );
    h.seam.set_link(PairedLink::Online { latency_ms: 20 });
    h.sync();
    assert!(!h.screen().contains("paused"));

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
fn footer_names_blob_size() {
    let (mut h, id, _) = open_session();
    let (blob_id, input) = blob(4300);
    pull(
        &mut h,
        vec![event(
            id,
            1,
            EventKind::ToolCall(ToolCall {
                parent: None,
                call: None,
                name: "write".into(),
                display: ToolDisplay {
                    kind: ToolKind::Other,
                    title: "a.txt".into(),
                    detail: None,
                    body: None,
                },
                input: Some(Full::Blob(input)),
            }),
        )],
    );
    h.key(KeyCode::Esc);
    h.key(KeyCode::Up);
    // The only blob is hidden: the call's line gives its size.
    assert_contains(&h.screen(), "> 12:01 • write a.txt · input 4.2 KiB ");
    assert_contains(
        &footer(&h),
        "Enter expand · d download (4.2 KiB) · y/Y copy",
    );
    assert_eq!(
        h.key(KeyCode::Char('d')),
        Some(Action::DownloadBlob(blob_id))
    );
    assert_contains(&h.screen(), "• write a.txt · input 4.2 KiB · requested ");
    // Asked for, `d` has nothing more to fetch.
    assert_contains(&footer(&h), "Enter expand · d download · y/Y copy");
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
                output: Some(Full::Blob(output)),
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
                        parent: None,
                        text: format!("message {i:02}"),
                        full: None,
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
            parent: None,
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
                parent: None,
                text: format!("chatter {i}"),
                full: None,
            },
        )
    }));
    events.push(event(
        id,
        3,
        EventKind::ToolCall(ToolCall {
            parent: None,
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
    assert_contains(&h.screen(), "> 12:02 ▌ agent: chatter 0");
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
    h.key(KeyCode::Char('G'));
    assert_contains(&h.screen(), "new context; switch to or view one");
    h.key(KeyCode::Char('?'));
    h.key(KeyCode::Char('i'));
    assert!(h.app.typing());
    h.key(KeyCode::Esc);
    assert_eq!(h.key(KeyCode::Esc), Some(Action::Back));
}

#[test]
fn drafts_survive_leaving_the_session() {
    let (mut h, id, _) = open_session();
    h.type_text("half a thöught");
    h.key(KeyCode::Left);
    h.key(KeyCode::Esc);
    // Leaving saves the draft first; the list marks the session.
    assert_eq!(h.key(KeyCode::Esc), Some(Action::Back));
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

// ---- unread -------------------------------------------------------------------

/// Draws a frame, then makes the read mark the event loop makes after it,
/// `after` the harness's start.
fn look(h: &mut Harness, after: Duration) -> String {
    let screen = h.screen();
    skywave_tui::mark_read(&mut h.app, &h.core, h.t0 + after, false);
    h.sync();
    screen
}

/// Back to the session list from the composer.
fn leave(h: &mut Harness) {
    h.key(KeyCode::Esc);
    assert_eq!(h.key(KeyCode::Esc), Some(Action::Back));
}

fn agent_messages(id: SessionId, prefix: &str, n: u32) -> Vec<Event> {
    (0..n)
        .map(|i| event(id, 1, agent(&format!("{prefix} {i:02}"))))
        .collect()
}

#[test]
fn unread_badge_clears_after_reaching_the_end() {
    let mut h = Harness::paired();
    let id = SessionId::new();
    let other = DeviceId::new();
    let mut events = vec![created(id, 0, "/srv/work"), event(id, 1, user(other, "go"))];
    events.extend(agent_messages(id, "news", 40));
    pull(&mut h, events);
    // Another device's message is not news; the agent's 40 are.
    assert_contains(&line_with(&h.screen(), "/srv/work"), "● 40");

    // Opened at the rule; nothing is read while scrolled back, not even
    // by leaving.
    assert_eq!(h.key(KeyCode::Enter), Some(Action::OpenSession(id)));
    assert_contains(&look(&mut h, Duration::ZERO), "scrolled back");
    leave(&mut h);
    assert_contains(&line_with(&h.screen(), "/srv/work"), "● 40");

    // `End` follows the newest item: read.
    h.key(KeyCode::Enter);
    look(&mut h, Duration::ZERO);
    h.key(KeyCode::Esc);
    h.key(KeyCode::End);
    let screen = look(&mut h, Duration::ZERO);
    assert_contains(&screen, "news 39");
    assert_eq!(h.key(KeyCode::Esc), Some(Action::Back));
    assert!(!line_with(&h.screen(), "/srv/work").contains('●'));

    // Survives a restart: the mark is stored.
    let h = h.restart();
    assert!(!line_with(&h.screen(), "/srv/work").contains('●'));
}

#[test]
fn new_divider_marks_first_unseen_event() {
    let (mut h, id, _) = open_session();
    pull(&mut h, vec![event(id, 1, agent("seen"))]);
    // Following: the first mark is made at once.
    assert!(!look(&mut h, Duration::ZERO).contains("── new"));
    leave(&mut h);
    assert!(!line_with(&h.screen(), "/srv/work").contains('●'));

    let other = DeviceId::new();
    pull(
        &mut h,
        vec![
            event(id, 2, user(other, "from the phone")),
            event(id, 3, agent("answer")),
        ],
    );
    assert_contains(&line_with(&h.screen(), "/srv/work"), "● 1");

    // The rule sits above the first unseen event; short news follows.
    h.key(KeyCode::Enter);
    let screen = look(&mut h, Duration::ZERO);
    let lines: Vec<&str> = screen.lines().collect();
    let row = |needle: &str| lines.iter().position(|l| l.contains(needle)).unwrap();
    let rule = row("── new ──");
    assert!(
        row("seen") < rule && rule < row(": from the phone"),
        "{screen}"
    );
    assert!(!screen.contains("scrolled back"));

    // More news while following stays below the rule; marks are spaced
    // out but come.
    pull(&mut h, vec![event(id, 4, agent("more"))]);
    let t1 = h.t0 + READ_MARK_EVERY / 2;
    assert_eq!(h.app.read_mark_due_in(t1), Some(READ_MARK_EVERY / 2));
    let screen = look(&mut h, READ_MARK_EVERY / 2);
    assert_eq!(line_with(&screen, "── new ──"), lines[rule]);
    assert_eq!(h.app.read_mark_due_in(t1), Some(READ_MARK_EVERY / 2));
    look(&mut h, READ_MARK_EVERY);
    assert_eq!(h.app.read_mark_due_in(h.t0 + READ_MARK_EVERY), None);

    // A last batch is marked on leaving, without waiting.
    pull(&mut h, vec![event(id, 5, agent("last"))]);
    h.screen();
    leave(&mut h);
    assert!(!line_with(&h.screen(), "/srv/work").contains('●'));
    // Gone once everything was read.
    h.key(KeyCode::Enter);
    assert!(!h.screen().contains("── new"));
}

#[test]
fn opening_pins_to_divider_when_unread_is_long() {
    let (mut h, id, _) = open_session();
    pull(&mut h, agent_messages(id, "old", 3));
    look(&mut h, Duration::ZERO);
    leave(&mut h);
    pull(&mut h, agent_messages(id, "new", 40));

    h.key(KeyCode::Enter);
    let screen = h.screen();
    // The rule is the timeline's first line, the newest item off screen.
    let lines: Vec<&str> = screen.lines().collect();
    let rule = lines.iter().position(|l| l.contains("── new ──")).unwrap();
    assert!(lines[rule + 1].contains("new 00"), "{screen}");
    assert!(!screen.contains("old 02"));
    assert!(!screen.contains("new 39"));
    assert_contains(&screen, "scrolled back, End follows");
    // Settled into a real scroll position: not following, nothing read.
    look(&mut h, Duration::ZERO);
    assert_eq!(h.screen(), screen);
    assert_eq!(h.app.read_mark_due_in(h.t0), None);

    // Keys move from there.
    h.key(KeyCode::PageUp);
    assert_contains(&h.screen(), "old 02");
    leave(&mut h);
    assert_contains(&line_with(&h.screen(), "/srv/work"), "● 40");
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

/// The exit a signal or a failed terminal read takes, with no quit key:
/// typing the idle timer has not saved yet survives it.
#[test]
fn shutting_down_saves_the_draft() {
    let (mut h, _, _) = open_session();
    h.type_text("cut off");
    assert_eq!(h.core_draft(), None, "nothing saved before the exit");
    assert!(!h.app.ui().quit);

    skywave_tui::shut_down(&mut h.app, &h.core);
    h.sync();
    assert_eq!(h.core_draft().map(|d| d.text), Some("cut off".into()));
    let mut h = h.restart();
    h.key(KeyCode::Enter);
    assert_eq!(h.app.composer().unwrap().text(), "cut off");
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
            cursor: 3,
            when: RunWhen::Always,
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
fn a_message_over_the_cap_is_refused_before_queuing() {
    let mut h = Harness::paired();
    let id = SessionId::new();
    pull(&mut h, vec![created(id, 0, "/srv/work")]);
    // Pasted in another session of this head: the saved draft fills the
    // composer, typing that much key by key would take long.
    let long = "x".repeat(limits::MESSAGE_MAX_BYTES);
    h.core
        .dispatch(Action::SaveDraft {
            session_id: id,
            mode: ComposerMode::Agent,
            text: long.clone(),
            cursor: long.len(),
            when: RunWhen::Always,
        })
        .unwrap();
    assert_eq!(h.key(KeyCode::Enter), Some(Action::OpenSession(id)));
    h.key(KeyCode::End);
    h.type_text("y");

    assert_eq!(h.key(KeyCode::Enter), None, "refused here");
    assert_eq!(
        h.app.ui().notice.as_deref(),
        Some("message too long: the limit is 256.0 KiB")
    );
    assert_eq!(h.app.composer().unwrap().text().len(), long.len() + 1);
    assert_eq!(h.drain_requests(), vec![], "nothing queued");

    // Positive control: cut back to the cap, it goes.
    h.key(KeyCode::Backspace);
    assert!(matches!(
        h.key(KeyCode::Enter),
        Some(Action::SendMessage { text, .. }) if text == long
    ));
    assert_eq!(h.drain_requests(), vec![Request::OutboxChanged]);
}

#[test]
fn failed_draft_save_is_retried_on_the_idle_cadence() {
    let (mut h, _, _) = open_session();
    h.type_text("abc");
    h.fail_draft_save(true);
    let t = Instant::now() + DRAFT_SAVE_AFTER;
    skywave_tui::save_draft(&mut h.app, &h.core, t, false);
    h.sync();
    assert!(h.app.ui().error.is_some());
    assert_eq!(h.core_draft(), None);

    // Still unsaved, due one cadence after the failure, not at once.
    assert_eq!(h.app.draft_due_in(t), Some(DRAFT_SAVE_AFTER));
    assert_eq!(h.app.draft_save(t, false), None, "no tight retry");
    let just_before = t + DRAFT_SAVE_AFTER - Duration::from_millis(1);
    assert_eq!(h.app.draft_save(just_before, false), None);

    // Failing again pushes the next try one more cadence out.
    let t2 = t + DRAFT_SAVE_AFTER;
    skywave_tui::save_draft(&mut h.app, &h.core, t2, false);
    assert_eq!(h.app.draft_due_in(t2), Some(DRAFT_SAVE_AFTER));

    // Once saving works, the retry saves and the draft is clean.
    h.fail_draft_save(false);
    let t3 = t2 + DRAFT_SAVE_AFTER;
    skywave_tui::save_draft(&mut h.app, &h.core, t3, false);
    h.sync();
    assert_eq!(
        h.core_draft(),
        Some(Draft {
            text: "abc".into(),
            cursor: 3,
            when: RunWhen::Always,
        })
    );
    assert_eq!(h.app.draft_due_in(t3), None);
    assert_eq!(h.app.draft_save(t3, true), None, "nothing left to save");
}

#[test]
fn edit_during_a_draft_save_is_not_lost() {
    let (mut h, _, _) = open_session();
    let later = Instant::now() + 10 * DRAFT_SAVE_AFTER;

    // A save that works does not mark the later edit saved.
    h.type_text("ab");
    let save = h.app.draft_save(later, false).unwrap();
    h.type_text("c");
    h.app.draft_saved(later, h.core.dispatch(save));
    h.sync();
    assert_eq!(h.core_draft().map(|d| d.text), Some("ab".into()));
    assert!(h.app.draft_due_in(Instant::now()).unwrap() <= DRAFT_SAVE_AFTER);

    // A save that fails keeps the edit's own due time, not the failure's.
    h.fail_draft_save(true);
    let save = h.app.draft_save(later, false).unwrap();
    h.type_text("d");
    h.app.draft_saved(later, h.core.dispatch(save));
    assert!(h.app.ui().error.is_some());
    assert!(h.app.draft_due_in(Instant::now()).unwrap() <= DRAFT_SAVE_AFTER);

    h.fail_draft_save(false);
    skywave_tui::save_draft(&mut h.app, &h.core, later, false);
    h.sync();
    assert_eq!(h.core_draft().map(|d| d.text), Some("abcd".into()));
    assert_eq!(h.app.draft_due_in(later), None);
}

/// [`open_session`] with `prompts` in the log, oldest first, sent from this
/// device.
fn session_with_prompts(prompts: &[&str]) -> (Harness, SessionId, DeviceId) {
    let (mut h, id, me) = open_session();
    let events = prompts
        .iter()
        .enumerate()
        .map(|(i, text)| event(id, i as u32 + 1, user(me, text)))
        .collect();
    pull(&mut h, events);
    (h, id, me)
}

fn composer_text(h: &Harness) -> String {
    h.app.composer().unwrap().text().to_owned()
}

#[test]
fn up_at_the_top_recalls_earlier_prompts_and_down_restores_the_draft() {
    let (mut h, id, me) = session_with_prompts(&["first", "two\nlines", "third"]);
    h.type_text("half");
    h.idle_save();
    h.key(KeyCode::Up);
    assert_eq!(composer_text(&h), "third");
    assert_contains(&h.screen(), "history 1/3");
    // The recalled text is the draft, saved like typed text.
    h.idle_save();
    assert_eq!(h.core_draft().map(|d| d.text), Some("third".into()));

    // A multi-line prompt: Up moves through its lines before going older.
    h.key(KeyCode::Up);
    assert_eq!(composer_text(&h), "two\nlines");
    assert_contains(&h.screen(), "history 2/3");
    h.key(KeyCode::Up);
    assert_eq!(composer_text(&h), "two\nlines", "moved to the first line");
    h.key(KeyCode::Up);
    assert_eq!(composer_text(&h), "first");
    h.key(KeyCode::Up);
    assert_eq!(composer_text(&h), "first", "the oldest stays");

    // New prompts while browsing do not shift it.
    pull(&mut h, vec![event(id, 9, user(me, "newer"))]);
    assert_contains(&h.screen(), "history 3/3");

    h.key(KeyCode::Down);
    assert_eq!(composer_text(&h), "two\nlines");
    h.key(KeyCode::Down);
    assert_eq!(composer_text(&h), "third");
    h.key(KeyCode::Down);
    let composer = h.app.composer().unwrap();
    assert_eq!((composer.text(), composer.cursor_chars()), ("half", 4));
    assert!(!h.screen().contains("history"));
    h.idle_save();
    assert_eq!(h.core_draft().map(|d| d.text), Some("half".into()));
    h.key(KeyCode::Down);
    assert_eq!(composer_text(&h), "half", "nothing newer than the draft");
}

#[test]
fn ctrl_p_recalls_from_any_line() {
    let (mut h, _, _) = session_with_prompts(&["older", "newest"]);
    h.type_text("line one");
    h.key_with(ctrl('j'));
    h.type_text("line two");
    h.key(KeyCode::Up);
    assert_eq!(composer_text(&h), "line one\nline two", "Up moved a line");
    h.key(KeyCode::Down);
    h.key_with(ctrl('p'));
    assert_eq!(composer_text(&h), "newest");
    h.key_with(ctrl('p'));
    assert_eq!(composer_text(&h), "older");
    h.key_with(ctrl('n'));
    h.key_with(ctrl('n'));
    assert_eq!(composer_text(&h), "line one\nline two");

    // No prompts: nothing to recall.
    let (mut h, _, _) = open_session();
    h.type_text("x");
    h.key_with(ctrl('p'));
    h.key(KeyCode::Up);
    assert_eq!(composer_text(&h), "x");
    assert!(!h.screen().contains("history"));
}

#[test]
fn editing_a_recalled_prompt_ends_browsing() {
    let (mut h, id, _) = session_with_prompts(&["older", "newest"]);
    h.type_text("draft");
    h.key(KeyCode::Up);
    h.type_text("!");
    assert_eq!(composer_text(&h), "newest!");
    assert!(!h.screen().contains("history"));
    // Down no longer goes back to the old draft; Up starts over.
    h.key(KeyCode::Down);
    assert_eq!(composer_text(&h), "newest!");
    h.key(KeyCode::Up);
    assert_eq!(composer_text(&h), "newest");
    h.key(KeyCode::Down);
    assert_eq!(composer_text(&h), "newest!");

    // A cursor move ends it too, and leaving keeps the shown text.
    h.key(KeyCode::Up);
    h.key(KeyCode::Up);
    h.key(KeyCode::Left);
    h.key(KeyCode::Down);
    assert_eq!(composer_text(&h), "older");
    h.key(KeyCode::Up);
    h.key(KeyCode::Esc);
    h.key(KeyCode::Esc);
    assert_eq!(h.key(KeyCode::Enter), Some(Action::OpenSession(id)));
    assert_eq!(composer_text(&h), "newest");
}

#[test]
fn history_includes_other_devices_and_pending_messages() {
    let (mut h, id, me) = open_session();
    let other = DeviceId::new();
    pull(
        &mut h,
        vec![
            event(id, 1, user(me, "mine")),
            event(id, 2, agent("ok")),
            event(id, 3, user(other, "from the laptop")),
            event(id, 4, user(other, "from the laptop")),
        ],
    );
    h.type_text("queued");
    h.key(KeyCode::Enter);
    let mut recalled = Vec::new();
    for _ in 0..4 {
        h.key_with(ctrl('p'));
        recalled.push(composer_text(&h));
    }
    assert_eq!(recalled, ["queued", "from the laptop", "mine", "mine"]);
    assert_contains(&h.screen(), "history 3/3");
}

#[test]
fn offline_composer_says_messages_wait() {
    let (mut h, _, _) = open_session();
    h.seam.set_link(PairedLink::Reconnecting {
        attempt: 1,
        retry_at: mockup_now() + TimeDelta::seconds(5),
        offline_since: mockup_now(),
        reason: skywave_core::LinkFailure::Unreachable,
    });
    h.sync();
    assert_contains(&h.screen(), "Message · offline, sends when back online");
}

#[test]
fn usage_totals_turns_across_sessions_by_range_and_grouping() {
    let mut h = Harness::paired();
    let (a, b) = (SessionId::new(), SessionId::new());
    let turn = |id, minute, cost| {
        event(
            id,
            minute,
            EventKind::TurnUsage(TurnUsage {
                tokens_in: Some(3_000),
                tokens_out: Some(500),
                cost_usd: cost,
                model: Some("model-a".into()),
                ..TurnUsage::default()
            }),
        )
    };
    pull(
        &mut h,
        vec![
            created(a, 0, "/srv/a"),
            created(b, 1, "/srv/b"),
            turn(a, 2, Some(1.25)),
            turn(b, 3, None),
        ],
    );
    assert_eq!(
        h.key(KeyCode::Char('u')),
        Some(Action::Navigate(Route::Usage))
    );
    // The turns are days old: the default week may leave them out
    // whatever the time zone, so take every day.
    h.key(KeyCode::Char('r'));
    h.key(KeyCode::Char('r'));
    let screen = h.screen();
    assert_contains(&screen, "[all]");
    assert_contains(
        &screen,
        "Total  $1.25 + 1 unpriced · 6.0k in · 1.0k out · 2 turns",
    );
    assert_contains(&screen, "deleted from this device");

    h.key(KeyCode::Tab);
    let screen = h.screen();
    assert_contains(&screen, "[by session]");
    assert!(line_with(&screen, "a  ").contains("$1.25"), "{screen}");
    assert!(
        line_with(&screen, "cost unknown").starts_with('b'),
        "{screen}"
    );

    h.key(KeyCode::Tab);
    h.key(KeyCode::Tab);
    let screen = h.screen();
    assert_contains(&screen, "model-a  $1.25 + 1 unpriced");

    assert_eq!(h.key(KeyCode::Esc), Some(Action::Back));
    assert!(matches!(h.app.view().screen, Screen::SessionList(_)));
}

#[test]
fn storage_asks_the_server_and_lists_what_takes_the_most() {
    let mut h = Harness::paired();
    let (a, gone) = (SessionId::new(), SessionId::new());
    pull(&mut h, vec![created(a, 0, "/srv/a")]);
    assert_eq!(
        h.key(KeyCode::Char('S')),
        Some(Action::Navigate(Route::Storage))
    );
    let req = match h.drain_requests().as_slice() {
        [Request::Storage { req }] => *req,
        other => panic!("expected one storage request, got {other:?}"),
    };
    assert_contains(&h.screen(), "Asking the server…");

    let report = skywave_proto::StorageReport {
        database: 3 * 1024 * 1024,
        event_blobs: 2 * 1024 * 1024,
        workdir_blobs: 0,
        shell_running: 0,
        log_file: Some(1024),
        free: Some(8 * 1024 * 1024 * 1024),
        min_free: 0,
        sessions: vec![skywave_proto::SessionStorage {
            session: a,
            events: 12,
            event_bytes: 4096,
            blobs: 1,
            blob_bytes: 2 * 1024 * 1024,
        }],
        blobs: vec![skywave_proto::BlobStorage {
            id: skywave_proto::BlobId::new(),
            size: 2 * 1024 * 1024,
            session: gone,
            seq: skywave_proto::Seq(7),
            holds: skywave_proto::BlobUse::ShellOutput,
        }],
        shell_runs: Vec::new(),
    };
    h.seam.storage_reported(req, Ok(report));
    h.sync();
    let screen = h.screen();
    assert_contains(&screen, "Free                  8.0 GiB");
    assert_contains(&screen, "[by session]");
    assert_contains(&screen, "2.0 MiB  a");
    assert_contains(&screen, "12 events · 4.0 KiB in the log · 1 blob 2.0 MiB");

    h.key(KeyCode::Tab);
    let screen = h.screen();
    assert_contains(&screen, "[largest blobs]");
    assert_contains(&screen, "(not on this device)");
    assert_contains(&screen, "shell output · event 7");
    h.key(KeyCode::Tab);
    assert_contains(&h.screen(), "No shell runs.");

    assert_eq!(h.key(KeyCode::Char('r')), Some(Action::RefreshStorage));
    assert!(matches!(
        h.drain_requests().as_slice(),
        [Request::Storage { .. }]
    ));
    assert_contains(&h.screen(), "Refreshing…");
    assert_eq!(h.key(KeyCode::Esc), Some(Action::Back));
    assert!(matches!(h.app.view().screen, Screen::SessionList(_)));
}

#[test]
fn usage_limits_are_asked_once_kept_and_refreshed_on_r() {
    let mut h = Harness::paired();
    assert_eq!(
        h.key(KeyCode::Char('L')),
        Some(Action::Navigate(Route::UsageLimits))
    );
    let req = match h.drain_requests().as_slice() {
        [Request::UsageLimits { req }] => *req,
        other => panic!("expected one usage limits request, got {other:?}"),
    };
    assert_contains(&h.screen(), "Asking the agent…");
    let read = skywave_proto::UsageLimitsResult::Read {
        at: mockup_now() - TimeDelta::minutes(12),
        limits: vec![
            skywave_proto::UsageLimit {
                window: skywave_proto::LimitWindow::Session,
                scope: None,
                percent: Some(75),
                severity: Some(skywave_proto::LimitSeverity::Warning),
                resets_at: Some(mockup_now() + TimeDelta::minutes(90)),
            },
            skywave_proto::UsageLimit {
                window: skywave_proto::LimitWindow::Weekly,
                scope: Some("Fable".into()),
                percent: None,
                severity: None,
                resets_at: None,
            },
        ],
    };
    h.seam.usage_limits_answered(req, Ok(read)).unwrap();
    h.sync();
    let screen = h.screen();
    assert_contains(&screen, "Read 12 min ago");
    assert_contains(&screen, "Session          75%  resets in 1 h 30 min");
    assert_contains(&screen, "Weekly · Fable");
    assert_contains(&screen, "use not reported");

    // Kept: after a restart the page shows them and asks nothing.
    let mut h = h.restart();
    h.drain_requests();
    h.key(KeyCode::Char('L'));
    assert!(h.drain_requests().is_empty());
    assert_contains(&h.screen(), "Weekly · Fable");

    assert_eq!(h.key(KeyCode::Char('r')), Some(Action::RefreshUsageLimits));
    assert!(matches!(
        h.drain_requests().as_slice(),
        [Request::UsageLimits { .. }]
    ));
    assert_contains(&h.screen(), "Refreshing…");
    assert_eq!(h.key(KeyCode::Esc), Some(Action::Back));
    assert!(matches!(h.app.view().screen, Screen::SessionList(_)));
}

#[test]
fn the_session_info_page_shows_the_session_its_usage_and_its_subagents() {
    let (mut h, id, _) = with_subagent(false);
    pull(&mut h, two_turns(id));
    h.drain_requests();
    assert_eq!(
        h.key(KeyCode::Char('I')),
        Some(Action::OpenSessionInfo { session_id: id })
    );
    assert!(matches!(
        h.drain_requests().as_slice(),
        [Request::UsageLimits { .. }]
    ));
    let screen = h.screen();
    assert_contains(&screen, "Workdir  /srv/work");
    assert_contains(&screen, "Asking the agent…");
    assert_contains(&screen, "Tokens in   3.1k");
    assert_contains(&screen, "Context     19k/1M · 2%");
    assert_contains(&screen, "Cost        $1.27");

    // The rest is below: scrolled to, it stops at the last line.
    for _ in 0..40 {
        h.key(KeyCode::Char('j'));
    }
    let screen = h.screen();
    assert_contains(&screen, "model-a        $1.23 · tokens unknown · 1 turn");
    assert_contains(&screen, "Subagents (1)");
    assert_contains(&screen, "Explore");
    assert_contains(&screen, "finished · 3 steps");
    assert!(h.app.ui().info.top < 40);

    assert_eq!(h.key(KeyCode::Char('r')), Some(Action::RefreshUsageLimits));
    assert_eq!(h.key(KeyCode::Esc), Some(Action::Back));
    assert!(matches!(h.app.view().screen, Screen::Session(_)));
}

#[test]
fn settings_explain_the_blob_policy() {
    let mut h = Harness::paired();
    h.key(KeyCode::Char('s'));
    let screen = h.screen();
    assert_contains(
        &screen,
        "(•) On demand  a tool's blob downloads when you expand",
    );
    assert_contains(
        &screen,
        "( ) Eager      every tool blob downloads as it syncs",
    );
    assert_contains(
        &screen,
        "Workdir diffs and files always download when opened",
    );
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
                    parent: None,
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
                    suggestions: Vec::new(),
                    shell: None,
                },
            ),
            event(
                id,
                3,
                EventKind::ApprovalResolved {
                    id: approval,
                    decision: Decision::Approve,
                    by: me,
                    rule: None,
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
        parent: None,
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
        device_id: device,
        text: text.into(),
        attachments: Vec::new(),
    }
}

fn agent(text: &str) -> EventKind {
    EventKind::AgentMessage {
        parent: None,
        text: text.into(),
        full: None,
    }
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
    let ev = |minute, second, kind: EventKind| Event {
        session_id: id,
        at: at_s(minute, second),
        command_id: kind.logged_by_command().then(CommandId::new),
        context: skywave_proto::ContextId::first_of(id),
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
        full: Some(Full::Blob(file)),
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
            suggestions: Vec::new(),
            shell: None,
        }
    };
    let resolve =
        |approval: ApprovalId, decision: Decision, by: DeviceId| EventKind::ApprovalResolved {
            id: approval,
            decision,
            by,
            rule: None,
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
                    output: Some(Full::Blob(output)),
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
┌ add a hello binary and wire it into main · /srv/work · approval ───────────────────────┐
│agent waiting for an approval                                                           │
│  12:01 ▌ you: add a hello binary and wire it into main                                 │
│        ▌ agent: I'll add the binary, run it, then use it from main.                    │
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
│        ▌ agent: Done: main now exits with run()'s code. A long reply wraps under its   │
│        ▌ own column instead of falling back to the left edge.                          │
│────────────────────────────────────────────────────────────────────────────────────────│
│  12:04 ▌ device …c0ffee42: run the tests and clean up                                  │
│        ❯ Bash $ cargo test                                               exit 101 · 12s│
│        │ running 3 tests                                                               │
│        │ test tests::adds ... FAILED                                                   │
│        │ test tests::greets ... ok                                                     │
│        │ … +20 lines · full 2.0 KiB · not downloaded                                   │
│        ❯ Bash $ rm -rf target                                               denied here│
│          Clean the build directory                                                     │
│        ▌ agent: One test fails; I left target/ alone.                                  │
│────────────────────────────────────────────────────────────────────────────────────────│
│  12:06 ▌ you: write it up in NOTES.md                                                  │
│        ✎ Write NOTES.md                                          cancelled: interrupted│
│        interrupted · the agent stopped its turn                                        │
│────────────────────────────────────────────────────────────────────────────────────────│
│  12:08 ▌ you: fix the failing test                                                     │
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
fn messages_carry_a_bar_and_calls_step_back() {
    let (h, _, _, _) = mockup();
    let frame = h.frame(90, 60, mockup_now());
    let cell = |needle: &str| {
        let screen = text(&frame);
        let (y, line) = screen
            .lines()
            .enumerate()
            .find(|(_, line)| line.contains(needle))
            .unwrap_or_else(|| panic!("{needle:?} not on screen:\n{screen}"));
        let x = line[..line.find(needle).unwrap()].chars().count();
        frame[(x as u16, y as u16)].clone()
    };
    let dim = |needle: &str| {
        cell(needle)
            .modifier
            .contains(ratatui::style::Modifier::DIM)
    };
    // The bar takes its author's colour, on every row of the message.
    assert_eq!(cell("▌ you: add a hello").fg, Color::Blue);
    assert_eq!(cell("▌ device …c0ffee42").fg, Color::Magenta);
    assert_eq!(cell("▌ agent: Done").fg, Color::Green);
    assert_eq!(cell("▌ own column").fg, Color::Green);
    assert!(!dim("agent: Done"));
    // Calls are dimmed, but the one an approval waits on.
    assert!(dim("✎ Write src/bin/hello.rs"));
    assert!(dim("❯ Bash $ cargo test  "));
    assert!(!dim("± Edit src/lib.rs   "));
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
    assert_contains(&h.screen(), "> 12:08 ▌ you: fix the failing test");
    h.key(KeyCode::Char('['));
    assert_contains(&h.screen(), "> 12:06 ▌ you: write it up in NOTES.md");
    h.key(KeyCode::Char('['));
    h.key(KeyCode::Char('['));
    let screen = h.screen();
    assert!(
        screen
            .lines()
            .nth(3)
            .unwrap()
            .starts_with("│> 12:01 ▌ you: add a hello binary"),
        "the turn is at the top, its message selected:\n{screen}"
    );
    h.key(KeyCode::Down);
    assert_contains(&h.screen(), ">       ▌ agent: I'll add the binary");
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
    assert_contains(&h.screen(), ">       ▌ agent: Done:");
    assert_eq!(h.key(KeyCode::Enter), None);
    assert!(
        h.app
            .ui()
            .session
            .as_ref()
            .unwrap()
            .shown
            .expanded
            .is_empty()
    );
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

/// How much laying out the open session's timeline took so far.
fn layout_stats(h: &Harness) -> ui::LayoutStats {
    h.app
        .ui()
        .session
        .as_ref()
        .unwrap()
        .shown
        .layout
        .borrow()
        .stats()
}

#[test]
fn a_redraw_lays_out_only_the_items_that_changed() {
    let (mut h, _, file_blob, output_blob) = mockup();
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
    let items = h.app.session().unwrap().0.timeline.len() as u64;
    // Selecting drew frames 100 columns wide, before the file was here.
    let before = layout_stats(&h);
    assert_eq!(before.blob_reads, 0);

    let now = mockup_now();
    let first = trimmed(&h.frame(90, 58, now));
    assert_contains(&first, "│ 10 line 10 ");
    let laid = layout_stats(&h);
    assert_eq!(
        laid.items,
        before.items + items,
        "a new width lays out every item"
    );
    assert_eq!(laid.blob_reads, 1, "the expanded file is read");

    // Nothing changed: nothing is laid out or read again, and the frame
    // is the same.
    assert_eq!(trimmed(&h.frame(90, 58, now)), first);
    assert_eq!(layout_stats(&h), laid);

    // The clock moved: only the running call's elapsed time changes.
    let later = now + chrono::Duration::seconds(1);
    h.frame(90, 58, later);
    let ticked = layout_stats(&h);
    assert_eq!(ticked.items, laid.items + 1);
    assert_eq!(ticked.blob_reads, 1);

    // One item's blob starts downloading: that item alone is laid out.
    h.seam
        .blob_status(
            output_blob,
            BlobStatus::Downloading {
                got: 1024,
                path: dir.path().join("partial"),
            },
        )
        .unwrap();
    h.sync();
    assert_contains(&trimmed(&h.frame(90, 58, later)), "downloading 50%");
    let changed = layout_stats(&h);
    assert_eq!(changed.items, ticked.items + 1);
    assert_eq!(changed.blob_reads, 1);

    // A new width lays out every item again, from the file as read.
    let narrow = trimmed(&h.frame(80, 58, later));
    assert_contains(&narrow, "│ 10 line 10 ");
    let resized = layout_stats(&h);
    assert_eq!(resized.items, changed.items + items);
    assert_eq!(resized.blob_reads, 1);
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
    let total = h
        .app
        .ui()
        .session
        .as_ref()
        .unwrap()
        .shown
        .geometry
        .borrow()
        .total;
    h.key(KeyCode::Enter);
    let after = h.screen();
    let grown = h
        .app
        .ui()
        .session
        .as_ref()
        .unwrap()
        .shown
        .geometry
        .borrow()
        .total;
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
        "│        ▌ agent: I'll add the binary, run it, then use it",
    );
    assert_contains(&screen, "│        ▌ from main. ");
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
    gated_offering(before, tool, Vec::new())
}

/// As [`gated`], the approval offering `suggestions`.
fn gated_offering(
    before: Vec<EventKind>,
    tool: ToolCall,
    suggestions: Vec<RuleSuggestion>,
) -> (Harness, SessionId, ApprovalId, Seq) {
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
        suggestions,
        shell: None,
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

fn rule(label: &str, scope: RuleScope) -> RuleSuggestion {
    RuleSuggestion {
        label: label.into(),
        scope,
    }
}

fn granted(approval: ApprovalId, by: DeviceId, rule: u8) -> EventKind {
    EventKind::ApprovalResolved {
        id: approval,
        decision: Decision::Approve,
        by,
        rule: Some(rule),
    }
}

#[test]
fn shift_a_grants_the_one_rule_offered() {
    let bash = call(CallId::new(), "Bash", ToolKind::Shell, "cargo test");
    let offered = vec![rule("Bash(cargo test)", RuleScope::Project)];
    let (mut h, id, approval, _) = gated_offering(vec![], bash, offered);
    let rows = panel(&h.screen());
    assert_contains(&rows[1], "A  always allow · Bash(cargo test) (project)");
    assert_contains(&rows[2], "Esc, then a approve · x deny");
    // Typed into the message, not an answer.
    assert_eq!(h.key(KeyCode::Char('A')), None);
    h.key(KeyCode::Backspace);
    h.key(KeyCode::Esc);
    assert_contains(
        &h.screen(),
        "a/x approve/deny · A always allow · v show the call",
    );

    assert_eq!(
        h.key(KeyCode::Char('A')),
        Some(Action::ResolveApproval {
            id: approval,
            decision: Decision::Approve,
            rule: Some(0),
        })
    );
    let rows = panel(&h.screen());
    assert_contains(
        rows.last().unwrap(),
        "you approved always · Bash(cargo test) (project) · queued",
    );
    assert!(
        !rows.iter().any(|r| r.contains("A  always allow")),
        "{rows:#?}"
    );

    // Logged: said on the call, counted in the status line, listed in help.
    let me = h.seam.pairing().unwrap().credentials.device_id;
    pull(&mut h, vec![event(id, 2, granted(approval, me, 0))]);
    let screen = h.screen();
    assert!(!screen.contains("Approval needed"), "{screen}");
    assert_contains(&screen, "approved always here · Bash(cargo test)");
    assert_contains(&screen, "agent idle · 1 rule");
    h.key(KeyCode::Char('?'));
    let screen = text(&h.frame(100, 60, mockup_now()));
    assert_contains(&screen, "Rules granted in this session");
    assert_contains(&screen, "  Bash(cargo test) (project)");
    assert_contains(
        &screen,
        "Granted here only; the agent's settings may hold more.",
    );
    assert_contains(&screen, "A             approve, always allowing a rule");
}

#[test]
fn shift_a_lists_several_rules_to_pick_one() {
    let bash = call(CallId::new(), "Bash", ToolKind::Shell, "cargo test");
    let before = (0..30).map(|i| agent(&format!("message {i}"))).collect();
    let offered = vec![
        rule("Bash(cargo test)", RuleScope::Project),
        rule("all edits", RuleScope::Session),
        rule("access to /srv/data", RuleScope::User),
    ];
    let (mut h, _, approval, _) = gated_offering(before, bash, offered);
    h.key(KeyCode::Esc);
    h.key(KeyCode::Up);
    let selected = h.app.ui().session.as_ref().unwrap().shown.selected;
    assert!(selected.is_some());
    assert_contains(
        &panel(&h.screen())[1],
        "A  always allow · choose one of 3 rules",
    );

    // Esc goes back to a / x, nothing answered.
    assert_eq!(h.key(KeyCode::Char('A')), None);
    let rows = panel(&h.screen());
    assert_contains(&rows[1], "› Bash(cargo test) (project)");
    assert_contains(&rows[2], "  all edits (this session)");
    assert_contains(&rows[3], "  access to /srv/data (user)");
    assert_contains(
        &rows[4],
        "↑↓ choose · Enter approve and always allow · Esc back to a/x",
    );
    assert_contains(
        &h.screen(),
        "↑↓ choose · Enter approve and always allow · Esc back to a/x · ? help",
    );
    assert_eq!(h.key(KeyCode::Esc), None);
    let rows = panel(&h.screen());
    assert_contains(&rows[1], "A  always allow · choose one of 3 rules");
    assert_contains(&rows[2], "a approve · x deny");
    assert!(
        h.app.session().is_some(),
        "Esc closed the list, not the session"
    );

    // ↑↓ pick in the list, clamped at its ends; the timeline stays put.
    h.key(KeyCode::Char('A'));
    for code in [KeyCode::Down, KeyCode::Down, KeyCode::Down, KeyCode::Up] {
        assert_eq!(h.key(code), None);
    }
    assert_eq!(
        h.app.ui().session.as_ref().unwrap().shown.selected,
        selected
    );
    assert_contains(&panel(&h.screen())[2], "› all edits (this session)");
    assert_eq!(
        h.key(KeyCode::Enter),
        Some(Action::ResolveApproval {
            id: approval,
            decision: Decision::Approve,
            rule: Some(1),
        })
    );
    assert_contains(
        panel(&h.screen()).last().unwrap(),
        "you approved always · all edits (this session) · queued",
    );
}

#[test]
fn the_rule_list_keeps_to_the_panel_row_cap() {
    let bash = call(CallId::new(), "Bash", ToolKind::Shell, "cargo test");
    let offered = (0..8)
        .map(|i| rule(&format!("rule {i}"), RuleScope::Session))
        .collect();
    let (mut h, _, _, _) = gated_offering(vec![], bash, offered);
    h.key(KeyCode::Esc);
    h.key(KeyCode::Char('A'));
    // 20 rows inside: six rows of rules, the highlighted one in view.
    let rows = panel(&h.screen());
    assert_eq!(rows.len(), 1 + 6 + 1, "{rows:#?}");
    for _ in 0..7 {
        h.key(KeyCode::Down);
    }
    let rows = panel(&h.screen());
    assert_contains(&rows[1], "  rule 2 (this session)");
    assert_contains(&rows[6], "› rule 7 (this session)");
}

#[test]
fn a_rule_granted_elsewhere_shows_with_the_device() {
    let bash = call(CallId::new(), "Bash", ToolKind::Shell, "cargo test");
    let offered = vec![rule("Bash(cargo test)", RuleScope::Project)];
    let (mut h, id, approval, _) = gated_offering(vec![], bash, offered);
    let phone = DeviceId::new();
    pull(&mut h, vec![event(id, 2, granted(approval, phone, 0))]);
    let screen = h.screen();
    assert_contains(
        &screen,
        &format!(
            "approved always on device …{} · Bash(cargo test)",
            tail(phone)
        ),
    );
    assert_contains(&screen, "· 1 rule");
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
    assert_eq!(ui.shown.selected, Some(ItemKey::Logged(seq)));
    assert!(ui.shown.expanded.contains(&seq));
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
                rule: None,
            },
        )],
    );
    let screen = h.screen();
    assert!(!screen.contains("Approval needed"), "{screen}");
    assert_eq!(
        h.app.ui().session.as_ref().unwrap().shown.selected,
        Some(ItemKey::Logged(seq))
    );
    assert_contains(&screen, ">       ± Edit src/lib.rs");
}

#[test]
fn a_call_revealed_before_its_result_downloads_the_output_when_it_comes() {
    let id_call = CallId::new();
    let mut edit = call(id_call, "Edit", ToolKind::FileEdit, "src/lib.rs");
    edit.display.body = body(BodyFormat::Diff, "+added\n");
    let (mut h, id, approval, _) = gated(Vec::new(), edit);
    h.key(KeyCode::Esc);
    assert_eq!(h.key(KeyCode::Char('v')), None, "nothing to download yet");
    assert_eq!(h.drain_requests(), vec![]);

    // Approved; its result logs with an output blob under the open call.
    let (blob_id, output) = blob(10_000);
    let mut done = result(id_call, false, "", "edited");
    done.output = Some(Full::Blob(output));
    pull(
        &mut h,
        vec![
            event(
                id,
                2,
                EventKind::ApprovalResolved {
                    id: approval,
                    decision: Decision::Approve,
                    by: DeviceId::new(),
                    rule: None,
                },
            ),
            event(id, 2, EventKind::ToolResult(done)),
        ],
    );
    assert_eq!(h.drain_requests(), vec![Request::DownloadBlob(blob_id)]);
    let screen = h.screen();
    assert!(!screen.contains("not downloaded"), "{screen}");
    assert_contains(&screen, "requested");

    // Asked for once, not on every snapshot while it is in flight.
    pull(&mut h, vec![event(id, 3, agent("still here"))]);
    h.sync();
    assert_eq!(h.drain_requests(), vec![]);
}

/// The scroll state of the open session: `None` follows.
fn scroll(h: &Harness) -> Option<Anchor> {
    h.app.ui().session.as_ref().unwrap().shown.scroll
}

#[test]
fn v_keeps_following_when_the_call_fits_at_the_bottom() {
    let mut edit = call(CallId::new(), "Edit", ToolKind::FileEdit, "src/lib.rs");
    edit.display.body = body(BodyFormat::Diff, "+added 1\n+added 2\n+added 3\n");
    let (mut h, id, _, seq) = gated(vec![agent("before")], edit);
    h.key(KeyCode::Esc);
    h.screen();
    assert_eq!(h.key(KeyCode::Char('v')), None);
    // As the main loop does: a read mark before the expanded frame is
    // drawn decides nothing yet.
    skywave_tui::mark_read(&mut h.app, &h.core, h.t0, false);
    let screen = h.screen();
    assert_contains(&screen, ">       ± Edit src/lib.rs");
    assert_contains(&screen, "│ +added 3");
    assert!(!screen.contains("scrolled back"), "{screen}");
    skywave_tui::mark_read(&mut h.app, &h.core, h.t0, false);
    assert_eq!(scroll(&h), None);
    assert_eq!(selected(&h), Some(ItemKey::Logged(seq)));

    // Still following: what arrives next shows.
    pull(&mut h, vec![event(id, 2, agent("newest"))]);
    let screen = h.screen();
    assert_contains(&screen, "newest");
    assert!(!screen.contains("scrolled back"), "{screen}");
}

#[test]
fn v_pins_the_call_when_it_is_off_screen() {
    let mut edit = call(CallId::new(), "Edit", ToolKind::FileEdit, "src/lib.rs");
    edit.display.body = body(BodyFormat::Diff, "+added 1\n");
    let (mut h, id, _, seq) = gated(vec![], edit);
    let after = (0..30)
        .map(|i| event(id, 2, agent(&format!("message {i}"))))
        .collect();
    pull(&mut h, after);
    h.key(KeyCode::Esc);
    let screen = h.screen();
    assert!(!screen.contains("       ± Edit src/lib.rs"), "{screen}");
    assert_eq!(h.key(KeyCode::Char('v')), None);
    let screen = h.screen();
    assert_contains(screen.lines().nth(3).unwrap(), "> 12:01 ± Edit src/lib.rs");
    assert_contains(&screen, "scrolled back, End follows");
    skywave_tui::mark_read(&mut h.app, &h.core, h.t0, false);
    assert_eq!(
        scroll(&h).map(|anchor| (anchor.key, anchor.offset)),
        Some((ItemKey::Logged(seq), 0))
    );

    // Pinned: what arrives next does not move it.
    pull(&mut h, vec![event(id, 3, agent("newest"))]);
    let screen = h.screen();
    assert!(!screen.contains("newest"), "{screen}");
    assert_contains(screen.lines().nth(3).unwrap(), "> 12:01 ± Edit src/lib.rs");
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
    h.app.ui().session.as_ref().unwrap().shown.selected
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
    assert_contains(&h.screen(), ">       ▌ you [queued]: not sent yet");
    // A message expands to nothing and has nothing to download.
    assert_eq!(h.key(KeyCode::Enter), None);
    assert_eq!(h.key(KeyCode::Char('d')), None);
    assert!(
        h.app
            .ui()
            .session
            .as_ref()
            .unwrap()
            .shown
            .expanded
            .is_empty()
    );
    h.key(KeyCode::Up);
    assert_contains(&h.screen(), "> 12:03 ▌ you: and this");
    h.key(KeyCode::Up);
    assert_contains(&h.screen(), "> 12:02 ▌ agent: on it");
    h.key(KeyCode::Up);
    let line = line_with(&h.screen(), ": from the phone");
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

    pull(&mut h, vec![message(id, 2, command, me, "hello")]);
    assert_eq!(selected(&h), Some(ItemKey::Logged(seq)));
    assert_contains(&h.screen(), "> 12:02 ▌ you [accepted]: hello");
    // And moves on from there.
    h.key(KeyCode::Up);
    assert_contains(&h.screen(), "> 12:01 ▌ agent: ready");
}

/// Which `pending NN` message each screen row shows, if any; the session
/// title, named after the first one, aside.
fn pending_rows(screen: &str) -> Vec<Option<String>> {
    screen
        .lines()
        .map(|line| {
            let at = line.find(": pending ")? + 2;
            line.get(at..at + "pending 00".len()).map(str::to_owned)
        })
        .collect()
}

#[test]
fn scroll_follows_pending_to_logged() {
    let (mut h, id, me) = open_session();
    let sent: Vec<CommandId> = (0..40)
        .map(|i| {
            h.type_text(&format!("pending {i:02}"));
            h.key(KeyCode::Enter);
            last_command(&h)
        })
        .collect();
    h.key(KeyCode::Esc);
    h.screen();
    h.key(KeyCode::PageUp);
    let before = pending_rows(&h.screen());
    let anchor = scroll(&h).expect("scrolled back");
    let ItemKey::Pending(command) = anchor.key else {
        panic!("anchored to {anchor:?}, not a pending message");
    };
    let at = sent.iter().position(|c| *c == command).unwrap();

    // Logged after a reply that lands above them all: the rows they show
    // move down in the timeline, and the view moves with them.
    let first = h.seam.cursor().unwrap().0 + 1;
    let mut events = vec![event(id, 2, agent("reply"))];
    events.extend(
        sent.iter()
            .enumerate()
            .map(|(i, command)| message(id, 3, *command, me, format!("pending {i:02}"))),
    );
    pull(&mut h, events);
    let screen = h.screen();
    assert_eq!(pending_rows(&screen), before, "{screen}");
    assert_eq!(
        scroll(&h),
        Some(Anchor {
            key: ItemKey::Logged(Seq(first + 1 + at as u64)),
            offset: anchor.offset,
        })
    );
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
    h.app.set_view(view);
    assert_eq!(selected(&h), Some(ItemKey::Pending(second)));
    assert_contains(&h.screen(), ">       ▌ you [queued]: second");
    let view = without(&h, ItemKey::Pending(second));
    h.app.set_view(view);
    assert_eq!(selected(&h), Some(ItemKey::Logged(Seq(2))));
    // Nothing left to select.
    let view = without(&h, ItemKey::Logged(Seq(2)));
    h.app.set_view(view);
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
    assert_contains(&h.screen(), ">       ▌ agent: b");
    // Back from inside a turn: its message; from there, the turn before.
    h.key(KeyCode::Char('['));
    assert_eq!(selected(&h), Some(ItemKey::Logged(Seq(4))));
    h.key(KeyCode::Char('['));
    assert_eq!(selected(&h), Some(ItemKey::Logged(Seq(2))));
    assert_contains(&h.screen(), "> 12:01 ▌ you: one");
    h.key(KeyCode::Char(']'));
    assert_eq!(selected(&h), Some(ItemKey::Logged(Seq(4))));
    // A pending message starts a turn too.
    h.key(KeyCode::Char(']'));
    assert_eq!(selected(&h), Some(ItemKey::Pending(three)));
}

#[test]
fn t_hides_tool_calls_until_pressed_again() {
    let (mut h, id, me) = open_session();
    let (read, write) = (CallId::new(), CallId::new());
    pull(
        &mut h,
        vec![
            event(id, 1, user(me, "go")),
            event(
                id,
                2,
                EventKind::ToolCall(call(read, "Read", ToolKind::FileRead, "a.txt")),
            ),
            event(id, 2, EventKind::ToolResult(result(read, false, "", "x"))),
            event(id, 3, agent("read it")),
            event(
                id,
                4,
                EventKind::ToolCall(call(write, "Write", ToolKind::FileWrite, "b.txt")),
            ),
        ],
    );
    h.key(KeyCode::Esc);
    h.key(KeyCode::Up);
    assert_eq!(selected(&h), Some(ItemKey::Logged(Seq(6))));
    assert_contains(&h.screen(), "Write b.txt");

    h.key(KeyCode::Char('t'));
    let screen = h.screen();
    assert!(!screen.contains("Read a.txt"), "{screen}");
    assert!(!screen.contains("Write b.txt"), "{screen}");
    assert_contains(&screen, "tool calls hidden · t shows them");
    // The selection was on the hidden write: it moves to the message above.
    assert_eq!(selected(&h), Some(ItemKey::Logged(Seq(5))));
    h.key(KeyCode::Up);
    assert_eq!(selected(&h), Some(ItemKey::Logged(Seq(2))));

    // A call arriving while hidden stays hidden.
    let more = CallId::new();
    pull(
        &mut h,
        vec![event(
            id,
            5,
            EventKind::ToolCall(call(more, "Bash", ToolKind::Shell, "make")),
        )],
    );
    assert!(!h.screen().contains("make"));

    h.key(KeyCode::Char('t'));
    let screen = h.screen();
    assert_contains(&screen, "Read a.txt");
    assert_contains(&screen, "Bash make");
    assert!(!screen.contains("tool calls hidden"), "{screen}");
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

    // Sent but unanswered, it can still be taken back: one that never gets
    // through would otherwise hold back everything behind it.
    h.seam.command_sending(sent[0]).unwrap();
    h.sync();
    assert!(
        footer(&h).starts_with("e edit · Del cancel"),
        "{}",
        footer(&h)
    );
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
                reason: RejectReason::UnknownSession,
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
                reason: RejectReason::WorkdirNotAllowed,
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
    pull(&mut h, vec![message(id, 2, command, me, "hello")]);
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
    h.app.set_view(view);
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
    h.app.set_view(view);
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
                reason: RejectReason::UnknownSession,
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
    h.app.set_view(view);
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
    let queued_at = delivery_of(&h, key).queued_at;
    let queued = format!("queued {}", queued_at.format("%H:%M:%S"));
    let later = queued_at + chrono::Duration::seconds(130);
    let screen = trimmed(&h.frame(100, 24, later));
    assert_eq!(line_after(&screen, "you [queued]: hello"), queued);

    for _ in 0..3 {
        assert!(h.seam.command_sending(command).unwrap());
    }
    h.sync();
    let screen = trimmed(&h.frame(100, 24, later));
    assert_eq!(
        line_after(&screen, "you [sending]: hello"),
        format!("{queued} · sending, try 3")
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
    let queued_at = delivery_of(&h, key).queued_at;
    let screen = trimmed(&h.frame(100, 24, queued_at + chrono::Duration::seconds(7)));
    assert_eq!(
        line_after(&screen, "you [queued]: hello"),
        format!(
            "queued {} · sent once, waiting for the server",
            queued_at.format("%H:%M:%S")
        )
    );
    // Unanswered, it can still be taken back.
    assert!(footer(&h).contains("e edit"), "{}", footer(&h));
}

#[test]
fn delivery_line_holds_still_while_waiting() {
    let (mut h, _, _) = open_session();
    h.type_text("hello");
    h.key(KeyCode::Enter);
    let command = last_command(&h);
    h.key(KeyCode::Esc);
    h.key(KeyCode::Up);
    let key = ItemKey::Pending(command);
    assert_eq!(selected(&h), Some(key));
    // A waiting message is no reason to redraw every second.
    assert!(!h.app.live(), "nothing counts up");
    let queued_at = delivery_of(&h, key).queued_at;
    for seconds in [10, 20] {
        let wall = queued_at + chrono::Duration::seconds(seconds);
        assert_eq!(
            line_after(&trimmed(&h.frame(100, 24, wall)), "you [queued]: hello"),
            format!("queued {}", queued_at.format("%H:%M:%S"))
        );
    }
}

#[test]
fn approval_answer_says_when_it_was_queued() {
    let (mut h, _, _, _) = gated(
        Vec::new(),
        call(CallId::new(), "Bash", ToolKind::Shell, "cargo test"),
    );
    h.key(KeyCode::Esc);
    h.key(KeyCode::Char('a'));
    let command = last_command(&h);
    let answer = |h: &Harness| {
        let (view, _) = h.app.session().unwrap();
        view.pending_approval
            .as_ref()
            .unwrap()
            .answer
            .clone()
            .unwrap()
    };
    let queued_at = answer(&h).delivery.unwrap().queued_at;
    let wall = queued_at + chrono::Duration::seconds(40);
    let queued = format!("you approved · queued {}", queued_at.format("%H:%M:%S"));
    assert_contains(&trimmed(&h.frame(100, 30, wall)), &queued);
    h.seam.command_sending(command).unwrap();
    h.sync();
    assert_contains(
        &trimmed(&h.frame(100, 30, wall)),
        &format!("{queued} · sending"),
    );
}

// ---- searching the timeline -------------------------------------------------

/// A session with three matches for "test", far apart: a message, a failed
/// shell call, and an agent message in upper case.
fn searchable() -> (Harness, SessionId, DeviceId) {
    let (mut h, id, me) = open_session();
    let shell = CallId::new();
    let mut events = vec![event(id, 1, user(me, "run the tests"))];
    events.extend((0..30).map(|i| event(id, 2, agent(&format!("chatter {i:02}")))));
    events.push(event(
        id,
        3,
        EventKind::ToolCall(call(shell, "Bash", ToolKind::Shell, "cargo test")),
    ));
    events.push(event(
        id,
        3,
        EventKind::ToolResult(result(shell, true, "exit 101", "assertion failed")),
    ));
    events.extend((30..60).map(|i| event(id, 4, agent(&format!("chatter {i:02}")))));
    events.push(event(id, 5, agent("the TESTS pass now")));
    pull(&mut h, events);
    (h, id, me)
}

fn search(h: &Harness) -> Option<&skywave_tui::app::Search> {
    h.app.ui().session.as_ref()?.shown.search.as_ref()
}

/// The position of the first character of `needle` on the line holding
/// `on`.
fn position_of(buffer: &Buffer, on: &str, needle: &str) -> (u16, u16) {
    let screen = text(buffer);
    let (y, line) = screen
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains(on))
        .unwrap_or_else(|| panic!("{on:?} not on screen:\n{screen}"));
    let x = line[..line.find(needle).unwrap()].chars().count();
    (x as u16, y as u16)
}

#[test]
fn slash_searches_and_n_jumps_between_matches() {
    let (mut h, _, _) = searchable();
    h.key(KeyCode::Esc);
    h.key(KeyCode::Char('/'));
    // The prompt takes letters, commands included.
    assert!(h.app.typing());
    h.type_text("test");
    assert_contains(&h.screen(), "/test▏ · 3 matches · Enter jump · Esc cancel");
    // Enter starts from the bottom of the screen: the newest match.
    h.key(KeyCode::Enter);
    assert!(!h.app.typing());
    let screen = h.screen();
    assert_contains(&screen, "/test · 3 of 3");
    assert_contains(&screen, "> 12:05 ▌ agent: the TESTS pass now");
    assert_contains(&screen, "n/N older/newer match");
    h.key(KeyCode::Char('n'));
    let screen = h.screen();
    assert_contains(&screen, "/test · 2 of 3");
    assert_contains(&screen, "> 12:03 ❯ Bash cargo test");
    h.key(KeyCode::Char('n'));
    let screen = h.screen();
    assert_contains(&screen, "/test · 1 of 3");
    assert_contains(&screen, "> 12:01 ▌ you: run the tests");
    h.key(KeyCode::Char('N'));
    assert_contains(&h.screen(), "/test · 2 of 3");

    // Smart case: an upper-case letter matches case.
    h.key(KeyCode::Char('/'));
    h.type_text("TESTS");
    h.key(KeyCode::Enter);
    let screen = h.screen();
    assert_contains(&screen, "/TESTS · 1 of 1");
    assert_contains(&screen, "> 12:05 ▌ agent: the TESTS pass now");

    // Ctrl-F opens it from the composer too; `q` is text there.
    h.key(KeyCode::Char('i'));
    h.key_with(ctrl('f'));
    h.type_text("q");
    assert!(!h.app.ui().quit);
    assert_eq!(search(&h).unwrap().query.as_str(), "q");
    assert_contains(&h.screen(), "/q▏ · no matches");
}

#[test]
fn n_wraps_and_says_so() {
    let (mut h, _, _) = searchable();
    h.key(KeyCode::Esc);
    h.key(KeyCode::Char('/'));
    h.type_text("test");
    h.key(KeyCode::Enter);
    h.key(KeyCode::Char('n'));
    h.key(KeyCode::Char('n'));
    assert!(!h.screen().contains("search wrapped"));
    // Past the oldest: round to the newest.
    h.key(KeyCode::Char('n'));
    let screen = h.screen();
    assert_contains(&screen, "/test · 3 of 3 · search wrapped");
    assert_contains(&screen, "> 12:05 ▌ agent: the TESTS pass now");
    // Said until the next key.
    h.key(KeyCode::Char('k'));
    assert!(!h.screen().contains("search wrapped"));
    // Past the newest: round to the oldest.
    h.key(KeyCode::Char('N'));
    let screen = h.screen();
    assert_contains(&screen, "/test · 1 of 3 · search wrapped");
    assert_contains(&screen, "> 12:01 ▌ you: run the tests");
}

#[test]
fn esc_clears_search_before_leaving() {
    let (mut h, _, _) = searchable();
    h.key(KeyCode::Esc);
    // Esc in the prompt cancels it.
    h.key(KeyCode::Char('/'));
    h.type_text("chatter");
    assert_eq!(h.key(KeyCode::Esc), None);
    assert!(search(&h).is_none());
    // Enter on an empty prompt closes it as well.
    h.key(KeyCode::Char('/'));
    h.key(KeyCode::Enter);
    assert!(search(&h).is_none());
    // A kept search goes first, then the session.
    h.key(KeyCode::Char('/'));
    h.type_text("test");
    h.key(KeyCode::Enter);
    assert_eq!(h.key(KeyCode::Esc), None);
    assert!(search(&h).is_none());
    assert!(!h.screen().contains("/test"));
    assert_eq!(h.key(KeyCode::Esc), Some(Action::Back));
}

#[test]
fn current_match_survives_new_items() {
    let (mut h, id, me) = searchable();
    // A queued message that matches; the search jumps to it.
    h.type_text("test once more");
    h.key(KeyCode::Enter);
    let command = last_command(&h);
    h.key_with(ctrl('f'));
    h.type_text("test");
    h.key(KeyCode::Enter);
    assert_eq!(search(&h).unwrap().current, Some(ItemKey::Pending(command)));
    assert_contains(&h.screen(), "/test · 4 of 4");

    // It is logged, and a newer match arrives: the current one stays.
    let seq = Seq(h.seam.cursor().unwrap().0 + 1);
    h.seam
        .command_result(command, &CommandResult::Accepted { seq })
        .unwrap();
    pull(
        &mut h,
        vec![
            message(id, 6, command, me, "test once more"),
            event(id, 7, agent("tests are green")),
        ],
    );
    assert_eq!(search(&h).unwrap().current, Some(ItemKey::Logged(seq)));
    assert_contains(&h.screen(), "/test · 4 of 5");
    h.key(KeyCode::Char('N'));
    assert_contains(&h.screen(), "/test · 5 of 5");
}

#[test]
fn matches_are_marked_and_the_current_one_bold() {
    use ratatui::style::Modifier;
    let (mut h, id, _) = open_session();
    pull(
        &mut h,
        vec![
            event(id, 1, agent("alpha needle one")),
            event(id, 2, agent("beta needle two")),
        ],
    );
    h.key(KeyCode::Esc);
    h.key(KeyCode::Char('/'));
    h.type_text("needle");
    // Marked while typing; none is current yet.
    let buffer = h.frame(100, 24, mockup_now());
    let cell = &buffer[position_of(&buffer, "alpha", "needle")];
    assert!(cell.modifier.contains(Modifier::REVERSED));
    assert!(!cell.modifier.contains(Modifier::BOLD));
    assert_eq!(cell.fg, Color::Yellow);

    h.key(KeyCode::Enter);
    let buffer = h.frame(100, 24, mockup_now());
    let (x, y) = position_of(&buffer, "beta", "needle");
    // Every char of the current match, bold, and nothing around it.
    for dx in 0..6 {
        let cell = &buffer[(x + dx, y)];
        assert!(cell.modifier.contains(Modifier::REVERSED | Modifier::BOLD));
    }
    assert!(!buffer[(x + 6, y)].modifier.contains(Modifier::REVERSED));
    assert!(!buffer[(x - 1, y)].modifier.contains(Modifier::REVERSED));
    let other = &buffer[position_of(&buffer, "alpha", "needle")];
    assert!(other.modifier.contains(Modifier::REVERSED));
    assert!(!other.modifier.contains(Modifier::BOLD));
    let word = &buffer[position_of(&buffer, "alpha", "alpha")];
    assert!(!word.modifier.contains(Modifier::REVERSED));
}

#[test]
fn a_match_only_in_hidden_output_expands_its_item() {
    let (mut h, id, _) = open_session();
    let write = CallId::new();
    let mut tool = call(write, "Write", ToolKind::FileWrite, "notes.md");
    tool.display.body = body(BodyFormat::Text, "# Notes");
    pull(
        &mut h,
        vec![
            event(id, 1, EventKind::ToolCall(tool)),
            event(
                id,
                1,
                EventKind::ToolResult(result(write, false, "", "wrote 7 bytes to notes.md")),
            ),
        ],
    );
    assert!(!h.screen().contains("wrote 7 bytes"));
    h.key(KeyCode::Esc);
    h.key(KeyCode::Char('/'));
    h.type_text("7 bytes");
    h.key(KeyCode::Enter);
    assert_eq!(h.app.ui().session.as_ref().unwrap().shown.expanded.len(), 1);
    assert_contains(&h.screen(), "wrote 7 bytes");
    // A match in the title shows collapsed: nothing expands.
    h.key(KeyCode::Enter);
    assert!(
        h.app
            .ui()
            .session
            .as_ref()
            .unwrap()
            .shown
            .expanded
            .is_empty()
    );
    h.key(KeyCode::Char('/'));
    h.type_text("notes.md");
    h.key(KeyCode::Enter);
    assert!(
        h.app
            .ui()
            .session
            .as_ref()
            .unwrap()
            .shown
            .expanded
            .is_empty()
    );
}

/// A shell call `command` whose output is `output`, its preview `preview`.
fn shell_output(
    id: SessionId,
    minute: u32,
    command: &str,
    preview: &str,
    output: BlobRef,
) -> [Event; 2] {
    let shell = CallId::new();
    let mut done = result(shell, false, "exit 0", preview);
    done.output = Some(Full::Blob(output));
    [
        event(
            id,
            minute,
            EventKind::ToolCall(call(shell, "Bash", ToolKind::Shell, command)),
        ),
        event(id, minute, EventKind::ToolResult(done)),
    ]
}

/// Writes `content` to `name` in `dir` and reports blob `id` downloaded
/// there.
fn downloaded(h: &mut Harness, dir: &tempfile::TempDir, name: &str, id: BlobId, content: &[u8]) {
    let path = dir.path().join(name);
    std::fs::write(&path, content).unwrap();
    h.seam
        .blob_status(id, BlobStatus::Available { path })
        .unwrap();
    h.sync();
}

/// How many of the open session's items are expanded.
fn expanded(h: &Harness) -> usize {
    h.app.ui().session.as_ref().unwrap().shown.expanded.len()
}

#[test]
fn search_finds_downloaded_blob_text_and_expands() {
    let (mut h, id, _) = open_session();
    let dir = tempfile::tempdir().unwrap();
    let log: String = (0..30)
        .map(|i| match i {
            20 => "test parser::nested ... FAILED\n".to_owned(),
            i => format!("test case_{i:02} ... ok\n"),
        })
        .collect();
    let (log_id, log_ref) = blob(log.len() as u64);
    let build = "   Compiling nested v0.1.0\n";
    let (build_id, build_ref) = blob(build.len() as u64);
    let mut events = shell_output(id, 1, "$ cargo test", "running 30 tests", log_ref).to_vec();
    events.extend(shell_output(id, 2, "$ cargo build", "Compiling", build_ref));
    events.push(event(id, 3, agent("one test fails")));
    pull(&mut h, events);
    downloaded(&mut h, &dir, "log", log_id, log.as_bytes());
    h.drain_requests();

    h.key(KeyCode::Esc);
    h.key(KeyCode::Char('/'));
    h.type_text("nested");
    // The build output is not here: said, and never fetched.
    assert_contains(
        &h.screen(),
        "/nested▏ · 1 match · 1 output not downloaded · Enter jump",
    );
    h.key(KeyCode::Enter);
    assert_eq!(expanded(&h), 1);
    let screen = h.screen();
    assert_contains(&screen, "/nested · 1 of 1 · 1 output not downloaded");
    assert_contains(&screen, "│ test parser::nested ... FAILED");
    assert_eq!(h.drain_requests(), []);

    // Downloaded mid-search: searched at once.
    downloaded(&mut h, &dir, "build", build_id, build.as_bytes());
    let screen = h.screen();
    assert_contains(&screen, "/nested · 1 of 2");
    assert!(!screen.contains("not downloaded"), "{screen}");
    h.key(KeyCode::Char('N'));
    assert_eq!(expanded(&h), 2);
    let screen = h.screen();
    assert_contains(&screen, "/nested · 2 of 2");
    assert_contains(&screen, "│    Compiling nested v0.1.0");
}

#[test]
fn status_counts_undownloaded_outputs() {
    let (mut h, id, _) = open_session();
    let dir = tempfile::tempdir().unwrap();
    let (waiting, waiting_ref) = blob(100);
    let (_, idle_ref) = blob(100);
    let (failed, failed_ref) = blob(100);
    let (coming, coming_ref) = blob(100);
    let (big, big_ref) = blob(1_200_000);
    let mut events = two_turns(id);
    for (minute, output) in [waiting_ref, idle_ref, failed_ref, coming_ref, big_ref]
        .into_iter()
        .enumerate()
    {
        events.extend(shell_output(
            id,
            4 + minute as u32,
            "$ make",
            "make: ok",
            output,
        ));
    }
    pull(&mut h, events);
    h.core.dispatch(Action::DownloadBlob(waiting)).unwrap();
    h.seam.blob_failed(failed, "blob size mismatch").unwrap();
    h.seam
        .blob_status(
            coming,
            BlobStatus::Downloading {
                got: 10,
                path: dir.path().join("coming.part"),
            },
        )
        .unwrap();
    // Past the head a search reads; the needle only past it.
    let mut content = vec![b'x'; 1_100_000];
    content.extend(b"\nneedle\n");
    downloaded(&mut h, &dir, "big", big, &content);
    h.drain_requests();

    h.key(KeyCode::Esc);
    h.key(KeyCode::Char('/'));
    h.type_text("make");
    h.key(KeyCode::Enter);
    let status = status_line(&h, 200);
    assert!(
        status.contains(
            "/make · 5 of 5 · 4 outputs not downloaded · 1 output searched only in its first 1.0 MiB"
        ),
        "{status:?}"
    );
    // The usage summary keeps to the right of it.
    assert!(status.ends_with("session $1.27 · model-a"), "{status:?}");
    h.key(KeyCode::Char('/'));
    h.type_text("needle");
    assert_contains(
        &h.screen(),
        "/needle▏ · no matches · 4 outputs not downloaded",
    );
    assert_eq!(h.drain_requests(), []);
}

#[test]
fn a_jump_into_a_tall_blob_pins_the_matching_line() {
    let (mut h, id, _) = open_session();
    let dir = tempfile::tempdir().unwrap();
    let rows = |n: usize, needle: usize| -> String {
        (0..n)
            .map(|i| match i == needle {
                true => format!("row {i} needle\n"),
                false => format!("row {i}\n"),
            })
            .collect()
    };
    // In what an expanded item shows, far below its top.
    let near = rows(200, 150);
    let (near_id, near_ref) = blob(near.len() as u64);
    // Past the 400 lines it shows.
    let far = rows(1000, 700);
    let (far_id, far_ref) = blob(far.len() as u64);
    let mut events = shell_output(id, 1, "$ seq near", "row 0", near_ref).to_vec();
    events.extend(shell_output(id, 2, "$ seq far", "row 0", far_ref));
    events.push(event(id, 3, agent("done")));
    pull(&mut h, events);
    downloaded(&mut h, &dir, "near", near_id, near.as_bytes());
    downloaded(&mut h, &dir, "far", far_id, far.as_bytes());

    h.key(KeyCode::Esc);
    h.key(KeyCode::Char('/'));
    h.type_text("needle");
    h.key(KeyCode::Enter);
    // The newest match first: the far one, shown from a few lines above.
    let screen = h.screen();
    assert_contains(&screen, "/needle · 2 of 2");
    assert_contains(&screen, "│ … 697 lines above");
    assert_contains(&screen, "│ row 700 needle");
    assert!(!screen.contains("$ seq near"), "{screen}");
    // The next key keeps it there.
    h.key(KeyCode::Char('x'));
    assert_contains(&h.screen(), "│ row 700 needle");

    h.key(KeyCode::Char('n'));
    let screen = h.screen();
    assert_contains(&screen, "/needle · 1 of 2");
    assert_contains(&screen, "│ row 150 needle");
    assert!(
        !screen.contains("$ seq near"),
        "scrolled past the top: {screen}"
    );
    // The far one is back to its head.
    assert!(!screen.contains("lines above"), "{screen}");
}

// ---- copying ----------------------------------------------------------------

/// A session holding `events`, with the newest item selected.
fn copy_session(events: impl FnOnce(SessionId) -> Vec<Event>) -> Harness {
    let (mut h, id, _) = open_session();
    pull(&mut h, events(id));
    h.key(KeyCode::Esc);
    h.key(KeyCode::Up);
    h
}

fn clip(h: &Harness) -> Option<Clip> {
    h.app.ui().copy.clone()
}

fn notice(h: &Harness) -> Option<String> {
    h.app.ui().notice.clone()
}

/// The event loop's copy step, with the terminal written to a buffer.
fn send_copy(h: &mut Harness, max: usize) -> Vec<u8> {
    let mut out = Vec::new();
    skywave_tui::copy(&mut h.app, &mut out, max);
    assert_eq!(clip(h), None, "taken");
    out
}

/// A long shell command: the title cut short, the whole command its body.
fn long_command(call_id: CallId, full: Option<BlobRef>) -> ToolCall {
    let mut tool = call(
        call_id,
        "Bash",
        ToolKind::Shell,
        "$ cargo test --workspace …",
    );
    tool.display.body = Some(Body {
        format: BodyFormat::Text,
        preview: "$ cargo test --workspace \\\n  --all-features".into(),
        lines: 2,
        full: full.map(Full::Blob),
    });
    tool
}

#[test]
fn y_copies_agent_text() {
    let mut h = copy_session(|id| vec![event(id, 1, agent("Done:\nall green"))]);
    assert_eq!(h.key(KeyCode::Char('y')), None);
    assert_eq!(clip(&h), Some(Clip::Text("Done:\nall green".into())));
    let out = send_copy(&mut h, clipboard::MAX);
    assert_eq!(out, clipboard::osc52(b"Done:\nall green").into_bytes());
    assert_eq!(
        notice(&h).as_deref(),
        Some("copied 15 B · sent to terminal clipboard")
    );
    assert_contains(&h.screen(), "copied 15 B · sent to terminal clipboard");
    // Gone with the next key.
    h.key(KeyCode::Up);
    assert_eq!(notice(&h), None);
    assert!(!h.screen().contains("copied"));
}

#[test]
fn y_copies_own_pending_message() {
    let (mut h, _, _) = open_session();
    h.type_text("hello there");
    h.key(KeyCode::Enter);
    h.key(KeyCode::Esc);
    h.key(KeyCode::Up);
    h.key(KeyCode::Char('y'));
    assert_eq!(clip(&h), Some(Clip::Text("hello there".into())));
}

#[test]
fn y_copies_shell_output_not_command() {
    let shell = CallId::new();
    let mut h = copy_session(|id| {
        vec![
            event(id, 1, EventKind::ToolCall(long_command(shell, None))),
            event(
                id,
                2,
                EventKind::ToolResult(result(shell, false, "exit 0", "test result: ok")),
            ),
        ]
    });
    h.key(KeyCode::Char('y'));
    assert_eq!(clip(&h), Some(Clip::Text("test result: ok".into())));
}

#[test]
fn y_copies_edit_diff() {
    let edit = CallId::new();
    let diff = "-    run();\n+    let code = run();\n";
    let mut h = copy_session(|id| {
        let mut tool = call(edit, "Edit", ToolKind::FileEdit, "src/main.rs");
        tool.display.body = body(BodyFormat::Diff, diff);
        vec![
            event(id, 1, EventKind::ToolCall(tool)),
            event(
                id,
                2,
                EventKind::ToolResult(result(edit, false, "", "updated src/main.rs")),
            ),
        ]
    });
    h.key(KeyCode::Char('y'));
    assert_eq!(clip(&h), Some(Clip::Text(diff.into())));
}

#[test]
fn y_on_undownloaded_blob_requests_it() {
    let read = CallId::new();
    let (blob_id, output) = blob(1_258_291);
    let mut h = copy_session(|id| {
        let mut done = result(read, false, "", "line 1");
        done.output = Some(Full::Blob(output));
        vec![
            event(
                id,
                1,
                EventKind::ToolCall(call(read, "Read", ToolKind::FileRead, "big.log")),
            ),
            event(id, 2, EventKind::ToolResult(done)),
        ]
    });
    // Never the preview alone.
    assert_eq!(
        h.key(KeyCode::Char('y')),
        Some(Action::DownloadBlob(blob_id))
    );
    assert_eq!(clip(&h), None);
    assert_eq!(
        notice(&h).as_deref(),
        Some("downloading 1.2 MiB · press y again when it is here")
    );
    assert_eq!(h.drain_requests(), vec![Request::DownloadBlob(blob_id)]);

    let dir = tempfile::tempdir().unwrap();
    let part = dir.path().join("blob.part");
    h.seam
        .blob_status(
            blob_id,
            BlobStatus::Downloading {
                got: 10,
                path: part,
            },
        )
        .unwrap();
    h.sync();
    assert_eq!(h.key(KeyCode::Char('y')), None, "already on its way");
    assert_eq!(clip(&h), None);
    assert_contains(&notice(&h).unwrap(), "press y again");

    // Here: the whole file, invalid UTF-8 as it is.
    let path = dir.path().join("blob");
    std::fs::write(&path, b"line 1\n\xff\n").unwrap();
    h.seam
        .blob_status(blob_id, BlobStatus::Available { path: path.clone() })
        .unwrap();
    h.sync();
    assert_eq!(h.key(KeyCode::Char('y')), None);
    assert_eq!(
        clip(&h),
        Some(Clip::File {
            path: path.clone(),
            strip: None
        })
    );
    assert_eq!(
        send_copy(&mut h, clipboard::MAX),
        clipboard::osc52(b"line 1\n\xff\n").into_bytes()
    );
    assert_eq!(
        notice(&h).as_deref(),
        Some("copied 9 B · sent to terminal clipboard")
    );

    // Deleted meanwhile: an error, nothing sent.
    h.key(KeyCode::Char('y'));
    std::fs::remove_file(&path).unwrap();
    assert!(send_copy(&mut h, clipboard::MAX).is_empty());
    assert_eq!(notice(&h), None);
    assert_contains(&h.app.ui().error.clone().unwrap(), "copy: cannot read");
}

#[test]
fn y_over_cap_names_the_file() {
    let shell = CallId::new();
    let (blob_id, output) = blob(80 * 1024);
    let mut h = copy_session(|id| {
        let mut done = result(shell, false, "exit 0", "a");
        done.output = Some(Full::Blob(output));
        vec![
            event(
                id,
                1,
                EventKind::ToolCall(call(shell, "Bash", ToolKind::Shell, "$ seq")),
            ),
            event(id, 2, EventKind::ToolResult(done)),
        ]
    });
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("blob");
    std::fs::write(&path, vec![b'a'; 80 * 1024]).unwrap();
    h.seam
        .blob_status(blob_id, BlobStatus::Available { path: path.clone() })
        .unwrap();
    h.sync();
    h.key(KeyCode::Char('y'));
    assert!(send_copy(&mut h, clipboard::MAX).is_empty(), "nothing sent");
    assert_eq!(
        notice(&h),
        Some(format!(
            "too large for the terminal clipboard (80.0 KiB > 74.0 KiB) · file: {}",
            path.display()
        ))
    );
    // A larger cap takes it.
    h.key(KeyCode::Char('y'));
    assert_eq!(
        send_copy(&mut h, 100 * 1024),
        clipboard::osc52(&vec![b'a'; 80 * 1024]).into_bytes()
    );
    assert_eq!(
        notice(&h).as_deref(),
        Some("copied 80.0 KiB · sent to terminal clipboard")
    );
}

#[test]
fn shift_y_copies_shell_command_without_dollar() {
    let shell = CallId::new();
    let mut h = copy_session(|id| {
        let mut tool = call(shell, "Bash", ToolKind::Shell, "$ cargo test");
        tool.display.detail = Some("Run the tests".into());
        vec![
            event(id, 1, EventKind::ToolCall(tool)),
            event(
                id,
                2,
                EventKind::ToolResult(result(shell, false, "exit 0", "ok")),
            ),
        ]
    });
    h.key(KeyCode::Char('Y'));
    assert_eq!(clip(&h), Some(Clip::Text("cargo test".into())));
    // Not a shell call: title and detail, as they are.
    let edit = CallId::new();
    let mut h = copy_session(|id| {
        let mut tool = call(edit, "Edit", ToolKind::FileEdit, "src/main.rs");
        tool.display.detail = Some("2 edits".into());
        vec![event(id, 1, EventKind::ToolCall(tool))]
    });
    h.key(KeyCode::Char('Y'));
    assert_eq!(clip(&h), Some(Clip::Text("src/main.rs\n2 edits".into())));
}

#[test]
fn shift_y_copies_long_command_body() {
    let shell = CallId::new();
    let mut h =
        copy_session(|id| vec![event(id, 1, EventKind::ToolCall(long_command(shell, None)))]);
    h.key(KeyCode::Char('Y'));
    assert_eq!(
        clip(&h),
        Some(Clip::Text(
            "cargo test --workspace \\\n  --all-features".into()
        ))
    );
    // No result yet: `y` gives the body as shown.
    h.key(KeyCode::Char('y'));
    assert_eq!(
        clip(&h),
        Some(Clip::Text(
            "$ cargo test --workspace \\\n  --all-features".into()
        ))
    );

    // A body too long for its preview is a blob: the whole file, unprompted.
    let shell = CallId::new();
    let (blob_id, full) = blob(60);
    let mut h = copy_session(|id| {
        vec![event(
            id,
            1,
            EventKind::ToolCall(long_command(shell, Some(full))),
        )]
    });
    assert_eq!(
        h.key(KeyCode::Char('Y')),
        Some(Action::DownloadBlob(blob_id))
    );
    assert_eq!(
        notice(&h).as_deref(),
        Some("downloading 60 B · press Y again when it is here")
    );
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("blob");
    let command = "cargo test --workspace \\\n  --all-features \\\n  -- --nocapture";
    std::fs::write(&path, format!("$ {command}")).unwrap();
    h.seam
        .blob_status(blob_id, BlobStatus::Available { path })
        .unwrap();
    h.sync();
    h.key(KeyCode::Char('Y'));
    assert_eq!(
        send_copy(&mut h, clipboard::MAX),
        clipboard::osc52(command.as_bytes()).into_bytes()
    );
}

#[test]
fn copy_keys_are_in_help_and_footer() {
    let mut h = copy_session(|id| vec![event(id, 1, agent("hi"))]);
    assert_contains(&h.screen(), "d download · y/Y copy");
    h.key(KeyCode::Char('?'));
    let screen = h.screen();
    assert_contains(&screen, "copy its output or text / its command");
    assert_contains(&screen, "set -g set-clipboard on");
}

fn approval(session_id: SessionId, minute: u32, id: ApprovalId, summary: &str) -> Event {
    event(
        session_id,
        minute,
        EventKind::ApprovalRequested {
            id,
            call: None,
            tool: "shell".into(),
            summary: summary.into(),
            input: None,
            suggestions: Vec::new(),
            shell: None,
        },
    )
}

fn approved(session_id: SessionId, minute: u32, id: ApprovalId) -> Event {
    event(
        session_id,
        minute,
        EventKind::ApprovalResolved {
            id,
            decision: Decision::Approve,
            by: DeviceId::new(),
            rule: None,
        },
    )
}

fn open_id(h: &Harness) -> Option<SessionId> {
    h.app.session().map(|(view, _)| view.id)
}

#[test]
fn the_agent_login_expiry_shows_in_settings_and_warns_in_the_banner_within_three_days() {
    let mut h = Harness::paired();
    h.seam.set_link(PairedLink::Online { latency_ms: 5 });
    let hello = |h: &mut Harness, expires| {
        h.seam
            .hello_received(&HelloResponse {
                protocol_version: skywave_proto::PROTOCOL_VERSION,
                server_id: skywave_proto::ServerId::new(),
                device_id: DeviceId::new(),
                head: Seq::ZERO,
                log: any_log(),
                devices: Vec::new(),
                shell: false,
                bypass: false,
                login_expires: expires,
                agent_login_offered: false,
                agent_logged_out: false,
            })
            .unwrap();
        h.sync();
    };
    let now = mockup_now();
    hello(&mut h, Some(now + chrono::TimeDelta::days(4)));
    assert_eq!(h.banner(), "online · 5 ms");
    hello(&mut h, Some(now + chrono::TimeDelta::hours(30)));
    assert_eq!(
        h.banner(),
        "online · 5 ms · agent login expires in 2 days · log in again on the server"
    );
    // The same expiry, read later: the count runs on the wall clock.
    assert_eq!(
        h.banner_at(now + chrono::TimeDelta::hours(31)),
        "online · 5 ms · agent login expired · log in again on the server"
    );

    // Settings say it however far off.
    h.key(KeyCode::Char('s'));
    let date = (now + chrono::TimeDelta::hours(30)).format("%Y-%m-%d");
    assert_contains(
        &h.screen(),
        &format!("Agent login: expires in 2 days · {date}"),
    );
    let later = now + chrono::TimeDelta::days(40);
    hello(&mut h, Some(later));
    let date = later.format("%Y-%m-%d");
    assert_contains(
        &h.screen(),
        &format!("Agent login: expires in 40 days · {date}"),
    );
    hello(&mut h, None);
    assert_eq!(h.banner(), "online · 5 ms");
    assert_contains(&h.screen(), "Agent login: no expiry reported");
}

#[test]
fn ctrl_g_opens_approvals_elsewhere_counted_in_the_banner() {
    use skywave_tui::app::Focus;
    let mut h = Harness::paired();
    h.seam.set_link(PairedLink::Online { latency_ms: 5 });
    let (a, b) = (SessionId::new(), SessionId::new());
    pull(
        &mut h,
        vec![created(a, 0, "/srv/a"), created(b, 1, "/srv/b")],
    );
    assert!(!h.banner().contains("elsewhere"), "{}", h.banner());
    assert_eq!(h.key_with(ctrl('g')), None, "nothing pending: a no-op");

    let mut ids = [ApprovalId::new(), ApprovalId::new()];
    ids.sort();
    pull(
        &mut h,
        vec![approval(b, 2, ids[1], "ls"), approval(a, 3, ids[0], "pwd")],
    );
    assert_eq!(h.banner(), "online · 5 ms · 2 approvals elsewhere (^G)");

    // The oldest first, on the timeline so `a` answers at once.
    assert_eq!(h.key_with(ctrl('g')), Some(Action::OpenNextApproval));
    assert_eq!(open_id(&h), Some(a));
    assert_eq!(h.app.session().unwrap().1.focus, Focus::Timeline);
    assert_eq!(h.banner(), "online · 5 ms · 1 approval elsewhere (^G)");

    // From the composer, mid-sentence: the draft stays behind.
    h.key(KeyCode::Tab);
    h.type_text("half a tho");
    assert_eq!(h.key_with(ctrl('g')), Some(Action::OpenNextApproval));
    assert_eq!(open_id(&h), Some(b));
    assert_eq!(h.app.session().unwrap().1.focus, Focus::Timeline);
    assert_eq!(
        h.key(KeyCode::Char('a')),
        Some(Action::ResolveApproval {
            id: ids[1],
            decision: Decision::Approve,
            rule: None,
        })
    );
    // Cycles back.
    assert_eq!(h.key_with(ctrl('g')), Some(Action::OpenNextApproval));
    assert_eq!(open_id(&h), Some(a));
    assert_eq!(h.core_draft().unwrap().text, "half a tho");

    // Answered here, b is no longer waiting; a is the open one.
    assert_eq!(h.banner(), "online · 5 ms · 1 queued");
    assert_eq!(h.key_with(ctrl('g')), None);
    assert_eq!(open_id(&h), Some(a));

    h.key(KeyCode::Esc);
    h.key(KeyCode::Char('?'));
    assert_contains(&h.screen(), "Ctrl-G        open the next session waiting");
}

#[test]
fn a_new_approval_alerts_once_naming_the_session_only() {
    let mut h = Harness::paired();
    let id = SessionId::new();
    pull(&mut h, vec![created(id, 0, "/srv/we\u{1b}]0;ird\u{9b};x")]);
    // Pending before going online: the catch-up has it, no alert yet.
    let first = ApprovalId::new();
    pull(&mut h, vec![approval(id, 1, first, "cat ~/.secret")]);
    assert_eq!(h.app.ui().alert, None);
    h.seam.set_link(PairedLink::Online { latency_ms: 5 });
    h.sync();

    let notify = skywave_tui::Notify::parse("bell,osc9").unwrap();
    let mut out = Vec::new();
    skywave_tui::alert(&mut h.app, &mut out, notify, false);
    let sent = String::from_utf8(out).unwrap();
    assert_eq!(
        sent,
        "\u{1b}]9;skywave: approval needed · we]0irdx\u{7}\u{7}"
    );
    assert!(!sent.contains("secret"), "never the tool input");
    assert_contains(&h.screen(), "approval needed · we]0irdx");

    // One ring per new approval, not per snapshot.
    let mut out = Vec::new();
    pull(&mut h, vec![approved(id, 2, first)]);
    skywave_tui::alert(&mut h.app, &mut out, notify, false);
    assert!(out.is_empty());
    pull(&mut h, vec![approval(id, 3, ApprovalId::new(), "ls")]);
    skywave_tui::alert(&mut h.app, &mut out, notify, false);
    skywave_tui::alert(&mut h.app, &mut out, notify, false);
    assert_eq!(
        out.iter().filter(|&&b| b == 0x07).count(),
        2,
        "OSC + bell once"
    );

    // Pending at a (re)start: never news.
    let mut h = h.restart();
    h.seam.set_link(PairedLink::Online { latency_ms: 5 });
    h.sync();
    let mut out = Vec::new();
    skywave_tui::alert(&mut h.app, &mut out, notify, false);
    assert!(out.is_empty());
    assert_contains(&h.banner(), "1 approval elsewhere (^G)");
}

#[test]
fn a_archives_the_selected_session_and_shift_a_switches_lists() {
    let mut h = Harness::paired();
    let (a, b, c) = (SessionId::new(), SessionId::new(), SessionId::new());
    pull(
        &mut h,
        vec![
            created(a, 0, "/srv/a"),
            created(b, 1, "/srv/b"),
            created(c, 2, "/srv/c"),
        ],
    );
    assert_contains(&h.screen(), "a archive · A archived");
    h.key(KeyCode::Down);
    assert_eq!(selected_workdir(&h).as_deref(), Some("/srv/b"));

    // The row leaves at once; the selection moves to its neighbour.
    assert_eq!(
        h.key(KeyCode::Char('a')),
        Some(Action::SetSessionArchived {
            session_id: b,
            archived: true,
        })
    );
    let screen = h.screen();
    assert_contains(&screen, "Sessions (2) · 1 archived");
    assert!(!screen.contains("/srv/b"), "{screen}");
    assert_eq!(selected_workdir(&h).as_deref(), Some("/srv/a"));

    // `A` shows the archived list; `n` makes no session there.
    assert_eq!(
        h.key(KeyCode::Char('A')),
        Some(Action::Navigate(Route::ArchivedSessions))
    );
    let screen = h.screen();
    assert_contains(&screen, "Archived (1) · 2 active");
    assert_contains(&screen, "a unarchive");
    assert_eq!(selected_workdir(&h).as_deref(), Some("/srv/b"));
    assert_eq!(h.key(KeyCode::Char('n')), None);
    assert!(h.app.ui().prompt.is_none());

    // It opens and works as usual, tagged; Esc goes back to its list.
    assert_eq!(h.key(KeyCode::Enter), Some(Action::OpenSession(b)));
    assert_contains(&h.screen(), "· archived · a message brings it back");
    h.key(KeyCode::Esc);
    assert_eq!(h.key(KeyCode::Esc), Some(Action::Back));
    // Esc on the archived list goes back to the active one, and `A` too.
    assert_eq!(h.key(KeyCode::Esc), Some(Action::Back));
    h.key(KeyCode::Char('A'));
    assert_eq!(
        h.key(KeyCode::Char('A')),
        Some(Action::Navigate(Route::SessionList))
    );

    // `a` on the archived list brings it back, in its old place.
    h.key(KeyCode::Char('A'));
    assert_eq!(
        h.key(KeyCode::Char('a')),
        Some(Action::SetSessionArchived {
            session_id: b,
            archived: false,
        })
    );
    assert_contains(&h.screen(), "No archived sessions.");
    assert_eq!(h.key(KeyCode::Char('a')), None, "nothing selected");
    h.key(KeyCode::Esc);
    let screen = h.screen();
    assert_contains(&screen, "Sessions (3)");
    let row = |dir: &str| screen.find(dir).unwrap();
    assert!(row("/srv/c") < row("/srv/b") && row("/srv/b") < row("/srv/a"));
}

#[test]
fn a_message_brings_an_archived_session_back() {
    let (mut h, id, _) = open_session();
    let phone = DeviceId::new();
    pull(
        &mut h,
        vec![event(
            id,
            1,
            EventKind::SessionArchived {
                archived: true,
                by: phone,
            },
        )],
    );
    let screen = h.screen();
    assert_contains(&screen, "· archived · a message brings it back");
    assert_contains(&screen, &format!("archived by device …{}", tail(phone)));
    h.type_text("one more thing");
    assert!(matches!(
        h.key(KeyCode::Enter),
        Some(Action::SendMessage { .. })
    ));
    assert!(!h.screen().contains("· archived ·"), "{}", h.screen());
    h.key(KeyCode::Esc);
    assert_eq!(h.key(KeyCode::Esc), Some(Action::Back));
    // On the active list, which no longer counts it as archived.
    let screen = h.screen();
    assert_contains(&screen, "Sessions (1) ");
    assert_contains(&screen, "/srv/work");
    assert!(!screen.contains("1 archived"), "{screen}");
}

#[test]
fn a_removed_worktree_shows_on_the_timeline() {
    let (mut h, id, _) = open_session();
    let phone = DeviceId::new();
    pull(
        &mut h,
        vec![event(id, 1, EventKind::WorktreeRemoved { by: phone })],
    );
    assert_contains(
        &h.screen(),
        &format!("worktree removed by device …{}", tail(phone)),
    );
}

#[test]
fn an_approval_in_an_archived_session_still_shows() {
    let mut h = Harness::paired();
    h.seam.set_link(PairedLink::Online { latency_ms: 5 });
    let (a, b) = (SessionId::new(), SessionId::new());
    pull(
        &mut h,
        vec![
            created(a, 0, "/srv/a"),
            created(b, 1, "/srv/b"),
            event(
                a,
                2,
                EventKind::SessionArchived {
                    archived: true,
                    by: DeviceId::new(),
                },
            ),
            approval(a, 3, ApprovalId::new(), "ls"),
        ],
    );
    assert_eq!(h.banner(), "online · 5 ms · 1 approval elsewhere (^G)");
    assert_contains(&h.screen(), "Sessions (1) · 1 archived");
    h.key(KeyCode::Char('A'));
    assert_contains(&line_with(&h.screen(), "/srv/a"), "! 1 approval pending");
    assert_eq!(h.key_with(ctrl('g')), Some(Action::OpenNextApproval));
    assert_eq!(open_id(&h), Some(a));
}

#[test]
fn shift_d_confirms_and_removes_row() {
    let mut h = Harness::paired();
    let (a, b) = (SessionId::new(), SessionId::new());
    let archived = |id, minute| {
        event(
            id,
            minute,
            EventKind::SessionArchived {
                archived: true,
                by: DeviceId::new(),
            },
        )
    };
    pull(
        &mut h,
        vec![
            created(a, 0, "/srv/a"),
            created(b, 1, "/srv/b"),
            archived(a, 2),
            archived(b, 3),
            approval(b, 4, ApprovalId::new(), "ls"),
        ],
    );
    // Not from the active list.
    assert_eq!(h.key(KeyCode::Char('D')), None);
    assert_eq!(h.app.ui().delete, None);

    h.key(KeyCode::Char('A'));
    assert_contains(&h.screen(), "D delete here");
    // `b` waits on an approval: refused, saying why.
    assert_eq!(selected_workdir(&h).as_deref(), Some("/srv/b"));
    assert_eq!(h.key(KeyCode::Char('D')), None);
    assert_contains(&footer(&h), "cannot delete b yet: answer or cancel first");

    // Any key but `y` keeps it.
    h.key(KeyCode::Down);
    assert_eq!(h.key(KeyCode::Char('D')), None);
    assert_contains(
        &footer(&h),
        "Delete a from this device for good? It stays on the server and other devices. y/N",
    );
    assert_eq!(h.key(KeyCode::Char('n')), None);
    assert_contains(&footer(&h), "D delete here");
    assert_contains(&h.screen(), "/srv/a");

    h.key(KeyCode::Char('D'));
    assert_eq!(
        h.key(KeyCode::Char('y')),
        Some(Action::DeleteSessionLocally { session_id: a })
    );
    assert_eq!(h.drain_requests(), vec![Request::BlobsPurged(Vec::new())]);
    let screen = h.screen();
    assert!(!screen.contains("/srv/a"), "{screen}");
    assert_contains(&screen, "Archived (1) · 0 active");
    assert_eq!(selected_workdir(&h).as_deref(), Some("/srv/b"));

    // Gone for good, even when used elsewhere.
    pull(
        &mut h,
        vec![event(
            a,
            5,
            EventKind::UserMessage {
                device_id: DeviceId::new(),
                text: "still here?".into(),
                attachments: Vec::new(),
            },
        )],
    );
    h.key(KeyCode::Esc);
    let screen = h.screen();
    assert_contains(&screen, "Sessions (0) · 1 archived");
    assert!(!screen.contains("/srv/a"), "{screen}");
}

// ---- turn usage -------------------------------------------------------------

/// Two turns with their usage: the first $1.23, the second (the last)
/// 12.4 s, 3.1k in, 812 out, $0.04.
fn two_turns(id: SessionId) -> Vec<Event> {
    let turn = |minute, usage| {
        [
            event(
                id,
                minute,
                EventKind::SessionStateChanged(LogState::AgentWorking),
            ),
            event(id, minute, agent("done")),
            event(id, minute, EventKind::TurnUsage(usage)),
            event(id, minute, EventKind::SessionStateChanged(LogState::Idle)),
        ]
    };
    let first = TurnUsage {
        duration_ms: Some(95_000),
        cost_usd: Some(1.23),
        model: Some("model-a".into()),
        ..TurnUsage::default()
    };
    let second = TurnUsage {
        duration_ms: Some(12_400),
        tokens_in: Some(3_100),
        tokens_out: Some(812),
        cost_usd: Some(0.04),
        model: None,
        context_tokens: Some(19_337),
        context_window: Some(1_000_000),
        context_compacted: false,
    };
    turn(1, first).into_iter().chain(turn(2, second)).collect()
}

/// The status line of a `width`-column frame, borders cut off.
fn status_line(h: &Harness, width: u16) -> String {
    let screen = text(&h.frame(width, 24, at(3)));
    line_with(&screen, "agent idle")
        .trim_end()
        .trim_matches('│')
        .trim_end()
        .to_owned()
}

#[test]
fn the_status_line_drops_usage_fields_from_the_left_as_it_narrows() {
    let (mut h, id, _) = open_session();
    pull(&mut h, two_turns(id));

    let wide = status_line(&h, 140);
    assert!(
        wide.ends_with(
            "last turn 12.4s · 3.1k in / 812 out · $0.04 · ctx 19k/1M · 2% · session $1.27 · model-a"
        ),
        "{wide:?}"
    );
    assert!(wide.contains("agent idle   "), "right-aligned: {wide:?}");

    let narrow = status_line(&h, 60);
    assert!(
        narrow.ends_with(" ctx 19k/1M · 2% · session $1.27 · model-a"),
        "{narrow:?}"
    );
    assert!(!narrow.contains("$0.04"), "{narrow:?}");

    let narrower = status_line(&h, 30);
    assert!(narrower.ends_with(" model-a"), "{narrower:?}");
    assert!(!narrower.contains("session"), "{narrower:?}");

    h.key(KeyCode::F(1));
    assert_contains(
        &text(&h.frame(140, 60, at(3))),
        "Status line, right: last turn's time",
    );
}

#[test]
fn each_turn_ends_with_its_time_and_cost_and_the_list_shows_the_session_cost() {
    let (mut h, id, _) = open_session();
    pull(&mut h, two_turns(id));
    let screen = text(&h.frame(140, 30, at(3)));
    assert_contains(&screen, "turn · 1m 35s · $1.23");
    assert_contains(&screen, "turn · 12.4s · $0.04");

    leave(&mut h);
    assert_contains(&line_with(&h.screen(), "/srv/work"), "· $1.27 · 2026-10-29");

    // A session whose agent reports no cost shows none, never $0.
    let other = SessionId::new();
    pull(&mut h, vec![created(other, 4, "/srv/free")]);
    assert!(!line_with(&h.screen(), "/srv/free").contains('$'));
}

/// Accepts this device's newest command and logs `kind` for it, as the
/// server does; returns the command.
fn logged_command(h: &mut Harness, id: SessionId, minute: u32, kind: EventKind) -> CommandId {
    let command = last_command(h);
    h.seam
        .command_result(command, &CommandResult::Accepted { seq: Seq(1) })
        .unwrap();
    pull(
        h,
        vec![Event {
            command_id: Some(command),
            ..event(id, minute, kind)
        }],
    );
    command
}

#[test]
fn the_mode_list_switches_and_bypass_asks_once_more() {
    let (mut h, id, me) = open_session();
    hello_with(&mut h, me, false, true);
    // A `p` in the composer is text, not the mode list.
    assert_eq!(h.key(KeyCode::Char('p')), None);
    assert!(h.app.ui().session.as_ref().unwrap().picker.is_none());
    h.key_with(ctrl('u'));
    h.key(KeyCode::Esc);
    assert_contains(&text(&h.frame(200, 24, at(3))), "p mode · m model");

    // Esc closes the list and sends nothing.
    assert_eq!(h.key(KeyCode::Char('p')), None);
    let screen = h.screen();
    assert_contains(&screen, "Permission mode · now unknown");
    assert_contains(&screen, "never asks; everything runs");
    assert_contains(&screen, "Enter switch · Esc cancel");
    assert!(!h.app.typing(), "the mode list takes no text");
    assert_eq!(h.key(KeyCode::Char('q')), None);
    assert!(!h.app.ui().quit, "the list takes every key");
    assert_eq!(h.key(KeyCode::Esc), None);
    assert!(h.app.ui().session.as_ref().unwrap().picker.is_none());
    assert!(matches!(h.app.view().screen, Screen::Session(_)));

    h.key(KeyCode::Char('p'));
    h.key(KeyCode::Down);
    assert_eq!(
        h.key(KeyCode::Enter),
        Some(Action::SetPermissionMode {
            session_id: id,
            mode: PermissionMode::AcceptEdits,
        })
    );
    assert!(h.app.ui().session.as_ref().unwrap().picker.is_none());
    assert_contains(
        &status_line(&h, 100),
        "agent idle · mode unknown → accept edits",
    );
    let command = logged_command(
        &mut h,
        id,
        1,
        EventKind::PermissionModeRequested {
            mode: PermissionMode::AcceptEdits,
            by: me,
        },
    );
    pull(
        &mut h,
        vec![event(
            id,
            2,
            EventKind::PermissionModeChanged {
                mode: PermissionMode::AcceptEdits,
                answers: Some(command),
            },
        )],
    );
    let screen = h.screen();
    assert_contains(&status_line(&h, 100), "agent idle · mode accept edits");
    assert_contains(&screen, "mode → accept edits by this device");

    // Bypass waits for `y`; any other key goes back to the list.
    h.key(KeyCode::Char('p'));
    assert_contains(
        &h.screen(),
        "accept edits  file edits run without asking · now",
    );
    for _ in 0..5 {
        h.key(KeyCode::Char('j'));
    }
    assert_eq!(h.key(KeyCode::Enter), None);
    let screen = h.screen();
    assert_contains(&screen, "Switch to bypass?");
    assert_contains(&screen, "Never asks; everything runs.");
    assert_contains(&screen, "y switch · any other key back");
    assert_eq!(h.key(KeyCode::Char('n')), None);
    assert!(!h.screen().contains("Switch to bypass?"));
    assert_eq!(h.key(KeyCode::Enter), None);
    assert_eq!(
        h.key(KeyCode::Char('y')),
        Some(Action::SetPermissionMode {
            session_id: id,
            mode: PermissionMode::Bypass,
        })
    );
    assert_contains(&status_line(&h, 100), "mode accept edits → bypass");

    // Refused: the old mode stays, and the status line says why until the
    // next switch.
    let command = logged_command(
        &mut h,
        id,
        3,
        EventKind::PermissionModeRequested {
            mode: PermissionMode::Bypass,
            by: me,
        },
    );
    pull(
        &mut h,
        vec![event(
            id,
            4,
            EventKind::PermissionModeRefused {
                mode: PermissionMode::Bypass,
                reason: "not launched for it".into(),
                answers: Some(command),
            },
        )],
    );
    let status = status_line(&h, 120);
    assert_contains(
        &status,
        "mode accept edits · bypass refused: not launched for it",
    );
    assert_contains(
        &h.screen(),
        "mode bypass refused: not launched for it by this device",
    );
}

#[test]
fn the_model_list_offers_the_agents_models_or_takes_a_typed_name() {
    let (mut h, id, me) = open_session();
    h.key(KeyCode::Esc);

    // No list yet: a name is typed, `q` and `?` included.
    assert_eq!(h.key(KeyCode::Char('m')), None);
    assert_eq!(h.key(KeyCode::Char('m')), None);
    let screen = h.screen();
    assert_contains(&screen, "Model · now unknown");
    assert_contains(&screen, "No list yet");
    assert_contains(&screen, "or type a model name");
    assert!(h.app.typing());
    // Nothing typed, nothing listed: Enter has nothing to send.
    assert_eq!(h.key(KeyCode::Enter), None);
    h.type_text("opus?q");
    assert!(!h.app.ui().quit && !h.app.ui().help);
    h.key(KeyCode::Backspace);
    h.key(KeyCode::Backspace);
    assert_contains(&h.screen(), "> opus");
    assert_eq!(
        h.key(KeyCode::Enter),
        Some(Action::SetModel {
            session_id: id,
            model: "opus".into(),
        })
    );
    assert!(!h.app.typing(), "back on the timeline");
    assert_contains(&status_line(&h, 100), "agent idle · model unknown → opus");
    let command = logged_command(
        &mut h,
        id,
        1,
        EventKind::ModelRequested {
            model: "opus".into(),
            by: me,
        },
    );
    pull(
        &mut h,
        vec![
            event(
                id,
                2,
                EventKind::ModelsOffered {
                    models: vec![
                        ModelChoice {
                            id: "opus".into(),
                            label: "Opus".into(),
                            detail: Some("most capable".into()),
                            resolved: Some("claude-opus-9".into()),
                            efforts: Vec::new(),
                        },
                        ModelChoice {
                            id: "haiku".into(),
                            label: "Haiku".into(),
                            detail: None,
                            resolved: None,
                            efforts: Vec::new(),
                        },
                    ],
                },
            ),
            event(
                id,
                3,
                EventKind::ModelChanged {
                    model: "claude-opus-9".into(),
                    answers: Some(command),
                },
            ),
        ],
    );
    assert_contains(&status_line(&h, 100), "agent idle · Opus");
    assert_contains(&h.screen(), "model → claude-opus-9 by this device");

    // The list opens on the running model, marked; ↓ Enter sends the
    // highlighted one's id.
    h.key(KeyCode::Char('m'));
    h.key(KeyCode::Char('m'));
    let screen = h.screen();
    assert_contains(&screen, "Model · now Opus");
    assert_contains(&screen, "› Opus  most capable · now");
    assert_contains(&screen, "  Haiku");
    h.key(KeyCode::Down);
    assert_contains(&h.screen(), "› Haiku");
    assert_eq!(
        h.key(KeyCode::Enter),
        Some(Action::SetModel {
            session_id: id,
            model: "haiku".into(),
        })
    );

    // A typed name wins over the highlighted model; Ctrl-U clears it.
    h.key(KeyCode::Char('m'));
    h.key(KeyCode::Char('m'));
    h.type_text("x");
    h.key_with(ctrl('u'));
    h.app.on_paste("  sonnet-5\n");
    assert_eq!(
        h.key(KeyCode::Enter),
        Some(Action::SetModel {
            session_id: id,
            model: "sonnet-5".into(),
        })
    );
}

#[test]
fn m_opens_a_menu_to_the_model_and_effort_pickers() {
    let (mut h, id, _) = open_session();
    h.key(KeyCode::Esc);
    pull(
        &mut h,
        vec![
            event(
                id,
                1,
                EventKind::ModelsOffered {
                    models: vec![ModelChoice {
                        id: "opus".into(),
                        label: "Opus".into(),
                        detail: None,
                        resolved: Some("claude-opus-9".into()),
                        efforts: vec![Effort::Low, Effort::High, Effort::Max],
                    }],
                },
            ),
            event(
                id,
                2,
                EventKind::ModelChanged {
                    model: "claude-opus-9".into(),
                    answers: None,
                },
            ),
            event(
                id,
                3,
                EventKind::EffortChanged {
                    effort: Some(Effort::High),
                    answers: None,
                },
            ),
        ],
    );
    assert_contains(&status_line(&h, 100), "Opus · effort high");

    // `m` opens the menu, not the model list.
    assert_eq!(h.key(KeyCode::Char('m')), None);
    let screen = h.screen();
    assert_contains(&screen, " Agent ");
    assert_contains(&screen, "model   Opus");
    assert_contains(&screen, "effort  high");
    assert!(!screen.contains("or type a model name"));
    assert!(!h.app.typing());

    // `m` and `e` jump; Esc goes back to the menu, and again closes it.
    h.key(KeyCode::Char('m'));
    assert_contains(&h.screen(), "or type a model name");
    h.key(KeyCode::Esc);
    assert_contains(&h.screen(), " Agent ");
    h.key(KeyCode::Char('e'));
    let screen = h.screen();
    assert_contains(&screen, "Effort · now effort high");
    assert_contains(&screen, "› high · now");
    assert_contains(&screen, "  low");
    assert_contains(&screen, "  max");
    assert!(!screen.contains("medium"), "only the model's levels");
    h.key(KeyCode::Esc);
    assert_contains(&h.screen(), " Agent ");
    h.key(KeyCode::Esc);
    let screen = h.screen();
    assert!(!screen.contains(" Agent ") && !screen.contains("Effort · now"));

    // The menu's rows open the pickers too.
    h.key(KeyCode::Char('m'));
    h.key(KeyCode::Down);
    h.key(KeyCode::Enter);
    assert_contains(&h.screen(), "Effort · now");
    h.key(KeyCode::Esc);
    h.key(KeyCode::Up);
    h.key(KeyCode::Enter);
    assert_contains(&h.screen(), "or type a model name");
    h.key(KeyCode::Esc);
    h.key(KeyCode::Esc);

    // Enter sends the highlighted level and closes everything.
    h.key(KeyCode::Char('m'));
    h.key(KeyCode::Char('e'));
    h.key(KeyCode::Down);
    assert_eq!(
        h.key(KeyCode::Enter),
        Some(Action::SetEffort {
            session_id: id,
            effort: Effort::Max,
        })
    );
    assert!(!h.screen().contains("Effort · now"));
    assert_contains(&status_line(&h, 100), "effort high → max");

    // The picker opens on the level asked for last.
    h.key(KeyCode::Char('m'));
    h.key(KeyCode::Char('e'));
    assert_contains(&h.screen(), "› max");
}

#[test]
fn an_empty_effort_list_sends_nothing() {
    let (mut h, id, _) = open_session();
    h.key(KeyCode::Esc);
    pull(
        &mut h,
        vec![
            event(
                id,
                1,
                EventKind::ModelsOffered {
                    models: vec![ModelChoice {
                        id: "haiku".into(),
                        label: "Haiku".into(),
                        detail: None,
                        resolved: None,
                        efforts: Vec::new(),
                    }],
                },
            ),
            event(
                id,
                2,
                EventKind::ModelChanged {
                    model: "haiku".into(),
                    answers: None,
                },
            ),
        ],
    );
    h.key(KeyCode::Char('m'));
    h.key(KeyCode::Char('e'));
    assert_contains(&h.screen(), "Haiku takes no effort level");
    assert_eq!(h.key(KeyCode::Enter), None);
    assert_contains(&h.screen(), "takes no effort level");
}

#[test]
fn agent_setting_keys_are_in_help() {
    let (mut h, _, _) = open_session();
    h.key(KeyCode::Tab);
    let help = read_help(&mut h);
    assert_keys(&help, &["p", "m"]);
    assert_contains(&help.join("\n"), "bypass lets every tool run");
}

fn task(
    id: &str,
    kind: skywave_proto::TaskKind,
    title: &str,
    parent: Option<&str>,
    ambient: bool,
) -> skywave_proto::BackgroundTask {
    skywave_proto::BackgroundTask {
        call: None,
        id: skywave_proto::TaskId(id.into()),
        kind,
        title: title.into(),
        parent: parent.map(|p| skywave_proto::TaskId(p.into())),
        ambient,
    }
}

fn a_loop(outlives_agent: Option<bool>) -> skywave_proto::Schedule {
    skywave_proto::Schedule {
        id: skywave_proto::ScheduleId("s1".into()),
        when: "*/1 * * * *".into(),
        recurring: true,
        prompt: "check the deploy".into(),
        outlives_agent,
    }
}

/// An open session (on the timeline) whose agent reported this work.
fn with_work(
    tasks: Vec<skywave_proto::BackgroundTask>,
    schedules: Vec<skywave_proto::Schedule>,
) -> (Harness, SessionId, DeviceId) {
    let (mut h, id, me) = open_session();
    h.key(KeyCode::Esc);
    let mut events = Vec::new();
    if !tasks.is_empty() {
        events.push(event(id, 1, EventKind::BackgroundTasksChanged { tasks }));
    }
    if !schedules.is_empty() {
        events.push(event(id, 2, EventKind::SchedulesChanged { schedules }));
    }
    pull(&mut h, events);
    (h, id, me)
}

fn one_task() -> Vec<skywave_proto::BackgroundTask> {
    vec![task(
        "t1",
        skywave_proto::TaskKind::Shell,
        "npm run dev",
        None,
        false,
    )]
}

#[test]
fn stop_with_background_work_warns_and_waits_for_y() {
    let (mut h, id, _) = with_work(one_task(), vec![a_loop(None)]);
    assert_eq!(h.key(KeyCode::Char('S')), None);
    let screen = h.screen();
    assert_contains(&screen, "Stop the agent?");
    assert_contains(
        &screen,
        "Stopping ends 1 background task and pauses 1 loop.",
    );
    // Any other key cancels and sends nothing.
    assert_eq!(h.key(KeyCode::Char('n')), None);
    assert!(!h.screen().contains("Stop the agent?"));
    assert_eq!(h.key(KeyCode::Char('y')), None);
    // Positive control: asked again, `y` stops.
    h.key(KeyCode::Char('S'));
    assert_eq!(
        h.key(KeyCode::Char('y')),
        Some(Action::StopSession { session_id: id })
    );
}

#[test]
fn stop_without_work_or_with_only_ambient_work_is_at_once() {
    let (mut h, id, _) = with_work(vec![], vec![]);
    assert_eq!(
        h.key(KeyCode::Char('S')),
        Some(Action::StopSession { session_id: id })
    );
    let ambient = vec![task(
        "w",
        skywave_proto::TaskKind::Shell,
        "watch",
        None,
        true,
    )];
    let (mut h, id, _) = with_work(ambient, vec![]);
    assert_eq!(
        h.key(KeyCode::Char('S')),
        Some(Action::StopSession { session_id: id })
    );
}

#[test]
fn clear_and_switch_with_work_warn_and_wait_for_y() {
    let (mut h, id, _) = two_contexts();
    pull(
        &mut h,
        vec![event(
            id,
            10,
            EventKind::BackgroundTasksChanged { tasks: one_task() },
        )],
    );
    // Switch to the earlier context.
    h.key(KeyCode::Char('C'));
    h.key(KeyCode::Up);
    assert_eq!(h.key(KeyCode::Enter), None);
    assert_contains(&h.screen(), "Switch context?");
    assert_contains(&h.screen(), "Stopping ends 1 background task.");
    assert_eq!(
        h.key(KeyCode::Char('y')),
        Some(Action::SwitchContext {
            session_id: id,
            context: skywave_proto::ContextId::first_of(id),
        })
    );
    // Clear: row 0.
    h.key(KeyCode::Char('C'));
    h.key(KeyCode::Up);
    h.key(KeyCode::Up);
    assert_eq!(h.key(KeyCode::Enter), None);
    assert_contains(&h.screen(), "Start a new context?");
    assert_eq!(h.key(KeyCode::Char('n')), None);
    h.key(KeyCode::Char('C'));
    h.key(KeyCode::Up);
    h.key(KeyCode::Up);
    h.key(KeyCode::Enter);
    assert_eq!(
        h.key(KeyCode::Char('y')),
        Some(Action::ClearContext { session_id: id })
    );
}

#[test]
fn moving_the_current_context_warns_but_another_does_not() {
    // The current context is the movable one: it has an answer in it.
    let (mut h, id, me) = open_session();
    h.key(KeyCode::Esc);
    pull(
        &mut h,
        vec![
            message(id, 1, CommandId::new(), me, "alpha question"),
            event(
                id,
                2,
                EventKind::SessionStateChanged(LogState::AgentWorking),
            ),
            event(
                id,
                3,
                EventKind::AgentMessage {
                    parent: None,
                    text: "alpha answer".into(),
                    full: None,
                },
            ),
            event(
                id,
                4,
                EventKind::BackgroundTasksChanged { tasks: one_task() },
            ),
        ],
    );
    h.key(KeyCode::Char('C'));
    assert_eq!(h.key(KeyCode::Char('M')), None);
    assert_contains(&h.screen(), "Move this context to a new session?");
    assert_contains(&h.screen(), "Stopping ends 1 background task.");
    assert_eq!(h.key(KeyCode::Char('n')), None);
    h.key(KeyCode::Char('C'));
    h.key(KeyCode::Char('M'));
    assert_eq!(
        h.key(KeyCode::Char('y')),
        Some(Action::MoveContext {
            session_id: id,
            context: skywave_proto::ContextId::first_of(id),
        })
    );

    // Another context moves with the ordinary confirm, work or not.
    let (mut h, id, _) = movable_context();
    pull(
        &mut h,
        vec![event(
            id,
            10,
            EventKind::BackgroundTasksChanged { tasks: one_task() },
        )],
    );
    h.key(KeyCode::Char('C'));
    h.key(KeyCode::Up);
    h.key(KeyCode::Char('M'));
    let screen = h.screen();
    assert_contains(&screen, "Move start · alpha question to a new session?");
    assert!(!screen.contains("Stopping"), "{screen}");
}

#[test]
fn the_background_picker_lists_work_and_stops_a_task_after_y() {
    let tasks = vec![
        task(
            "t1",
            skywave_proto::TaskKind::Subagent,
            "review the diff",
            None,
            false,
        ),
        task(
            "t2",
            skywave_proto::TaskKind::Shell,
            "cargo watch",
            Some("t1"),
            false,
        ),
    ];
    let (mut h, id, _) = with_work(tasks, vec![a_loop(Some(true))]);
    assert_eq!(h.key(KeyCode::Char('B')), None);
    let screen = h.screen();
    assert_contains(&screen, "2 background tasks · 1 loop");
    let parent = line_with(&screen, "review the diff");
    let child = line_with(&screen, "cargo watch");
    // The child sits one level (two columns) in under its parent.
    let col = |l: &str, n: &str| l[..l.find(n).unwrap()].chars().count();
    assert_eq!(
        col(&child, "cargo watch"),
        col(&parent, "review the diff") + 2,
        "{screen}"
    );
    assert_contains(&screen, "*/1 * * * * repeats check the deploy");
    assert_contains(&screen, "saved in this folder");
    assert_contains(&screen, "as of the last turn");

    // Enter on the child asks, `n` backs off, `y` stops that task.
    h.key(KeyCode::Down);
    assert_eq!(h.key(KeyCode::Enter), None);
    assert_contains(&h.screen(), "Stop cargo watch?");
    assert_eq!(h.key(KeyCode::Char('n')), None);
    h.key(KeyCode::Enter);
    assert_eq!(
        h.key(KeyCode::Char('y')),
        Some(Action::StopTask {
            session_id: id,
            task: skywave_proto::TaskId("t2".into()),
        })
    );
}

#[test]
fn enter_on_a_loop_sends_nothing() {
    let (mut h, _, _) = with_work(one_task(), vec![a_loop(None)]);
    h.key(KeyCode::Char('B'));
    h.key(KeyCode::Down);
    assert_eq!(h.key(KeyCode::Enter), None);
    let screen = h.screen();
    assert!(!screen.contains("y stops it"), "{screen}");
    assert_eq!(h.key(KeyCode::Char('y')), None);
    // Positive control: the same keys on the task do ask.
    h.key(KeyCode::Up);
    h.key(KeyCode::Enter);
    assert_contains(&h.screen(), "Stop npm run dev?");
}

/// An open session, on the timeline, whose main agent started a subagent
/// ("Explore") that made three calls, the last `Read c.rs`. Its task runs
/// in the background while `live`. Returns the `Agent` call.
fn with_subagent(live: bool) -> (Harness, SessionId, skywave_proto::CallId) {
    let (mut h, id, me) = open_session();
    h.key(KeyCode::Esc);
    let agent = skywave_proto::CallId::new();
    let call = |minute: u32, call: skywave_proto::CallId, parent, kind, title: &str| {
        event(
            id,
            minute,
            EventKind::ToolCall(ToolCall {
                call: Some(call),
                parent,
                name: "tool".into(),
                display: ToolDisplay {
                    kind,
                    title: title.into(),
                    detail: None,
                    body: None,
                },
                input: None,
            }),
        )
    };
    let mut events = vec![
        message(id, 1, CommandId::new(), me, "look around"),
        call(2, agent, None, ToolKind::Subagent, "Explore"),
        event(
            id,
            3,
            EventKind::ToolResult(ToolResult {
                call: Some(agent),
                is_error: false,
                status: None,
                preview: "Async agent launched".into(),
                lines: 1,
                output: None,
            }),
        ),
    ];
    for (minute, name) in [(4, "a.rs"), (5, "b.rs"), (6, "c.rs")] {
        events.push(call(
            minute,
            skywave_proto::CallId::new(),
            Some(agent),
            ToolKind::FileRead,
            &format!("Read {name}"),
        ));
    }
    if live {
        let mut explore = task(
            "t1",
            skywave_proto::TaskKind::Subagent,
            "Explore",
            None,
            false,
        );
        explore.call = Some(agent);
        events.push(event(
            id,
            7,
            EventKind::BackgroundTasksChanged {
                tasks: vec![explore],
            },
        ));
    }
    pull(&mut h, events);
    (h, id, agent)
}

/// Selects the `Agent` row on the timeline.
fn select_row(h: &mut Harness, title: &str) {
    for step in 0..8 {
        let selected = h.app.ui().session.as_ref().unwrap().shown.selected;
        let on_it = match (selected, h.app.session()) {
            (Some(key), Some((view, _))) => skywave_present::sections::find(view, key)
                .is_some_and(|i| format!("{:?}", view.timeline[i]).contains(title)),
            _ => false,
        };
        if on_it {
            return;
        }
        h.key(if step < 4 { KeyCode::Down } else { KeyCode::Up });
    }
    panic!("{title} not selectable:\n{}", h.screen());
}

#[test]
fn a_subagent_row_shows_how_it_stands_and_o_opens_it() {
    let (mut h, id, agent) = with_subagent(true);
    let screen = h.screen();
    assert_contains(
        &line_with(&screen, "Explore"),
        "running · 3 steps · Read c.rs",
    );
    // The launch's own result is not what the row says.
    assert!(!screen.contains("Async agent launched"), "{screen}");
    // The main view holds only the Agent row, none of its calls.
    assert!(!screen.contains("Read a.rs"), "{screen}");

    // `o` with the row not selected does nothing (positive control below).
    h.key(KeyCode::Char('G'));
    select_row(&mut h, "Explore");
    assert_eq!(
        h.key(KeyCode::Char('o')),
        Some(Action::ViewSubagent {
            session_id: id,
            call: Some(agent),
        })
    );
    let screen = h.screen();
    assert_contains(&screen, "Read a.rs");
    // The header names the trail and the status; the composer says whom it
    // messages.
    assert_contains(&screen, "look around › Explore");
    assert_contains(&screen, "running · 3 steps · Read c.rs · S stops it");
    assert_contains(&screen, "to the main agent");

    // Esc steps out.
    assert_eq!(h.key(KeyCode::Esc), Some(Action::Back));
    assert_contains(&h.screen(), "look around");
}

#[test]
fn o_does_nothing_off_a_subagent_row() {
    let (mut h, _, _) = with_subagent(true);
    h.key(KeyCode::Up);
    h.key(KeyCode::Up);
    h.key(KeyCode::Up);
    assert_eq!(h.key(KeyCode::Char('o')), None);
}

#[test]
fn a_finished_subagent_row_says_finished() {
    let (h, _, _) = with_subagent(false);
    // No task, and its call is no longer running: it ended.
    let screen = h.screen();
    assert_contains(&line_with(&screen, "Explore"), "finished · 3 steps");
}

#[test]
fn leaving_a_subagent_selects_its_agent_row_and_keeps_the_main_composer() {
    let (mut h, id, agent) = with_subagent(true);
    // Unsaved text in the composer: the draft saver has not run, and the
    // save every action makes first (D5) fails, so only the live composer
    // can still hold the text after the round trip.
    h.fail_draft_save(true);
    h.key(KeyCode::Char('i'));
    h.type_text("half a thought");
    h.key(KeyCode::Esc);
    assert_eq!(h.core_draft(), None);
    // The selection is elsewhere, not on the Agent row.
    select_row(&mut h, "look around");
    h.key(KeyCode::Char('G'));
    let before = h.app.ui().session.as_ref().unwrap().shown.selected;
    select_row(&mut h, "Explore");
    assert_ne!(
        h.app.ui().session.as_ref().unwrap().shown.selected,
        before,
        "the control needs the selection to move"
    );
    h.key(KeyCode::Char('o'));
    h.key(KeyCode::Down);
    let inside = h.app.ui().session.as_ref().unwrap().shown.selected;
    assert!(inside.is_some());
    assert_eq!(h.key(KeyCode::Esc), Some(Action::Back));

    // Back in the main view the Agent row is selected, not what the
    // subagent view had selected.
    let (view, ui) = h.app.session().unwrap();
    assert!(view.subagent.is_none());
    let row = ui
        .shown
        .selected
        .and_then(|key| skywave_present::sections::find(view, key).map(|i| &view.timeline[i]));
    let Some(skywave_core::TimelineItem::Logged {
        entry: skywave_core::Entry::Tool(tool),
        ..
    }) = row
    else {
        panic!("the selected row is not the Agent call: {row:?}");
    };
    assert_eq!(tool.call, agent);
    assert_ne!(ui.shown.selected, inside);

    // The main composer kept its unsaved text through both views.
    assert_eq!(h.app.composer().unwrap().text(), "half a thought");
    assert_eq!(h.core_draft(), None);
    let _ = id;
}

#[test]
fn s_in_a_subagent_view_stops_its_task_after_y() {
    let (mut h, id, agent) = with_subagent(true);
    // Positive control: in the main view S still stops the agent.
    h.key(KeyCode::Char('G'));
    assert_eq!(h.key(KeyCode::Char('S')), None);
    assert_contains(&h.screen(), "Stop the agent?");
    assert_eq!(
        h.key(KeyCode::Char('y')),
        Some(Action::StopSession { session_id: id })
    );

    select_row(&mut h, "Explore");
    h.key(KeyCode::Char('o'));
    assert_eq!(h.key(KeyCode::Char('S')), None);
    let screen = h.screen();
    assert_contains(&screen, "Stop this subagent? Explore");
    assert!(!screen.contains("Stop the agent?"), "{screen}");
    // Anything but `y` backs off.
    assert_eq!(h.key(KeyCode::Char('n')), None);
    h.key(KeyCode::Char('S'));
    assert_eq!(
        h.key(KeyCode::Char('y')),
        Some(Action::StopTask {
            session_id: id,
            task: skywave_proto::TaskId("t1".into()),
        })
    );
    let _ = agent;
}

#[test]
fn s_in_a_foreground_subagent_view_has_nothing_to_stop() {
    let (mut h, _, _) = with_subagent(false);
    select_row(&mut h, "Explore");
    h.key(KeyCode::Char('o'));
    assert_eq!(h.key(KeyCode::Char('S')), None);
    let screen = h.screen();
    assert!(!screen.contains("Stop"), "{screen}");
    assert!(!screen.contains("S stops it"), "{screen}");
}

#[test]
fn o_in_the_background_picker_opens_the_subagent_and_a_shell_task_does_not() {
    let (mut h, id, agent) = with_subagent(true);
    h.key(KeyCode::Char('B'));
    assert_eq!(
        h.key(KeyCode::Char('o')),
        Some(Action::ViewSubagent {
            session_id: id,
            call: Some(agent),
        })
    );
    // The picker closed, the subagent shows.
    let screen = h.screen();
    assert!(h.app.ui().session.as_ref().unwrap().picker.is_none());
    assert_contains(&screen, "Read a.rs");

    // A task with no call has nothing to open.
    let (mut h, _, _) = with_work(one_task(), vec![]);
    h.key(KeyCode::Char('B'));
    assert_eq!(h.key(KeyCode::Char('o')), None);
    assert!(h.app.ui().session.as_ref().unwrap().picker.is_some());
}

#[test]
fn an_approval_from_a_subagent_says_so_and_where_to_look() {
    let (mut h, id, agent) = with_subagent(true);
    let gated = skywave_proto::CallId::new();
    let approval = ApprovalId::new();
    pull(
        &mut h,
        vec![
            event(
                id,
                8,
                EventKind::ToolCall(ToolCall {
                    call: Some(gated),
                    parent: Some(agent),
                    name: "Write".into(),
                    display: ToolDisplay {
                        kind: ToolKind::FileWrite,
                        title: "src/sync.rs".into(),
                        detail: None,
                        body: None,
                    },
                    input: None,
                }),
            ),
            event(
                id,
                9,
                EventKind::ApprovalRequested {
                    id: approval,
                    call: Some(gated),
                    tool: "Write".into(),
                    summary: "Write src/sync.rs".into(),
                    input: None,
                    suggestions: vec![],
                    shell: None,
                },
            ),
        ],
    );
    let screen = h.screen();
    assert_contains(&screen, "from subagent: Explore · open it to see the call");
    // In the subagent's view the call is there, so the label says only who.
    select_row(&mut h, "Explore");
    h.key(KeyCode::Char('o'));
    let screen = h.screen();
    assert_contains(&screen, "from subagent: Explore");
    assert!(!screen.contains("open it to see the call"), "{screen}");
}

#[test]
fn the_help_lists_o_and_the_subagent_stop() {
    let (mut h, _, _) = with_subagent(true);
    let help = read_help(&mut h).join("\n");
    assert_contains(&help, "open the selected subagent");
    assert_contains(&help, "in a subagent's view: stop its task");
}

#[test]
fn without_the_servers_leave_the_mode_list_has_no_bypass() {
    let (mut h, id, me) = open_session();
    hello_with(&mut h, me, false, false);
    h.key(KeyCode::Esc);
    h.key(KeyCode::Char('p'));
    let screen = h.screen();
    assert_contains(&screen, "never asks; what no rule allows is denied");
    assert!(!screen.contains("never asks; everything runs"), "{screen}");
    // The last row is don't ask: going on stays there, and Enter sends it
    // without asking once more.
    for _ in 0..8 {
        h.key(KeyCode::Char('j'));
    }
    assert_eq!(
        h.key(KeyCode::Enter),
        Some(Action::SetPermissionMode {
            session_id: id,
            mode: PermissionMode::DontAsk,
        })
    );
}

// ---- shell mode -------------------------------------------------------------

/// A log for a `hello` the view takes alone (no link checks it).
fn any_log() -> skywave_proto::LogRun {
    skywave_proto::LogRun {
        log: skywave_proto::LogId::new(),
        run: skywave_proto::RunId::new(),
        since: None,
    }
}

/// The server's `hello`, saying whether it runs shell commands.
fn hello(h: &mut Harness, me: DeviceId, shell: bool) {
    hello_with(h, me, shell, false);
}

/// The server's `hello`, saying whether it runs shell commands and
/// whether it allows bypass mode.
fn hello_with(h: &mut Harness, me: DeviceId, shell: bool, bypass: bool) {
    h.seam
        .hello_received(&HelloResponse {
            protocol_version: skywave_proto::PROTOCOL_VERSION,
            server_id: skywave_proto::ServerId::new(),
            device_id: me,
            head: Seq::ZERO,
            log: any_log(),
            devices: Vec::new(),
            shell,
            bypass,
            login_expires: None,
            agent_login_offered: false,
            agent_logged_out: false,
        })
        .unwrap();
    h.sync();
}

/// [`open_session`] on a server that runs shell commands.
fn shell_session() -> (Harness, SessionId, DeviceId) {
    let (mut h, id, me) = open_session();
    hello(&mut h, me, true);
    (h, id, me)
}

/// The open session as the core shows it.
fn session_view(h: &Harness) -> skywave_core::SessionView {
    match h.core.view().screen {
        Screen::Session(view) => *view,
        other => panic!("expected the session screen, got {other:?}"),
    }
}

fn mode(h: &Harness) -> ComposerMode {
    h.app.session().unwrap().1.mode
}

/// `command`, run `run`, as the log holds its request from `by`.
fn requested(id: SessionId, minute: u32, run: CommandId, by: DeviceId, command: &str) -> Event {
    Event {
        command_id: Some(run),
        ..event(
            id,
            minute,
            EventKind::ShellRequested {
                command: command.into(),
                when: RunWhen::Always,
                by,
            },
        )
    }
}

fn started(id: SessionId, at: DateTime<Utc>, run: CommandId) -> Event {
    Event {
        at,
        ..event(
            id,
            0,
            EventKind::ShellStarted {
                run,
                cwd: "/srv/work".into(),
            },
        )
    }
}

/// The end of run `run`: `exit 1` at `at`, with `preview` and, when it
/// does not hold all of it, the `output` blob.
fn finished(
    id: SessionId,
    at: DateTime<Utc>,
    run: CommandId,
    by: DeviceId,
    started_at: DateTime<Utc>,
    preview: &str,
    output: Option<BlobRef>,
) -> Event {
    Event {
        at,
        ..event(
            id,
            0,
            EventKind::ShellFinished {
                run,
                by,
                started_at: Some(started_at),
                command_line: "make test".into(),
                end: ShellEnd::Exited { code: 1 },
                cwd: Some("/srv/work".into()),
                preview: preview.into(),
                lines: preview.lines().count() as u32,
                truncated: output.is_some(),
                output,
            },
        )
    }
}

/// The composer's border line: the prompt in shell mode.
fn composer_border(h: &Harness) -> String {
    let screen = h.screen();
    let lines: Vec<&str> = screen.lines().collect();
    // The composer is the last box: its top border is the last line with
    // a top-left corner.
    lines
        .iter()
        .rev()
        .find(|l| l.contains('┌'))
        .unwrap()
        .to_string()
}

#[test]
fn ctrl_t_switches_to_shell_commands_and_keeps_both_drafts() {
    let (mut h, id, me) = shell_session();
    let run = CommandId::new();
    pull(
        &mut h,
        vec![
            message(id, 1, CommandId::new(), me, "an earlier prompt"),
            requested(id, 2, run, me, "make"),
            started(id, at(2), run),
        ],
    );
    h.type_text("hello agent");
    assert_contains(&h.screen(), "you: an earlier prompt");

    assert_eq!(
        h.key_with(ctrl('t')),
        Some(Action::SetComposerMode {
            session_id: id,
            mode: ComposerMode::Shell,
        })
    );
    assert_eq!(mode(&h), ComposerMode::Shell);
    // The agent's draft was saved on the way; the shell one is empty.
    assert_eq!(h.core_draft().unwrap().text, "hello agent");
    assert_eq!(composer_text(&h), "");
    // The runs stand in for the timeline.
    let screen = h.screen();
    assert_contains(&screen, "$ make");
    assert!(!screen.contains("you: an earlier prompt"), "{screen}");
    assert_contains(&composer_border(&h), "work $");

    // `&&` is text; the condition is not.
    h.type_text("ls && pwd");
    assert_eq!(h.key_with(ctrl('o')), None);
    let border = composer_border(&h);
    assert_contains(&border, "work [if ok ✓] $");
    assert!(!border.contains("ls"), "{border}");
    let typed = line_with(&h.screen(), "ls && pwd");
    assert!(!typed.contains("if ok"), "{typed}");
    assert_eq!(composer_text(&h), "ls && pwd");
    h.key_with(ctrl('o'));
    assert_contains(&composer_border(&h), "work [if failed ✗] $");
    h.key_with(ctrl('o'));
    let border = composer_border(&h);
    assert_contains(&border, "work $");
    assert!(!border.contains("if "), "{border}");
    h.key_with(ctrl('o'));

    // Back: each mode has its own draft, the condition with the shell's.
    assert_eq!(
        h.key_with(ctrl('t')),
        Some(Action::SetComposerMode {
            session_id: id,
            mode: ComposerMode::Agent,
        })
    );
    assert_eq!(composer_text(&h), "hello agent");
    assert_contains(&h.screen(), "you: an earlier prompt");
    let shell = session_view(&h).shell_draft.unwrap();
    assert_eq!(
        (shell.text.as_str(), shell.when),
        ("ls && pwd", RunWhen::OnSuccess)
    );
    h.key_with(ctrl('t'));
    assert_eq!(composer_text(&h), "ls && pwd");
    assert_contains(&composer_border(&h), "[if ok ✓] $");

    // Enter runs it with its condition, which goes back to none.
    assert_eq!(
        h.key(KeyCode::Enter),
        Some(Action::RunShell {
            session_id: id,
            command: "ls && pwd".into(),
            when: RunWhen::OnSuccess,
        })
    );
    assert_eq!(composer_text(&h), "");
    assert_eq!(h.app.session().unwrap().1.when, RunWhen::Always);
    assert!(!composer_border(&h).contains("if ok"));
    let row = line_with(&h.screen(), "$ ls && pwd");
    assert_contains(&row, "[if ok ✓]");
    h.idle_save();
    assert_eq!(session_view(&h).shell_draft, None);
    // A blank command is not sent; the condition picked stays.
    h.key_with(ctrl('o'));
    assert_eq!(h.key(KeyCode::Enter), None);
    assert_eq!(h.app.session().unwrap().1.when, RunWhen::OnSuccess);
}

#[test]
fn the_shell_mode_and_condition_survive_a_restart() {
    let (mut h, id, _) = shell_session();
    h.key_with(ctrl('t'));
    h.type_text("cargo test");
    h.key_with(ctrl('o'));
    h.idle_save();

    let mut h = h.restart();
    assert_eq!(h.key(KeyCode::Enter), Some(Action::OpenSession(id)));
    assert_eq!(mode(&h), ComposerMode::Shell);
    assert_eq!(composer_text(&h), "cargo test");
    assert_contains(&composer_border(&h), "[if ok ✓] $");
}

#[test]
fn an_edited_run_comes_back_with_its_condition() {
    let (mut h, id, _) = shell_session();
    h.key_with(ctrl('t'));
    h.type_text("make deploy");
    h.key_with(ctrl('o'));
    h.key_with(ctrl('o'));
    assert_eq!(
        h.key(KeyCode::Enter),
        Some(Action::RunShell {
            session_id: id,
            command: "make deploy".into(),
            when: RunWhen::OnFailure,
        })
    );
    let run = last_command(&h);
    assert!(!composer_border(&h).contains("if failed"));

    // Never sent: `e` on it takes it back into the composer.
    h.key(KeyCode::Tab);
    h.key(KeyCode::Char('k'));
    assert_contains(&footer(&h), "e edit");
    assert_eq!(h.key(KeyCode::Char('e')), Some(Action::EditQueued(run)));
    assert_eq!(
        h.app.session().unwrap().1.focus,
        skywave_tui::app::Focus::Composer
    );
    assert_eq!(composer_text(&h), "make deploy");
    assert_contains(&composer_border(&h), "[if failed ✗] $");
    assert!(session_view(&h).shell.runs.is_empty());

    // Sent again and taken back with Delete instead.
    h.key(KeyCode::Enter);
    let run = last_command(&h);
    h.key(KeyCode::Tab);
    h.key(KeyCode::Char('k'));
    assert_eq!(h.key(KeyCode::Delete), Some(Action::Discard(run)));
    assert!(session_view(&h).shell.runs.is_empty());
}

#[test]
fn ctrl_x_stops_a_shell_run_in_shell_mode_and_interrupts_otherwise() {
    let (mut h, id, me) = shell_session();
    let run = CommandId::new();
    pull(
        &mut h,
        vec![
            requested(id, 1, run, me, "sleep 60"),
            started(id, at(1), run),
        ],
    );
    assert_eq!(
        h.key_with(ctrl('x')),
        Some(Action::Interrupt { session_id: id })
    );
    h.key_with(ctrl('t'));
    assert_eq!(
        h.key_with(ctrl('x')),
        Some(Action::StopShell {
            session_id: id,
            run
        })
    );
    let row = line_with(&h.screen(), "running");
    assert_contains(&row, "stopping");
    // Already stopping: nothing else to stop.
    assert_eq!(h.key_with(ctrl('x')), None);
}

#[test]
fn up_recalls_shell_commands_in_shell_mode_and_prompts_otherwise() {
    let (mut h, id, me) = shell_session();
    let run = CommandId::new();
    pull(
        &mut h,
        vec![
            message(id, 1, CommandId::new(), me, "a prompt"),
            requested(id, 2, run, me, "make test"),
        ],
    );
    h.key(KeyCode::Up);
    assert_eq!(composer_text(&h), "a prompt");
    h.key(KeyCode::Down);
    h.key_with(ctrl('t'));
    h.key(KeyCode::Up);
    assert_eq!(composer_text(&h), "make test");
    assert_contains(&footer(&h), "history 1/1");
    assert_contains(&footer(&h), "Enter run");
    h.key(KeyCode::Up);
    assert_eq!(
        composer_text(&h),
        "make test",
        "the prompt is not a command"
    );
}

#[test]
fn with_shell_commands_off_there_is_no_toggle() {
    let (mut h, _, me) = open_session();
    assert!(!footer(&h).contains("Ctrl-T"), "{}", footer(&h));
    assert_eq!(h.key_with(ctrl('t')), None);
    assert_eq!(mode(&h), ComposerMode::Agent);
    assert_eq!(
        h.app.ui().notice.as_deref(),
        Some("shell commands are off on this server")
    );
    let help = read_help(&mut h).join("\n");
    assert!(!help.contains("Ctrl-T"), "{help}");

    // On a server that runs them, the same keys show the toggle.
    hello(&mut h, me, true);
    assert_contains(&footer(&h), "Ctrl-T shell");
    let help = read_help(&mut h).join("\n");
    assert_contains(&help, "Ctrl-T");
    assert!(matches!(
        h.key_with(ctrl('t')),
        Some(Action::SetComposerMode { .. })
    ));
    // Turned off while in shell mode: the way back stays.
    hello(&mut h, me, false);
    assert_eq!(
        h.key_with(ctrl('t')),
        Some(Action::SetComposerMode {
            session_id: h.app.session().unwrap().0.id,
            mode: ComposerMode::Agent,
        })
    );
}

#[test]
fn y_copies_a_runs_output_with_ansi_stripped() {
    let (mut h, id, me) = shell_session();
    let run = CommandId::new();
    let (blob_id, output) = blob(40);
    pull(
        &mut h,
        vec![
            requested(id, 1, run, me, "make test"),
            started(id, at(1), run),
            finished(id, at_s(1, 5), run, me, at(1), "the tail\n", Some(output)),
        ],
    );
    h.key_with(ctrl('t'));
    h.key(KeyCode::Tab);
    h.key(KeyCode::Char('k'));
    let row = line_with(&h.screen(), "exit 1");
    assert_contains(&row, &took_label(Duration::from_secs(5)));

    // Not here yet: downloaded first, never just the preview copied.
    assert_eq!(
        h.key(KeyCode::Char('y')),
        Some(Action::DownloadBlob(blob_id))
    );
    assert_eq!(clip(&h), None);
    assert_eq!(
        notice(&h).as_deref(),
        Some("downloading 40 B · press y again when it is here")
    );
    assert_eq!(h.drain_requests(), vec![Request::DownloadBlob(blob_id)]);
    // On its way: asked for once only.
    assert_eq!(h.key(KeyCode::Char('y')), None);
    assert_eq!(clip(&h), None);
    assert_contains(&notice(&h).unwrap(), "downloading");
    assert_eq!(h.key(KeyCode::Char('d')), None, "on its way already");

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("blob");
    std::fs::write(&path, b"\x1b[31mred\x1b[0m\nthe tail\n").unwrap();
    h.seam
        .blob_status(blob_id, BlobStatus::Available { path: path.clone() })
        .unwrap();
    h.sync();
    assert_eq!(h.key(KeyCode::Char('d')), None, "here already");
    assert_eq!(h.key(KeyCode::Char('y')), None);
    assert_eq!(clip(&h), Some(Clip::Output { path: path.clone() }));
    assert_eq!(
        send_copy(&mut h, clipboard::MAX),
        clipboard::osc52(b"red\nthe tail\n").into_bytes()
    );
    assert_eq!(
        notice(&h).as_deref(),
        Some("copied 13 B · sent to terminal clipboard"),
        "all of it: no note"
    );
    // Expanded, the whole output shows, stripped too.
    h.key(KeyCode::Enter);
    let screen = h.screen();
    let red = line_with(&screen, "red");
    assert!(!red.contains('\u{1b}') && !red.contains("[31m"), "{red}");
}

/// A run whose output was deleted on the server shows the tombstone once
/// expanded, with nothing to download; one whose output was cropped says
/// what the crop kept, and downloads the cropped blob.
#[test]
fn a_runs_deleted_or_cropped_output_says_so() {
    let shown = |change: &dyn Fn(SessionId, BlobId, DeviceId) -> Event| {
        let (mut h, id, me) = shell_session();
        let run = CommandId::new();
        let (blob_id, output) = blob(4000);
        let mut end = finished(id, at_s(1, 5), run, me, at(1), "the tail\n", Some(output));
        if let EventKind::ShellFinished { lines, .. } = &mut end.kind {
            *lines = 40;
        }
        pull(
            &mut h,
            vec![
                requested(id, 1, run, me, "make test"),
                started(id, at(1), run),
                end,
                change(id, blob_id, me),
            ],
        );
        h.key_with(ctrl('t'));
        h.key(KeyCode::Tab);
        h.key(KeyCode::Char('k'));
        let download = h.key(KeyCode::Enter);
        (h.screen(), download, h)
    };

    let (screen, download, mut h) = shown(&|id, blob, me| blob_deleted(id, 3, blob, me));
    assert_contains(&screen, "… 39 earlier lines · Deleted by you");
    assert_eq!(download, None, "nothing to download");
    // Copied, it is the kept tail, and the notice says that is all.
    assert_eq!(h.key(KeyCode::Char('y')), None);
    assert_eq!(clip(&h), Some(Clip::Text("the tail\n".into())));
    send_copy(&mut h, clipboard::MAX);
    assert_eq!(
        notice(&h).as_deref(),
        Some("copied 9 B · sent to terminal clipboard · only the kept tail · Deleted by you")
    );

    let (into_id, into) = blob(300);
    let (screen, download, mut h) =
        shown(&|id, blob, me| blob_cropped(id, 3, blob, into.clone(), me));
    assert_contains(
        &screen,
        "… 1 earlier lines · downloading 300 B · Cropped by you: kept 2 of 40 lines",
    );
    assert_eq!(download, Some(Action::DownloadBlob(into_id)));
    // Downloaded, the two kept lines are all of it: none are earlier.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("blob");
    std::fs::write(&path, b"kept one\nkept two\n").unwrap();
    h.seam
        .blob_status(into_id, BlobStatus::Available { path })
        .unwrap();
    h.sync();
    let screen = h.screen();
    assert_contains(&screen, "kept two");
    assert!(!screen.contains("earlier lines"), "{screen}");
}

#[test]
fn a_long_expanded_output_pages_to_its_end_and_back() {
    let (mut h, id, me) = shell_session();
    let run = CommandId::new();
    let (blob_id, output) = blob(8000);
    // More lines than the timeline's expanded cap (400), coloured.
    let raw: String = (1..=600)
        .map(|n| format!("\x1b[32mout {n:03}\x1b[0m\n"))
        .collect();
    let mut end = finished(
        id,
        at_s(1, 5),
        run,
        me,
        at(1),
        "out 599\nout 600\n",
        Some(output),
    );
    if let EventKind::ShellFinished { lines, .. } = &mut end.kind {
        *lines = 600;
    }
    pull(
        &mut h,
        vec![
            requested(id, 1, run, me, "make test"),
            started(id, at(1), run),
            end,
        ],
    );
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("blob");
    std::fs::write(&path, raw).unwrap();
    h.seam
        .blob_status(blob_id, BlobStatus::Available { path })
        .unwrap();
    h.sync();
    h.key_with(ctrl('t'));
    h.key(KeyCode::Tab);
    h.key(KeyCode::Char('k'));
    assert_contains(&footer(&h), "PgUp/PgDn");
    h.key(KeyCode::Enter);

    // Expanded, it shows from its top: every line, the first included.
    let screen = h.screen();
    assert_contains(&screen, "$ make test");
    assert_contains(&screen, "out 001");
    assert!(!screen.contains("out 600"), "{screen}");
    assert!(!screen.contains("[32m"), "{screen}");

    // PgDn pages down to its last line and stops there.
    let mut pages = 0;
    while !h.screen().contains("out 600") {
        h.key(KeyCode::PageDown);
        pages += 1;
        assert!(pages < 100, "never reached the end:\n{}", h.screen());
    }
    assert!(pages > 1, "a long output takes more than a page");
    let last = h.screen();
    assert!(!last.contains("$ make test"), "{last}");
    h.key(KeyCode::PageDown);
    assert_eq!(h.screen(), last, "no further than its end");

    // And PgUp as far back up, to the command.
    for _ in 0..pages {
        h.key(KeyCode::PageUp);
    }
    assert_contains(&h.screen(), "$ make test");
    assert_contains(&h.screen(), "out 001");
}

#[test]
fn the_clock_ticks_while_a_run_runs() {
    let (mut h, id, me) = shell_session();
    let run = CommandId::new();
    h.key_with(ctrl('t'));
    pull(&mut h, vec![requested(id, 1, run, me, "sleep 60")]);
    assert!(!h.app.live(), "queued: no clock");
    pull(&mut h, vec![started(id, at(1), run)]);
    assert!(h.app.live());
    let running = |h: &Harness, s: u32| line_with(&h.screen_at(at_s(1, s)), "running");
    assert_contains(
        &running(&h, 5),
        &format!("running {}", took_label(Duration::from_secs(5))),
    );
    assert_contains(
        &running(&h, 8),
        &format!("running {}", took_label(Duration::from_secs(8))),
    );
    pull(
        &mut h,
        vec![finished(id, at_s(1, 9), run, me, at(1), "", None)],
    );
    assert!(!h.app.live(), "over: the clock stops");
}

#[test]
fn a_long_run_alerts_when_it_finishes_out_of_sight() {
    let (mut h, id, me) = shell_session();
    let run = CommandId::new();
    pull(
        &mut h,
        vec![
            requested(id, 1, run, me, "make test"),
            started(id, at(1), run),
            finished(id, at(3), run, me, at(1), "", None),
        ],
    );
    let notify = skywave_tui::Notify::parse("osc9").unwrap();
    let mut out = Vec::new();
    skywave_tui::alert(&mut h.app, &mut out, notify, false);
    let sent = String::from_utf8(out).unwrap();
    assert_contains(&sent, "skywave: make test · exit 1 · ");
    assert_contains(&h.app.ui().notice.clone().unwrap(), "make test · exit 1");
    // Once.
    let mut out = Vec::new();
    skywave_tui::alert(&mut h.app, &mut out, notify, false);
    assert!(out.is_empty());
}

// ---- workdir ----------------------------------------------------------------

/// The one workdir read the core asked the engine for since the last drain.
fn workdir_asked(h: &Harness) -> (u64, WorkdirAsk) {
    let asks: Vec<(u64, WorkdirAsk)> = h
        .drain_requests()
        .into_iter()
        .filter_map(|request| match request {
            Request::Workdir { req, ask } => Some((req, ask)),
            _ => None,
        })
        .collect();
    assert_eq!(asks.len(), 1, "{asks:?}");
    asks.into_iter().next().unwrap()
}

/// The session open on its timeline, then `w`: on its changes, asked for.
fn open_workdir() -> (Harness, SessionId, u64, WorkdirAsk) {
    let (mut h, id, _) = open_session();
    // In the composer, `w` is text.
    assert_eq!(h.key(KeyCode::Char('w')), None);
    assert!(matches!(h.app.view().screen, Screen::Session(_)));
    h.key(KeyCode::Backspace);
    h.key(KeyCode::Esc);
    h.drain_requests();
    assert_eq!(
        h.key(KeyCode::Char('w')),
        Some(Action::OpenWorkdir { session_id: id })
    );
    assert!(matches!(h.app.view().screen, Screen::Workdir(_)));
    let (req, ask) = workdir_asked(&h);
    assert_eq!(ask, WorkdirAsk::Changes { session: id });
    (h, id, req, ask)
}

/// A modified file with its diff in `diff`, and an untracked one.
fn change_set(diff: BlobRef) -> ChangeSet {
    ChangeSet {
        at: at(5),
        branch: Some("main".into()),
        head: Some("1a2b3c4".into()),
        files: vec![
            ChangedFile {
                path: "src/main.rs".into(),
                from: None,
                change: FileChange::Modified,
                added: Some(3),
                removed: Some(1),
                diff: DiffBody::Blob(diff),
            },
            ChangedFile {
                path: "notes.txt".into(),
                from: None,
                change: FileChange::Untracked,
                added: Some(2),
                removed: Some(0),
                diff: DiffBody::Omitted,
            },
        ],
        total: 2,
    }
}

fn answered(h: &mut Harness, req: u64, ask: WorkdirAsk, answer: WorkdirAnswer) {
    h.seam.workdir_answered(req, ask, Ok(answer)).unwrap();
    h.sync();
}

/// Writes `content` as blob `id`'s file and reports it downloaded.
fn arrived(h: &mut Harness, id: BlobId, content: &str) {
    let path = h.dir.path().join(format!("arrived-{id:?}"));
    std::fs::write(&path, content).unwrap();
    h.seam
        .blob_status(id, BlobStatus::Available { path })
        .unwrap();
    h.sync();
}

#[test]
fn workdir_w_opens_the_changes_and_renders_them() {
    let (mut h, _, req, ask) = open_workdir();
    let screen = h.screen();
    assert_contains(&screen, " Changes  Files ");
    assert_contains(&screen, "Loading…");
    assert!(!screen.contains("Not fetched yet"), "{screen}");

    let (_, diff_ref) = blob(300);
    answered(
        &mut h,
        req,
        ask,
        WorkdirAnswer::Changes(ChangesResult::Changes(change_set(diff_ref))),
    );
    let screen = h.screen();
    assert_contains(&screen, "main @ 1a2b3c4 · taken 12:05:00");
    assert!(!screen.contains("Loading"), "{screen}");
    assert_contains(&line_with(&screen, "src/main.rs"), "> M src/main.rs  +3 −1");
    assert_contains(&screen, "  ? notes.txt  +2 −0");
    assert_contains(&screen, "Enter open · Tab changes/files");
}

#[test]
fn workdir_not_git_says_so() {
    let (mut h, _, req, ask) = open_workdir();
    answered(
        &mut h,
        req,
        ask,
        WorkdirAnswer::Changes(ChangesResult::NotGit),
    );
    assert_contains(
        &h.screen(),
        "Not a git repository. Browse the files under Files.",
    );
    // Nothing to open.
    assert_eq!(h.key(KeyCode::Enter), None);
}

#[test]
fn workdir_enter_shows_a_diff_coloured_by_line_kind() {
    let (mut h, id, req, ask) = open_workdir();
    let (diff, diff_ref) = blob(300);
    answered(
        &mut h,
        req,
        ask,
        WorkdirAnswer::Changes(ChangesResult::Changes(change_set(diff_ref))),
    );
    assert_eq!(
        h.key(KeyCode::Enter),
        Some(Action::ShowWorkdir(WorkdirPane::Diff {
            path: "src/main.rs".into(),
            untracked: false,
        }))
    );
    // Not here yet: where it stands, as the timeline words it.
    assert_contains(&h.screen(), "src/main.rs  +3 −1");
    assert!(
        h.screen().contains("requested") || h.screen().contains("downloading"),
        "{}",
        h.screen()
    );

    arrived(
        &mut h,
        diff,
        "diff --git a/src/main.rs b/src/main.rs\n\
         --- a/src/main.rs\n\
         +++ b/src/main.rs\n\
         @@ -1,2 +1,4 @@\n\
         \x20fn main() {\n\
         -    old();\n\
         ---- dashes removed\n\
         +    new();\n\
         +\x1b[31mevil\n",
    );
    let frame = h.frame(100, 24, mockup_now());
    let screen = text(&frame);
    assert_contains(&screen, "@@ -1,2 +1,4 @@");
    assert_eq!(colour_of(&frame, "+    new();"), Color::Green);
    assert_eq!(colour_of(&frame, "-    old();"), Color::Red);
    assert_eq!(colour_of(&frame, "@@ -1,2"), Color::Cyan);
    assert_eq!(colour_of(&frame, " fn main() {"), Color::Reset);
    // Inside a hunk a line is what its sign says, even `---…`.
    assert_eq!(colour_of(&frame, "---- dashes removed"), Color::Red);
    // Server text never reaches the terminal raw.
    assert_contains(&screen, "+[31mevil");
    assert!(!screen.contains('\x1b'), "{screen:?}");

    // Esc: back to the changes, on the file come from.
    assert_eq!(h.key(KeyCode::Esc), Some(Action::Back));
    let screen = h.screen();
    assert_contains(&screen, "main @ 1a2b3c4");
    assert_contains(&line_with(&screen, "src/main.rs"), "> M src/main.rs");
    assert_eq!(h.key(KeyCode::Char('j')), None);
    assert_contains(&line_with(&h.screen(), "notes.txt"), "> ? notes.txt");

    // Esc again: the session.
    assert_eq!(h.key(KeyCode::Esc), Some(Action::Back));
    match &h.app.view().screen {
        Screen::Session(view) => assert_eq!(view.id, id),
        other => panic!("expected the session, got {other:?}"),
    }
}

#[test]
fn workdir_tab_to_files_and_into_a_directory_and_a_file() {
    let (mut h, id, req, ask) = open_workdir();
    answered(
        &mut h,
        req,
        ask,
        WorkdirAnswer::Changes(ChangesResult::NotGit),
    );

    assert_eq!(
        h.key(KeyCode::Tab),
        Some(Action::ShowWorkdir(WorkdirPane::Dir {
            path: String::new()
        }))
    );
    let (req, ask) = workdir_asked(&h);
    assert_eq!(
        ask,
        WorkdirAsk::List {
            session: id,
            path: String::new()
        }
    );
    let entry = |name: &str, kind, size| DirEntry {
        name: name.into(),
        kind,
        size,
    };
    answered(
        &mut h,
        req,
        ask,
        WorkdirAnswer::List(ListResult::Listed {
            at: at(6),
            path: String::new(),
            entries: vec![
                entry("b.txt", EntryKind::File, Some(812)),
                entry("src", EntryKind::Dir, None),
                entry(
                    "docs",
                    EntryKind::Link {
                        to: LinkTarget::Dir,
                    },
                    None,
                ),
            ],
            truncated: false,
        }),
    );
    let screen = h.screen();
    assert_contains(&screen, "./ · taken 12:06:00");
    assert!(
        !screen.contains("../"),
        "no way up from the workdir:\n{screen}"
    );
    // Directories first, a link to one among them.
    let docs = screen.find("docs  → directory").expect(&screen);
    let src = screen.find("src/").expect(&screen);
    let file = screen.find("b.txt  812 B").expect(&screen);
    assert!(docs < src && src < file, "{screen}");

    h.key(KeyCode::Char('j'));
    assert_eq!(
        h.key(KeyCode::Enter),
        Some(Action::ShowWorkdir(WorkdirPane::Dir { path: "src".into() }))
    );
    let (req, ask) = workdir_asked(&h);
    let (content, content_ref) = blob(14);
    answered(
        &mut h,
        req,
        ask,
        WorkdirAnswer::List(ListResult::Listed {
            at: at(7),
            path: "src".into(),
            entries: vec![entry("main.rs", EntryKind::File, Some(14))],
            truncated: false,
        }),
    );
    let screen = h.screen();
    assert_contains(&screen, "./src · taken 12:07:00");
    assert_contains(&line_with(&screen, "../"), "> ../");

    // Into the file: its content with line numbers.
    h.key(KeyCode::Char('j'));
    assert_eq!(
        h.key(KeyCode::Enter),
        Some(Action::ShowWorkdir(WorkdirPane::File {
            path: "src/main.rs".into()
        }))
    );
    let (req, ask) = workdir_asked(&h);
    answered(
        &mut h,
        req,
        ask,
        WorkdirAnswer::File(FileResult::File {
            at: at(8),
            path: "src/main.rs".into(),
            size: 14,
            content: FileBody::Blob(content_ref),
        }),
    );
    arrived(&mut h, content, "fn main() {}\n\tx\n");
    let screen = h.screen();
    assert_contains(&screen, "src/main.rs  14 B · taken 12:08:00");
    assert_contains(&screen, "1 │ fn main() {}");
    assert_contains(&screen, "2 │     x");

    // Esc: up to the directory, on the file; again: up to the workdir, on
    // `src/`, from what is stored (no new read); again: the session.
    assert_eq!(h.key(KeyCode::Esc), Some(Action::Back));
    assert_contains(&line_with(&h.screen(), "main.rs"), "> main.rs");
    assert_eq!(h.key(KeyCode::Esc), Some(Action::Back));
    assert_contains(&line_with(&h.screen(), "src/"), "> src/");
    assert!(
        !h.drain_requests()
            .iter()
            .any(|r| matches!(r, Request::Workdir { .. })),
        "stored reads show without asking again"
    );
    assert_eq!(h.key(KeyCode::Esc), Some(Action::Back));
    assert!(matches!(h.app.view().screen, Screen::Session(_)));
}

/// A tool result whose whole output is `output`, `lines` lines long.
fn output_result(lines: u32, output: BlobRef) -> EventKind {
    EventKind::ToolResult(ToolResult {
        call: None,
        is_error: false,
        status: None,
        preview: "line 001".into(),
        lines,
        output: Some(Full::Blob(output)),
    })
}

fn outbox_len(h: &Harness) -> usize {
    h.seam.pending_outbox().unwrap().len()
}

/// `D` on a call's output asks first: `Esc` (any key but `y`) sends
/// nothing, `y` sends the delete of the blob the item shows. While it is
/// pending the item says so, `D` says how it stands, and `Del` takes it
/// back. `K` opens the crop page, which offers the download of an output
/// not here.
#[test]
fn shift_d_on_an_output_asks_and_only_y_deletes_it() {
    let (mut h, id, _) = open_session();
    let (blob_id, output) = blob(10_000);
    pull(&mut h, vec![event(id, 1, output_result(40, output))]);
    h.key(KeyCode::Esc);
    h.key(KeyCode::Up);

    assert_eq!(
        h.key(KeyCode::Char('K')),
        Some(Action::OpenCrop {
            blob: blob_id,
            session: id,
            holds: BlobUse::ToolOutput,
            title: "line 001".into(),
        })
    );
    let screen = h.screen();
    assert_contains(
        &screen,
        "This output isn't on this device. Download 9.8 KiB to choose lines.",
    );
    assert_contains(&screen, "not downloaded · 9.8 KiB · press d to download");
    assert_contains(&footer(&h), "d download");
    assert_eq!(
        h.key(KeyCode::Char('d')),
        Some(Action::DownloadBlob(blob_id))
    );
    assert_eq!(h.key(KeyCode::Esc), Some(Action::Back));
    assert!(matches!(h.app.view().screen, Screen::Session(_)));
    // Back on the session, it opens on the composer, as from any screen.
    h.key(KeyCode::Esc);
    h.key(KeyCode::Up);

    let before = outbox_len(&h);
    assert_eq!(h.key(KeyCode::Char('D')), None);
    assert_eq!(
        footer(&h),
        "Delete this tool output from the server and from every device? This can't be undone. y/N"
    );
    assert_eq!(h.key(KeyCode::Esc), None, "Esc answers no");
    assert!(matches!(h.app.view().screen, Screen::Session(_)));
    assert!(!footer(&h).contains("y/N"), "{}", footer(&h));
    assert_eq!(outbox_len(&h), before, "nothing sent");

    h.key(KeyCode::Char('D'));
    assert_eq!(h.key(KeyCode::Char('y')), Some(Action::DeleteBlob(blob_id)));
    assert_eq!(outbox_len(&h), before + 1);
    let delete = last_command(&h);
    assert_contains(&h.screen(), "Delete queued");
    // Pending: nothing more is offered; the keys say how it stands.
    assert_eq!(h.key(KeyCode::Char('D')), None);
    assert_eq!(h.app.ui().notice.as_deref(), Some("Delete queued"));
    assert!(h.app.ui().confirm.is_none());
    assert_eq!(h.key(KeyCode::Char('K')), None);
    assert_eq!(h.key(KeyCode::Delete), Some(Action::Discard(delete)));
    assert!(!h.screen().contains("Delete queued"));
}

/// The keys beside `D` and `K` still do what they did.
#[test]
fn x_still_denies_and_shift_c_still_opens_contexts() {
    let (mut h, id, _) = open_session();
    let approval = ApprovalId::new();
    pull(
        &mut h,
        vec![event(
            id,
            1,
            EventKind::ApprovalRequested {
                id: approval,
                call: None,
                tool: "Bash".into(),
                summary: "rm -rf target".into(),
                input: None,
                suggestions: Vec::new(),
                shell: None,
            },
        )],
    );
    h.key(KeyCode::Esc);
    // An approval has no output: `D` asks nothing.
    assert_eq!(h.key(KeyCode::Char('D')), None);
    assert!(h.app.ui().confirm.is_none());
    assert_eq!(
        h.key(KeyCode::Char('x')),
        Some(Action::ResolveApproval {
            id: approval,
            decision: Decision::Deny,
            rule: None,
        })
    );
    assert_eq!(h.key(KeyCode::Char('C')), None);
    assert!(matches!(
        h.app.ui().session.as_ref().unwrap().picker,
        Some(skywave_tui::app::Picker::Contexts { .. })
    ));
}

/// The id of the line count the crop page asked for.
fn count_request(h: &Harness) -> u64 {
    match h.drain_requests().as_slice() {
        [Request::CountLines { req, .. }] => *req,
        other => panic!("expected one line count, got {other:?}"),
    }
}

fn crop_keep(h: &Harness) -> Vec<LineSpan> {
    match &h.app.view().screen {
        Screen::Crop(view) => view.keep.clone(),
        other => panic!("not on the crop page: {other:?}"),
    }
}

fn span(first: u32, count: u32) -> LineSpan {
    LineSpan { first, count }
}

/// On the crop page a number key keeps a preset and `v` … `v` toggles a
/// range in or out; `Enter` asks, `Esc` sends nothing, and `y` crops to
/// the selection.
#[test]
fn the_crop_page_keeps_a_preset_or_a_range_and_enter_crops_to_it() {
    let (mut h, id, _) = open_session();
    let (blob_id, output) = blob(3000);
    pull(&mut h, vec![event(id, 1, output_result(300, output))]);
    let content: String = (1..=300).map(|n| format!("line {n:03}\n")).collect();
    let dir = tempfile::tempdir().unwrap();
    downloaded(&mut h, &dir, "out", blob_id, content.as_bytes());
    h.key(KeyCode::Esc);
    h.key(KeyCode::Up);
    h.drain_requests();
    assert!(matches!(
        h.key(KeyCode::Char('K')),
        Some(Action::OpenCrop { .. })
    ));
    assert_contains(&h.screen(), "Counting its lines");
    let req = count_request(&h);
    h.seam.lines_counted(req, Ok(300));
    h.sync();
    let screen = h.screen();
    assert_contains(&screen, "keeps 0 of 300 lines");
    assert_contains(
        &screen,
        "1 First 50 · 2 Last 50 · 3 First and last 50 · 4 First 200 · 5 Last 200",
    );
    assert_contains(&screen, " >  1 │ line 001");
    assert_contains(&screen, "    2 │ line 002");

    assert_eq!(
        h.key(KeyCode::Char('2')),
        Some(Action::SetCropKeep(vec![span(250, 50)]))
    );
    assert_eq!(crop_keep(&h), [span(250, 50)]);
    h.key(KeyCode::Char('1'));
    assert_eq!(crop_keep(&h), [span(0, 50)]);
    let screen = h.screen();
    assert_contains(&screen, "keeps 50 of 300 lines");
    assert_contains(&screen, "+>  1 │ line 001");
    assert_contains(&screen, "+   2 │ line 002");

    // Lines 2 to 3, all kept: taken out.
    h.key(KeyCode::Char('j'));
    assert_eq!(h.key(KeyCode::Char('v')), None);
    h.key(KeyCode::Char('j'));
    assert_contains(&footer(&h), "v keep or drop the range");
    assert_eq!(
        h.key(KeyCode::Char('v')),
        Some(Action::SetCropKeep(vec![span(0, 1), span(3, 47)]))
    );
    assert_contains(&h.screen(), "    2 │ line 002");
    // Lines 3 to 5, partly kept: put back in.
    h.key(KeyCode::Char('v'));
    h.key(KeyCode::Char('j'));
    h.key(KeyCode::Char('j'));
    h.key(KeyCode::Char('v'));
    assert_eq!(crop_keep(&h), [span(0, 1), span(2, 48)]);
    // Esc closes an open range before it leaves.
    h.key(KeyCode::Char('v'));
    assert_eq!(h.key(KeyCode::Esc), None);
    assert!(matches!(h.app.view().screen, Screen::Crop(_)));

    let before = outbox_len(&h);
    assert_eq!(h.key(KeyCode::Enter), None);
    assert_eq!(
        footer(&h),
        "Keep 49 of 300 lines? Every device keeps only these. This can't be undone. y/N"
    );
    assert_eq!(h.key(KeyCode::Esc), None);
    assert_eq!(outbox_len(&h), before, "nothing sent");
    assert!(matches!(h.app.view().screen, Screen::Crop(_)));
    h.key(KeyCode::Enter);
    assert_eq!(
        h.key(KeyCode::Char('y')),
        Some(Action::CropBlob {
            blob: blob_id,
            keep: vec![span(0, 1), span(2, 48)],
        })
    );
    // Queued, the page is left for the session it was opened on, whose
    // item says the crop is on its way.
    assert!(matches!(h.app.view().screen, Screen::Session(_)));
    assert_eq!(outbox_len(&h), before + 1);
    h.key(KeyCode::Esc);
    assert_contains(&h.screen(), "Crop queued");
}

/// The storage screen's rows: `j`/`k` move the cursor; `D` deletes the
/// selected row's blob, a shell run's output blob, after asking; `K` crops
/// only an output whose session this device has.
#[test]
fn storage_rows_move_with_j_k_and_act_on_their_blob() {
    let mut h = Harness::paired();
    let (here, gone) = (SessionId::new(), SessionId::new());
    pull(&mut h, vec![created(here, 0, "/srv/a")]);
    h.key(KeyCode::Char('S'));
    let req = match h.drain_requests().as_slice() {
        [Request::Storage { req }] => *req,
        other => panic!("expected one storage request, got {other:?}"),
    };
    let (mine, theirs, run_output) = (BlobId::new(), BlobId::new(), BlobId::new());
    let stored = |id, size, session, holds| skywave_proto::BlobStorage {
        id,
        size,
        session,
        seq: Seq(7),
        holds,
    };
    let run = |command_line: &str, output_blob| skywave_proto::ShellRunStorage {
        run: CommandId::new(),
        session: here,
        command_line: command_line.into(),
        event_bytes: 100,
        output: 2048,
        output_blob,
    };
    let report = skywave_proto::StorageReport {
        database: 0,
        event_blobs: 0,
        workdir_blobs: 0,
        shell_running: 0,
        log_file: None,
        free: None,
        min_free: 0,
        sessions: vec![skywave_proto::SessionStorage {
            session: here,
            events: 3,
            event_bytes: 100,
            blobs: 1,
            blob_bytes: 3 * 1024 * 1024,
        }],
        blobs: vec![
            stored(mine, 3 * 1024 * 1024, here, BlobUse::ToolOutput),
            stored(theirs, 2 * 1024 * 1024, gone, BlobUse::ToolOutput),
        ],
        shell_runs: vec![run("make all", Some(run_output)), run("true", None)],
    };
    h.seam.storage_reported(req, Ok(report));
    h.sync();
    h.key(KeyCode::Tab);
    assert_contains(&line_with(&h.screen(), "> "), "3.0 MiB  a");
    h.key(KeyCode::Char('j'));
    assert_contains(&line_with(&h.screen(), "> "), "(not on this device)");
    h.key(KeyCode::Char('k'));
    assert_contains(&line_with(&h.screen(), "> "), "3.0 MiB  a");
    h.key(KeyCode::Char('j'));

    // Not on this device: nothing to crop it from.
    assert_eq!(h.key(KeyCode::Char('K')), None);
    let before = outbox_len(&h);
    assert_eq!(h.key(KeyCode::Char('D')), None);
    assert_contains(&footer(&h), "Delete this tool output from the server");
    assert_eq!(h.key(KeyCode::Esc), None);
    assert!(matches!(h.app.view().screen, Screen::Storage(_)));
    assert_eq!(outbox_len(&h), before, "nothing sent");
    h.key(KeyCode::Char('D'));
    assert_eq!(h.key(KeyCode::Char('y')), Some(Action::DeleteBlob(theirs)));
    assert_contains(&line_with(&h.screen(), "> "), "Delete queued");

    // A run acts on its output blob; one kept as no blob offers nothing.
    h.key(KeyCode::Tab);
    assert_contains(&line_with(&h.screen(), "> "), "$ make all");
    h.key(KeyCode::Char('D'));
    assert_contains(&footer(&h), "Delete this shell output from the server");
    assert_eq!(
        h.key(KeyCode::Char('y')),
        Some(Action::DeleteBlob(run_output))
    );
    h.key(KeyCode::Char('j'));
    assert_contains(&line_with(&h.screen(), "> "), "$ true");
    assert_eq!(h.key(KeyCode::Char('D')), None);
    assert!(h.app.ui().confirm.is_none());

    // The session's own output crops, from its row.
    h.key(KeyCode::Tab);
    h.key(KeyCode::Tab);
    assert_eq!(
        h.key(KeyCode::Char('K')),
        Some(Action::OpenCrop {
            blob: mine,
            session: here,
            holds: BlobUse::ToolOutput,
            title: "a · tool output".into(),
        })
    );
}
