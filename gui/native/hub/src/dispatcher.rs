//! Runs `Core::dispatch` off the hub task, in the order actions were sent.
//!
//! `dispatch` writes SQLite before it returns, so it must not run on the
//! hub's only runtime thread. `spawn_blocking` would not keep the order the
//! head relies on (a draft saved before the send that follows it, a read
//! mark before navigating away), so one dedicated thread takes actions from
//! a queue one at a time. Each result goes back to the hub task with the tag
//! its action was sent with.

use std::sync::mpsc;
use std::thread::{self, JoinHandle};

use skywave_core::{Action, Core, CoreError};
use tokio::sync::mpsc::UnboundedSender;

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

/// The dispatch thread and its queue. It holds a clone of the core until
/// [`stop`](Dispatcher::stop), which the core's shutdown needs first.
pub struct Dispatcher {
    queue: mpsc::Sender<(Tag, Action)>,
    thread: JoinHandle<()>,
}

impl Dispatcher {
    /// Starts the thread over `core`; every result goes to `results`.
    pub fn start(core: Core, results: UnboundedSender<Dispatched>) -> std::io::Result<Dispatcher> {
        let (queue, actions) = mpsc::channel::<(Tag, Action)>();
        let thread = thread::Builder::new()
            .name("skywave-hub-dispatch".into())
            .spawn(move || {
                for (tag, action) in actions {
                    let result = core.dispatch(action);
                    if let Err(err) = &result {
                        tracing::warn!(tag, "dispatch failed: {err}");
                    }
                    // The hub task is gone during shutdown; the action is
                    // applied all the same.
                    let _ = results.send(Dispatched { tag, result });
                }
            })?;
        Ok(Dispatcher { queue, thread })
    }

    /// Queues `action` behind every action sent before it. Never blocks.
    pub fn send(&self, tag: Tag, action: Action) {
        if self.queue.send((tag, action)).is_err() {
            // Only if the thread died, which `dispatch` does not make it do.
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
        seam.pairing_succeeded("srv:7443", credentials()).unwrap();
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
                    workdir: workdir.clone(),
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
        while let Ok(done) = results.try_recv() {
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
        assert_eq!(results.try_recv().map(|d| d.tag).ok(), Some(7));
        // The dispatcher's clone is gone, so the store closes.
        core.shutdown(std::time::Duration::from_secs(5)).unwrap();
    }
}
