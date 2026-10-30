//! [`SkywaveCore`]: the client core as one UniFFI object, and
//! [`ViewListener`], the Kotlin-implemented callback that receives its
//! snapshots (D17).
//!
//! The process opens one core and keeps it (D17). [`SkywaveCore::shutdown`]
//! stops it deterministically: listeners are detached before it returns and
//! every later call fails with [`SkywaveError::ShutDown`] or does nothing,
//! so nothing depends on when Kotlin's GC finalises the object.

use std::cell::Cell;
use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use skywave_core as sc;

use crate::action::Action;
use crate::error::SkywaveError;
use crate::view::ViewState;

/// Receives every [`ViewState`] the core publishes.
///
/// There is no dedicated delivery thread: the listener runs on whichever
/// thread delivers — the caller's own thread inside `subscribe` (the first
/// snapshot), inside `dispatch` and `unsubscribe` (snapshots published
/// meanwhile, possibly several), or a core engine thread. So it must be
/// thread-safe and must not assume a thread (in particular not the main
/// thread); hand the snapshot on (e.g. to a `MutableStateFlow`) and return
/// at once, since delivery to every listener, and the call that happened
/// to deliver, waits for it. Snapshots arrive in order; one may be skipped
/// in favour of a newer one. The listener may call `dispatch`, but not
/// `subscribe`, `unsubscribe` or `shutdown` (they fail with
/// `SkywaveException.Reentrant`). An exception it throws is logged and
/// otherwise ignored: the listener stays subscribed and the core goes on.
#[uniffi::export(with_foreign)]
pub trait ViewListener: Send + Sync {
    fn on_view(&self, view: ViewState);
}

/// The client core: local replica, sync engine, and the snapshots a head
/// renders (D2).
#[derive(uniffi::Object)]
pub struct SkywaveCore {
    inner: Mutex<Inner>,
}

struct Inner {
    /// `None` once shut down.
    core: Option<sc::Core>,
    subscriptions: HashMap<u64, sc::SubscriptionId>,
    next_id: u64,
}

#[uniffi::export]
impl SkywaveCore {
    /// Opens the core over `data_dir` (created if missing) and starts the
    /// sync engine. `device_name` is what the server lists the device as
    /// when it pairs; `None` uses [`default_device_name`](crate::helpers::default_device_name).
    #[uniffi::constructor]
    pub fn open(data_dir: String, device_name: Option<String>) -> Result<Arc<Self>, SkywaveError> {
        let config = sc::EngineConfig {
            device_name,
            ..sc::EngineConfig::default()
        };
        Ok(Arc::new(SkywaveCore::with_core(sc::Core::open_with(
            Path::new(&data_dir),
            config,
        )?)))
    }

    /// Applies a user action. When this returns, everything the action
    /// changed is on disk.
    pub fn dispatch(&self, action: Action) -> Result<(), SkywaveError> {
        let action = sc::Action::try_from(action)?;
        Ok(self.core()?.dispatch(action)?)
    }

    /// Registers `listener`, calls it with the current snapshot right away,
    /// then after every change. Returns the id for
    /// [`unsubscribe`](SkywaveCore::unsubscribe).
    pub fn subscribe(&self, listener: Arc<dyn ViewListener>) -> Result<u64, SkywaveError> {
        not_in_listener("subscribe")?;
        let core = self.core()?;
        let subscription = core.subscribe(move |view| deliver(listener.as_ref(), view));
        let mut inner = self.lock();
        if inner.core.is_none() {
            // Shut down while we subscribed: shutdown never saw this one.
            drop(inner);
            core.unsubscribe(subscription);
            return Err(SkywaveError::ShutDown);
        }
        let id = inner.next_id;
        inner.next_id += 1;
        inner.subscriptions.insert(id, subscription);
        Ok(id)
    }

    /// Detaches the listener `id`; once this returns it is not called
    /// again. Unknown ids, and any id after shutdown, are ignored.
    pub fn unsubscribe(&self, id: u64) -> Result<(), SkywaveError> {
        not_in_listener("unsubscribe")?;
        let (core, subscription) = {
            let mut inner = self.lock();
            let Some(subscription) = inner.subscriptions.remove(&id) else {
                return Ok(());
            };
            (inner.core.clone(), subscription)
        };
        if let Some(core) = core {
            core.unsubscribe(subscription);
        }
        Ok(())
    }

