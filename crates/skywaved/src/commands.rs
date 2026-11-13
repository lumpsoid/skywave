//! Command handling (architecture.md "Server"): dedup, validation and append in one
//! transaction, so a [`CommandId`] has exactly one outcome however often it
//! is resent (D7).
//!
//! What each command appends, and the seq its `Accepted` names:
//! - `CreateSession` → `SessionCreated` with the canonical workdir, a new
//!   [`SessionId`] and a generated name for its first context; when it
//!   adopts an outside conversation, that `SessionCreated` has `from` =
//!   [`SessionOrigin::Agent`] with the checked id and names no command, and
//!   is followed by an `Imported` for each item of the conversation's
//!   history, then `ImportFinished` with the import's outcome;
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
//!   source;
//! - `RunShell` → `ShellRequested` with the command verbatim;
//! - `StopShell` → `ShellStopRequested`;
//! - `DeleteBlob` → `BlobDeleted` with what the blob held;
//! - `CropBlob` → `BlobCropped` with what the blob held and the new blob.
//!
//! Each appends exactly one event naming the command
//! ([`Event::command_id`](skywave_proto::Event::command_id)), the last it
//! appends, and nothing else but the new session's `SessionCreated` a
//! `MoveContext` logs before it, and an adopting `CreateSession`'s
//! `SessionCreated` and `Imported`s before its `ImportFinished`. Every
//! adopting create's own event is its `ImportFinished`, whatever the import's
//! outcome. So the log acknowledges an accepted
//! command as its `Accepted` does: a device whose reply was lost settles
//! the command when it pulls that event, and batches are contiguous, so it
//! has whatever the command appended before it too. A rejected command
//! appends nothing; only its stored result answers it.
//!
//! **A low disk** ([`crate::disk`]): a command that starts new work reads
//! the free space ([`Disk::refresh`](crate::disk::Disk::refresh): a reading
//! up to 5 s old is reused) and, while the disk is low, is `DiskLow` before
//! anything else is checked:
//! `CreateSession`, `SendMessage`, `SetPermissionMode`, `SetModel`,
//! `SetEffort`, `ClearContext`, `MoveContext` and `RunShell`. The rest is
//! control the user needs while the agents are paused, or one small event
//! that starts nothing, and is handled as usual: `ResolveApproval`,
//! `Interrupt`, `StopSession`, `StopTask`, `StopShell`, `RenameSession`,
//! `SetSessionArchived`, `SwitchContext`, `DeleteBlob` and `CropBlob`
//! (freeing space is what a low disk needs). The refusal is stored like any
//! other, so the command keeps it; the user sends the work again once the
//! server has room.
//!
//! **A server stopping after its running work** ([`crate::control`]): the
//! same commands are `ServerStopping`, checked first and inside the
//! command's write transaction, until the server stops or the stop is
//! cancelled. Control is taken as usual, so the running turns can end.
//!
//! **Context stamps.** The server stamps each event with the context it
//! belongs to ([`Event::context`](skywave_proto::Event::context)):
//! `SessionCreated` with the session's first context, which has the
//! session's id, and so an adopting create's `Imported`s and
//! `ImportFinished`; `ContextStarted` and `ContextSwitched` with the context
//! they make current; `ApprovalResolved` with the approval's context;
//! `TurnInterrupted` with the context of the running turn, which after a
//! switch is the old context until its agent reports the turn over;
//! `ContextMoved` with the context it moves; `BlobDeleted` and
//! `BlobCropped` with the session and context of the event that referenced
//! the blob; every other event with the session's current context.
//!
//! **CreateSession.** Refused, in this order: the workdir must canonicalise
//! under a root (`WorkdirNotAllowed`, `WorkdirNotFound`); then a
//! `conversation` must pass [`skywave_proto::check_conversation_id`], else
//! `BadConversation`. That is a syntax check only, and the id is logged
//! trimmed, never cut. Whether the agent has such a conversation in the
//! workdir is not checked here: the agent settles it when the session
//! first starts. Its history is imported before the transaction (D37): the
//! API reads it off the database within
//! [`IMPORT_DEADLINE`](crate::agent::import::IMPORT_DEADLINE) and bounds it
//! into blobs it has written but not recorded
//! ([`crate::agent::import`]); [`EventLog::handle_with`] logs it. An
//! import that found nothing, failed or timed out is an outcome of the
//! `ImportFinished` (`NotFound`, `Failed`), never a refusal: the session is
//! created all the same. A refused create logs nothing and leaves no blob.
//!
//! **SendMessage.** `ContextChanged` when its `context` is no longer the
//! session's current one: the message was written for a conversation the
//! agent has left. Then `MessageTooLong` when its text is over
//! [`limits::MESSAGE_MAX_BYTES`]: logged verbatim or not at all, never cut.
//! Then, for a message naming images (staged by hash with `PUT
//! /v1/attachments`): `TooManyAttachments` for more than
//! [`limits::ATTACHMENTS_MAX`] names, `AttachmentsTooLarge` when the images
//! they name total more than [`limits::ATTACHMENTS_MAX_TOTAL_BYTES`], then
//! `UnknownAttachment` for a name that is not a staged image. The images are
//! prepared like a crop ([`crate::attachments`]): the transaction records each
//! one's new blob, as an `attachment`, and appends the `UserMessage` naming
//! them; the staged files go after the commit, and a refused message keeps
//! them. `DiskLow` comes before all of these, as for any new work.
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
//! the turn was interrupted: from that seq the session has no active turn.
//! A second `Interrupt` before the agent ends the turn (the session's
//! newest state event is still that `TurnInterrupted`) is accepted too and
//! appends another, stamped with the same context: the supervisor stops an
//! agent that has not answered the first. Once the turn has ended an
//! `Interrupt` is `NoActiveTurn`. The supervisor (T10) reacts to the
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
//! `UnknownModel`. A `SetPermissionMode` asking for
//! [`PermissionMode::Bypass`] is `BypassDisabled` while the log does not
//! allow bypass ([`EventLog::with_bypass`]), before anything else is
//! checked: the server refuses it from its own config, never leaving it to
//! the agent.
//!
//! **StopTask.** `UnknownTask` unless the task is in the session's live
//! set as last logged (the folded `BackgroundTasksChanged`): a definitive
//! check, so a device with a stale list cannot stop a task that already
//! ended. Accepted whether or not the agent runs. The supervisor asks a
//! running agent to stop the task and never starts one for it; the task
//! leaving the next `BackgroundTasksChanged` is the success, a
//! `TaskStopRefused` naming the command the failure, `"agent not running"`
//! among them.
//!
//! **RunShell, StopShell.** Both are `ShellDisabled` while the log has shell
//! commands off ([`EventLog::with_shell`]), before anything else is
//! checked; then `UnknownSession`. A `RunShell` command blank after
//! trimming or over [`limits::SHELL_COMMAND_MAX_BYTES`] bytes is
//! `BadShellCommand`; it is logged verbatim, never trimmed or cut, and its
//! command id is the run's id. A `StopShell` naming no run of the session
//! (one of another session included) is `UnknownRun`, one whose run has its
//! `ShellFinished` is `RunFinished`. A second stop of a running run is
//! accepted and logged; the first is the one the runner acts on.
//!
//! **DeleteBlob.** `UnknownBlob` when no event referenced the blob (no
//! `event_blobs` row: a workdir blob, an id never minted), `BlobGone` when
//! one did but its `blobs` row is gone. Otherwise the `blobs` row is
//! deleted and `BlobDeleted` appended in the command's transaction; the
//! `event_blobs` row stays, so a repeat is `BlobGone`. The file is removed
//! after the commit, from the store the log was given
//! ([`EventLog::with_blobs`]): a crash between the two, or a log with no
//! store, leaves a file without a row, which the startup sweep removes
//! ([`BlobStore::sweep`](crate::blobs::BlobStore::sweep)). The referencing
//! event keeps its preview; nothing else in the log changes.
//!
//! **CropBlob.** Refused like `DeleteBlob`, then `NotCroppable` unless the
//! blob holds a tool output or a shell run's output, then `BadCrop` for
//! spans of the wrong shape ([`skywave_proto::crop::check_spans`]). The
//! new blob is written before the transaction ([`crate::crop`]), which
//! hands [`EventLog::handle_with`] either the new blob or the refusal the
//! crop came to (`BadCrop` for spans that do not fit the content,
//! `DiskLow` for no room), stored like any. The transaction records the
//! new blob, deletes the old `blobs` row and appends `BlobCropped`;
//! `kept` is `keep` as sent. The new blob's `event_blobs` row takes the
//! old row's session, seq and holds ([`event_log::append_in`]), so it
//! stands for the event that referenced the old one. The old file goes
//! after the commit, like a deleted one's. Without a preparation (plain
//! [`EventLog::handle`]) a crop that passes its checks is an error.

