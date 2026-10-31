//! Maps one session's [`AdapterEvent`]s to log events.

use std::collections::{HashMap, VecDeque};

use serde_json::Value;
use skywave_proto::limits::{
    DETAIL_MAX_CHARS, MODEL_DETAIL_MAX_CHARS, MODEL_LABEL_MAX_CHARS, MODEL_MAX_CHARS, MODELS_MAX,
    PREVIEW_MAX_BYTES, PREVIEW_MAX_LINES, REFUSAL_MAX_CHARS, RULE_LABEL_MAX_CHARS,
    RULE_SUGGESTIONS_MAX, STATUS_MAX_CHARS, TITLE_MAX_CHARS,
};
use skywave_proto::{
    self as proto, AgentSettings, ApprovalId, BlobRef, Body, BodyFormat, CallId, CommandId,
    EventKind, ModelChoice, PermissionMode, Seq, SessionId, SessionState, ToolCall, ToolKind,
    ToolResult,
};

use super::{
    AdapterEvent, AgentCallId, AgentRequestId, DisplayBody, ResultDisplay, ToolDisplay,
    TurnOutcome, store_resume_handle,
};
use crate::error::Result;
use crate::{EventLog, Storage};

/// Reason for approvals still pending when their turn ends.
const TURN_ENDED: &str = "turn ended";
const TURN_INTERRUPTED: &str = "turn interrupted";
const AGENT_EXITED: &str = "agent exited";

