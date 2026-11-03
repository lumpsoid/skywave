//! SIGHUP, SIGTERM and SIGINT as event-loop messages, so a closed SSH
//! session or a `kill` leaves through the same exit as the quit key.
//!
//! Raw mode turns Ctrl-C into a key, so SIGINT arrives only from `kill`.
//! A second signal while the first is still being handled ends the process
//! at once, so a wedged loop stays killable without SIGKILL.

use std::io;
use std::sync::mpsc::Sender;

/// Stops forwarding when closed.
pub struct Forwarding {
    #[cfg(unix)]
    handle: signal_hook::iterator::Handle,
}

impl Forwarding {
    /// Stops the forwarding thread. The handlers stay installed: one more
    /// signal is swallowed, a second ends the process.
    pub fn close(self) {
        #[cfg(unix)]
        self.handle.close();
    }
}

/// Sends `msg()` on `tx` for every SIGHUP, SIGTERM and SIGINT, from a
/// thread of its own.
#[cfg(unix)]
pub fn forward<M: Send + 'static>(tx: Sender<M>, msg: fn() -> M) -> io::Result<Forwarding> {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    use signal_hook::flag;
    use signal_hook::iterator::Signals;

    const SIGNALS: [i32; 3] = [SIGHUP, SIGTERM, SIGINT];
    let caught = Arc::new(AtomicBool::new(false));
    for signal in SIGNALS {
        // Exits only if `caught` is already set, so it goes first.
        flag::register_conditional_shutdown(signal, 1, Arc::clone(&caught))?;
        flag::register(signal, Arc::clone(&caught))?;
    }
    let mut signals = Signals::new(SIGNALS)?;
    let handle = signals.handle();
    std::thread::spawn(move || {
        for _ in signals.forever() {
            if tx.send(msg()).is_err() {
                return;
            }
        }
    });
    Ok(Forwarding { handle })
}

/// No signals to forward off Unix.
#[cfg(not(unix))]
pub fn forward<M>(_tx: Sender<M>, _msg: fn() -> M) -> io::Result<Forwarding> {
    Ok(Forwarding {})
}