use std::path::PathBuf;

use rusqlite::{Connection, OptionalExtension, params};
use skywave_proto::{
    BlobUse, Command, CommandId, CommandKind, CommandResult, ContextId, ContextRef, Decision,
    DeviceId, EventKind, ImportOutcome, PermissionMode, RejectReason, Seq, SessionId,
    SessionOrigin, Worktree, answers_fit, check_conversation_id, context_name, limits,
    normalise_context_name,
};

use crate::agent::import;
use crate::attachments;
use crate::crop;
use crate::db::Db;
use crate::dirs;
use crate::error::{Error, Result};
use crate::event_log::{self, Closed, EventLog};
use crate::worktree;

/// What a command needs done before its transaction, off the database
/// ([`EventLog::handle_with`]). Each owns the blob files it wrote and
/// removes them when dropped unless its transaction committed them.
#[derive(Debug)]
pub enum Preparation {
    /// An adopting `CreateSession`'s history ([`import::prepare`]).
    Import(import::Prepared),
    /// A `CropBlob`'s new blob ([`crop::prepare`]).
    Crop(crop::Prepared),
    /// A `SendMessage`'s staged images, linked as blobs
    /// ([`attachments::prepare`]).
    Attachments(attachments::Prepared),
    /// A `CreateSession` naming a worktree: the worktree
    /// ([`worktree::create`]), removed when its session is not committed.
    Worktree(worktree::Prepared),
}

