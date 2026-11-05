//! Stopping and restarting `serve` safely (architecture.md "Server", D41).
//!
//! A stop ends every running turn and shell run (D8: they are lost, and
//! reported so on the next start). So `skywaved stop` and `skywaved
//! restart` ask the running `serve` over a Unix socket in its data dir
//! ([`DataDir::control_socket`](crate::data_dir::DataDir::control_socket)),
//! one JSON line per [`Request`] and per [`Reply`], and `serve` decides
//! from what its log says runs, never from a timer:
//! - **Now** ([`When::Now`]): with nothing running it stops at once.
//!   Otherwise it refuses with the work it would lose ([`Reply::Busy`]),
//!   and stops when asked again with that list ([`Confirm::Seen`]), unless
//!   more started since, which is refused the same way. [`Confirm::Force`]
//!   skips the question.
//! - **After the running work** ([`When::AfterWork`]): commands that start
//!   new work are refused `ServerStopping` from then on
//!   ([`EventLog::set_stopping`]), and `serve` stops once every turn and
//!   shell run it had accepted has ended: no turn active or owed in the
//!   log's projections and the supervisor's [`Activity`], having read the
//!   log at least up to where the refusals began, and no shell run left
//!   unfinished. It is checked again at every append and every step of the
//!   supervisor. [`Request::Cancel`] ends the wait and takes work again.
//!
//! A stop decided either way is the same graceful stop as SIGTERM's.
//! SIGTERM itself is unchanged: a service manager escalates a refused one
//! to SIGKILL, which loses more. A restart is that stop, then `serve`
//! executing its own binary again with the same arguments, so a binary
//! replaced since is the one that starts.
//!
//! The same socket answers [`Request::Status`], `skywaved status`: the
//! local health check.
//!
//! The socket is private to the daemon's user: it sits in the data dir,
//! which only that user can enter.

use std::fmt;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use skywave_proto::{CommandId, SessionId};

use crate::error::Result;
use crate::event_log::parse_id;

/// What a client asks `serve`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Stop `serve`, and start it again when `restart`.
    Stop {
        restart: bool,
        when: When,
        confirm: Confirm,
    },
    /// Cancel a stop waiting for the running work, and take work again.
    Cancel,
    /// Report how `serve` is ([`Reply::Status`]): the local health check.
    Status,
}

/// When a [`Request::Stop`] stops `serve`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum When {
    /// Now, losing what runs (after a confirmation, if anything does).
    Now,
    /// Once the running turns and shell runs have ended, taking no new ones.
    AfterWork,
}

/// What a client confirms a [`When::Now`] stop loses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "work", rename_all = "snake_case")]
pub enum Confirm {
    /// Nothing: refused if anything runs.
    Nothing,
    /// This work, as a [`Reply::Busy`] listed it: refused if anything else
    /// runs.
    Seen(Vec<Work>),
    /// Whatever runs.
    Force,
}

/// What `serve` answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Reply {
    /// Refused: a stop now would lose this work. Ask again confirming it.
    Busy { work: Vec<Work> },
    /// Stopping once this work has ended, taking no new work; sent again
    /// whenever it changes. Empty while the agents still take up what was
    /// sent before the wait began.
    Waiting { work: Vec<Work> },
    /// `serve` is stopping. The connection closes once it has.
    Stopping { restart: bool },
    /// The wait was cancelled: `serve` runs on and takes work again.
    Cancelled,
    /// [`Request::Cancel`] while no stop waits.
    NothingToCancel,
    /// How `serve` is, for [`Request::Status`]. A `serve` already stopping
    /// answers [`Reply::Stopping`] instead.
    Status {
        protocol_version: u32,
        /// Waiting for the running work to stop ([`When::AfterWork`]).
        stopping_after_work: bool,
        /// The data dir's disk is below `disk.min_free`: agents are paused
        /// and new work is refused.
        disk_low: bool,
        /// What runs now.
        work: Vec<Work>,
    },
    /// The request could not be handled.
    Failed { message: String },
}