    /// The current snapshot, for a head that polls.
    pub fn view(&self) -> Result<ViewState, SkywaveError> {
        Ok(self.core()?.view().into())
    }

    /// Pauses the network while the app is in the background; actions
    /// still persist and queue. Does nothing after shutdown.
    pub fn suspend(&self) {
        if let Ok(core) = self.core() {
            core.suspend();
        }
    }

    /// Undoes [`suspend`](SkywaveCore::suspend). Does nothing after
    /// shutdown.
    pub fn resume(&self) {
        if let Ok(core) = self.core() {
            core.resume();
        }
    }

    /// Detaches every listener and stops the sync engine. Later calls fail
    /// with `SkywaveException.ShutDown` (`suspend`, `resume`, `unsubscribe`
    /// and a second `shutdown` do nothing). A `dispatch` already running
    /// completes first; what it wrote stays on disk.
    pub fn shutdown(&self) -> Result<(), SkywaveError> {
        not_in_listener("shutdown")?;
        let (core, subscriptions) = {
            let mut inner = self.lock();
            let subscriptions: Vec<_> = inner.subscriptions.drain().map(|(_, s)| s).collect();
            (inner.core.take(), subscriptions)
        };
        if let Some(core) = core {
            for subscription in subscriptions {
                core.unsubscribe(subscription);
            }
            // The engine stops with the last clone of the core.
            drop(core);
        }
        Ok(())
    }
}

impl SkywaveCore {
    fn with_core(core: sc::Core) -> SkywaveCore {
        SkywaveCore {
            inner: Mutex::new(Inner {
                core: Some(core),
                subscriptions: HashMap::new(),
                next_id: 1,
            }),
        }
    }