impl Preparation {
    fn kept(self) {
        match self {
            Preparation::Import(prepared) => prepared.kept(),
            Preparation::Crop(prepared) => prepared.kept(),
            Preparation::Attachments(prepared) => prepared.kept(),
            Preparation::Worktree(prepared) => prepared.kept(),
        }
    }
}

impl From<import::Prepared> for Preparation {
    fn from(prepared: import::Prepared) -> Preparation {
        Preparation::Import(prepared)
    }
}

impl From<crop::Prepared> for Preparation {
    fn from(prepared: crop::Prepared) -> Preparation {
        Preparation::Crop(prepared)
    }
}

impl From<attachments::Prepared> for Preparation {
    fn from(prepared: attachments::Prepared) -> Preparation {
        Preparation::Attachments(prepared)
    }
}

impl From<worktree::Prepared> for Preparation {
    fn from(prepared: worktree::Prepared) -> Preparation {
        Preparation::Worktree(prepared)
    }
}

impl EventLog {
    /// Handles `command` from `device` and returns its result.
    ///
    /// A known command id returns the stored result, `Rejected` included,
    /// and appends nothing. Otherwise the command is validated, its event
    /// appended and its result stored in one transaction. `Err` means the
    /// database failed and nothing was recorded, so a resend is still safe.
    ///
    /// Blocking: it runs SQLite and, for `CreateSession`, file system calls.
    /// An adopting `CreateSession` imports nothing here: its
    /// `ImportFinished` says `Unavailable` (see [`EventLog::handle_with`]).
    /// A `CropBlob` that passes its checks, or a `SendMessage` naming images,
    /// needs its preparation: here it is an error, and nothing is recorded.
    pub fn handle(
        &self,
        db: &mut Db,
        device: DeviceId,
        command: &Command,
    ) -> Result<CommandResult> {
        self.handle_with(db, device, command, None)
    }

