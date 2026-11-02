//! Command handling (spec §5): dedup, validation and append in one
//! transaction, so a [`CommandId`] has exactly one outcome however often it
//! is resent (D7).
//!
//! What each command appends, and the seq its `Accepted` names:
//! - `CreateSession` → `SessionCreated` with the canonical workdir, a new
//!   [`SessionId`], a generated name for its first context and, when it
//!   adopts an outside conversation, `from` = [`SessionOrigin::Agent`] with
//!   the checked id;
//! - `SendMessage` → `UserMessage`;
//! - `ResolveApproval` → `ApprovalResolved`, echoing the granted `rule`;
//! - `Interrupt` → `TurnInterrupted`;
//! - `StopSession` → `SessionStopped`;
//! - `RenameSession` → `SessionRenamed` with the normalised name;
//! - `SetSessionArchived` → `SessionArchived`;
//! - `SetPermissionMode` → `PermissionModeRequested`;
//! - `SetModel` → `ModelRequested` with the normalised model;
//! - `SetEffort` → `EffortRequested`;
//! - `ClearContext` → `ContextStarted` with the normalised name;
//! - `SwitchContext` → `ContextSwitched`;
//! - `StopTask` → `TaskStopRequested`;
//! - `MoveContext` → `SessionCreated { from }` ([`SessionOrigin::Context`])
//!   for the new session, naming no command, then `ContextMoved` in the
//!   source.
//!
//! Each appends exactly one event naming the command
//! ([`Event::command_id`](skywave_proto::Event::command_id)), the last it
//! appends, and nothing else but the new session's `SessionCreated` a
//! `MoveContext` logs before it. So the log acknowledges an accepted
//! command as its `Accepted` does: a device whose reply was lost settles
//! the command when it pulls that event, and batches are contiguous, so it
//! has whatever the command appended before it too. A rejected command
//! appends nothing; only its stored result answers it.
//!
//! **Context stamps.** The server stamps each event with the context it
//! belongs to ([`Event::context`](skywave_proto::Event::context)):
//! `SessionCreated` with the session's first context, which has the
//! session's id; `ContextStarted` and `ContextSwitched` with the context
//! they make current; `ApprovalResolved` with the approval's context;
//! `TurnInterrupted` with the context of the running turn, which after a
//! switch is the old context until its agent reports the turn over;
//! `ContextMoved` with the context it moves; every other event with the
//! session's current context.
//!
//! **CreateSession.** Refused, in this order: the workdir must canonicalise
//! under a root (`WorkdirNotAllowed`, `WorkdirNotFound`); then a
//! `conversation` must pass [`skywave_proto::check_conversation_id`], else
//! `BadConversation`. That is a syntax check only, and the id is logged
//! trimmed, never cut. Whether the agent has such a conversation in the
//! workdir is not checked here, and cannot be: the log has no agent
//! adapter, the transaction is synchronous, and the conversation lives in
//! the agent's own storage. The agent settles it when the session first
//! starts.
//!
//! **SendMessage.** `ContextChanged` when its `context` is no longer the
//! session's current one: the message was written for a conversation the
//! agent has left.
//!
//! **ClearContext.** The new context's id is the client's, so an offline
//! device can address it before the server answers; one that is already a
//! context of any session is `ContextExists` (a resend of the same command
//! is answered by its stored result first). The name is logged as
//! [`skywave_proto::normalise_context_name`] leaves it; one that ends up
//! empty is replaced by a generated name, never refused: a refusal would
//! turn the message the device queued next into `ContextChanged`.
//!
//! **SwitchContext.** Absolute: switching to the current context is
//! accepted and logged. A context of another session, or none, is
//! `UnknownContext`; a moved one is `ContextMoved`.
//!
//! **MoveContext.** Refused, in this order: `UnknownSession`;
//! `UnknownContext` for a context not of the session; `ContextMoved` for
//! one already moved; `ContextCurrent` for the session's current context
//! (a client clears first to move it, D30); `NoConversation` for one with
//! no resume handle (the adapters store it before `AgentWorking` is
//! logged, so every context that had a turn has one); then the source's
//! stored workdir must still canonicalise under a root
//! (`WorkdirNotAllowed`, `WorkdirNotFound`) and to itself, else
//! `WorkdirNotFound`: the new session runs in that path verbatim, where
//! the agent finds the conversation it forks. The new session's id is the
//! server's; its first context is named as `ClearContext` names one. A
//! moved context is never current again, so no `SendMessage` can reach
//! it.
//!
//! **Interrupt.** `TurnInterrupted` is appended when the command is
//! accepted, not when the agent has actually stopped. The result is then
//! settled inside the command's transaction like every other command, and
//! `Accepted { seq }` names the command's own event. The event records that
//! the turn was interrupted: from that seq the session has no active turn, so
//! a second `Interrupt` is `NoActiveTurn`. The supervisor (T10) reacts to the
//! event by interrupting the agent, cancels approvals the agent abandons
//! with [`EventLog::cancel_approval`], and appends
//! `SessionStateChanged(Idle)` once the agent has stopped.
//!
//! **StopSession.** Accepted for any known session: whether its agent runs
//! is the supervisor's business, not the log's, and stopping an agent that
//! is not running does nothing. The supervisor reacts to `SessionStopped` by
//! shutting the agent down (interrupting a running turn first); what the
//! agent still reports, such as `TurnEnded(Interrupted)`, is logged as usual.
//!
//! **RenameSession.** Accepted for any known session, even with its current
//! name: the event carries the absolute name, so a resend (deduped on the
//! command id anyway) or a repeat changes nothing, and the last rename in log
//! order wins on every device. The name is logged as
//! [`skywave_proto::normalise_session_name`] leaves it; one that ends up
//! empty restores the default title.
//!
//! **ResolveApproval.** The first answer wins (D5): a later one is
//! `ApprovalAlreadyResolved`. A cancelled approval is closed too, and an
//! answer to it is `ApprovalCancelled`, so the device can tell an answer
//! that came too late from one another device beat. A `rule` must come with an approval and index one of the
//! request's suggestions, else the answer is `UnknownRule`: the log never
//! claims a rule the agent was not offered.
//!
//! **SetSessionArchived.** Accepted for any known session, even one already
//! so: the event carries the absolute value, never a toggle, so a repeat
//! changes nothing. The server keeps no archived flag (nothing here reads
//! it); clients fold [`EventKind::archived_effect`], under which a later
//! message brings the session back. Archiving does not stop the agent.
//!
//! **SetPermissionMode, SetModel, SetEffort.** Accepted for any known session, whether
//! its agent runs or not, even asking for what it already has: the event
//! records the request, not its outcome. The supervisor reacts by asking
//! the agent (starting it if needed), and logs its answer as a `…Changed`
//! or `…Refused` naming the command. A model is logged as
//! [`skywave_proto::normalise_model`] leaves it; a blank one is
//! `UnknownModel`.
//!
//! **StopTask.** `UnknownTask` unless the task is in the session's live
//! set as last logged (the folded `BackgroundTasksChanged`): a definitive
//! check, so a device with a stale list cannot stop a task that already
//! ended. Accepted whether or not the agent runs. The supervisor asks a
//! running agent to stop the task and never starts one for it; the task
//! leaving the next `BackgroundTasksChanged` is the success, a
//! `TaskStopRefused` naming the command the failure, `"agent not running"`
//! among them.