/// Work a stop would lose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Work {
    /// A session's agent has a turn running, or a message handed to it
    /// whose turn has not started.
    Turn { session: SessionId, workdir: String },
    /// A shell run not over yet: running, or queued when not `started`.
    Shell {
        run: CommandId,
        session: SessionId,
        workdir: String,
        command: String,
        started: bool,
    },
}

impl Work {
    /// Whether `self` and `other` are the same turn's session or the same
    /// shell run, whatever else changed (a queued run that started).
    pub fn same(&self, other: &Work) -> bool {
        match (self, other) {
            (Work::Turn { session: a, .. }, Work::Turn { session: b, .. }) => a == b,
            (Work::Shell { run: a, .. }, Work::Shell { run: b, .. }) => a == b,
            _ => false,
        }
    }
}

impl fmt::Display for Work {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Work::Turn { session, workdir } => {
                write!(f, "agent turn in {workdir} (session {})", session.0)
            }
            Work::Shell {
                command,
                workdir,
                started,
                ..
            } => {
                let state = if *started { "running" } else { "queued" };
                write!(f, "shell run `{command}` in {workdir} ({state})")
            }
        }
    }
}

/// The work a stop now would lose, as the log's projections have it: the
/// sessions with a turn active or a message owed to their agent, then the
/// shell runs not finished, in log order.
pub fn active_work(conn: &Connection) -> Result<Vec<Work>> {
    let mut work = Vec::new();
    let mut turns = conn.prepare(
        "SELECT id, workdir FROM sessions
         WHERE turn_active = 1 OR id IN (SELECT session_id FROM supervisor_owed)
         ORDER BY rowid",
    )?;
    let rows = turns.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get(1)?)))?;
    for row in rows {
        let (session, workdir) = row?;
        work.push(Work::Turn {
            session: parse_id(&session)?,
            workdir,
        });
    }
    let mut shells = conn.prepare(
        "SELECT r.command_id, r.session_id, s.workdir,
                json_extract(e.kind, '$.command'), r.started_seq IS NOT NULL
         FROM shell_runs r
         JOIN sessions s ON s.id = r.session_id
         JOIN events e ON e.seq = r.seq
         WHERE r.finished_seq IS NULL
         ORDER BY r.seq",
    )?;
    let rows = shells.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get(4)?,
        ))
    })?;
    for row in rows {
        let (run, session, workdir, command, started) = row?;
        work.push(Work::Shell {
            run: parse_id(&run)?,
            session: parse_id(&session)?,
            workdir,
            command: command.unwrap_or_default(),
            started,
        });
    }
    Ok(work)
}

/// Whether a [`When::Now`] stop that confirms `confirm` may lose `work`.
pub fn confirmed(confirm: &Confirm, work: &[Work]) -> bool {
    match confirm {
        Confirm::Nothing => work.is_empty(),
        Confirm::Seen(seen) => work.iter().all(|w| seen.iter().any(|s| s.same(w))),
        Confirm::Force => true,
    }
}

#[cfg(unix)]
pub use unix::{Client, Control, Stop};