    /// [`EventLog::handle`], with what the command needs prepared
    /// beforehand off the database: the history an adopting
    /// `CreateSession` imports ([`import::prepare`]), the new blob of a
    /// `CropBlob` ([`crop::prepare`]) or the staged images of a
    /// `SendMessage` ([`attachments::prepare`]).
    ///
    /// An import's blob rows and `Imported` events are logged in the
    /// command's transaction. It is logged only for an accepted adopting
    /// create whose workdir canonicalises to the one it was read for; one
    /// that canonicalises elsewhere now logs `Failed("workdir changed")`,
    /// and with none the outcome is `Unavailable`. A crop's new blob is
    /// recorded, and its old one's row deleted, only if the old blob is
    /// still stored; its old file goes after the commit. A message's images
    /// are recorded as its attachments' blobs, and their staged files go
    /// after the commit. Whatever is not logged and committed, a stored or
    /// refused command or a failed transaction included, has its blob files
    /// removed.
    pub fn handle_with(
        &self,
        db: &mut Db,
        device: DeviceId,
        command: &Command,
        mut prepared: Option<Preparation>,
    ) -> Result<CommandResult> {
        let tx = event_log::begin(db)?;
        if let Some(stored) = stored_result(&tx, command.id)? {
            return Ok(stored);
        }
        let result = match self.apply(&tx, device, command, prepared.as_mut())? {
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
        if let Some(prepared) = prepared {
            prepared.kept();
        }
        // Only once its row is gone for good: a crash before this leaves a
        // file without a row, which the startup sweep removes.
        if let (CommandKind::DeleteBlob { blob }, CommandResult::Accepted { .. }, Some(blobs)) =
            (&command.kind, &result, self.blobs())
        {
            blobs.discard(*blob);
        }
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

    /// The stored result of command `id`, if it was handled.
    pub fn command_result(&self, db: &Db, id: CommandId) -> Result<Option<CommandResult>> {
        stored_result(db.conn(), id)
    }

    /// Validates `command` and appends its events, returning the seq of the
    /// one naming it, the last, or why the command is refused.
    fn apply(
        &self,
        tx: &Connection,
        device: DeviceId,
        command: &Command,
        prepared: Option<&mut Preparation>,
    ) -> Result<Result<Seq, RejectReason>> {
        // Read only for work, at most once per check interval: control
        // and an idle server read nothing.
        if starts_work(&command.kind) {
            if self.stopping() {
                return Ok(Err(RejectReason::ServerStopping));
            }
            if self.disk().refresh() {
                return Ok(Err(RejectReason::DiskLow));
            }
        }
        let known = |id: SessionId| event_log::session_in(tx, id);
        let (session_id, context, kind) = match &command.kind {
            // D30: a conversation resumed from another cwd is untested, so
            // the refusal comes before anything is created.
            CommandKind::CreateSession {
                conversation: Some(_),
                worktree: Some(_),
                ..
            } => return Ok(Err(RejectReason::BadConversation)),
            CommandKind::CreateSession {
                worktree: Some(_), ..
            } => {
                let created = match prepared_worktree(self.roots(), prepared) {
                    Ok(created) => created,
                    Err(reason) => return Ok(Err(reason)),
                };
                let session = SessionId::new();
                let context = ContextId::first_of(session);
                let kind = EventKind::SessionCreated {
                    workdir: created.path,
                    context_name: context_name::random(),
                    from: None,
                    worktree: Some(Worktree {
                        repo: created.repo,
                        branch: created.branch,
                    }),
                };
                (session, context, kind)
            }
            CommandKind::CreateSession {
                workdir,
                conversation,
                worktree: None,
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
                let context = ContextId::first_of(session);
                let adopting = from.is_some();
                let kind = EventKind::SessionCreated {
                    workdir: workdir.clone(),
                    context_name: context_name::random(),
                    from,
                    worktree: None,
                };
                if !adopting {
                    (session, context, kind)
                } else {
                    // The session first, naming no command, then its
                    // imported history; the command names itself on the
                    // `ImportFinished` after it, the last event it appends.
                    event_log::append_in(tx, session, context, None, &kind)?;
                    let outcome = match prepared {
                        Some(Preparation::Import(prepared))
                            if prepared.workdir() == Some(workdir.as_str()) =>
                        {
                            prepared.log(tx, session, context)?
                        }
                        // Renamed or relinked since it was read: the history
                        // read may be another directory's. Or refused then
                        // and accepted now: nothing was read.
                        Some(Preparation::Import(_)) => ImportOutcome::Failed {
                            reason: import::WORKDIR_CHANGED.to_owned(),
                        },
                        None
                        | Some(
                            Preparation::Crop(_)
                            | Preparation::Attachments(_)
                            | Preparation::Worktree(_),
                        ) => ImportOutcome::Unavailable,
                    };
                    (session, context, EventKind::ImportFinished { outcome })
                }
            }
            CommandKind::SendMessage {
                session_id,
                context,
                text,
                attachments,
            } => {
                let Some(session) = known(*session_id)? else {
                    return Ok(Err(RejectReason::UnknownSession));
                };
                if session.context != *context {
                    return Ok(Err(RejectReason::ContextChanged));
                }
                if text.len() > limits::MESSAGE_MAX_BYTES {
                    return Ok(Err(RejectReason::MessageTooLong));
                }
                let attachments = if attachments.is_empty() {
                    Vec::new()
                } else {
                    let Some(Preparation::Attachments(prepared)) = prepared else {
                        return Err(Error::Unprepared("SendMessage"));
                    };
                    if let Err(reason) = prepared.decide() {
                        return Ok(Err(reason));
                    }
                    prepared.log(tx)?
                };
                let kind = EventKind::UserMessage {
                    device_id: device,
                    text: text.clone(),
                    attachments,
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
                // A question is answered with `AnswerQuestion`; a denial
                // declines it.
                if approval.questions.is_some() && *decision == Decision::Approve {
                    return Ok(Err(RejectReason::BadAnswer));
                }
                let kind = EventKind::ApprovalResolved {
                    id: *id,
                    decision: *decision,
                    by: device,
                    rule: *rule,
                };
                (approval.session_id, approval.context, kind)
            }
            CommandKind::AnswerQuestion { id, answers } => {
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
                let fits = approval
                    .questions
                    .as_ref()
                    .is_some_and(|questions| answers_fit(questions, answers));
                if !fits {
                    return Ok(Err(RejectReason::BadAnswer));
                }
                let kind = EventKind::QuestionAnswered {
                    id: *id,
                    by: device,
                    answers: answers.clone(),
                };
                (approval.session_id, approval.context, kind)
            }
            CommandKind::Interrupt { session_id } => {
                let context = match known(*session_id)? {
                    None => return Ok(Err(RejectReason::UnknownSession)),
                    Some(session) if session.turn_active => session.running_turn_context(),
                    // Interrupted, and the agent has not ended the turn
                    // yet: a repeat, stamped like the first.
                    Some(_) => match event_log::last_state_event_in(tx, *session_id)? {
                        Some((EventKind::TurnInterrupted, context)) => context,
                        _ => return Ok(Err(RejectReason::NoActiveTurn)),
                    },
                };
                (*session_id, context, EventKind::TurnInterrupted)
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
                        worktree: None,
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
                if *mode == PermissionMode::Bypass && !self.bypass_allowed() {
                    return Ok(Err(RejectReason::BypassDisabled));
                }
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
            CommandKind::RunShell {
                session_id,
                command: shell,
                when,
            } => {
                if !self.shell_enabled() {
                    return Ok(Err(RejectReason::ShellDisabled));
                }
                let Some(session) = known(*session_id)? else {
                    return Ok(Err(RejectReason::UnknownSession));
                };
                if shell.trim().is_empty() || shell.len() > limits::SHELL_COMMAND_MAX_BYTES {
                    return Ok(Err(RejectReason::BadShellCommand));
                }
                let kind = EventKind::ShellRequested {
                    command: shell.clone(),
                    when: *when,
                    by: device,
                };
                (*session_id, session.context, kind)
            }
            CommandKind::StopShell { session_id, run } => {
                if !self.shell_enabled() {
                    return Ok(Err(RejectReason::ShellDisabled));
                }
                let Some(session) = known(*session_id)? else {
                    return Ok(Err(RejectReason::UnknownSession));
                };
                let Some(row) = event_log::shell_run_in(tx, *run)?
                    .filter(|row| row.run.session_id == *session_id)
                else {
                    return Ok(Err(RejectReason::UnknownRun));
                };
                if row.finished_seq.is_some() {
                    return Ok(Err(RejectReason::RunFinished));
                }
                let kind = EventKind::ShellStopRequested {
                    run: *run,
                    by: device,
                };
                (*session_id, session.context, kind)
            }
            CommandKind::DeleteBlob { blob } => {
                let Some(row) = event_log::event_blob_in(tx, *blob)? else {
                    return Ok(Err(RejectReason::UnknownBlob));
                };
                if !row.stored {
                    return Ok(Err(RejectReason::BlobGone));
                }
                // The file goes once this commits (`handle_with`).
                event_log::delete_blob_row_in(tx, *blob)?;
                let kind = EventKind::BlobDeleted {
                    blob: *blob,
                    holds: row.holds,
                    by: device,
                };
                (row.session_id, row.context, kind)
            }
            CommandKind::CropBlob { blob, keep } => {
                let Some(row) = event_log::event_blob_in(tx, *blob)? else {
                    return Ok(Err(RejectReason::UnknownBlob));
                };
                // Deleted, or cropped already, while this crop was
                // prepared: its new blob goes with the preparation.
                if !row.stored {
                    return Ok(Err(RejectReason::BlobGone));
                }
                if !matches!(row.holds, BlobUse::ToolOutput | BlobUse::ShellOutput) {
                    return Ok(Err(RejectReason::NotCroppable));
                }
                if !skywave_proto::crop::check_spans(keep) {
                    return Ok(Err(RejectReason::BadCrop));
                }
                let Some(Preparation::Crop(prepared)) = prepared else {
                    return Err(Error::Unprepared("CropBlob"));
                };
                let (into, lines) = match prepared.log(tx)? {
                    Ok(cropped) => cropped,
                    Err(reason) => return Ok(Err(reason)),
                };
                let kind = EventKind::BlobCropped {
                    blob: *blob,
                    holds: row.holds,
                    into,
                    kept: keep.clone(),
                    lines,
                    by: device,
                };
                (row.session_id, row.context, kind)
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

/// The worktree `prepare_worktree` made for a `CreateSession` naming one,
/// its path checked under the roots again. It is marked logged only once
/// that passes: a refusal after that would keep a worktree whose session
/// was never logged.
fn prepared_worktree(
    roots: &[PathBuf],
    prepared: Option<&mut Preparation>,
) -> std::result::Result<worktree::Created, RejectReason> {
    let Some(Preparation::Worktree(prepared)) = prepared else {
        return Err(RejectReason::WorktreeFailed);
    };
    let created = prepared.created()?;
    dirs::canonical_under_roots(roots, &created.path)?;
    prepared.logged();
    Ok(created)
}

/// Whether a command of this kind starts new work, which a low disk refuses
/// (`DiskLow`), as does a server stopping after its work (`ServerStopping`): it creates a session or context, gives the agent a message
/// or a setting request (either may start an agent), or runs a shell
/// command. Everything else is control the user needs while the agents are
/// paused (an interrupt, a stop, an approval answer) or one small event
/// that starts nothing (a rename, an archive, a switch to a context that
/// exists, a blob deleted or cropped). Exhaustive, so a new command must be
/// placed.
fn starts_work(kind: &CommandKind) -> bool {
    match kind {
        CommandKind::CreateSession { .. }
        | CommandKind::SendMessage { .. }
        | CommandKind::SetPermissionMode { .. }
        | CommandKind::SetModel { .. }
        | CommandKind::SetEffort { .. }
        | CommandKind::ClearContext { .. }
        | CommandKind::MoveContext { .. }
        | CommandKind::RunShell { .. } => true,
        CommandKind::ResolveApproval { .. }
        | CommandKind::AnswerQuestion { .. }
        | CommandKind::Interrupt { .. }
        | CommandKind::StopSession { .. }
        | CommandKind::StopTask { .. }
        | CommandKind::StopShell { .. }
        | CommandKind::RenameSession { .. }
        | CommandKind::SetSessionArchived { .. }
        | CommandKind::SwitchContext { .. }
        // Freeing space is what a low disk needs.
        | CommandKind::DeleteBlob { .. }
        | CommandKind::CropBlob { .. } => false,
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
