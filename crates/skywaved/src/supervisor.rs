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
//! - `SessionStopped` → [`AgentSession::interrupt`] if a turn runs or a
//!   delivered message's turn has not ended, then
//!   [`AgentSession::shutdown`]. The agent reports `TurnEnded(Interrupted)`
//!   (so the mapper appends `SessionStateChanged(Idle)`) and `Exited`. A
//!   session whose agent is not running is left alone. Like an idle
//!   shutdown, the next `UserMessage` resumes the agent.
//! - `PermissionModeRequested` / `ModelRequested` / `EffortRequested` →
//!   [`AgentSession::set_permission_mode`] / [`AgentSession::set_model`] /
//!   [`AgentSession::set_effort`], noted with the mapper so the agent's answer names the command. A
//!   session whose agent is not running is started for it (no message is
//!   sent, so no turn runs) and kept like any idle session. Only the
//!   session's open request goes out, once per agent: one a newer request
//!   replaced before the supervisor reached it is skipped, so requests
//!   never land out of order, and one the agent's start already asked for
//!   is not asked again.
//!
//! **Mode, model and effort.** Every agent starts with the mode, model and
//! effort the session's log wants
//! ([`Setting::wanted`](skywave_proto::Setting::wanted)): an open request,
//! else what the agent last ran with (an effort its model takes none of is
//! not asked for). So a restarted or resumed agent keeps all three, and a start made for a request carries that
//! request out; each is asked for under the open request's command, if
//! any, so its answer settles that request. An agent start that fails
//! refuses the request it was made for. The models an agent offers are
//! logged as `ModelsOffered` only when they differ from the newest list in
//! the whole log.
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
//! - a mode, model or effort request still open whose event the supervisor had
//!   already handled is refused ([`DAEMON_RESTARTED`]): the agent it was
//!   handed to is gone. One after the cursor is handled as usual.
//!
//! [`SupervisorHandle::shutdown`] stops the agents without interrupting them
//! ([`AgentSession::shutdown`] abandons a running turn, so the agent cancels
//! none of its approvals) and does not record their exit. Open turns and
//! approvals stay as they are in the log, and the next start settles them the
//! same way whether the daemon stopped cleanly or crashed. A pending approval
//! cannot outlive the daemon: the agent's request for it lives only in the
//! agent process (D8).
//!
//! **Threading.** SQLite and the mapper block, so the supervisor runs on its
//! own thread with a current-thread runtime and its own [`Storage`]; agent
//! sessions (and any tasks their adapter spawns) live on that runtime. All
//! work is serialized there: agent requests are awaited one at a time, so an
//! adapter must return from them promptly (they only tell the agent).

use std::collections::{HashMap, HashSet};
use std::future::poll_fn;
use std::path::PathBuf;
use std::task::Poll;
use std::time::Duration;

use rusqlite::OptionalExtension;
use skywave_proto::{
    CommandId, Effort, Event, EventKind, ModelChoice, PermissionMode, Seq, SessionId, SessionState,
};
use tokio::sync::oneshot;
use tokio::time::Instant;

