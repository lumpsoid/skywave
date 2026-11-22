//! The screen runner over a fake adb: the runner and the Android backend run as
//! the daemon runs them, against `tests/support/fake-adb.sh` in the test's own
//! directory. Requests go through `EventLog::handle` as the HTTPS API would
//! send them, and every wait is driven by the log's notifier with a bound,
//! never by a sleep.
#![cfg(unix)]

use std::io::{Cursor, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use skywave_proto::{
    BlobId, BlobRef, CaptureFormat, Command, CommandId, CommandKind, CommandResult, DeviceId,
    Event, EventKind, RejectReason, ScreenAction, ScreenImage, ScreenOutcome, ScreenShot, Seq,
    SessionId,
};
use skywaved::screen::{DISABLED_REASON, KEEP_CAPTURES, ScreenConfig, ScreenHandle, ScreenRunner};
use skywaved::{EventLog, Storage};
use tempfile::TempDir;

/// Upper bound for any wait; a passing test never gets near it.
const WAIT: Duration = Duration::from_secs(20);

const FAKE_ADB: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/fake-adb.sh");
/// The serial and model the fake adb reports.
const SERIAL: &str = "emulator-5554";
/// The screen at first, and the screen an input leaves behind (`next.png`).
const SCREEN: [u8; 3] = [30, 60, 120];
const NEXT: [u8; 3] = [200, 40, 40];

struct Fixture {
    _tmp: TempDir,
    data: PathBuf,
    root: PathBuf,
    /// The fake adb's directory: the `adb` script, its screens and flags, `calls.log`.
    adb_dir: PathBuf,
    storage: Storage,
    log: EventLog,
    /// The first session, working in `root/project`.
    session: SessionId,
}

/// A solid 1080x2400 screen of `colour`, as a PNG.
fn screen_png(colour: [u8; 3]) -> Vec<u8> {
    let [r, g, b] = colour;
    let screen = image::RgbaImage::from_pixel(1080, 2400, image::Rgba([r, g, b, 255]));
    let mut png = Cursor::new(Vec::new());
    screen.write_to(&mut png, image::ImageFormat::Png).unwrap();
    png.into_inner()
}

impl Fixture {
    fn new() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let data = tmp.path().join("data");
        let adb_dir = tmp.path().join("adb");
        std::fs::create_dir_all(&adb_dir).unwrap();
        let adb = adb_dir.join("adb");
        std::fs::copy(FAKE_ADB, &adb).unwrap();
        std::fs::set_permissions(&adb, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(adb_dir.join("screen.png"), screen_png(SCREEN)).unwrap();
        let storage = Storage::open(&data).unwrap();
        let log = EventLog::open(&storage.db, vec![root.clone()])
            .unwrap()
            .with_screen(true);
        let mut f = Fixture {
            _tmp: tmp,
            data,
            root,
            adb_dir,
            storage,
            log,
            session: SessionId::new(),
        };
        f.session = f.create_session("project");
        f
    }

    fn create_session(&mut self, name: &str) -> SessionId {
        let workdir = self.root.join(name);
        std::fs::create_dir_all(&workdir).unwrap();
        let seq = accepted(self.handle(
            DeviceId::new(),
            CommandId::new(),
            CommandKind::CreateSession {
                workdir: workdir.to_str().unwrap().to_owned(),
                conversation: None,
                worktree: None,
            },
        ));
        self.log
            .read(&self.storage.db, Seq(seq.0 - 1), 1)
            .unwrap()
            .events[0]
            .session_id
    }

    /// A runner config for tests: enabled, with the fake adb.
    fn config(&self) -> ScreenConfig {
        ScreenConfig {
            enabled: true,
            adb: self.adb_dir.join("adb"),
        }
    }

    async fn start(&self) -> ScreenHandle {
        self.start_with(self.config()).await
    }

    async fn start_with(&self, config: ScreenConfig) -> ScreenHandle {
        ScreenRunner::start(&self.data, self.log.clone(), config)
            .await
            .unwrap()
    }

    fn handle(&mut self, device: DeviceId, id: CommandId, kind: CommandKind) -> CommandResult {
        self.log
            .handle(&mut self.storage.db, device, &Command { id, kind })
            .unwrap()
    }

    /// Sends `action` in the first session, as device `DeviceId::new()`, and
    /// returns its command id with the result.
    fn screen(&mut self, action: ScreenAction) -> (CommandId, CommandResult) {
        let id = CommandId::new();
        let kind = CommandKind::Screen {
            session_id: self.session,
            action,
        };
        (id, self.handle(DeviceId::new(), id, kind))
    }

    /// Sends `action`, which must be accepted; returns its command id.
    fn request(&mut self, action: ScreenAction) -> CommandId {
        let (id, result) = self.screen(action);
        accepted(result);
        id
    }

    fn events(&self) -> Vec<Event> {
        self.log
            .read(&self.storage.db, Seq::ZERO, 5000)
            .unwrap()
            .events
    }

    /// Waits until the log satisfies `ready`.
    async fn wait_for(&self, what: &str, ready: impl Fn(&[Event]) -> bool) {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let head = self.log.head();
            let events = self.events();
            if ready(&events) {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}; log: {events:#?}"
            );
            self.log.wait_after(head, WAIT).await;
        }
    }

    /// Waits for `request`'s `ScreenDone` and returns its outcome and the
    /// capture blobs it pruned.
    async fn settled(&self, request: CommandId) -> (ScreenOutcome, Vec<BlobId>) {
        self.wait_for("ScreenDone", |events| {
            events.iter().any(|e| is_done(e, request))
        })
        .await;
        self.events()
            .into_iter()
            .find_map(|event| match event.kind {
                EventKind::ScreenDone {
                    request: done,
                    outcome,
                    pruned,
                    ..
                } if done == request => Some((outcome, pruned)),
                _ => None,
            })
            .unwrap()
    }

    async fn done(&self, request: CommandId) -> ScreenOutcome {
        self.settled(request).await.0
    }

    fn blob(&self, blob: &BlobRef) -> Vec<u8> {
        let mut content = Vec::new();
        self.storage
            .open_blob(blob.id)
            .unwrap()
            .expect("the blob exists")
            .read_to_end(&mut content)
            .unwrap();
        assert_eq!(
            blob.sha256,
            skywave_proto::ContentHash::of(&content),
            "the reference carries the stored content's hash"
        );
        content
    }

    /// Writes the fake adb's screen `name` (`screen.png` or `next.png`).
    fn write_png(&self, name: &str, colour: [u8; 3]) {
        std::fs::write(self.adb_dir.join(name), screen_png(colour)).unwrap();
    }

    /// Creates the fake adb's flag file `name`.
    fn touch(&self, name: &str) {
        std::fs::write(self.adb_dir.join(name), b"").unwrap();
    }

    /// Removes the fake adb's flag file `name`.
    fn remove(&self, name: &str) {
        std::fs::remove_file(self.adb_dir.join(name)).unwrap();
    }

    /// The lines the fake adb has been called with, in order.
    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.adb_dir.join("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            let violations = self.storage.db.context_stamp_violations().unwrap();
            assert!(violations.is_empty(), "context stamps: {violations:#?}");
        }
    }
}