use rusqlite::{Connection, OptionalExtension, params};
use skywave_proto::{
    Command, CommandId, CommandKind, CommandResult, ContextId, ContextRef, Decision, DeviceId,
    EventKind, RejectReason, Seq, SessionId, SessionOrigin, check_conversation_id, context_name,
    normalise_context_name,
};

use crate::db::Db;
use crate::dirs;
use crate::error::Result;
use crate::event_log::{self, Closed, EventLog};

impl EventLog {
    /// Handles `command` from `device` and returns its result.
    ///
    /// A known command id returns the stored result, `Rejected` included,
    /// and appends nothing. Otherwise the command is validated, its event
    /// appended and its result stored in one transaction. `Err` means the
    /// database failed and nothing was recorded, so a resend is still safe.
    ///
    /// Blocking: it runs SQLite and, for `CreateSession`, file system calls.
    pub fn handle(
        &self,
        db: &mut Db,
        device: DeviceId,
        command: &Command,
    ) -> Result<CommandResult> {
        let tx = event_log::begin(db)?;
        if let Some(stored) = stored_result(&tx, command.id)? {
            return Ok(stored);
        }
        let result = match self.apply(&tx, device, command)? {
            Ok(seq) => CommandResult::Accepted { seq },
            Err(reason) => CommandResult::Rejected { reason },
        };
        tx.execute(
            "INSERT INTO commands (command_id, device_id, result) VALUES (?1, ?2, ?3)",
            params![
                command.id.0.to_string(),
                device.0.to_string(),
                serde_json::to_string(&result)?
            ],
        )?;
        tx.commit()?;
        match &result {
            CommandResult::Accepted { seq } => {
                tracing::debug!(command = %command.id.0, seq = seq.0, "command accepted");
                self.published(*seq);
            }
            CommandResult::Rejected { reason } => {
                tracing::debug!(command = %command.id.0, ?reason, "command rejected");
            }
        }
        Ok(result)
    }

