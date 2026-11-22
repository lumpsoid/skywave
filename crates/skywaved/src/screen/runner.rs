//! The screen runner (D65): runs the screen requests devices send (`Screen`)
//! in the daemon, through a [`ScreenBackend`]. A sibling of the shell runner
//! ([`crate::shell`]) and built the same way: its own thread with a
//! current-thread runtime and its own [`Storage`], following the log with
//! [`EventLog::subscribe`] and [`EventLog::read`].
//!
//! **Queue.** One FIFO for the whole server, in log order of
//! `ScreenRequested`, and one request at a time: the next starts once the
//! last one's `ScreenDone` is logged.
//!
//! **At most once.** No "started" event is logged. At start every request
//! with no `ScreenDone` is settled `Lost`, or `NotStarted` while the screen
//! runner is disabled, and is never run again
//! ([`EventLog::unfinished_screen_requests`]). A disabled runner settles
//! them and ends.
//!
//! **Stale re-check.** An input's aim was checked at accept, but a capture
//! may have landed since: the runner checks it again just before the input
//! runs. A stale input is `Stale { latest }` and sends nothing.
//!
//! **Shots.** Every action but `Devices` takes a shot after it. A shot whose
//! pixels equal the latest capture's is `Unchanged` and keeps that capture's
//! id. A new capture prunes the session's captures beyond the newest
//! [`KEEP_CAPTURES`]: their rows go in the `ScreenDone` transaction and their
//! files are discarded after it.
//!
//! **Disk.** While the disk is low a request is `Failed { "disk low" }` and
//! runs nothing.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use skywave_proto::{
    BlobId, CommandId, DeviceId, Event, EventKind, ScreenAction, ScreenImage, ScreenOutcome,
    ScreenShot, Seq, SessionId,
};
use tokio::sync::oneshot;

use super::android::Android;
use super::backend::{BackendError, ScreenBackend};
use super::capture::{self, Processed};
use super::{DISABLED_REASON, KEEP_CAPTURES, ScreenConfig};
use crate::error::{Error, Result};
use crate::event_log::{LatestCapture, ScreenRecovery, delete_blob_row_in, session_in};
use crate::{EventLog, Storage};

/// Events read from the log per batch.
const BATCH: u32 = 500;

/// A running screen runner. Mirrors [`ShellHandle`](crate::shell::ShellHandle).
#[derive(Debug)]
pub struct ScreenHandle {
    stop: Option<oneshot::Sender<()>>,
    done: oneshot::Receiver<Result<()>>,
    /// What the runner ended with, once [`ScreenHandle::failed`] has seen it.
    ended: Option<Result<()>>,
}

impl ScreenHandle {
    /// Stops the runner and waits until it has. A request it was running is
    /// settled `Lost` at the next start. Returns the error the runner stopped
    /// on, if it failed earlier.
    pub async fn shutdown(mut self) -> Result<()> {
        drop(self.stop.take());
        self.done().await
    }

    /// Waits until the runner ends on its own, as a disabled one does once it
    /// has settled what an earlier request left.
    pub async fn join(mut self) -> Result<()> {
        self.done().await
    }

    /// Returns once the runner has stopped on an error (or panicked); never
    /// while it runs, nor after it ended cleanly as a disabled one does.
    /// [`ScreenHandle::shutdown`] then returns the error. Cancel safe.
    pub async fn failed(&mut self) {
        if self.ended.is_none() {
            let result = Self::outcome(&mut self.done).await;
            self.ended = Some(result);
        }
        if matches!(self.ended, Some(Ok(()))) {
            std::future::pending::<()>().await;
        }
    }

    async fn done(&mut self) -> Result<()> {
        match self.ended.take() {
            Some(result) => result,
            None => Self::outcome(&mut self.done).await,
        }
    }

    async fn outcome(done: &mut oneshot::Receiver<Result<()>>) -> Result<()> {
        match done.await {
            Ok(result) => result,
            Err(_) => Err(Error::Screen("screen runner thread panicked".into())),
        }
    }
}

/// The screen runner; see the [module docs](self).
pub struct ScreenRunner<B> {
    log: EventLog,
    storage: Storage,
    config: ScreenConfig,
    backend: B,
    /// Seq of the last log event handled.
    cursor: Seq,
    /// Requests not run yet, in log order.
    queue: VecDeque<Request>,
}

/// A request not run yet: the `Screen` command's id, its session, the device
/// that asked, and what it asks.
struct Request {
    request: CommandId,
    session_id: SessionId,
    by: DeviceId,
    action: ScreenAction,
}

