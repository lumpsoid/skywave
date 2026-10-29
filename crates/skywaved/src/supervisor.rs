//! The agent supervisor (spec §5): runs one agent session per Skywave
//! session that has work, generic over the [`AgentAdapter`].
//!
//! **Following the log.** The supervisor learns about commands from the log
//! itself, like any other reader: it reads every event after its cursor and
//! reacts to
//! - `UserMessage` → [`AgentSession::send_message`], starting the session's
//!   agent first if it is not running (lazily, resumed with the stored
//!   [`ResumeHandle`](crate::agent::ResumeHandle) when there is one);
//! - `ApprovalResolved` → [`EventMapper::take_approval`] and
//!   [`AgentSession::answer_approval`]. Command handling already lets only the
//!   first answer in (D5), so the agent hears exactly one answer per request;
//! - `TurnInterrupted` → [`AgentSession::interrupt`], if the agent's current
//!   turn began before the interrupt. The agent then reports
//!   `TurnEnded(Interrupted)` and the mapper cancels leftover approvals and
//!   appends `SessionStateChanged(Idle)`. An interrupt aimed at an earlier
//!   turn (that already ended) is ignored, so it can't stop a later one.
//!
//! Everything the agent reports goes through the session's [`EventMapper`].
//! An agent that exits on its own (crash) is logged by the mapper (`Errored`
//! when it was mid-turn, its approvals cancelled) and dropped; the next
//! `UserMessage` starts it again with its resume handle.
//!
//! **Cursor and delivery.** The cursor (`supervisor_cursor`, the seq of the
//! last event handled) is persisted, so a message accepted but not yet
//! handled when the daemon stopped reaches the agent after the restart. A
//! `UserMessage`'s seq is stored *before* the message is given to the agent:
//! delivery is **at most once**. A crash between the two loses that message
//! (the user sees it in the log with no reply and can resend) rather than
//! running an instruction twice, which an agent with side effects could not
//! undo. Answers and interrupts need no such care: after a restart their
//! approvals and turns are already settled (below), so replaying them does
//! nothing.
//!
//! **Lifecycle.** An agent session is started on the first message and kept
//! while it has work: a turn open or a delivered message whose turn has not
//! ended. Once idle it is kept warm for [`SupervisorConfig::idle_timeout`]
//! and then shut down; the next message resumes it.
//!
//! **Daemon start.** Before following the log the supervisor settles what the
//! previous run left open (F13):
//! - every pending approval gets `ApprovalCancelled { reason: "daemon
//!   restarted" }` ([`DAEMON_RESTARTED`]);
//! - a session whose last state event is `SessionStateChanged(AgentWorking)`
//!   lost its turn with the daemon and gets `SessionStateChanged(Errored)`
//!   ([`TURN_LOST`]);
//! - a session whose last state event is `TurnInterrupted` gets
//!   `SessionStateChanged(Idle)`: its agent stopped with the daemon.
//!
//! [`SupervisorHandle::shutdown`] stops the agents but leaves their open
//! turns and approvals as they are in the log, so the next start settles them
//! the same way whether the daemon stopped cleanly or crashed.
//!
//! **Threading.** SQLite and the mapper block, so the supervisor runs on its
//! own thread with a current-thread runtime and its own [`Storage`]; agent
//! sessions (and any tasks their adapter spawns) live on that runtime. All
//! work is serialized there: agent requests are awaited one at a time, so an
//! adapter must return from them promptly (they only tell the agent).

use std::collections::HashMap;
use std::future::poll_fn;
use std::path::PathBuf;
use std::task::Poll;
use std::time::Duration;

use rusqlite::OptionalExtension;
use skywave_proto::{Event, EventKind, Seq, SessionId, SessionState};
use tokio::sync::oneshot;
use tokio::time::Instant;

use crate::agent::{
    AdapterEvent, AdapterEvents, AgentAdapter, AgentSession, EventMapper, StartSpec,
    load_resume_handle,
};
use crate::db::Db;
use crate::error::{Error, Result};
use crate::{EventLog, Storage};