/// Turns one session's adapter events into log events.
///
/// The supervisor owns one mapper per running agent session and feeds it
/// every [`AdapterEvent`] in order:
///
/// | adapter event | log |
/// |---------------|-----|
/// | `TurnStarted` | `SessionStateChanged(AgentWorking)` |
/// | `Text` | `AgentMessage` |
/// | `ToolCall` | `ToolCall` with a new [`CallId`] and a bounded display |
/// | `ToolResult` | `ToolResult` linked to its call, with a bounded preview |
/// | `ApprovalRequested` | `ApprovalRequested` with a new [`ApprovalId`], linked to its call |
/// | `ApprovalCancelled` | `ApprovalCancelled` (if still pending) |
/// | `TurnUsage` | `TurnUsage`, as is (only in an open turn, and not empty) |
/// | `TurnEnded(Completed \| Interrupted)` | leftover approvals cancelled, `SessionStateChanged(Idle)` |
/// | `TurnEnded(Failed)` | leftover approvals cancelled, `SessionStateChanged(Errored)` |
/// | `ResumeHandle` | stored in `sessions.agent_state`, no event |
/// | `PermissionMode` / `Model` | `PermissionModeChanged` / `ModelChanged` answering nothing, only when it differs from what the log has |
/// | `PermissionModeSet` / `ModelSet` | `PermissionModeChanged` / `ModelChanged` answering the oldest request asked; for one asked under no command, only when it differs from what the log has |
/// | `PermissionModeRefused` / `ModelRefused` | `PermissionModeRefused` / `ModelRefused` answering the oldest request asked |
/// | `ModelsOffered` | nothing: the supervisor logs it, comparing across sessions |
/// | `Exited` | leftover approvals cancelled, leftover mode and model requests refused; `Errored` if a turn was open or the exit was abnormal |
///
/// **Settings.** The supervisor tells the mapper of every mode or model
/// request it hands the agent ([`EventMapper::asked_mode`],
/// [`EventMapper::asked_model`]), with the command asking, if any. The agent
/// answers requests in order, so each answer names the oldest one asked.
/// What the agent reports unasked is logged only when it changes what the
/// log has, starting from the [`AgentSettings`] the mapper is made with.
///
/// **Calls.** Every `ToolCall` gets a fresh [`CallId`]. The mapper remembers
/// which [`AgentCallId`] it stands for until the turn ends; a result or
/// approval naming that id carries the same [`CallId`], one naming no id or
/// an unknown one carries `call: None`.
///
/// **Bounds.** Every event is cut to `skywave_proto::limits`, whatever the
/// adapter handed over: a title, detail or status to its first non-blank
/// line and its char cap (a cut one ends in `…`), and so is a rule
/// suggestion's label, of which only the first [`RULE_SUGGESTIONS_MAX`]
/// are kept (the indices the adapter knows them by stay); a body or result preview
/// to its first [`PREVIEW_MAX_LINES`] lines and [`PREVIEW_MAX_BYTES`] bytes.
/// Cuts fall on char boundaries; `lines` counts the whole content. Failure
/// is `is_error`, never a marker in the text. A cut title of a display with
/// no body gets a `Text` body holding its whole text (bounded and blobbed
/// like any body), so a long or multi-line command is never shown in part
/// only; such a title no longer counts as cut for the input blob.
///
/// **Blobs** are made only for content that was cut: a body's text
/// (`Body::full`) when its preview is shorter; a result's output when its
/// preview does not hold all of it (it was cut, or the adapter previewed
/// other text, e.g. without notes meant for the model); a call's input
/// (pretty JSON, or the string itself) when its display does not show all
/// of it: its title, detail or body was cut, or the display (the adapter's,
/// by `shows_all_input`, or the generic one) left fields out. Agent text stays inline:
/// `AgentMessage` has no blob field.
///
/// **Generic display.** A call the adapter gives no display for is shown as
/// kind [`ToolKind::Other`] titled with its input: the input itself if it is
/// a string, else its first string field, else its compact JSON.
///
/// **Approvals.** An approval for a known call is summarized by that call's
/// title and carries no input when its input equals the call's; otherwise
/// its input is a blob. An approval for no known call is summarized like a
/// generic display and blobs its input under the same rule. The mapper
/// remembers which [`AgentRequestId`] each [`ApprovalId`] it logged stands
/// for. When an `ApprovalResolved` for the session appears in the log, the
/// supervisor calls [`EventMapper::take_approval`] for the agent's id and
/// answers the agent.
///
/// Blocking: it runs SQLite and blob writes.
#[derive(Debug)]
pub struct EventMapper {
    session_id: SessionId,
    /// Approvals this mapper logged that no answer has taken yet, oldest
    /// first.
    approvals: Vec<(AgentRequestId, ApprovalId)>,
    /// Calls logged in the current turn, by the agent's id.
    calls: HashMap<AgentCallId, LoggedCall>,
    /// A `TurnStarted` has been seen and its `TurnEnded` not yet.
    turn_open: bool,
    /// The session's mode and model as the log has them.
    settings: AgentSettings,
    /// Mode requests the agent has not answered, oldest first, with the
    /// command asking.
    asked_modes: VecDeque<(PermissionMode, Option<CommandId>)>,
    /// Model requests likewise.
    asked_models: VecDeque<(String, Option<CommandId>)>,
}

/// What the mapper keeps of a logged call to link its result and approval.
#[derive(Debug)]
struct LoggedCall {
    id: CallId,
    /// The title as logged.
    title: String,
    input: Value,
}

impl EventMapper {
    /// A mapper for `session_id`, whose log holds `settings`.
    pub fn new(session_id: SessionId, settings: AgentSettings) -> EventMapper {
        EventMapper {
            session_id,
            approvals: Vec::new(),
            calls: HashMap::new(),
            turn_open: false,
            settings,
            asked_modes: VecDeque::new(),
            asked_models: VecDeque::new(),
        }
    }

    /// The agent was asked to run in `mode`, for `command` if one asked.
    pub fn asked_mode(&mut self, mode: PermissionMode, command: Option<CommandId>) {
        self.asked_modes.push_back((mode, command));
    }

    /// The agent was asked to run `model`, for `command` if one asked.
    pub fn asked_model(&mut self, model: String, command: Option<CommandId>) {
        self.asked_models.push_back((model, command));
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    /// A turn has started and not yet ended.
    pub fn turn_open(&self) -> bool {
        self.turn_open
    }

    /// Approvals logged by this mapper that are still waiting for an answer.
    pub fn pending_approvals(&self) -> impl Iterator<Item = ApprovalId> + '_ {
        self.approvals.iter().map(|(_, id)| *id)
    }

