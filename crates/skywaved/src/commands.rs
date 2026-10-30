//! Command handling (spec §5): dedup, validation and append in one
//! transaction, so a [`CommandId`] has exactly one outcome however often it
//! is resent (D7).
//!
//! What each command appends, and the seq its `Accepted` names:
//! - `CreateSession` → `SessionCreated` with the canonical workdir and a new
//!   [`SessionId`];
//! - `SendMessage` → `UserMessage`;
//! - `ResolveApproval` → `ApprovalResolved`;
//! - `Interrupt` → `TurnInterrupted`;
//! - `StopSession` → `SessionStopped`;
//! - `RenameSession` → `SessionRenamed` with the normalised name;
//! - `SetSessionArchived` → `SessionArchived`.
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
//! **SetSessionArchived.** Accepted for any known session, even one already
//! so: the event carries the absolute value, never a toggle, so a repeat
//! changes nothing. The server keeps no archived flag (nothing here reads
//! it); clients fold [`EventKind::archived_effect`], under which a later
//! message brings the session back. Archiving does not stop the agent.

use rusqlite::{Connection, OptionalExtension, params};
use skywave_proto::{
    Command, CommandId, CommandKind, CommandResult, DeviceId, EventKind, RejectReason, Seq,
    SessionId,
};

use crate::db::Db;
use crate::dirs;
use crate::error::Result;
use crate::event_log::{self, EventLog};

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

    /// Validates `command` and appends its event, returning the event's seq
    /// or why the command is refused.
    fn apply(
        &self,
        tx: &Connection,
        device: DeviceId,
        command: &Command,
    ) -> Result<Result<Seq, RejectReason>> {
        let (session_id, kind) = match &command.kind {
            CommandKind::CreateSession { workdir } => {
                match dirs::canonical_under_roots(self.roots(), workdir) {
                    Ok(workdir) => (SessionId::new(), EventKind::SessionCreated { workdir }),
                    Err(reason) => return Ok(Err(reason)),
                }
            }
            CommandKind::SendMessage { session_id, text } => {
                if event_log::session_in(tx, *session_id)?.is_none() {
                    return Ok(Err(RejectReason::UnknownSession));
                }
                let kind = EventKind::UserMessage {
                    command_id: command.id,
                    device_id: device,
                    text: text.clone(),
                };
                (*session_id, kind)
            }
            CommandKind::ResolveApproval { id, decision } => {
                // A cancelled approval is closed too: no answer can win it.
                let session_id = match event_log::approval_in(tx, *id)? {
                    None => return Ok(Err(RejectReason::UnknownApproval)),
                    Some((_, Some(_closed))) => {
                        return Ok(Err(RejectReason::ApprovalAlreadyResolved));
                    }
                    Some((session_id, None)) => session_id,
                };
                let kind = EventKind::ApprovalResolved {
                    id: *id,
                    decision: *decision,
                    by: device,
                };
                (session_id, kind)
            }
            CommandKind::Interrupt { session_id } => {
                match event_log::session_in(tx, *session_id)? {
                    None => return Ok(Err(RejectReason::UnknownSession)),
                    Some(session) if !session.turn_active => {
                        return Ok(Err(RejectReason::NoActiveTurn));
                    }
                    Some(_) => {}
                }
                (*session_id, EventKind::TurnInterrupted)
            }
            CommandKind::StopSession { session_id } => {
                if event_log::session_in(tx, *session_id)?.is_none() {
                    return Ok(Err(RejectReason::UnknownSession));
                }
                (*session_id, EventKind::SessionStopped)
            }
            CommandKind::RenameSession { session_id, name } => {
                if event_log::session_in(tx, *session_id)?.is_none() {
                    return Ok(Err(RejectReason::UnknownSession));
                }
                let kind = EventKind::SessionRenamed {
                    name: name
                        .as_deref()
                        .and_then(skywave_proto::normalise_session_name),
                    by: device,
                };
                (*session_id, kind)
            }
            CommandKind::SetSessionArchived {
                session_id,
                archived,
            } => {
                if event_log::session_in(tx, *session_id)?.is_none() {
                    return Ok(Err(RejectReason::UnknownSession));
                }
                let kind = EventKind::SessionArchived {
                    archived: *archived,
                    by: device,
                };
                (*session_id, kind)
            }
        };
        Ok(Ok(event_log::append_in(tx, session_id, &kind)?))
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