fn accepted(result: CommandResult) -> Seq {
    match result {
        CommandResult::Accepted { seq } => seq,
        other => panic!("expected Accepted, got {other:?}"),
    }
}

fn rejected(result: CommandResult) -> RejectReason {
    match result {
        CommandResult::Rejected { reason } => reason,
        other => panic!("expected Rejected, got {other:?}"),
    }
}

fn is_done(event: &Event, request: CommandId) -> bool {
    matches!(&event.kind, EventKind::ScreenDone { request: r, .. } if *r == request)
}

/// The new image a `Done` outcome shows.
fn new_image(outcome: ScreenOutcome) -> ScreenImage {
    match outcome {
        ScreenOutcome::Done {
            shot: ScreenShot::Image(image),
        } => image,
        other => panic!("expected a new image, got {other:?}"),
    }
}

#[tokio::test]
async fn devices_lists_what_the_backend_reports() {
    let mut f = Fixture::new();
    let runner = f.start().await;
    let request = f.request(ScreenAction::Devices);
    match f.done(request).await {
        ScreenOutcome::Devices { devices } => {
            assert_eq!(devices.len(), 1, "{devices:?}");
            assert_eq!(devices[0].serial, SERIAL);
            assert_eq!(devices[0].state, "device");
            assert_eq!(devices[0].model.as_deref(), Some("Pixel_7"));
        }
        other => panic!("expected the devices, got {other:?}"),
    }
    runner.shutdown().await.unwrap();
}