/// The capture blobs a request wrote and pruned.
#[derive(Default)]
struct Blobs {
    /// The capture the request stored, if it stored one.
    written: Option<BlobId>,
    /// The older captures of its session that the request prunes.
    pruned: Vec<BlobId>,
}

impl ScreenRunner<Android> {
    /// Starts the runner over the adb backend `[screen] adb` names; see
    /// [`ScreenRunner::start_with`].
    pub async fn start(
        data_dir: impl Into<PathBuf>,
        log: EventLog,
        config: ScreenConfig,
    ) -> Result<ScreenHandle> {
        let backend = Android::new(config.adb.clone());
        ScreenRunner::start_with(data_dir, log, config, backend).await
    }
}

impl<B: ScreenBackend + Send + 'static> ScreenRunner<B> {
    /// Starts the runner on its own thread (`skywave-screen`) over the data
    /// directory at `data_dir` (opened with [`Storage::open`]; the runner uses
    /// its own connection), the process's shared `log` and `backend`.
    ///
    /// Returns once what an earlier request left is settled. Enabled, the
    /// runner then follows the log until [`ScreenHandle::shutdown`]; disabled,
    /// it ends there.
    pub async fn start_with(
        data_dir: impl Into<PathBuf>,
        log: EventLog,
        config: ScreenConfig,
        backend: B,
    ) -> Result<ScreenHandle> {
        let data_dir = data_dir.into();
        let (ready_tx, ready_rx) = oneshot::channel();
        let (stop_tx, stop_rx) = oneshot::channel();
        let (done_tx, done_rx) = oneshot::channel();
        std::thread::Builder::new()
            .name("skywave-screen".into())
            .spawn(move || {
                let result = run_thread(data_dir, log, config, backend, ready_tx, stop_rx);
                if let Err(error) = &result {
                    tracing::error!(%error, "screen runner stopped");
                }
                let _ = done_tx.send(result);
            })
            .map_err(|e| Error::Screen(format!("cannot spawn thread: {e}")))?;
        match ready_rx.await {
            Ok(Ok(())) => Ok(ScreenHandle {
                stop: Some(stop_tx),
                done: done_rx,
                ended: None,
            }),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(Error::Screen("screen runner thread panicked".into())),
        }
    }
}

impl<B: ScreenBackend> ScreenRunner<B> {
    /// Settles what the previous run left; see the module docs.
    fn recover(&mut self) -> Result<()> {
        let ScreenRecovery { head, requests } =
            self.log.unfinished_screen_requests(&self.storage.db)?;
        self.cursor = head;
        for open in requests {
            let outcome = if self.config.enabled {
                ScreenOutcome::Lost
            } else {
                ScreenOutcome::NotStarted {
                    reason: DISABLED_REASON.to_owned(),
                }
            };
            let kind = EventKind::ScreenDone {
                request: open.request,
                by: open.by,
                outcome,
                pruned: Vec::new(),
            };
            self.log.screen_event(&mut self.storage.db, kind)?;
        }
        Ok(())
    }

    /// Follows the log, and runs the queued requests one at a time, until
    /// `stop` fires or an error ends it. A request a stop interrupts stays
    /// unfinished, and is settled `Lost` at the next start.
    async fn serve(&mut self, mut stop: oneshot::Receiver<()>) -> Result<()> {
        let mut head = self.log.subscribe();
        loop {
            head.borrow_and_update();
            if self.log.head() > self.cursor {
                self.follow()?;
                continue;
            }
            if let Some(request) = self.queue.pop_front() {
                let run = tokio::select! {
                    biased;
                    _ = &mut stop => return Ok(()),
                    run = self.execute(request) => run,
                };
                run?;
                continue;
            }
            tokio::select! {
                biased;
                _ = &mut stop => return Ok(()),
                _ = head.changed() => {}
            }
        }
    }

    /// Handles one batch of log events after the cursor.
    fn follow(&mut self) -> Result<()> {
        let batch = self.log.read(&self.storage.db, self.cursor, BATCH)?;
        let mut seq = batch.from;
        for event in batch.events {
            self.on_log_event(event);
            self.cursor = seq;
            seq = seq.next();
        }
        Ok(())
    }

