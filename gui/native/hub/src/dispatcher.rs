//! Runs `Core::dispatch` off the hub task, in the order actions were sent.
//!
//! `dispatch` writes SQLite before it returns, so it must not run on the
//! hub's only runtime thread. `spawn_blocking` would not keep the order the
//! head relies on (a draft saved before the send that follows it, a read
//! mark before navigating away), so one dedicated thread takes actions from
//! a queue one at a time. Each result goes back to the hub task with the tag
//! its action was sent with.
//!
//! A panic in `dispatch` is caught on the thread and reported instead of a
//! result; the thread then ends, so nothing queued after the action that
//! panicked is applied to a core that may be half way through a change.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};

use skywave_core::{Action, Core, CoreError};
use tokio::sync::mpsc::UnboundedSender;

use crate::panics::panic_message;

/// Names one sent action, so the hub task can tell which result is whose.
/// The hub picks the values; the dispatcher only carries them.
pub type Tag = u64;

/// The outcome of one action, sent back to the hub task.
#[derive(Debug)]
pub struct Dispatched {
    pub tag: Tag,
    /// `Err` means nothing of the action was persisted.
    pub result: Result<(), CoreError>,
}

/// What the dispatch thread sends the hub task.
#[derive(Debug)]
pub enum Report {
    Done(Dispatched),
    /// Dispatching the action `tag` panicked with this message. It is the
    /// thread's last report: nothing queued after it is applied.
    Panicked {
        tag: Tag,
        message: String,
    },
}

/// The dispatch thread and its queue. It holds a clone of the core until
/// [`stop`](Dispatcher::stop), which the core's shutdown needs first.
pub struct Dispatcher {
    queue: mpsc::Sender<(Tag, Action)>,
    thread: JoinHandle<()>,
}

impl Dispatcher {
    /// Starts the thread over `core`; every report goes to `reports`.
    pub fn start(core: Core, reports: UnboundedSender<Report>) -> std::io::Result<Dispatcher> {
        Dispatcher::start_with(reports, move |action| core.dispatch(action))
    }

    /// Starts the thread over `apply`, which is `Core::dispatch` but in
    /// tests.
    pub(crate) fn start_with(
        reports: UnboundedSender<Report>,
        mut apply: impl FnMut(Action) -> Result<(), CoreError> + Send + 'static,
    ) -> std::io::Result<Dispatcher> {
        let (queue, actions) = mpsc::channel::<(Tag, Action)>();
        let thread = thread::Builder::new()
            .name("skywave-hub-dispatch".into())
            .spawn(move || {
                for (tag, action) in actions {
                    // The hub task is gone during shutdown, so sends may
                    // fail; the action is applied all the same.
                    match catch_unwind(AssertUnwindSafe(|| apply(action))) {
                        Ok(result) => {
                            if let Err(err) = &result {
                                tracing::warn!(tag, "dispatch failed: {err}");
                            }
                            let _ = reports.send(Report::Done(Dispatched { tag, result }));
                        }
                        Err(payload) => {
                            let message = panic_message(payload.as_ref());
                            tracing::error!(tag, "dispatch panicked: {message}");
                            let _ = reports.send(Report::Panicked { tag, message });
                            return;
                        }
                    }
                }
            })?;
        Ok(Dispatcher { queue, thread })
    }

    /// Queues `action` behind every action sent before it. Never blocks.
    pub fn send(&self, tag: Tag, action: Action) {
        if self.queue.send((tag, action)).is_err() {
            // The thread ended after a panic, which the hub task is about
            // to report from `Report::Panicked`.
            tracing::error!(tag, "dispatch thread is gone; action dropped");
        }
    }