#[tokio::test]
async fn capture_logs_a_scaled_jpeg() {
    let mut f = Fixture::new();
    let runner = f.start().await;
    let request = f.request(ScreenAction::Capture {
        device: SERIAL.into(),
        full: false,
    });
    let image = new_image(f.done(request).await);
    assert_eq!(image.format, CaptureFormat::Jpeg);
    assert_eq!((image.width, image.height), (576, 1280));
    assert_eq!((image.screen_width, image.screen_height), (1080, 2400));
    let bytes = f.blob(&image.image);
    let decoded = image::load_from_memory_with_format(&bytes, image::ImageFormat::Jpeg).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (576, 1280));
    runner.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_tap_is_scaled_to_the_screen_and_shot_again() {
    let mut f = Fixture::new();
    let runner = f.start().await;
    let capture = f.request(ScreenAction::Capture {
        device: SERIAL.into(),
        full: false,
    });
    let before = new_image(f.done(capture).await);
    f.write_png("next.png", NEXT);
    let tap = f.request(ScreenAction::Tap {
        device: SERIAL.into(),
        aim: capture,
        x: 288,
        y: 640,
    });
    let after = new_image(f.done(tap).await);
    assert_ne!(after.pixels, before.pixels, "the tap shows the new screen");
    assert!(
        f.calls()
            .iter()
            .any(|line| line.ends_with("shell input tap 540 1200")),
        "{:?}",
        f.calls()
    );
    runner.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_unchanged_screen_keeps_the_capture() {
    let mut f = Fixture::new();
    let runner = f.start().await;
    let capture = f.request(ScreenAction::Capture {
        device: SERIAL.into(),
        full: false,
    });
    new_image(f.done(capture).await);
    let tap = f.request(ScreenAction::Tap {
        device: SERIAL.into(),
        aim: capture,
        x: 288,
        y: 640,
    });
    match f.done(tap).await {
        ScreenOutcome::Done {
            shot: ScreenShot::Unchanged { capture: kept },
        } => assert_eq!(kept, capture),
        other => panic!("expected an unchanged screen, got {other:?}"),
    }
    // The aim stays valid, so a second tap at the same capture is accepted.
    let again = f.request(ScreenAction::Tap {
        device: SERIAL.into(),
        aim: capture,
        x: 288,
        y: 640,
    });
    match f.done(again).await {
        ScreenOutcome::Done {
            shot: ScreenShot::Unchanged { capture: kept },
        } => assert_eq!(kept, capture),
        other => panic!("expected an unchanged screen, got {other:?}"),
    }
    runner.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_input_aimed_at_an_old_capture_is_refused() {
    let mut f = Fixture::new();
    let runner = f.start().await;
    let first = f.request(ScreenAction::Capture {
        device: SERIAL.into(),
        full: false,
    });
    new_image(f.done(first).await);
    f.write_png("next.png", NEXT);
    let tap = f.request(ScreenAction::Tap {
        device: SERIAL.into(),
        aim: first,
        x: 288,
        y: 640,
    });
    new_image(f.done(tap).await);
    let (_, result) = f.screen(ScreenAction::Tap {
        device: SERIAL.into(),
        aim: first,
        x: 288,
        y: 640,
    });
    assert_eq!(rejected(result), RejectReason::ScreenStale);
    runner.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_queued_input_gone_stale_is_not_run() {
    let mut f = Fixture::new();
    let runner = f.start().await;
    let capture = f.request(ScreenAction::Capture {
        device: SERIAL.into(),
        full: false,
    });
    new_image(f.done(capture).await);
    f.write_png("next.png", NEXT);
    // The fake adb holds each input while `hold` exists, so A is still in
    // flight when B is accepted: C1 is the latest capture then.
    f.touch("hold");
    let a = f.request(ScreenAction::Tap {
        device: SERIAL.into(),
        aim: capture,
        x: 288,
        y: 640,
    });
    let b = f.request(ScreenAction::Tap {
        device: SERIAL.into(),
        aim: capture,
        x: 288,
        y: 640,
    });
    f.remove("hold");
    new_image(f.done(a).await);
    match f.done(b).await {
        ScreenOutcome::Stale { latest } => assert_eq!(latest, Some(a)),
        other => panic!("expected a stale input, got {other:?}"),
    }
    let taps = f
        .calls()
        .iter()
        .filter(|line| line.contains("shell input tap"))
        .count();
    assert_eq!(taps, 1, "{:?}", f.calls());
    runner.shutdown().await.unwrap();
}

#[tokio::test]
async fn unfinished_requests_are_lost_at_start() {
    let mut f = Fixture::new();
    let request = f.request(ScreenAction::Capture {
        device: SERIAL.into(),
        full: false,
    });
    let runner = f.start().await;
    assert_eq!(f.done(request).await, ScreenOutcome::Lost);
    assert!(
        !f.calls().iter().any(|line| line.contains("screencap")),
        "{:?}",
        f.calls()
    );
    runner.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_disabled_runner_settles_requests_not_started() {
    let mut f = Fixture::new();
    let request = f.request(ScreenAction::Capture {
        device: SERIAL.into(),
        full: false,
    });
    let runner = f
        .start_with(ScreenConfig {
            enabled: false,
            ..f.config()
        })
        .await;
    runner.join().await.unwrap();
    assert_eq!(
        f.done(request).await,
        ScreenOutcome::NotStarted {
            reason: DISABLED_REASON.to_owned()
        }
    );
    assert!(f.calls().is_empty(), "{:?}", f.calls());

    // A log without `with_screen(true)` refuses screen commands outright.
    let plain = EventLog::open(&f.storage.db, vec![f.root.clone()]).unwrap();
    let command = Command {
        id: CommandId::new(),
        kind: CommandKind::Screen {
            session_id: f.session,
            action: ScreenAction::Devices,
        },
    };
    let result = plain
        .handle(&mut f.storage.db, DeviceId::new(), &command)
        .unwrap();
    assert_eq!(rejected(result), RejectReason::ScreenDisabled);
}

#[tokio::test]
async fn captures_beyond_the_keep_limit_are_pruned() {
    let mut f = Fixture::new();
    let runner = f.start().await;
    let mut blobs = Vec::new();
    let mut pruned = Vec::new();
    for _ in 0..=KEEP_CAPTURES {
        let request = f.request(ScreenAction::Capture {
            device: SERIAL.into(),
            full: true,
        });
        let (outcome, now_pruned) = f.settled(request).await;
        blobs.push(new_image(outcome).image.id);
        pruned = now_pruned;
    }
    assert_eq!(pruned, vec![blobs[0]], "the last capture prunes the first");
    assert!(
        matches!(f.storage.open_blob(blobs[0]), Ok(None) | Err(_)),
        "the first capture is gone"
    );
    assert!(
        f.storage.open_blob(blobs[1]).unwrap().is_some(),
        "the second capture is kept"
    );
    runner.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_failed_capture_fails_the_capture() {
    let mut f = Fixture::new();
    f.touch("fail-capture");
    let runner = f.start().await;
    let request = f.request(ScreenAction::Capture {
        device: SERIAL.into(),
        full: false,
    });
    match f.done(request).await {
        ScreenOutcome::Failed { reason } => {
            assert!(reason.contains("device offline"), "{reason}");
        }
        other => panic!("expected a failed capture, got {other:?}"),
    }
    f.remove("fail-capture");
    let capture = f.request(ScreenAction::Capture {
        device: SERIAL.into(),
        full: false,
    });
    new_image(f.done(capture).await);
    // With the capture failing after an input, the input still ran and its
    // shot is the failure.
    f.touch("fail-capture");
    let tap = f.request(ScreenAction::Tap {
        device: SERIAL.into(),
        aim: capture,
        x: 288,
        y: 640,
    });
    match f.done(tap).await {
        ScreenOutcome::Done {
            shot: ScreenShot::Failed { .. },
        } => {}
        other => panic!("expected a failed shot, got {other:?}"),
    }
    runner.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_failed_input_runs_no_shot() {
    let mut f = Fixture::new();
    let runner = f.start().await;
    let capture = f.request(ScreenAction::Capture {
        device: SERIAL.into(),
        full: false,
    });
    new_image(f.done(capture).await);
    f.touch("fail-input");
    let tap = f.request(ScreenAction::Tap {
        device: SERIAL.into(),
        aim: capture,
        x: 288,
        y: 640,
    });
    let outcome = f.done(tap).await;
    assert!(
        matches!(outcome, ScreenOutcome::Failed { .. }),
        "{outcome:?}"
    );
    let calls = f.calls();
    let tap_line = calls
        .iter()
        .position(|line| line.contains("shell input tap"))
        .expect("the input ran");
    assert!(
        !calls[tap_line + 1..]
            .iter()
            .any(|line| line.contains("screencap")),
        "{calls:?}"
    );
    runner.shutdown().await.unwrap();
}

#[tokio::test]
async fn bad_input_is_refused() {
    let mut f = Fixture::new();
    let runner = f.start().await;
    let capture = f.request(ScreenAction::Capture {
        device: SERIAL.into(),
        full: false,
    });
    new_image(f.done(capture).await);
    // Past the capture's 576-pixel width.
    let (_, result) = f.screen(ScreenAction::Tap {
        device: SERIAL.into(),
        aim: capture,
        x: 576,
        y: 0,
    });
    assert_eq!(rejected(result), RejectReason::BadScreenInput);
    let (_, result) = f.screen(ScreenAction::Text {
        device: SERIAL.into(),
        aim: capture,
        text: "héllo".into(),
    });
    assert_eq!(rejected(result), RejectReason::BadScreenInput);
    let (_, result) = f.screen(ScreenAction::Install {
        device: SERIAL.into(),
        apk: "../x.apk".into(),
    });
    assert_eq!(rejected(result), RejectReason::BadScreenInput);
    runner.shutdown().await.unwrap();
}

#[tokio::test]
async fn install_resolves_the_apk_in_the_workdir() {
    let mut f = Fixture::new();
    let runner = f.start().await;
    let workdir = f.root.join("project");
    std::fs::write(workdir.join("app.apk"), b"apk").unwrap();
    let request = f.request(ScreenAction::Install {
        device: SERIAL.into(),
        apk: "app.apk".into(),
    });
    let outcome = f.done(request).await;
    assert!(matches!(outcome, ScreenOutcome::Done { .. }), "{outcome:?}");
    let apk = std::fs::canonicalize(workdir.join("app.apk")).unwrap();
    let line = format!("-s {SERIAL} install -r {}", apk.display());
    assert!(f.calls().contains(&line), "{:?}", f.calls());
    runner.shutdown().await.unwrap();
}