    /// Validates `command` and appends its events, returning the seq of the
    /// one naming it, the last, or why the command is refused.
    fn apply(
        &self,
        tx: &Connection,
        device: DeviceId,
        command: &Command,
    ) -> Result<Result<Seq, RejectReason>> {
        let known = |id: SessionId| event_log::session_in(tx, id);
        let (session_id, context, kind) = match &command.kind {
            CommandKind::CreateSession {
                workdir,
                conversation,
            } => {
                let workdir = match dirs::canonical_under_roots(self.roots(), workdir) {
                    Ok(workdir) => workdir,
                    Err(reason) => return Ok(Err(reason)),
                };
                let from = match conversation.as_deref().map(check_conversation_id) {
                    None => None,
                    Some(Some(id)) => Some(SessionOrigin::Agent {
                        conversation: id.to_owned(),
                    }),
                    Some(None) => return Ok(Err(RejectReason::BadConversation)),
                };
                let session = SessionId::new();
                let kind = EventKind::SessionCreated {
                    workdir,
                    context_name: context_name::random(),
                    from,
                };
                (session, ContextId::first_of(session), kind)
            }
            CommandKind::SendMessage {
                session_id,
                context,
                text,
            } => {
                let Some(session) = known(*session_id)? else {
                    return Ok(Err(RejectReason::UnknownSession));
                };
                if session.context != *context {
                    return Ok(Err(RejectReason::ContextChanged));
                }
                let kind = EventKind::UserMessage {
                    device_id: device,
                    text: text.clone(),
                };
                (*session_id, session.context, kind)
            }
            CommandKind::ResolveApproval { id, decision, rule } => {
                let Some(approval) = event_log::approval_in(tx, *id)? else {
                    return Ok(Err(RejectReason::UnknownApproval));
                };
                match approval.closed {
                    Some(Closed::Resolved) => {
                        return Ok(Err(RejectReason::ApprovalAlreadyResolved));
                    }
                    Some(Closed::Cancelled) => return Ok(Err(RejectReason::ApprovalCancelled)),
                    None => {}
                }
                let offered = |rule: u8| usize::from(rule) < approval.rules;
                if rule.is_some_and(|rule| *decision == Decision::Deny || !offered(rule)) {
                    return Ok(Err(RejectReason::UnknownRule));
                }
                let kind = EventKind::ApprovalResolved {
                    id: *id,
                    decision: *decision,
                    by: device,
                    rule: *rule,
                };
                (approval.session_id, approval.context, kind)
            }
            CommandKind::Interrupt { session_id } => {
                let session = match known(*session_id)? {
                    None => return Ok(Err(RejectReason::UnknownSession)),
                    Some(session) if !session.turn_active => {
                        return Ok(Err(RejectReason::NoActiveTurn));
                    }
                    Some(session) => session,
                };
                (
                    *session_id,
                    session.running_turn_context(),
                    EventKind::TurnInterrupted,
                )
            }
            CommandKind::ClearContext {
                session_id,
                context,
                name,
            } => {
                if known(*session_id)?.is_none() {
                    return Ok(Err(RejectReason::UnknownSession));
                }
                if event_log::context_session_in(tx, *context)?.is_some() {
                    return Ok(Err(RejectReason::ContextExists));
                }
                let kind = EventKind::ContextStarted {
                    by: device,
                    name: normalise_context_name(name).unwrap_or_else(context_name::random),
                };
                (*session_id, *context, kind)
            }
            CommandKind::SwitchContext {
                session_id,
                context,
            } => {
                if known(*session_id)?.is_none() {
                    return Ok(Err(RejectReason::UnknownSession));
                }
                let Some(row) = event_log::context_in(tx, *context)?
                    .filter(|row| row.session_id == *session_id)
                else {
                    return Ok(Err(RejectReason::UnknownContext));
                };
                if row.moved_to.is_some() {
                    return Ok(Err(RejectReason::ContextMoved));
                }
                (
                    *session_id,
                    *context,
                    EventKind::ContextSwitched { by: device },
                )
            }
            CommandKind::MoveContext {
                session_id,
                context,
                name,
            } => {
                let Some(session) = known(*session_id)? else {
                    return Ok(Err(RejectReason::UnknownSession));
                };
                let Some(row) = event_log::context_in(tx, *context)?
                    .filter(|row| row.session_id == *session_id)
                else {
                    return Ok(Err(RejectReason::UnknownContext));
                };
                if row.moved_to.is_some() {
                    return Ok(Err(RejectReason::ContextMoved));
                }
                if session.context == *context {
                    return Ok(Err(RejectReason::ContextCurrent));
                }
                if !row.has_handle {
                    return Ok(Err(RejectReason::NoConversation));
                }
                // The agent finds the conversation it forks by the workdir it
                // ran in, so the new session runs in that very path: one that
                // canonicalises elsewhere now (a symlink on it changed) is no
                // longer the directory the source's conversation lives in.
                match dirs::canonical_under_roots(self.roots(), &session.workdir) {
                    Ok(canonical) if canonical == session.workdir => {}
                    Ok(_) => return Ok(Err(RejectReason::WorkdirNotFound)),
                    Err(reason) => return Ok(Err(reason)),
                }
                // The new session's event first, naming no command: the
                // command names itself on the source's `ContextMoved`, the
                // last event it appends, so a device that settles it from
                // the log already has the new session too.
                let moved_to = SessionId::new();
                event_log::append_in(
                    tx,
                    moved_to,
                    ContextId::first_of(moved_to),
                    None,
                    &EventKind::SessionCreated {
                        workdir: session.workdir,
                        context_name: normalise_context_name(name)
                            .unwrap_or_else(context_name::random),
                        from: Some(SessionOrigin::Context(ContextRef {
                            session: *session_id,
                            context: *context,
                        })),
                    },
                )?;
                (
                    *session_id,
                    *context,
                    EventKind::ContextMoved {
                        to: moved_to,
                        by: device,
                    },
                )
            }
            CommandKind::StopSession { session_id } => {
                let Some(session) = known(*session_id)? else {
                    return Ok(Err(RejectReason::UnknownSession));
                };
                (*session_id, session.context, EventKind::SessionStopped)
            }
            CommandKind::RenameSession { session_id, name } => {
                let Some(session) = known(*session_id)? else {
                    return Ok(Err(RejectReason::UnknownSession));
                };
                let kind = EventKind::SessionRenamed {
                    name: name
                        .as_deref()
                        .and_then(skywave_proto::normalise_session_name),
                    by: device,
                };
                (*session_id, session.context, kind)
            }
            CommandKind::SetSessionArchived {
                session_id,
                archived,
            } => {
                let Some(session) = known(*session_id)? else {
                    return Ok(Err(RejectReason::UnknownSession));
                };
                let kind = EventKind::SessionArchived {
                    archived: *archived,
                    by: device,
                };
                (*session_id, session.context, kind)
            }
            CommandKind::SetPermissionMode { session_id, mode } => {
                let Some(session) = known(*session_id)? else {
                    return Ok(Err(RejectReason::UnknownSession));
                };
                let kind = EventKind::PermissionModeRequested {
                    mode: *mode,
                    by: device,
                };
                (*session_id, session.context, kind)
            }
            CommandKind::SetModel { session_id, model } => {
                let Some(session) = known(*session_id)? else {
                    return Ok(Err(RejectReason::UnknownSession));
                };
                let Some(model) = skywave_proto::normalise_model(model) else {
                    return Ok(Err(RejectReason::UnknownModel));
                };
                (
                    *session_id,
                    session.context,
                    EventKind::ModelRequested { model, by: device },
                )
            }
            CommandKind::SetEffort { session_id, effort } => {
                let Some(session) = known(*session_id)? else {
                    return Ok(Err(RejectReason::UnknownSession));
                };
                let kind = EventKind::EffortRequested {
                    effort: *effort,
                    by: device,
                };
                (*session_id, session.context, kind)
            }
            CommandKind::StopTask { session_id, task } => {
                let Some(session) = known(*session_id)? else {
                    return Ok(Err(RejectReason::UnknownSession));
                };
                let work = event_log::agent_work_in(tx, *session_id)?;
                if !work.tasks.iter().any(|live| live.id == *task) {
                    return Ok(Err(RejectReason::UnknownTask));
                }
                let kind = EventKind::TaskStopRequested {
                    task: task.clone(),
                    by: device,
                };
                (*session_id, session.context, kind)
            }
        };
        Ok(Ok(event_log::append_in(
            tx,
            session_id,
            context,
            Some(command.id),
            &kind,
        )?))
    }
}

fn stored_result(conn: &Connection, id: CommandId) -> Result<Option<CommandResult>> {
    let json: Option<String> = conn
        .query_row(
            "SELECT result FROM commands WHERE command_id = ?1",
            [id.0.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    Ok(json.as_deref().map(serde_json::from_str).transpose()?)
}
