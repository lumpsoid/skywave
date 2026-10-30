//! Skywave terminal head: renders the core's [`ViewState`] and turns keys
//! into core [`Action`](skywave_core::Action)s. No client logic lives here
//! (D2).
//!
//! - [`app`]: the latest snapshot plus head-only presentation state; keys in,
//!   actions out.
//! - [`ui`]: pure render functions over an [`App`].
//! - [`clipboard`]: copies to the terminal's clipboard (OSC 52).
//! - [`notify`]: alerts for new approvals (bell, OSC 9/777).
//! - [`run`]: the terminal event loop; [`handle_key`], [`copy`], [`alert`],
//!   [`save_draft`] and [`mark_read`] are its steps.

pub mod app;
pub mod clipboard;
pub mod composer;
pub mod notify;
pub mod timeline;
pub mod ui;

use std::io::{self, Write};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyEvent};
use crossterm::execute;
use skywave_core::{Action, Core, Link, ViewState};

pub use app::App;
pub use notify::Notify;

/// How often the screen refreshes while a retry countdown or a running
/// tool's clock is shown.
const TICK: Duration = Duration::from_millis(250);

enum Msg {
    View(Box<ViewState>),
    Term(Event),
    TermFailed(io::Error),
}

/// Runs the TUI over `core` until the user quits, alerting to new
/// approvals as `notify` says. Restores the terminal on return and on
/// panic.
pub fn run(core: &Core, notify: Notify) -> io::Result<()> {
    let tmux = notify::in_tmux();
    let (tx, rx) = mpsc::channel();

    // Snapshots: the callback only forwards, so it never blocks the core.
    let views = tx.clone();
    let subscription = core.subscribe(move |view| {
        let _ = views.send(Msg::View(Box::new(view)));
    });
    let Ok(Msg::View(first)) = rx.recv() else {
        unreachable!("subscribe delivers the current snapshot right away");
    };
    let mut app = App::new(*first);

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
            alert(&mut app, &mut io::stdout(), notify, tmux);
            terminal.draw(|frame| ui::render(frame, &app))?;
            let tick = matches!(app.view().link, Link::Reconnecting { .. }) || app.live();
            let now = Instant::now();
            let wait = [
                tick.then_some(TICK),
                app.draft_due_in(now),
                app.read_mark_due_in(now),
            ]
            .into_iter()
            .flatten()
            .min();
            let first = if let Some(wait) = wait {
                match rx.recv_timeout(wait) {
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
                    Msg::View(view) => app.set_view(*view),
                    Msg::Term(Event::Key(key)) => {
                        handle_key(&mut app, core, key);
                        copy(&mut app, &mut io::stdout(), clipboard::max());
                    }
                    Msg::Term(Event::Paste(text)) => app.on_paste(&text),
                    Msg::Term(_) => {}
                    Msg::TermFailed(err) => return Err(err),
                }
                if app.ui().quit {
                    return Ok(());
                }
            }
            save_draft(&mut app, core, Instant::now(), false);
            mark_read(&mut app, core, Instant::now(), false);
        }
    })();
    core.unsubscribe(subscription);
    let _ = execute!(io::stdout(), DisableBracketedPaste);
    ratatui::try_restore()?;
    result
}

/// Handles a key press: its action goes to `core`. The open session's draft
/// is saved first, so leaving the session (or anything else) keeps it,
/// except before a message, whose enqueue drops the draft anyway; and again
/// before quitting. Leaving and quitting also mark what was seen read.
pub fn handle_key(app: &mut App, core: &Core, key: KeyEvent) -> Option<Action> {
    let action = app.on_key(key);
    if let Some(action) = &action {
        if !matches!(action, Action::SendMessage { .. }) {
            save_draft(app, core, Instant::now(), true);
        }
        if matches!(
            action,
            Action::Navigate(_) | Action::Back | Action::OpenSession(_) | Action::OpenNextApproval
        ) {
            mark_read(app, core, Instant::now(), true);
        }
        app.dispatched(core.dispatch(action.clone()));
    }
    if app.ui().quit {
        save_draft(app, core, Instant::now(), true);
        mark_read(app, core, Instant::now(), true);
    }
    action
}

/// Sends the copy [`App::take_copy`] has, if any, to the terminal's
/// clipboard through `out`, at most `max` bytes of it.
pub fn copy(app: &mut App, out: &mut impl Write, max: usize) {
    if let Some(clip) = app.take_copy() {
        app.copied(clipboard::send(out, &clip, max));
    }
}

/// Sends the alert [`App::take_alert`] has, if any, through `out`; `tmux`
/// wraps desktop notifications for its passthrough. A failed write is
/// dropped: the screen shows the approval anyway.
pub fn alert(app: &mut App, out: &mut impl Write, notify: Notify, tmux: bool) {
    if let Some(title) = app.take_alert() {
        let _ = notify::send(out, notify, Some(&title), tmux);
    }
}

/// Saves the open session's draft if [`App::draft_save`] has one for `now`.
pub fn save_draft(app: &mut App, core: &Core, now: Instant, force: bool) {
    if let Some(action) = app.draft_save(now, force) {
        app.draft_saved(now, core.dispatch(action));
    }
}

/// Marks the open session read if [`App::read_mark`] has a mark for `now`.
pub fn mark_read(app: &mut App, core: &Core, now: Instant, force: bool) {
    if let Some(action) = app.read_mark(now, force) {
        app.read_marked(core.dispatch(action));
    }
}