/// Reason of the `ApprovalCancelled` appended on daemon start (spec §5).
pub const DAEMON_RESTARTED: &str = "daemon restarted";
/// `Errored` message for a session whose turn was running when the daemon
/// stopped.
pub const TURN_LOST: &str = "daemon restarted during a turn";
/// Default for [`SupervisorConfig::idle_timeout`].
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Events read from the log per batch.
const BATCH: u32 = 500;

/// Tuning for the supervisor.
#[derive(Debug, Clone)]
pub struct SupervisorConfig {
    /// How long an agent session with no work is kept before it is shut
    /// down. The next message resumes it.
    pub idle_timeout: Duration,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        SupervisorConfig {
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
        }
    }
}

/// A running supervisor. Dropping it stops the supervisor in the background;
/// [`SupervisorHandle::shutdown`] stops it and waits.
#[derive(Debug)]
pub struct SupervisorHandle {
    stop: Option<oneshot::Sender<()>>,
    done: oneshot::Receiver<Result<()>>,
}

impl SupervisorHandle {
    /// Stops every agent session and the supervisor, and waits until they
    /// have stopped. Returns the error the supervisor stopped on, if it
    /// failed earlier.
    pub async fn shutdown(mut self) -> Result<()> {
        drop(self.stop.take());
        match (&mut self.done).await {
            Ok(result) => result,
            Err(_) => Err(Error::Supervisor("supervisor thread panicked".into())),
        }
    }
}

/// The agent supervisor; see the [module docs](self).
pub struct Supervisor<A: AgentAdapter> {
    adapter: A,
    log: EventLog,
    storage: Storage,
    config: SupervisorConfig,
    /// Seq of the last log event handled.
    cursor: Seq,
    /// The cursor as last persisted.
    stored_cursor: Seq,
    running: HashMap<SessionId, Running<A::Session>>,
}

/// One running agent session.
struct Running<S> {
    session: S,
    events: AdapterEvents,
    mapper: EventMapper,
    /// Seq of the `SessionStateChanged(AgentWorking)` that opened the current
    /// turn; `None` between turns.
    turn_started: Option<Seq>,
    /// Messages delivered whose turn has not ended yet.
    owed_turns: usize,
    /// When the session last ran out of work; `None` while it has some.
    idle_since: Option<Instant>,
}

impl<S> Running<S> {
    fn idle_deadline(&self, timeout: Duration) -> Option<Instant> {
        self.idle_since.map(|since| since + timeout)
    }
}

enum Wake {
    Stop,
    Log,
    Agent(SessionId, Option<AdapterEvent>),
    Idle,
}

impl<A: AgentAdapter> Supervisor<A> {
    /// Starts the supervisor on its own thread over the data directory at
    /// `data_dir` (opened with [`Storage::open`]; the supervisor uses its own
    /// connection) and the process's shared `log`.
    ///
    /// Returns once the previous run's leftovers are settled (see the
    /// [module docs](self)); the supervisor then follows the log until
    /// [`SupervisorHandle::shutdown`].
    pub async fn start(
        adapter: A,
        data_dir: impl Into<PathBuf>,
        log: EventLog,
        config: SupervisorConfig,
    ) -> Result<SupervisorHandle> {
        let data_dir = data_dir.into();
        let (ready_tx, ready_rx) = oneshot::channel();
        let (stop_tx, stop_rx) = oneshot::channel();
        let (done_tx, done_rx) = oneshot::channel();
        std::thread::Builder::new()
            .name("skywave-supervisor".into())
            .spawn(move || {
                let result = run_thread(adapter, data_dir, log, config, ready_tx, stop_rx);
                if let Err(error) = &result {
                    tracing::error!(%error, "agent supervisor stopped");
                }
                let _ = done_tx.send(result);
            })
            .map_err(|e| Error::Supervisor(format!("cannot spawn thread: {e}")))?;
        match ready_rx.await {
            Ok(Ok(())) => Ok(SupervisorHandle {
                stop: Some(stop_tx),
                done: done_rx,
            }),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(Error::Supervisor("supervisor thread panicked".into())),
        }
    }