    /// The agent's id for approval `id`, forgetting it: call once when the
    /// log shows `id` resolved, then answer the agent. `None` if `id` is not
    /// this mapper's or was already taken or cancelled.
    pub fn take_approval(&mut self, id: ApprovalId) -> Option<AgentRequestId> {
        let index = self.approvals.iter().position(|(_, a)| *a == id)?;
        Some(self.approvals.remove(index).0)
    }

    /// Records `event` in the log and returns the seqs it appended, in order.
    pub fn apply(
        &mut self,
        log: &EventLog,
        storage: &mut Storage,
        event: AdapterEvent,
    ) -> Result<Vec<Seq>> {
        let mut seqs = Vec::new();
        match event {
            AdapterEvent::TurnStarted => {
                if !self.turn_open {
                    self.turn_open = true;
                    seqs.push(self.append(
                        log,
                        storage,
                        EventKind::SessionStateChanged(SessionState::AgentWorking),
                    )?);
                }
            }
            AdapterEvent::Text(text) => {
                seqs.push(self.append(log, storage, EventKind::AgentMessage { text })?);
            }
            AdapterEvent::ToolCall {
                call,
                name,
                input,
                display,
            } => {
                let id = CallId::new();
                let (display, input_blob) = call_display(storage, &input, display)?;
                if let Some(agent_id) = call {
                    let logged = LoggedCall {
                        id,
                        title: display.title.clone(),
                        input,
                    };
                    if let Some(earlier) = self.calls.insert(agent_id, logged) {
                        tracing::debug!(call = %earlier.id.0, "agent reused a call id; linking the newer call");
                    }
                }
                seqs.push(self.append(
                    log,
                    storage,
                    EventKind::ToolCall(ToolCall {
                        call: Some(id),
                        name,
                        display,
                        input: input_blob,
                    }),
                )?);
            }
            AdapterEvent::ToolResult { call, result } => {
                let call = self.linked(call.as_ref()).map(|c| c.id);
                let result = tool_result(storage, call, result)?;
                seqs.push(self.append(log, storage, EventKind::ToolResult(result))?);
            }
            AdapterEvent::ApprovalRequested {
                request,
                call,
                tool,
                input,
                suggestions,
            } => {
                let (call, summary, input) = match self.linked(call.as_ref()) {
                    Some(logged) => {
                        let blob = if logged.input == input {
                            None
                        } else {
                            Some(input_blob(storage, &input)?)
                        };
                        (Some(logged.id), logged.title.clone(), blob)
                    }
                    None => {
                        let (summary, shown_whole) = generic_title(&input);
                        let blob = if shown_whole {
                            None
                        } else {
                            Some(input_blob(storage, &input)?)
                        };
                        (None, summary, blob)
                    }
                };
                let id = ApprovalId::new();
                seqs.push(self.append(
                    log,
                    storage,
                    EventKind::ApprovalRequested {
                        id,
                        call,
                        tool,
                        summary,
                        input,
                        suggestions: bounded_suggestions(suggestions),
                    },
                )?);
                self.approvals.push((request, id));
            }
            AdapterEvent::ApprovalCancelled { request, reason } => {
                match self.approvals.iter().position(|(r, _)| *r == request) {
                    Some(index) => {
                        let (_, id) = self.approvals.remove(index);
                        seqs.extend(log.cancel_approval(&mut storage.db, id, &reason)?);
                    }
                    None => {
                        tracing::debug!(request = %request.0, "agent cancelled an approval that is not pending");
                    }
                }
            }
            AdapterEvent::TurnUsage(usage) => {
                if self.turn_open && !usage.is_empty() {
                    seqs.push(self.append(log, storage, EventKind::TurnUsage(usage))?);
                } else {
                    tracing::debug!("dropping turn usage outside a turn or with nothing in it");
                }
            }
            AdapterEvent::TurnEnded(outcome) => {
                self.turn_open = false;
                self.calls.clear();
                let (reason, state) = match outcome {
                    TurnOutcome::Completed => (TURN_ENDED, SessionState::Idle),
                    TurnOutcome::Interrupted => (TURN_INTERRUPTED, SessionState::Idle),
                    TurnOutcome::Failed(message) => (TURN_ENDED, SessionState::Errored { message }),
                };
                self.cancel_pending(log, storage, reason, &mut seqs)?;
                seqs.push(self.append(log, storage, EventKind::SessionStateChanged(state))?);
            }
            AdapterEvent::ResumeHandle(handle) => {
                store_resume_handle(&storage.db, self.session_id, &handle)?;
            }
            AdapterEvent::PermissionMode(mode) => {
                if self.settings.mode.current != Some(mode) {
                    let kind = EventKind::PermissionModeChanged {
                        mode,
                        answers: None,
                    };
                    seqs.push(self.append(log, storage, kind)?);
                }
            }
            AdapterEvent::PermissionModeSet(mode) => {
                let answers = self.answer_mode();
                // Restoring what the log has, for no request, says nothing new.
                if answers.is_some() || self.settings.mode.current != Some(mode) {
                    let kind = EventKind::PermissionModeChanged { mode, answers };
                    seqs.push(self.append(log, storage, kind)?);
                }
            }
            AdapterEvent::PermissionModeRefused { mode, reason } => {
                let kind = EventKind::PermissionModeRefused {
                    mode,
                    reason: cut_line(&reason, REFUSAL_MAX_CHARS).0,
                    answers: self.answer_mode(),
                };
                seqs.push(self.append(log, storage, kind)?);
            }
            AdapterEvent::Model(model) => {
                let model = cut_line(&model, MODEL_MAX_CHARS).0;
                if !model.is_empty() && self.settings.model.current.as_ref() != Some(&model) {
                    let kind = EventKind::ModelChanged {
                        model,
                        answers: None,
                    };
                    seqs.push(self.append(log, storage, kind)?);
                }
            }
            AdapterEvent::ModelSet(model) => {
                let model = cut_line(&model, MODEL_MAX_CHARS).0;
                let answers = self.answer_model();
                if answers.is_some() || self.settings.model.current.as_ref() != Some(&model) {
                    let kind = EventKind::ModelChanged { model, answers };
                    seqs.push(self.append(log, storage, kind)?);
                }
            }
            AdapterEvent::ModelRefused { model, reason } => {
                let kind = EventKind::ModelRefused {
                    model: cut_line(&model, MODEL_MAX_CHARS).0,
                    reason: cut_line(&reason, REFUSAL_MAX_CHARS).0,
                    answers: self.answer_model(),
                };
                seqs.push(self.append(log, storage, kind)?);
            }
            AdapterEvent::ModelsOffered(_) => {
                tracing::debug!("models offered reach the log through the supervisor");
            }
            AdapterEvent::Exited { error } => {
                self.cancel_pending(log, storage, AGENT_EXITED, &mut seqs)?;
                self.refuse_asked(log, storage, &mut seqs)?;
                self.calls.clear();
                let message = match error {
                    Some(error) => Some(format!("agent exited: {error}")),
                    None if self.turn_open => Some("agent exited during a turn".to_owned()),
                    None => None,
                };
                self.turn_open = false;
                if let Some(message) = message {
                    seqs.push(self.append(
                        log,
                        storage,
                        EventKind::SessionStateChanged(SessionState::Errored { message }),
                    )?);
                }
            }
        }
        Ok(seqs)
    }

