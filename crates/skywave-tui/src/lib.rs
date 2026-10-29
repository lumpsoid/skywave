//! Skywave terminal head: renders the core's [`ViewState`] and turns keys
//! into core [`Action`](skywave_core::Action)s. No client logic lives here
//! (D2).
//!
//! - [`app`]: the latest snapshot plus head-only presentation state; keys in,
//!   actions out.
//! - [`ui`]: pure render functions over an [`App`].
//! - [`run`]: the terminal event loop.

pub mod app;
pub mod ui;

use std::io;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event};
use crossterm::execute;
use skywave_core::{Core, Link, ViewState};

pub use app::App;

/// How often the screen refreshes while a retry countdown is running.
const TICK: Duration = Duration::from_millis(250);

enum Msg {
    View(Box<ViewState>),
    Term(Event),
    TermFailed(io::Error),
}

/// Runs the TUI over `core` until the user quits. Restores the terminal on
/// return and on panic.
pub fn run(core: &Core) -> io::Result<()> {
    let (tx, rx) = mpsc::channel();

    // Snapshots: the callback only forwards, so it never blocks the core.
    let views = tx.clone();
    let subscription = core.subscribe(move |view| {
        let _ = views.send(Msg::View(Box::new(view)));
    });
    let Ok(Msg::View(first)) = rx.recv() else {
        unreachable!("subscribe delivers the current snapshot right away");
    };
    let mut app = App::new(*first, Instant::now());

    // Terminal input on its own thread; it ends with the process.
    thread::spawn(move || {
        loop {
            let msg = match event::read() {
                Ok(event) => Msg::Term(event),
                Err(err) => Msg::TermFailed(err),
            };
            let failed = matches!(msg, Msg::TermFailed(_));
            if tx.send(msg).is_err() || failed {
                return;
            }
        }
    });

    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(io::stdout(), DisableBracketedPaste);
        previous_hook(info);
    }));
    // Installs its own panic hook that restores the terminal first.
    let mut terminal = ratatui::try_init()?;
    let result = (|| {
        execute!(io::stdout(), EnableBracketedPaste)?;
        loop {
            terminal.draw(|frame| ui::render(frame, &app, Instant::now()))?;
            let tick = matches!(app.view().link, Link::Reconnecting { .. });
            let first = if tick {
                match rx.recv_timeout(TICK) {
                    Ok(msg) => Some(msg),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return Ok(()),
                }
            } else {
                match rx.recv() {
                    Ok(msg) => Some(msg),
                    Err(_) => return Ok(()),
                }
            };
            // Handle everything that is already waiting before redrawing.
            for msg in first
                .into_iter()
                .chain(std::iter::from_fn(|| rx.try_recv().ok()))
            {
                match msg {
                    Msg::View(view) => app.set_view(*view, Instant::now()),
                    Msg::Term(Event::Key(key)) => {
                        if let Some(action) = app.on_key(key) {
                            app.dispatched(core.dispatch(action));
                        }
                    }
                    Msg::Term(Event::Paste(text)) => app.on_paste(&text),
                    Msg::Term(_) => {}
                    Msg::TermFailed(err) => return Err(err),
                }
                if app.ui().quit {
                    return Ok(());
                }
            }
        }
    })();
    core.unsubscribe(subscription);
    let _ = execute!(io::stdout(), DisableBracketedPaste);
    ratatui::try_restore()?;
    result
}