    fn on_log_event(&mut self, event: Event) {
        let EventKind::ScreenRequested { action, by } = event.kind else {
            return;
        };
        // A `ScreenRequested` always names its command: `append_in` refuses one
        // that does not.
        let Some(request) = event.command_id else {
            return;
        };
        self.queue.push_back(Request {
            request,
            session_id: event.session_id,
            by,
            action,
        });
    }

    /// Runs `request` and logs its `ScreenDone`, then discards the capture
    /// files it pruned. A session gone meanwhile takes its rows with it, so
    /// the capture this request stored is discarded instead.
    async fn execute(&mut self, request: Request) -> Result<()> {
        let (outcome, blobs) = self.settle(&request).await?;
        let Blobs { written, pruned } = blobs;
        let kind = EventKind::ScreenDone {
            request: request.request,
            by: request.by,
            outcome,
            pruned: pruned.clone(),
        };
        if self.log.screen_event(&mut self.storage.db, kind)?.is_some() {
            for id in pruned {
                self.storage.blobs.discard(id);
            }
        } else if let Some(id) = written {
            delete_blob_row_in(self.storage.db.conn(), id)?;
            self.storage.blobs.discard(id);
        }
        Ok(())
    }

    /// What `request` came to, and the capture blobs it wrote and pruned.
    async fn settle(&mut self, request: &Request) -> Result<(ScreenOutcome, Blobs)> {
        if self.log.disk().refresh() {
            let outcome = ScreenOutcome::Failed {
                reason: "disk low".to_owned(),
            };
            return Ok((outcome, Blobs::default()));
        }
        let Some(device) = request.action.device() else {
            let outcome = match self.backend.devices().await {
                Ok(devices) => ScreenOutcome::Devices { devices },
                Err(BackendError(reason)) => ScreenOutcome::Failed { reason },
            };
            return Ok((outcome, Blobs::default()));
        };
        let latest = self
            .log
            .latest_capture(&self.storage.db, request.session_id, device)?;
        let latest_id = latest.as_ref().map(|l| l.capture);
        if request
            .action
            .aim()
            .is_some_and(|aim| Some(aim) != latest_id)
        {
            let outcome = ScreenOutcome::Stale { latest: latest_id };
            return Ok((outcome, Blobs::default()));
        }
        if let Err(BackendError(reason)) = self.send(request, device, latest.as_ref()).await? {
            return Ok((ScreenOutcome::Failed { reason }, Blobs::default()));
        }
        let full = matches!(request.action, ScreenAction::Capture { full: true, .. });
        let (shot, blobs) = self
            .shoot(request.session_id, device, full, latest.as_ref())
            .await?;
        let is_capture = matches!(request.action, ScreenAction::Capture { .. });
        let outcome = match shot {
            ScreenShot::Failed { reason } if is_capture => ScreenOutcome::Failed { reason },
            shot => ScreenOutcome::Done { shot },
        };
        Ok((outcome, blobs))
    }

    /// Sends `request`'s input to `device`. `Err` is why it did not happen: a
    /// backend error, or an APK that cannot be installed. A `Capture` sends
    /// nothing.
    async fn send(
        &mut self,
        request: &Request,
        device: &str,
        latest: Option<&LatestCapture>,
    ) -> Result<std::result::Result<(), BackendError>> {
        let sent = match &request.action {
            ScreenAction::Devices | ScreenAction::Capture { .. } => Ok(()),
            ScreenAction::Tap { x, y, .. } => match on_screen(latest, *x, *y) {
                Some((x, y)) => self.backend.tap(device, x, y).await,
                None => Err(unaimed()),
            },
            ScreenAction::Swipe {
                from_x,
                from_y,
                to_x,
                to_y,
                duration_ms,
                ..
            } => match (
                on_screen(latest, *from_x, *from_y),
                on_screen(latest, *to_x, *to_y),
            ) {
                (Some(from), Some(to)) => self.backend.swipe(device, from, to, *duration_ms).await,
                _ => Err(unaimed()),
            },
            ScreenAction::Text { text, .. } => self.backend.text(device, text).await,
            ScreenAction::Key { key, .. } => self.backend.key(device, *key).await,
            ScreenAction::Install { apk, .. } => {
                match self.apk_in_workdir(request.session_id, apk)? {
                    Ok(path) => self.backend.install(device, &path).await,
                    Err(reason) => Err(BackendError(reason)),
                }
            }
            ScreenAction::Launch { app, .. } => self.backend.launch(device, app).await,
            ScreenAction::Stop { app, .. } => self.backend.stop(device, app).await,
        };
        Ok(sent)
    }

