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
//!   [`save_draft`] and [`mark_read`] are its steps, and [`shut_down`] its
//!   one exit, for quit, SIGHUP/SIGTERM/SIGINT and a failed terminal read.

pub mod app;
pub mod clipboard;
pub mod composer;
pub mod notify;
mod signals;
pub mod ui;

use std::io::{self, Write};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyEvent};
use crossterm::execute;
use skywave_core::{Action, Core, Link, ViewState};
use skywave_present::policy::{DraftSaver, ReadMarker};

pub use app::App;
pub use notify::Notify;

/// How often the screen refreshes while a retry countdown or a running
/// tool's clock is shown.
const TICK: Duration = Duration::from_millis(250);

enum Msg {
    View(Box<ViewState>),
    Term(Event),
    TermFailed(io::Error),
    /// SIGHUP, SIGTERM or SIGINT: leave as quitting does.
    Signal,
}

/// Runs the TUI over `core` until the user quits or a SIGHUP, SIGTERM or
/// SIGINT arrives, alerting to new approvals as `notify` says. Every exit
/// (quit, signal, a failed terminal read) goes through [`shut_down`] and
/// restores the terminal; a panic restores it too.
pub fn run(core: &Core, notify: Notify) -> io::Result<()> {
    let tmux = notify::in_tmux();
    let (tx, rx) = mpsc::channel();

    // Snapshots, from the core's delivery thread: the callback only
    // forwards, so it never holds up delivery.
    let views = tx.clone();
    let subscription = core.subscribe(move |view| {
        let _ = views.send(Msg::View(Box::new(view)));
    });
    let Ok(Msg::View(first)) = rx.recv() else {
        unreachable!("the first message is the snapshot subscribe delivers");
    };
    let mut app = App::new(*first);

    // Only after the first snapshot, which the line above expects first.
    let signals = signals::forward(tx.clone(), || Msg::Signal)?;

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
                    Msg::Signal => return Ok(()),
                }
                if app.ui().quit {
                    return Ok(());
                }
            }
            fetch_expanded(&mut app, core);
            save_draft(&mut app, core, Instant::now(), false);
            mark_read(&mut app, core, Instant::now(), false);
        }
    })();
    // A quit key has saved already, and this finds nothing left; a signal
    // or a failed read (an SSH drop) has not.
    shut_down(&mut app, core);
    signals.close();
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
        if DraftSaver::flushes_before(action) {
            save_draft(app, core, Instant::now(), true);
        }
        if ReadMarker::marks_before(action) {
            mark_read(app, core, Instant::now(), true);
        }
        app.dispatched(core.dispatch(action.clone()));
    }
    if app.ui().quit {
        shut_down(app, core);
    }
    action
}

/// Saves what the TUI holds unsaved before it exits, however it exits: the
/// open session's draft and its read mark, both forced past their idle
/// wait.
pub fn shut_down(app: &mut App, core: &Core) {
    save_draft(app, core, Instant::now(), true);
    mark_read(app, core, Instant::now(), true);
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
/// dropped: the screen shows the approval or the run anyway.
pub fn alert(app: &mut App, out: &mut impl Write, notify: Notify, tmux: bool) {
    if let Some(message) = app.take_alert() {
        let _ = notify::send(out, notify, &message, tmux);
    }
}

/// Saves the open session's draft if [`App::draft_save`] has one for `now`.
pub fn save_draft(app: &mut App, core: &Core, now: Instant, force: bool) {
    if let Some(action) = app.draft_save(now, force) {
        app.draft_saved(now, core.dispatch(action));
    }
}

/// Dispatches the downloads [`App::expanded_fetches`] has: run after every
/// snapshot, so a blob that arrives under an open item is fetched too.
pub fn fetch_expanded(app: &mut App, core: &Core) {
    for action in app.expanded_fetches() {
        app.fetched(core.dispatch(action));
    }
}

/// Marks the open session read if [`App::read_mark`] has a mark for `now`.
pub fn mark_read(app: &mut App, core: &Core, now: Instant, force: bool) {
    if let Some(action) = app.read_mark(now, force) {
        app.read_marked(core.dispatch(action));
    }
}