#[cfg(unix)]
mod unix {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    use skywave_proto::Seq;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader as AsyncBufReader};
    use tokio::net::unix::OwnedWriteHalf;
    use tokio::net::{UnixListener, UnixStream};
    use tokio::sync::watch;
    use tokio::task::{AbortHandle, JoinHandle};

    use super::{Reply, Request, When, Work, active_work, confirmed};
    use crate::db::Db;
    use crate::error::{Error, IoContext, Result};
    use crate::event_log::{self, seq_from_sql};
    use crate::supervisor::Activity;
    use crate::{EventLog, lock};

    /// A stop `serve` decided on.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Stop {
        /// Start `serve` again once it has stopped.
        pub restart: bool,
    }

    /// The control socket of a running `serve`. Dropping it stops listening
    /// and removes the socket; connections already open stay until the
    /// runtime ends, which is how a client learns that `serve` stopped.
    pub struct Control {
        shared: Arc<Shared>,
        decided: watch::Receiver<Option<Stop>>,
        accept: JoinHandle<()>,
        path: PathBuf,
    }

    struct Shared {
        log: EventLog,
        /// Read and locked on blocking threads only.
        db: Mutex<Db>,
        activity: watch::Receiver<Activity>,
        decided: watch::Sender<Option<Stop>>,
        drain: watch::Sender<Drain>,
        /// The task waiting for the running work, while one does.
        waiter: Mutex<Option<AbortHandle>>,
    }

    /// Where a stop after the running work is.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Drain {
        /// None asked for, or cancelled.
        Off,
        /// Waiting for `work` to end.
        Waiting { restart: bool, work: Vec<Work> },
    }

    impl Control {
        /// Listens on `path`, replacing what a `serve` that ended without
        /// removing it left there: the caller holds the serve lock, so no
        /// other `serve` listens on it. The work is read through a
        /// connection of its own to the database at `db_path`; `activity`
        /// is the supervisor's.
        pub fn bind(
            path: PathBuf,
            db_path: &Path,
            log: EventLog,
            activity: watch::Receiver<Activity>,
        ) -> Result<Control> {
            match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    return Err(e).context(|| format!("removing {}", path.display()));
                }
                _ => {}
            }
            let listener =
                UnixListener::bind(&path).context(|| format!("listening on {}", path.display()))?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .context(|| format!("restricting {}", path.display()))?;
            let (decided_tx, decided) = watch::channel(None);
            let shared = Arc::new(Shared {
                log,
                db: Mutex::new(Db::open(db_path)?),
                activity,
                decided: decided_tx,
                drain: watch::Sender::new(Drain::Off),
                waiter: Mutex::new(None),
            });
            let accept = tokio::spawn(accept(listener, shared.clone()));
            Ok(Control {
                shared,
                decided,
                accept,
                path,
            })
        }

        /// Completes once a stop is decided. Cancel safe.
        pub async fn stopped(&mut self) -> Stop {
            match self.decided.wait_for(Option::is_some).await {
                Ok(stop) => stop.expect("waited for a decision"),
                // The sender lives in `shared`, which `self` holds.
                Err(_) => std::future::pending().await,
            }
        }
    }

    impl Drop for Control {
        fn drop(&mut self) {
            self.accept.abort();
            if let Some(waiter) = lock(&self.shared.waiter).take() {
                waiter.abort();
            }
            let _ = std::fs::remove_file(&self.path);
        }
    }

    async fn accept(listener: UnixListener, shared: Arc<Shared>) {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    tokio::spawn(converse(stream, shared.clone()));
                }
                Err(error) => {
                    tracing::warn!(%error, "control socket: accept failed");
                }
            }
        }
    }

    /// Answers one client's requests until it hangs up, or until `serve`
    /// ends while it waits for the stop.
    async fn converse(stream: UnixStream, shared: Arc<Shared>) {
        let (read, mut write) = stream.into_split();
        let mut lines = AsyncBufReader::new(read).lines();
        loop {
            let request = match lines.next_line().await {
                Ok(Some(line)) => serde_json::from_str::<Request>(&line),
                Ok(None) | Err(_) => return,
            };
            let reply = match request {
                Ok(request) => handle(&shared, request).await,
                Err(e) => Err(Error::Control(format!("bad request: {e}"))),
            };
            let reply = reply.unwrap_or_else(|error| Reply::Failed {
                message: error.to_string(),
            });
            if send(&mut write, &reply).await.is_err() {
                return;
            }
            match reply {
                Reply::Waiting { .. } => return follow_wait(&shared, &mut write, reply).await,
                // Closed when the runtime ends with `serve`.
                Reply::Stopping { .. } => std::future::pending().await,
                _ => {}
            }
        }
    }

    /// After `sent`, a [`Reply::Waiting`]: tells the client how the wait
    /// goes and how it ends, then holds the connection until `serve` ends
    /// if it stops.
    async fn follow_wait(shared: &Shared, write: &mut OwnedWriteHalf, mut sent: Reply) {
        let mut decided = shared.decided.subscribe();
        let mut drain = shared.drain.subscribe();
        loop {
            let stop = *decided.borrow_and_update();
            if let Some(stop) = stop {
                if send(
                    write,
                    &Reply::Stopping {
                        restart: stop.restart,
                    },
                )
                .await
                .is_ok()
                {
                    // Closed when the runtime ends with `serve`.
                    std::future::pending::<()>().await;
                }
                return;
            }
            let now = match &*drain.borrow_and_update() {
                Drain::Off => Reply::Cancelled,
                Drain::Waiting { work, .. } => Reply::Waiting { work: work.clone() },
            };
            if now != sent {
                if send(write, &now).await.is_err() || now == Reply::Cancelled {
                    return;
                }
                sent = now;
            }
            tokio::select! {
                _ = decided.changed() => {}
                _ = drain.changed() => {}
            }
        }
    }

    async fn send(write: &mut OwnedWriteHalf, reply: &Reply) -> std::io::Result<()> {
        let mut line = serde_json::to_vec(reply).map_err(std::io::Error::other)?;
        line.push(b'\n');
        write.write_all(&line).await
    }

    async fn handle(shared: &Arc<Shared>, request: Request) -> Result<Reply> {
        if let Some(stop) = *shared.decided.borrow() {
            return Ok(Reply::Stopping {
                restart: stop.restart,
            });
        }
        match request {
            Request::Stop {
                restart,
                when: When::Now,
                confirm,
            } => {
                let work = read_work(shared).await?;
                if confirmed(&confirm, &work) {
                    if !work.is_empty() {
                        tracing::warn!(
                            restart,
                            lost = work.len(),
                            "stop confirmed over running work"
                        );
                    }
                    decide(shared, Stop { restart });
                    Ok(Reply::Stopping { restart })
                } else {
                    Ok(Reply::Busy { work })
                }
            }
            Request::Stop {
                restart,
                when: When::AfterWork,
                confirm: _,
            } => start_drain(shared, restart).await,
            Request::Cancel => Ok(cancel(shared)),
            Request::Status => {
                let work = read_work(shared).await?;
                Ok(Reply::Status {
                    protocol_version: skywave_proto::PROTOCOL_VERSION,
                    stopping_after_work: matches!(*shared.drain.borrow(), Drain::Waiting { .. }),
                    disk_low: shared.log.disk().is_low(),
                    work,
                })
            }
        }
    }

    /// Refuses new work, then waits for the running work in a task of its
    /// own. A stop already waiting only takes the new `restart`.
    async fn start_drain(shared: &Arc<Shared>, restart: bool) -> Result<Reply> {
        // Claimed before anything is awaited, so two requests start one wait.
        let mut fresh = false;
        shared.drain.send_modify(|drain| match drain {
            Drain::Waiting { restart: r, .. } => *r = restart,
            Drain::Off => {
                *drain = Drain::Waiting {
                    restart,
                    work: Vec::new(),
                };
                fresh = true;
            }
        });
        if !fresh {
            return Ok(current_wait(shared));
        }
        shared.log.set_stopping(true);
        // A command checks the flag inside its write transaction: once
        // this one has the write lock, every command accepted without
        // seeing it is in the log at or before `through`.
        let read = blocking(shared, |db| {
            let tx = event_log::begin(db)?;
            let head: i64 =
                tx.query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |r| r.get(0))?;
            Ok((seq_from_sql(head)?, active_work(&tx)?))
        })
        .await;
        let (through, work) = match read {
            Ok(read) => read,
            Err(error) => {
                end_drain(shared);
                return Err(error);
            }
        };
        let still = shared.drain.send_if_modified(|drain| match drain {
            Drain::Waiting { work: current, .. } => {
                *current = work;
                true
            }
            // Cancelled meanwhile.
            Drain::Off => false,
        });
        if !still {
            return Ok(Reply::Cancelled);
        }
        tracing::info!(
            restart,
            through = through.0,
            "stopping once the running work ends; refusing new work"
        );
        let waiter = tokio::spawn(wait_for_work(shared.clone(), through));
        *lock(&shared.waiter) = Some(waiter.abort_handle());
        Ok(current_wait(shared))
    }

    /// The reply for the wait as it is.
    fn current_wait(shared: &Shared) -> Reply {
        match &*shared.drain.borrow() {
            Drain::Waiting { work, .. } => Reply::Waiting { work: work.clone() },
            Drain::Off => Reply::Cancelled,
        }
    }

    /// Stops `serve` once the work it accepted before `through` has ended;
    /// see the module docs.
    async fn wait_for_work(shared: Arc<Shared>, through: Seq) {
        let mut head = shared.log.subscribe();
        let mut activity = shared.activity.clone();
        loop {
            head.borrow_and_update();
            let supervisor = *activity.borrow_and_update();
            let work = match read_work(&shared).await {
                Ok(work) => work,
                Err(error) => {
                    tracing::error!(%error, "cannot read the running work; taking work again");
                    end_drain(&shared);
                    return;
                }
            };
            if work.is_empty() && !supervisor.turns && supervisor.cursor >= through {
                let restart = match &*shared.drain.borrow() {
                    Drain::Waiting { restart, .. } => *restart,
                    Drain::Off => return,
                };
                tracing::info!(restart, "the running work has ended");
                decide(&shared, Stop { restart });
                return;
            }
            shared.drain.send_if_modified(|drain| match drain {
                Drain::Waiting { work: current, .. } if *current != work => {
                    *current = work;
                    true
                }
                _ => false,
            });
            tokio::select! {
                Ok(()) = head.changed() => {}
                Ok(()) = activity.changed() => {}
                else => std::future::pending().await,
            }
        }
    }

    fn cancel(shared: &Shared) -> Reply {
        if *shared.drain.borrow() == Drain::Off {
            return Reply::NothingToCancel;
        }
        if let Some(waiter) = lock(&shared.waiter).take() {
            waiter.abort();
        }
        end_drain(shared);
        tracing::info!("stop cancelled; taking work again");
        Reply::Cancelled
    }

    fn end_drain(shared: &Shared) {
        shared.log.set_stopping(false);
        shared.drain.send_replace(Drain::Off);
    }

    /// Decides the stop, unless one already is. New work is refused from
    /// here on, while `serve` stops.
    fn decide(shared: &Shared, stop: Stop) {
        shared.log.set_stopping(true);
        shared.decided.send_if_modified(|decided| {
            if decided.is_some() {
                return false;
            }
            *decided = Some(stop);
            true
        });
    }

    async fn read_work(shared: &Arc<Shared>) -> Result<Vec<Work>> {
        blocking(shared, |db| active_work(db.conn())).await
    }

    async fn blocking<T: Send + 'static>(
        shared: &Arc<Shared>,
        f: impl FnOnce(&mut Db) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let shared = shared.clone();
        tokio::task::spawn_blocking(move || f(&mut lock(&shared.db)))
            .await
            .map_err(|e| Error::Control(format!("database task failed: {e}")))?
    }

    /// A client of a running `serve`'s control socket.
    pub struct Client {
        reader: BufReader<std::os::unix::net::UnixStream>,
        writer: std::os::unix::net::UnixStream,
    }

    impl Client {
        /// Connects to the socket at `path`; [`Error::NotServing`] when no
        /// `serve` listens there.
        pub fn connect(path: &Path) -> Result<Client> {
            let writer = match std::os::unix::net::UnixStream::connect(path) {
                Ok(stream) => stream,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    ) =>
                {
                    return Err(Error::NotServing {
                        path: path.to_owned(),
                    });
                }
                Err(e) => return Err(e).context(|| format!("connecting to {}", path.display())),
            };
            let reader = BufReader::new(
                writer
                    .try_clone()
                    .context(|| format!("connecting to {}", path.display()))?,
            );
            Ok(Client { reader, writer })
        }

        pub fn send(&mut self, request: &Request) -> Result<()> {
            let mut line = serde_json::to_vec(request)?;
            line.push(b'\n');
            self.writer
                .write_all(&line)
                .context(|| "writing to the control socket".into())
        }

        /// The next reply; `None` once `serve` has closed the connection.
        pub fn reply(&mut self) -> Result<Option<Reply>> {
            let mut line = String::new();
            let read = self
                .reader
                .read_line(&mut line)
                .context(|| "reading from the control socket".into())?;
            if read == 0 {
                return Ok(None);
            }
            Ok(Some(serde_json::from_str(&line)?))
        }
    }
}