    /// Settles what the previous run left open; see the module docs.
    fn recover(&mut self) -> Result<()> {
        let db = &mut self.storage.db;
        for approval in self.log.pending_approvals(db)? {
            self.log
                .cancel_approval(db, approval.id, DAEMON_RESTARTED)?;
        }
        for session in self.log.sessions(db)? {
            let settle = match last_state_event(db, session.id)? {
                Some(EventKind::SessionStateChanged(SessionState::AgentWorking)) => {
                    SessionState::Errored {
                        message: TURN_LOST.to_owned(),
                    }
                }
                Some(EventKind::TurnInterrupted) => SessionState::Idle,
                _ => continue,
            };
            tracing::info!(session = %session.id.0, ?settle, "settling session after restart");
            self.log
                .append(db, session.id, EventKind::SessionStateChanged(settle))?;
        }
        self.cursor = load_cursor(db)?;
        self.stored_cursor = self.cursor;
        Ok(())
    }

    async fn run(mut self, mut stop: oneshot::Receiver<()>) -> Result<()> {
        let mut head = self.log.subscribe();
        loop {
            head.borrow_and_update();
            if self.log.head() > self.cursor {
                self.follow().await?;
                continue;
            }
            let deadline = self.next_idle_deadline();
            let wake = tokio::select! {
                biased;
                _ = &mut stop => Wake::Stop,
                (id, event) = next_event(&mut self.running) => Wake::Agent(id, event),
                _ = head.changed() => Wake::Log,
                () = sleep_until(deadline) => Wake::Idle,
            };
            match wake {
                Wake::Stop => break,
                Wake::Log => {}
                Wake::Agent(id, event) => self.on_agent_event(id, event).await?,
                Wake::Idle => self.stop_idle().await?,
            }
        }
        let ids: Vec<SessionId> = self.running.keys().copied().collect();
        for id in ids {
            self.stop_session(id, false).await?;
        }
        Ok(())
    }

    /// Handles one batch of log events after the cursor.
    async fn follow(&mut self) -> Result<()> {
        let batch = self.log.read(&self.storage.db, self.cursor, BATCH)?;
        let mut seq = batch.from;
        for event in batch.events {
            self.on_log_event(seq, event).await?;
            self.cursor = seq;
            seq = seq.next();
        }
        self.store_cursor()
    }