    /// The shot taken after an action on `device`, processed into the image a
    /// session stores. A new capture is written and the captures it prunes are
    /// named; a failure to shoot is `ScreenShot::Failed`.
    async fn shoot(
        &mut self,
        session: SessionId,
        device: &str,
        full: bool,
        latest: Option<&LatestCapture>,
    ) -> Result<(ScreenShot, Blobs)> {
        let png = match self.backend.capture(device).await {
            Ok(png) => png,
            Err(BackendError(reason)) => {
                return Ok((ScreenShot::Failed { reason }, Blobs::default()));
            }
        };
        let processed = match capture::process(&png, full, latest.map(|l| &l.pixels)) {
            Ok(processed) => processed,
            Err(BackendError(reason)) => {
                return Ok((ScreenShot::Failed { reason }, Blobs::default()));
            }
        };
        match processed {
            Processed::Unchanged => {
                let Some(latest) = latest else {
                    return Err(Error::Screen("unchanged capture with no latest one".into()));
                };
                let shot = ScreenShot::Unchanged {
                    capture: latest.capture,
                };
                Ok((shot, Blobs::default()))
            }
            Processed::New(encoded) => {
                let meta = match self.storage.blobs.write(encoded.bytes.as_slice()) {
                    Ok(meta) => meta,
                    Err(error) => {
                        let reason = format!("cannot store the capture: {error}");
                        return Ok((ScreenShot::Failed { reason }, Blobs::default()));
                    }
                };
                self.storage.blobs.record(self.storage.db.conn(), &meta)?;
                let pruned =
                    self.log
                        .prunable_captures(&self.storage.db, session, KEEP_CAPTURES - 1)?;
                let image = ScreenImage {
                    image: meta.blob_ref(),
                    format: encoded.format,
                    width: encoded.width,
                    height: encoded.height,
                    screen_width: encoded.screen_width,
                    screen_height: encoded.screen_height,
                    pixels: encoded.pixels,
                };
                let blobs = Blobs {
                    written: Some(meta.id),
                    pruned,
                };
                Ok((ScreenShot::Image(image), blobs))
            }
        }
    }

    /// The APK `apk` names, in the workdir of session `session`, as the path
    /// to install. `Err` is why it cannot be installed.
    fn apk_in_workdir(
        &self,
        session: SessionId,
        apk: &str,
    ) -> Result<std::result::Result<PathBuf, String>> {
        let Some(info) = session_in(self.storage.db.conn(), session)? else {
            return Ok(Err("no such session".to_owned()));
        };
        let Ok(path) = std::fs::canonicalize(Path::new(&info.workdir).join(apk)) else {
            return Ok(Err("no such apk".to_owned()));
        };
        let Ok(root) = std::fs::canonicalize(&info.workdir) else {
            return Ok(Err("no such workdir".to_owned()));
        };
        if !path.starts_with(&root) {
            return Ok(Err("apk outside the workdir".to_owned()));
        }
        Ok(Ok(path))
    }
}

/// The point `(x, y)` of `latest`'s image, as the device screen has it.
fn on_screen(latest: Option<&LatestCapture>, x: u32, y: u32) -> Option<(u32, u32)> {
    latest.map(|latest| capture::to_screen(x, y, latest))
}

/// The refusal of an input with no capture to map its point onto. The stale
/// check lets an aimed input through only when its aim is the latest capture,
/// so this is not reached.
fn unaimed() -> BackendError {
    BackendError("no capture to aim at".to_owned())
}

fn run_thread<B: ScreenBackend>(
    data_dir: PathBuf,
    log: EventLog,
    config: ScreenConfig,
    backend: B,
    ready: oneshot::Sender<Result<()>>,
    stop: oneshot::Receiver<()>,
) -> Result<()> {
    let setup = || -> Result<_> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| Error::Screen(format!("cannot build runtime: {e}")))?;
        let mut runner = ScreenRunner {
            log,
            storage: Storage::open(data_dir)?,
            config,
            backend,
            cursor: Seq::ZERO,
            queue: VecDeque::new(),
        };
        runner.recover()?;
        Ok((runtime, runner))
    };
    let (runtime, mut runner) = match setup() {
        Ok(parts) => {
            let _ = ready.send(Ok(()));
            parts
        }
        Err(error) => {
            let message = error.to_string();
            let _ = ready.send(Err(error));
            return Err(Error::Screen(message));
        }
    };
    if !runner.config.enabled {
        return Ok(());
    }
    runtime.block_on(runner.serve(stop))
}