use crate::agent::{
    AdapterEvent, AdapterEvents, AgentAdapter, AgentSession, EventMapper, StartSpec,
    bounded_models, load_resume_handle,
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
    /// The models of the newest `ModelsOffered` in the log.
    models: Option<Vec<ModelChoice>>,
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
    /// The setting requests this agent was asked for, by command.
    asked: HashSet<Option<CommandId>>,
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
        for session in self.log.sessions(db)? {
            let settings = self.log.agent_settings(db, session.id)?;
            let handled = |command| -> Result<bool> {
                Ok(self
                    .log
                    .command_seq(db, command)?
                    .is_some_and(|seq| seq <= self.cursor))
            };
            let mut lost = Vec::new();
            if let Some(request) = &settings.mode.requested
                && handled(request.command)?
            {
                lost.push(EventKind::PermissionModeRefused {
                    mode: request.value,
                    reason: DAEMON_RESTARTED.to_owned(),
                    answers: Some(request.command),
                });
            }
            if let Some(request) = &settings.model.requested
                && handled(request.command)?
            {
                lost.push(EventKind::ModelRefused {
                    model: request.value.clone(),
                    reason: DAEMON_RESTARTED.to_owned(),
                    answers: Some(request.command),
                });
            }
            if let Some(request) = settings.effort.requested_effort()
                && handled(request.command)?
            {
                lost.push(EventKind::EffortRefused {
                    effort: request.value,
                    reason: DAEMON_RESTARTED.to_owned(),
                    answers: Some(request.command),
                });
            }
            for kind in lost {
                tracing::info!(session = %session.id.0, ?kind, "refusing a request lost with the daemon");
                self.log.append(db, session.id, kind)?;
            }
        }
        self.models = self.log.offered_models(db)?;
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
            EventKind::ApprovalResolved {
                id, decision, rule, ..
            } => {
                let Some(running) = self.running.get_mut(&session_id) else {
                    return Ok(());
                };
                // `None`: the approval belongs to an agent run that is gone,
                // and was cancelled when it went.
                let Some(request) = running.mapper.take_approval(id) else {
                    return Ok(());
                };
                if let Err(error) = running
                    .session
                    .answer_approval(request, decision, rule)
                    .await
                {
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
            EventKind::SessionStopped => {
                let Some(running) = self.running.get_mut(&session_id) else {
                    return Ok(());
                };
                // Shutting down mid-turn would abandon the turn and log it as
                // lost; end it as interrupted instead. A delivered message
                // counts too: its turn may have begun before we heard.
                if (running.turn_started.is_some() || running.owed_turns > 0)
                    && let Err(error) = running.session.interrupt().await
                {
                    tracing::warn!(session = %session_id.0, %error, "cannot interrupt agent");
                }
                tracing::info!(session = %session_id.0, "stopping agent on request");
                self.stop_session(session_id, true).await?;
            }
            EventKind::PermissionModeRequested { mode, .. } => {
                self.set(session_id, event.command_id, Setting::Mode(mode))
                    .await?;
            }
            EventKind::ModelRequested { model, .. } => {
                self.set(session_id, event.command_id, Setting::Model(model))
                    .await?;
            }
            EventKind::EffortRequested { effort, .. } => {
                self.set(session_id, event.command_id, Setting::Effort(effort))
                    .await?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Asks the session's agent for `setting` on behalf of `command`. An
    /// agent that is not running is started, and the start asks for what
    /// the log wants, this request included.
    async fn set(
        &mut self,
        session_id: SessionId,
        command: Option<CommandId>,
        setting: Setting,
    ) -> Result<()> {
        if !self.running.contains_key(&session_id) {
            if let Err(message) = self.start_session(session_id).await? {
                tracing::warn!(session = %session_id.0, %message, "agent did not start for a setting");
                let reason = format!("agent did not start: {message}");
                let kind = match setting {
                    Setting::Mode(mode) => EventKind::PermissionModeRefused {
                        mode,
                        reason,
                        answers: command,
                    },
                    Setting::Model(model) => EventKind::ModelRefused {
                        model,
                        reason,
                        answers: command,
                    },
                    Setting::Effort(effort) => EventKind::EffortRefused {
                        effort,
                        reason,
                        answers: command,
                    },
                };
                self.log.append(&mut self.storage.db, session_id, kind)?;
            }
            return Ok(());
        }
        // Only the open request is carried out, once: a newer one replaced
        // it (and goes out after it), or the start already asked for it.
        let settings = self.log.agent_settings(&self.storage.db, session_id)?;
        let open = match &setting {
            Setting::Mode(_) => settings.mode.requested.as_ref().map(|r| r.command),
            Setting::Model(_) => settings.model.requested.as_ref().map(|r| r.command),
            Setting::Effort(_) => settings.effort.requested_effort().map(|r| r.command),
        };
        let Some(running) = self.running.get_mut(&session_id) else {
            return Ok(());
        };
        if command.is_none() || open != command || running.asked.contains(&command) {
            tracing::debug!(session = %session_id.0, "skipping a setting request already asked or replaced");
            return Ok(());
        }
        running.asked.insert(command);
        let sent = match setting {
            Setting::Mode(mode) => {
                running.mapper.asked_mode(mode, command);
                running.session.set_permission_mode(mode).await
            }
            Setting::Model(model) => {
                running.mapper.asked_model(model.clone(), command);
                running.session.set_model(model).await
            }
            Setting::Effort(effort) => {
                running.mapper.asked_effort(effort, command);
                running.session.set_effort(effort).await
            }
        };
        if let Err(error) = sent {
            // The agent is gone; its `Exited` refuses what it was asked.
            tracing::warn!(session = %session_id.0, %error, "cannot ask the agent for a setting");
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
        let settings = self.log.agent_settings(&self.storage.db, session_id)?;
        let mode = settings.mode.wanted().copied();
        let model = settings.model.wanted().cloned();
        // An effort the running model takes none of, like an unknown one,
        // asks for nothing.
        let effort = settings.effort.wanted().copied().flatten();
        let spec = StartSpec {
            session_id,
            workdir: info.workdir.into(),
            resume,
            mode,
            model: model.clone(),
            effort,
        };
        match self.adapter.start(spec).await {
            Ok((session, events)) => {
                tracing::info!(session = %session_id.0, resumed, "agent started");
                let mut mapper = EventMapper::new(session_id, settings.clone());
                let mut asked = HashSet::new();
                if let Some(mode) = mode {
                    let command = settings.mode.requested.as_ref().map(|r| r.command);
                    mapper.asked_mode(mode, command);
                    asked.insert(command);
                }
                if let Some(model) = model {
                    let command = settings.model.requested.as_ref().map(|r| r.command);
                    mapper.asked_model(model, command);
                    asked.insert(command);
                }
                if let Some(effort) = effort {
                    let command = settings.effort.requested_effort().map(|r| r.command);
                    mapper.asked_effort(effort, command);
                    asked.insert(command);
                }
                self.running.insert(
                    session_id,
                    Running {
                        session,
                        events,
                        mapper,
                        turn_started: None,
                        owed_turns: 0,
                        // Nothing to do until a message comes, if one does.
                        idle_since: Some(Instant::now()),
                        asked,
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
        if let AdapterEvent::ModelsOffered(models) = event {
            return offer_models(&self.log, &mut self.storage, &mut self.models, id, models);
        }
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
            if let AdapterEvent::ModelsOffered(models) = event {
                offer_models(&self.log, &mut self.storage, &mut self.models, id, models)?;
            } else {
                mapper.apply(&self.log, &mut self.storage, event)?;
            }
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
            models: None,
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

/// A setting a device asked a session's agent for.
enum Setting {
    Mode(PermissionMode),
    Model(String),
    Effort(Effort),
}

/// Logs `models`, offered by session `id`'s agent, as `ModelsOffered` when
/// they differ from `known`, the newest list in the log, and makes them so.
fn offer_models(
    log: &EventLog,
    storage: &mut Storage,
    known: &mut Option<Vec<ModelChoice>>,
    id: SessionId,
    models: Vec<ModelChoice>,
) -> Result<()> {
    let models = bounded_models(models);
    if models.is_empty() || known.as_ref() == Some(&models) {
        return Ok(());
    }
    log.append(
        &mut storage.db,
        id,
        EventKind::ModelsOffered {
            models: models.clone(),
        },
    )?;
    *known = Some(models);
    Ok(())
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