    fn append(&mut self, log: &EventLog, storage: &mut Storage, kind: EventKind) -> Result<Seq> {
        if AgentSettings::folds(&kind) {
            self.settings.apply(None, &kind);
        }
        log.append(&mut storage.db, self.session_id, kind)
    }

    /// The command of the oldest mode request, now answered.
    fn answer_mode(&mut self) -> Option<CommandId> {
        let asked = self.asked_modes.pop_front();
        if asked.is_none() {
            tracing::debug!("agent answered a mode request nobody asked");
        }
        asked.and_then(|(_, command)| command)
    }

    /// The command of the oldest model request, now answered.
    fn answer_model(&mut self) -> Option<CommandId> {
        let asked = self.asked_models.pop_front();
        if asked.is_none() {
            tracing::debug!("agent answered a model request nobody asked");
        }
        asked.and_then(|(_, command)| command)
    }

    /// Refuses every mode and model request the agent left unanswered.
    fn refuse_asked(
        &mut self,
        log: &EventLog,
        storage: &mut Storage,
        seqs: &mut Vec<Seq>,
    ) -> Result<()> {
        for (mode, answers) in std::mem::take(&mut self.asked_modes) {
            let kind = EventKind::PermissionModeRefused {
                mode,
                reason: AGENT_EXITED.to_owned(),
                answers,
            };
            seqs.push(self.append(log, storage, kind)?);
        }
        for (model, answers) in std::mem::take(&mut self.asked_models) {
            let kind = EventKind::ModelRefused {
                model,
                reason: AGENT_EXITED.to_owned(),
                answers,
            };
            seqs.push(self.append(log, storage, kind)?);
        }
        Ok(())
    }