    /// A handle on the core, taken without holding our lock through the
    /// call: a listener's `dispatch` runs inside another call's delivery.
    fn core(&self) -> Result<sc::Core, SkywaveError> {
        self.lock().core.clone().ok_or(SkywaveError::ShutDown)
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // Every critical section leaves `Inner` consistent.
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

thread_local! {
    /// How many listener calls this thread is inside.
    static IN_LISTENER: Cell<u32> = const { Cell::new(0) };
}

/// Calls `listener` with `view`, marking the thread as inside a listener
/// and containing a panic — which is also how UniFFI reports an exception
/// thrown by the Kotlin side — so it cannot unwind into the core's delivery.
fn deliver(listener: &dyn ViewListener, view: sc::ViewState) {
    IN_LISTENER.with(|depth| depth.set(depth.get() + 1));
    let outcome = catch_unwind(AssertUnwindSafe(|| listener.on_view(view.into())));
    IN_LISTENER.with(|depth| depth.set(depth.get() - 1));
    if let Err(panic) = outcome {
        let reason = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap_or("unknown panic");
        tracing::error!("ViewListener failed: {reason}");
    }
}

/// Refuses `call` from inside a listener: delivery holds the subscriber
/// list, which `call` would wait for.
fn not_in_listener(call: &str) -> Result<(), SkywaveError> {
    if IN_LISTENER.with(Cell::get) > 0 {
        return Err(SkywaveError::Reentrant {
            call: call.to_owned(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;
    use crate::view::{PairingStatus, Screen};

    const SOON: Duration = Duration::from_secs(10);

    struct Channel(Mutex<mpsc::Sender<ViewState>>);

    impl ViewListener for Channel {
        fn on_view(&self, view: ViewState) {
            let _ = self.0.lock().unwrap().send(view);
        }
    }

    fn channel() -> (Arc<dyn ViewListener>, mpsc::Receiver<ViewState>) {
        let (tx, rx) = mpsc::channel();
        (Arc::new(Channel(Mutex::new(tx))), rx)
    }

    /// A core without an engine: nothing touches the network.
    fn detached() -> (tempfile::TempDir, SkywaveCore) {
        let dir = tempfile::tempdir().unwrap();
        let core = SkywaveCore::with_core(sc::Core::open_detached(dir.path()).unwrap());
        (dir, core)
    }

    struct Panics;

    impl ViewListener for Panics {
        fn on_view(&self, _: ViewState) {
            panic!("listener blew up");
        }
    }

    #[test]
    fn a_panicking_listener_stops_nothing() {
        let (_dir, core) = detached();
        core.subscribe(Arc::new(Panics)).unwrap();
        let (listener, views) = channel();
        core.subscribe(listener).unwrap();
        views.recv_timeout(SOON).unwrap();
        // Incomplete, so it fails at once and changes the view.
        core.dispatch(Action::Pair {
            address: "host:7443".into(),
            code: " ".into(),
            fingerprint: "ab:cd".into(),
        })
        .unwrap();
        let view = views.recv_timeout(SOON).unwrap();
        assert!(
            matches!(&view.screen, Screen::Pairing { view }
                if matches!(view.status, PairingStatus::Failed { .. })),
            "{view:?}"
        );
    }

    #[test]
    fn reentrant_calls_fail_instead_of_deadlocking() {
        struct Reenters {
            core: Arc<SkywaveCore>,
            results: Mutex<mpsc::Sender<Vec<Result<(), SkywaveError>>>>,
        }
        impl ViewListener for Reenters {
            fn on_view(&self, _: ViewState) {
                let results = vec![
                    self.core.subscribe(Arc::new(Panics)).map(drop),
                    self.core.unsubscribe(1),
                    self.core.shutdown(),
                ];
                let _ = self.results.lock().unwrap().send(results);
            }
        }
        let (_dir, core) = detached();
        let core = Arc::new(core);
        let (tx, rx) = mpsc::channel();
        core.subscribe(Arc::new(Reenters {
            core: core.clone(),
            results: Mutex::new(tx),
        }))
        .unwrap();
        let results = rx.recv_timeout(SOON).unwrap();
        let reentrant = |call: &str| {
            Err(SkywaveError::Reentrant {
                call: call.to_owned(),
            })
        };
        assert_eq!(
            results,
            [
                reentrant("subscribe"),
                reentrant("unsubscribe"),
                reentrant("shutdown")
            ]
        );
        // Dispatch from a listener is fine, and the core still works.
        core.dispatch(Action::Navigate {
            route: crate::action::Route::Settings,
        })
        .unwrap();
        core.shutdown().unwrap();
    }

    #[test]
    fn unsubscribed_listeners_hear_nothing_more() {
        let (_dir, core) = detached();
        let (listener, views) = channel();
        let id = core.subscribe(listener).unwrap();
        views.recv_timeout(SOON).unwrap();
        core.unsubscribe(id).unwrap();
        core.unsubscribe(id).unwrap();
        core.unsubscribe(999).unwrap();
        core.dispatch(Action::Navigate {
            route: crate::action::Route::Settings,
        })
        .unwrap();
        assert!(views.try_recv().is_err());
    }

    #[test]
    fn calls_after_shutdown_fail_or_do_nothing() {
        let (_dir, core) = detached();
        let (listener, views) = channel();
        let id = core.subscribe(listener).unwrap();
        views.recv_timeout(SOON).unwrap();
        core.shutdown().unwrap();
        core.shutdown().unwrap();
        let navigate = Action::Navigate {
            route: crate::action::Route::Settings,
        };
        assert_eq!(core.dispatch(navigate), Err(SkywaveError::ShutDown));
        assert_eq!(core.view(), Err(SkywaveError::ShutDown));
        let (again, _) = channel();
        assert_eq!(core.subscribe(again).err(), Some(SkywaveError::ShutDown));
        core.unsubscribe(id).unwrap();
        core.suspend();
        core.resume();
        // The listener was detached, and dropped with its closure.
        assert!(matches!(
            views.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn a_bad_id_is_a_typed_error() {
        let (_dir, core) = detached();
        let err = core
            .dispatch(Action::OpenSession {
                session_id: "not-a-uuid".into(),
            })
            .unwrap_err();
        assert_eq!(
            err,
            SkywaveError::InvalidId {
                name: "session_id".into(),
                input: "not-a-uuid".into()
            }
        );
    }

    #[test]
    fn open_fails_cleanly_on_an_unusable_dir() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, "").unwrap();
        let err = SkywaveCore::open(file.display().to_string(), None)
            .err()
            .unwrap();
        assert!(matches!(err, SkywaveError::Store { .. }), "{err:?}");
    }
}