    async fn on_log_event(&mut self, seq: Seq, event: Event) -> Result<()> {
        let session_id = event.session_id;
        match event.kind {
            EventKind::UserMessage { text, .. } => {
                // At most once: the cursor passes the message before the
                // agent gets it.
                self.cursor = seq;
                self.store_cursor()?;
                self.deliver(session_id, text).await?;
            }
            EventKind::ApprovalResolved { id, decision, .. } => {
                let Some(running) = self.running.get_mut(&session_id) else {
                    return Ok(());
                };
                // `None`: the approval belongs to an agent run that is gone,
                // and was cancelled when it went.
                let Some(request) = running.mapper.take_approval(id) else {
                    return Ok(());
                };
                if let Err(error) = running.session.answer_approval(request, decision).await {
                    tracing::warn!(session = %session_id.0, %error, "cannot answer approval");
                }
            }
            EventKind::TurnInterrupted => {
                let Some(running) = self.running.get_mut(&session_id) else {
                    return Ok(());
                };
                if running.turn_started.is_some_and(|started| started < seq)
                    && let Err(error) = running.session.interrupt().await
                {
                    tracing::warn!(session = %session_id.0, %error, "cannot interrupt agent");
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Gives `text` to the session's agent, starting it if needed. A session
    /// found closed is reaped and restarted once.
    async fn deliver(&mut self, session_id: SessionId, text: String) -> Result<()> {
        let mut failure = String::new();
        for _ in 0..2 {
            if !self.running.contains_key(&session_id)
                && let Err(message) = self.start_session(session_id).await?
            {
                failure = message;
                break;
            }
            let Some(running) = self.running.get_mut(&session_id) else {
                break;
            };
            match running.session.send_message(text.clone()).await {
                Ok(()) => {
                    running.owed_turns += 1;
                    running.idle_since = None;
                    return Ok(());
                }
                Err(error) => {
                    tracing::warn!(session = %session_id.0, %error, "cannot send message; restarting agent");
                    failure = format!("cannot deliver message: {error}");
                    self.stop_session(session_id, true).await?;
                }
            }
        }
        tracing::warn!(session = %session_id.0, %failure, "message not delivered");
        self.log.append(
            &mut self.storage.db,
            session_id,
            EventKind::SessionStateChanged(SessionState::Errored { message: failure }),
        )?;
        Ok(())
    }

    /// Starts or resumes the session's agent. `Err` carries why the agent
    /// could not start, for the log.
    async fn start_session(&mut self, session_id: SessionId) -> Result<Result<(), String>> {
        let Some(info) = self.log.session(&self.storage.db, session_id)? else {
            return Ok(Err(format!("unknown session {}", session_id.0)));
        };
        let resume = load_resume_handle(&self.storage.db, session_id)?;
        let resumed = resume.is_some();
        let spec = StartSpec {
            session_id,
            workdir: info.workdir.into(),
            resume,
        };
        match self.adapter.start(spec).await {
            Ok((session, events)) => {
                tracing::info!(session = %session_id.0, resumed, "agent started");
                let mapper = EventMapper::new(session_id);
                self.running.insert(
                    session_id,
                    Running {
                        session,
                        events,
                        mapper,
                        turn_started: None,
                        owed_turns: 0,
                        idle_since: None,
                    },
                );
                Ok(Ok(()))
            }
            Err(error) => {
                tracing::warn!(session = %session_id.0, %error, "agent did not start");
                Ok(Err(error.to_string()))
            }
        }
    }

    async fn on_agent_event(&mut self, id: SessionId, event: Option<AdapterEvent>) -> Result<()> {
        // A stream that ends without `Exited` is an agent that went away.
        let event = event.unwrap_or_else(|| AdapterEvent::Exited {
            error: Some("event stream ended".into()),
        });
        let exited = matches!(event, AdapterEvent::Exited { .. });
        self.map(id, event)?;
        if exited && let Some(running) = self.running.remove(&id) {
            tracing::info!(session = %id.0, "agent exited");
            // Reap it; it has already stopped.
            let _ = running.session.shutdown().await;
        }
        Ok(())
    }

    /// Records `event` from session `id`'s agent in the log.
    fn map(&mut self, id: SessionId, event: AdapterEvent) -> Result<()> {
        let Some(running) = self.running.get_mut(&id) else {
            return Ok(());
        };
        let started = matches!(event, AdapterEvent::TurnStarted);
        let ended = matches!(
            event,
            AdapterEvent::TurnEnded(_) | AdapterEvent::Exited { .. }
        );
        let seqs = running.mapper.apply(&self.log, &mut self.storage, event)?;
        if started && let Some(&seq) = seqs.first() {
            running.turn_started = Some(seq);
        }
        if ended {
            running.turn_started = None;
            running.owed_turns = running.owed_turns.saturating_sub(1);
            if running.owed_turns == 0 && !running.mapper.turn_open() {
                running.idle_since = Some(Instant::now());
            }
        }
        Ok(())
    }

    /// Shuts down session `id`'s agent and records what it still reports.
    /// With `map_exit` false (daemon shutdown) its final `Exited` is not
    /// recorded, leaving open turns and approvals for the next start.
    async fn stop_session(&mut self, id: SessionId, map_exit: bool) -> Result<()> {
        let Some(mut running) = self.running.remove(&id) else {
            return Ok(());
        };
        // Drain while it stops: an agent may block on a full event channel.
        let drain = async {
            let mut events = Vec::new();
            while let Some(event) = running.events.recv().await {
                let exited = matches!(event, AdapterEvent::Exited { .. });
                events.push(event);
                if exited {
                    break;
                }
            }
            events
        };
        let (stopped, mut events) = tokio::join!(running.session.shutdown(), drain);
        if let Err(error) = stopped {
            tracing::warn!(session = %id.0, %error, "agent shutdown failed");
        }
        if !matches!(events.last(), Some(AdapterEvent::Exited { .. })) {
            events.push(AdapterEvent::Exited { error: None });
        }
        if !map_exit {
            events.pop();
        }
        // Put the mapper back for `map`; the session itself is gone.
        let mut mapper = running.mapper;
        for event in events {
            mapper.apply(&self.log, &mut self.storage, event)?;
        }
        tracing::info!(session = %id.0, "agent stopped");
        Ok(())
    }

    async fn stop_idle(&mut self) -> Result<()> {
        let now = Instant::now();
        let timeout = self.config.idle_timeout;
        let idle: Vec<SessionId> = self
            .running
            .iter()
            .filter(|(_, r)| r.idle_deadline(timeout).is_some_and(|d| d <= now))
            .map(|(id, _)| *id)
            .collect();
        for id in idle {
            self.stop_session(id, true).await?;
        }
        Ok(())
    }

    fn next_idle_deadline(&self) -> Option<Instant> {
        self.running
            .values()
            .filter_map(|r| r.idle_deadline(self.config.idle_timeout))
            .min()
    }

    fn store_cursor(&mut self) -> Result<()> {
        if self.cursor != self.stored_cursor {
            store_cursor(&self.storage.db, self.cursor)?;
            self.stored_cursor = self.cursor;
        }
        Ok(())
    }
}

fn run_thread<A: AgentAdapter>(
    adapter: A,
    data_dir: PathBuf,
    log: EventLog,
    config: SupervisorConfig,
    ready: oneshot::Sender<Result<()>>,
    stop: oneshot::Receiver<()>,
) -> Result<()> {
    let setup = || -> Result<_> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| Error::Supervisor(format!("cannot build runtime: {e}")))?;
        let mut supervisor = Supervisor {
            adapter,
            log,
            storage: Storage::open(data_dir)?,
            config,
            cursor: Seq::ZERO,
            stored_cursor: Seq::ZERO,
            running: HashMap::new(),
        };
        supervisor.recover()?;
        Ok((runtime, supervisor))
    };
    let (runtime, supervisor) = match setup() {
        Ok(ready_parts) => {
            let _ = ready.send(Ok(()));
            ready_parts
        }
        Err(error) => {
            let message = error.to_string();
            let _ = ready.send(Err(error));
            return Err(Error::Supervisor(message));
        }
    };
    runtime.block_on(supervisor.run(stop))
}

/// The next event from any running agent session. Pending while none runs.
async fn next_event<S>(
    running: &mut HashMap<SessionId, Running<S>>,
) -> (SessionId, Option<AdapterEvent>) {
    poll_fn(|cx| {
        for (id, session) in running.iter_mut() {
            if let Poll::Ready(event) = session.events.poll_recv(cx) {
                return Poll::Ready((*id, event));
            }
        }
        Poll::Pending
    })
    .await
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// The session's newest `SessionStateChanged` or `TurnInterrupted`.
fn last_state_event(db: &Db, session_id: SessionId) -> Result<Option<EventKind>> {
    let kind: Option<String> = db
        .conn()
        .prepare_cached(
            "SELECT kind FROM events WHERE session_id = ?1 \
             AND json_extract(kind, '$.type') IN ('session_state_changed', 'turn_interrupted') \
             ORDER BY seq DESC LIMIT 1",
        )?
        .query_row([session_id.0.to_string()], |r| r.get(0))
        .optional()?;
    Ok(kind.as_deref().map(serde_json::from_str).transpose()?)
}

fn load_cursor(db: &Db) -> Result<Seq> {
    let seq: i64 =
        db.conn()
            .query_row("SELECT seq FROM supervisor_cursor WHERE id = 1", [], |r| {
                r.get(0)
            })?;
    u64::try_from(seq)
        .map(Seq)
        .map_err(|_| Error::Corrupt(format!("negative supervisor cursor {seq}")))
}

fn store_cursor(db: &Db, seq: Seq) -> Result<()> {
    let seq =
        i64::try_from(seq.0).map_err(|_| Error::Corrupt(format!("seq {} too large", seq.0)))?;
    db.conn()
        .prepare_cached("UPDATE supervisor_cursor SET seq = ?1 WHERE id = 1")?
        .execute([seq])?;
    Ok(())
}