    /// The call logged in this turn for the agent's `call`, if any.
    fn linked(&self, call: Option<&AgentCallId>) -> Option<&LoggedCall> {
        let call = call?;
        let logged = self.calls.get(call);
        if logged.is_none() {
            tracing::debug!(call = %call.0, "agent named a call not logged in this turn");
        }
        logged
    }

    /// Cancels every approval still pending, oldest first. One already
    /// answered in the log stays answered (D5).
    fn cancel_pending(
        &mut self,
        log: &EventLog,
        storage: &mut Storage,
        reason: &str,
        seqs: &mut Vec<Seq>,
    ) -> Result<()> {
        for (_, id) in std::mem::take(&mut self.approvals) {
            seqs.extend(log.cancel_approval(&mut storage.db, id, reason)?);
        }
        Ok(())
    }
}

/// The bounded display for a call and, when it does not show all of the
/// input, the input as a blob.
fn call_display(
    storage: &Storage,
    input: &Value,
    display: Option<ToolDisplay>,
) -> Result<(proto::ToolDisplay, Option<BlobRef>)> {
    let (display, shown_whole) = match display {
        Some(display) => {
            let omits_input = !display.shows_all_input;
            let (title, title_cut) = cut_line(&display.title, TITLE_MAX_CHARS);
            let (detail, detail_cut) = match &display.detail {
                Some(detail) => {
                    let (detail, cut) = cut_line(detail, DETAIL_MAX_CHARS);
                    (Some(detail), cut)
                }
                None => (None, false),
            };
            let (body, title_cut) = match display.body {
                Some(body) => (Some(display_body(storage, body)?), title_cut),
                None => (title_body(storage, &display.title, title_cut)?, false),
            };
            let body_cut = body.as_ref().is_some_and(|b| b.full.is_some());
            let display = proto::ToolDisplay {
                kind: display.kind,
                title,
                detail,
                body,
            };
            (
                display,
                !(omits_input || title_cut || detail_cut || body_cut),
            )
        }
        None => {
            let (text, whole) = generic_text(input);
            let (title, title_cut) = cut_line(&text, TITLE_MAX_CHARS);
            let body = title_body(storage, &text, title_cut)?;
            let body_cut = body.as_ref().is_some_and(|b| b.full.is_some());
            let display = proto::ToolDisplay {
                kind: ToolKind::Other,
                title,
                detail: None,
                body,
            };
            (display, whole && !body_cut)
        }
    };
    let blob = if shown_whole {
        None
    } else {
        Some(input_blob(storage, input)?)
    };
    Ok((display, blob))
}

/// The body standing in for a cut title: its whole text, so a long or
/// multi-line title (a shell command) is never shown in part only.
fn title_body(storage: &Storage, title: &str, cut: bool) -> Result<Option<Body>> {
    if !cut {
        return Ok(None);
    }
    let body = DisplayBody {
        format: BodyFormat::Text,
        text: title.trim().to_owned(),
    };
    display_body(storage, body).map(Some)
}