    /// Closes the queue, waits for the thread to apply what is still queued,
    /// and drops its core handle. Blocks; call it off the runtime thread.
    pub fn stop(self) {
        let Dispatcher { queue, thread } = self;
        drop(queue);
        if thread.join().is_err() {
            tracing::error!("dispatch thread panicked");
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use skywave_core::{DeviceId, PairedCredentials};
    use skywave_proto::CommandKind;
    use tokio::sync::mpsc::unbounded_channel;

    use super::*;

    fn credentials() -> PairedCredentials {
        PairedCredentials {
            device_id: DeviceId::new(),
            device_key_pem: "KEY".into(),
            device_cert_pem: "CERT".into(),
            ca_cert_pem: "CA".into(),
        }
    }

    /// A detached core over `dir`, paired, so actions enqueue commands.
    fn paired_core(dir: &std::path::Path) -> Core {
        let core = Core::open_detached(dir).unwrap();
        let seam = core.seam();
        core.dispatch(Action::Pair {
            address: "srv:7443".into(),
            code: "ABCDE12345".into(),
            fingerprint: "ab:cd:ef".into(),
        })
        .unwrap();
        seam.pairing_succeeded(&"srv:7443".parse().unwrap(), credentials())
            .unwrap();
        core
    }

    #[test]
    fn actions_are_applied_in_the_order_sent() {
        let dir = tempfile::tempdir().unwrap();
        let core = paired_core(dir.path());
        let seam = core.seam();
        let (results_tx, mut results) = unbounded_channel();
        let dispatcher = Dispatcher::start(core.clone(), results_tx).unwrap();

        let workdirs: Vec<String> = (0..20).map(|i| format!("/w/{i}")).collect();
        for (tag, workdir) in workdirs.iter().enumerate() {
            dispatcher.send(
                tag as Tag,
                Action::CreateSession {
                    conversation: None,
                    workdir: workdir.clone(),
                    worktree: None,
                },
            );
        }
        dispatcher.stop();

        let queued: Vec<String> = seam
            .pending_outbox()
            .unwrap()
            .into_iter()
            .filter_map(|entry| match entry.command.kind {
                CommandKind::CreateSession { workdir, .. } => Some(workdir),
                _ => None,
            })
            .collect();
        assert_eq!(queued, workdirs);

        let mut tags = Vec::new();
        while let Ok(report) = results.try_recv() {
            let Report::Done(done) = report else {
                panic!("{report:?}");
            };
            assert!(done.result.is_ok(), "{:?}", done.result);
            tags.push(done.tag);
        }
        assert_eq!(tags, (0..20).collect::<Vec<Tag>>());
    }

    #[test]
    fn stop_drains_the_queue_and_releases_the_core() {
        let dir = tempfile::tempdir().unwrap();
        let core = paired_core(dir.path());
        let (results_tx, mut results) = unbounded_channel();
        let dispatcher = Dispatcher::start(core.clone(), results_tx).unwrap();
        dispatcher.send(7, Action::RetryNow);
        dispatcher.stop();
        assert!(matches!(
            results.try_recv(),
            Ok(Report::Done(Dispatched { tag: 7, .. }))
        ));
        // The dispatcher's clone is gone, so the store closes.
        core.shutdown(std::time::Duration::from_secs(5)).unwrap();
    }

    #[test]
    fn a_panicking_dispatch_is_reported_and_ends_the_thread() {
        let (reports_tx, mut reports) = unbounded_channel();
        let (applied_tx, applied_rx) = std::sync::mpsc::channel();
        let dispatcher = Dispatcher::start_with(reports_tx, move |action| {
            if matches!(action, Action::Back) {
                panic!("dispatch blew up");
            }
            applied_tx.send(action).unwrap();
            Ok(())
        })
        .unwrap();
        dispatcher.send(1, Action::RetryNow);
        dispatcher.send(2, Action::Back);
        dispatcher.send(3, Action::RetryNow);
        dispatcher.stop();

        assert!(matches!(
            reports.try_recv(),
            Ok(Report::Done(Dispatched { tag: 1, .. }))
        ));
        match reports.try_recv() {
            Ok(Report::Panicked { tag: 2, message }) => assert_eq!(message, "dispatch blew up"),
            other => panic!("{other:?}"),
        }
        // Nothing after the panic was applied or reported, and the
        // thread's sender is gone: the hub sees the channel close.
        let applied: Vec<Action> = applied_rx.try_iter().collect();
        assert_eq!(applied, [Action::RetryNow]);
        assert!(matches!(
            reports.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
        ));
    }
}