/// A body's preview, with its text as a blob when the preview is shorter.
fn display_body(storage: &Storage, body: DisplayBody) -> Result<Body> {
    let (preview, cut) = cut_preview(&body.text);
    let full = if cut {
        Some(storage.put_blob(body.text.as_bytes())?)
    } else {
        None
    };
    Ok(Body {
        format: body.format,
        preview,
        lines: line_count(&body.text),
        full,
    })
}

/// A result's preview (of the adapter's preview text, else of the output),
/// with the output as a blob when the preview does not hold all of it.
fn tool_result(
    storage: &Storage,
    call: Option<CallId>,
    result: ResultDisplay,
) -> Result<ToolResult> {
    let shown = result.preview.as_deref().unwrap_or(&result.output);
    let (preview, cut) = cut_preview(shown);
    let output = if cut || shown != result.output {
        Some(storage.put_blob(result.output.as_bytes())?)
    } else {
        None
    };
    Ok(ToolResult {
        call,
        is_error: result.is_error,
        status: result
            .status
            .map(|status| cut_line(&status, STATUS_MAX_CHARS).0),
        preview,
        lines: line_count(&result.output),
        output,
    })
}

/// Title of the generic display for `input`, cut, and whether it shows all
/// of the input (see [`generic_text`]).
fn generic_title(input: &Value) -> (String, bool) {
    let (text, whole) = generic_text(input);
    let (title, cut) = cut_line(&text, TITLE_MAX_CHARS);
    (title, whole && !cut)
}

/// Title text of the generic display for `input`, uncut, and whether it
/// holds all of the input: the input itself if it is a string, else its
/// first string field, else its compact JSON.
fn generic_text(input: &Value) -> (String, bool) {
    match input {
        Value::String(text) => (text.clone(), true),
        Value::Object(fields) => match fields.values().find_map(Value::as_str) {
            Some(text) => (text.to_owned(), fields.len() == 1),
            None => (input.to_string(), true),
        },
        Value::Null => (String::new(), true),
        _ => (input.to_string(), true),
    }
}

/// `input` stored whole: a string as itself, anything else pretty-printed.
fn input_blob(storage: &Storage, input: &Value) -> Result<BlobRef> {
    let bytes = match input {
        Value::String(text) => text.clone().into_bytes(),
        _ => serde_json::to_vec_pretty(input)?,
    };
    storage.put_blob(bytes.as_slice())
}

/// The first non-blank line of `text`, trimmed, at most `max_chars` chars,
/// and whether anything but surrounding whitespace was left out. A cut line
/// ends in `…` (within `max_chars`).
fn cut_line(text: &str, max_chars: usize) -> (String, bool) {
    let text = text.trim();
    let line = text.lines().next().unwrap_or("").trim_end();
    let mut out: String = line.chars().take(max_chars).collect();
    let cut = line.len() < text.len() || out.len() < line.len();
    if cut {
        if out.chars().count() >= max_chars {
            out.pop();
        }
        out.push('…');
    }
    (out, cut)
}

/// The first [`RULE_SUGGESTIONS_MAX`] suggestions, labels cut to one line
/// of [`RULE_LABEL_MAX_CHARS`]. Cutting the end keeps every index valid.
fn bounded_suggestions(mut suggestions: Vec<proto::RuleSuggestion>) -> Vec<proto::RuleSuggestion> {
    suggestions.truncate(RULE_SUGGESTIONS_MAX);
    for suggestion in &mut suggestions {
        suggestion.label = cut_line(&suggestion.label, RULE_LABEL_MAX_CHARS).0;
    }
    suggestions
}

/// The first [`MODELS_MAX`] models, each field cut to one line of its cap;
/// a model with a blank id is dropped.
pub(crate) fn bounded_models(models: Vec<ModelChoice>) -> Vec<ModelChoice> {
    models
        .into_iter()
        .filter_map(|model| {
            let id = cut_line(&model.id, MODEL_MAX_CHARS).0;
            let label = cut_line(&model.label, MODEL_LABEL_MAX_CHARS).0;
            let detail = model
                .detail
                .map(|detail| cut_line(&detail, MODEL_DETAIL_MAX_CHARS).0)
                .filter(|detail| !detail.is_empty());
            let resolved = model
                .resolved
                .map(|resolved| cut_line(&resolved, MODEL_MAX_CHARS).0)
                .filter(|resolved| !resolved.is_empty());
            (!id.is_empty()).then(|| ModelChoice {
                label: if label.is_empty() { id.clone() } else { label },
                id,
                detail,
                resolved,
            })
        })
        .take(MODELS_MAX)
        .collect()
}

/// The head of `text`: at most [`PREVIEW_MAX_LINES`] lines and
/// [`PREVIEW_MAX_BYTES`] bytes, cut on a char boundary, and whether it
/// leaves anything out (a lone trailing newline does not count).
fn cut_preview(text: &str) -> (String, bool) {
    // The newline ending the last line kept, unless it ends the text too.
    let mut end = match text.match_indices('\n').nth(PREVIEW_MAX_LINES - 1) {
        Some((i, _)) if i + 1 < text.len() => i,
        _ => text.len(),
    };
    if end > PREVIEW_MAX_BYTES {
        end = PREVIEW_MAX_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
    }
    let cut = !matches!(&text[end..], "" | "\n");
    (text[..end].to_owned(), cut)
}

fn line_count(text: &str) -> u32 {
    u32::try_from(text.lines().count()).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_line_is_cut_on_a_char_boundary_with_an_ellipsis() {
        assert_eq!(cut_line("  ls -la \n", 120), ("ls -la".to_owned(), false));
        let (line, cut) = cut_line(&"é".repeat(300), TITLE_MAX_CHARS);
        assert!(cut);
        assert_eq!(line, format!("{}…", "é".repeat(TITLE_MAX_CHARS - 1)));
        assert_eq!(
            cut_line("\n first\nsecond", 120),
            ("first…".to_owned(), true)
        );
        assert_eq!(cut_line("", 120), (String::new(), false));
    }

    #[test]
    fn a_preview_keeps_the_head_within_lines_and_bytes() {
        let text: String = (0..20).map(|i| format!("line {i}\n")).collect();
        let (preview, cut) = cut_preview(&text);
        assert!(cut);
        assert_eq!(
            preview,
            text.lines()
                .take(PREVIEW_MAX_LINES)
                .collect::<Vec<_>>()
                .join("\n")
        );

        let (preview, cut) = cut_preview(&"é".repeat(400));
        assert!(cut);
        assert_eq!(preview, "é".repeat(PREVIEW_MAX_BYTES / 2));
        let (preview, _) = cut_preview(&format!("x{}", "é".repeat(400)));
        assert_eq!(preview.len(), PREVIEW_MAX_BYTES - 1);

        assert_eq!(cut_preview("a\nb\n"), ("a\nb\n".to_owned(), false));
        assert_eq!(cut_preview(""), (String::new(), false));
        let eight: String = (0..PREVIEW_MAX_LINES).map(|i| format!("{i}\n")).collect();
        assert!(!cut_preview(&eight).1);
    }

    #[test]
    fn the_generic_title_is_the_first_string_field() {
        assert_eq!(
            generic_text(&json!({ "command": "ls" })),
            ("ls".to_owned(), true)
        );
        assert_eq!(
            generic_text(&json!({ "count": 3, "path": "a.txt", "text": "hi" })),
            ("a.txt".to_owned(), false)
        );
        assert_eq!(
            generic_text(&json!({ "count": 3 })),
            (r#"{"count":3}"#.to_owned(), true)
        );
        assert_eq!(generic_text(&json!("echo")), ("echo".to_owned(), true));
        assert_eq!(generic_text(&Value::Null), (String::new(), true));
    }
}
