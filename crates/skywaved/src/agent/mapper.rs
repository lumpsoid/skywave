//! Maps one session's [`AdapterEvent`]s to log events.

use serde_json::Value;
use skywave_proto::{ApprovalId, BlobRef, EventKind, Seq, SessionId, SessionState};

use super::{AdapterEvent, AgentRequestId, TurnOutcome, store_resume_handle};
use crate::error::Result;
use crate::{EventLog, Storage};

/// Payloads larger than this many bytes become blobs (spec §5: 4 KiB).
pub const DEFAULT_BLOB_THRESHOLD: usize = 4096;
/// Longest first-line preview a summary of a blob payload carries.
pub const SUMMARY_MAX_CHARS: usize = 120;

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
/// | `ToolCall` | `ToolCall` (input as summary or blob) |
/// | `ToolResult` | `ToolResult` (output as summary or blob) |
/// | `ApprovalRequested` | `ApprovalRequested` with a new [`ApprovalId`] |
/// | `ApprovalCancelled` | `ApprovalCancelled` (if still pending) |
/// | `TurnEnded(Completed \| Interrupted)` | leftover approvals cancelled, `SessionStateChanged(Idle)` |
/// | `TurnEnded(Failed)` | leftover approvals cancelled, `SessionStateChanged(Errored)` |
/// | `ResumeHandle` | stored in `sessions.agent_state`, no event |
/// | `Exited` | leftover approvals cancelled; `Errored` if a turn was open or the exit was abnormal |
///
/// **Payloads.** A tool input, tool output or approval input of at most
/// the threshold ([`DEFAULT_BLOB_THRESHOLD`] bytes unless set with
/// [`EventMapper::with_blob_threshold`]) is carried whole in the event's
/// `summary`, with no blob. A larger one is stored as a blob and the summary
/// is a preview: its first non-blank line, cut at [`SUMMARY_MAX_CHARS`], and
/// its line count and size. JSON inputs are summarized compactly and stored
/// pretty-printed. A failed tool's summary starts with `[error] `. Agent text
/// stays inline: `AgentMessage` has no blob field.
///
/// **Approvals.** The mapper remembers which [`AgentRequestId`] each
/// [`ApprovalId`] it logged stands for. When an `ApprovalResolved` for the
/// session appears in the log, the supervisor calls
/// [`EventMapper::take_approval`] for the agent's id and answers the agent.
///
/// Blocking: it runs SQLite and blob writes.
#[derive(Debug)]
pub struct EventMapper {
    session_id: SessionId,
    blob_threshold: usize,
    /// Approvals this mapper logged that no answer has taken yet, oldest
    /// first.
    approvals: Vec<(AgentRequestId, ApprovalId)>,
    /// A `TurnStarted` has been seen and its `TurnEnded` not yet.
    turn_open: bool,
}

impl EventMapper {
    pub fn new(session_id: SessionId) -> EventMapper {
        EventMapper {
            session_id,
            blob_threshold: DEFAULT_BLOB_THRESHOLD,
            approvals: Vec::new(),
            turn_open: false,
        }
    }

    /// Payloads above `bytes` become blobs.
    pub fn with_blob_threshold(mut self, bytes: usize) -> EventMapper {
        self.blob_threshold = bytes;
        self
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
            AdapterEvent::ToolCall { name, input } => {
                let (summary, input) = self.json_payload(storage, &input)?;
                seqs.push(self.append(
                    log,
                    storage,
                    EventKind::ToolCall {
                        name,
                        summary,
                        input,
                    },
                )?);
            }
            AdapterEvent::ToolResult { output, is_error } => {
                let (mut summary, output) = if output.is_empty() {
                    ("(no output)".to_owned(), None)
                } else {
                    self.payload(storage, output.clone(), output.into_bytes())?
                };
                if is_error {
                    summary.insert_str(0, "[error] ");
                }
                seqs.push(self.append(log, storage, EventKind::ToolResult { summary, output })?);
            }
            AdapterEvent::ApprovalRequested {
                request,
                tool,
                input,
            } => {
                let (summary, input) = self.json_payload(storage, &input)?;
                let id = ApprovalId::new();
                seqs.push(self.append(
                    log,
                    storage,
                    EventKind::ApprovalRequested {
                        id,
                        tool,
                        summary,
                        input,
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
            AdapterEvent::TurnEnded(outcome) => {
                self.turn_open = false;
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
            AdapterEvent::Exited { error } => {
                self.cancel_pending(log, storage, AGENT_EXITED, &mut seqs)?;
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

    fn append(&self, log: &EventLog, storage: &mut Storage, kind: EventKind) -> Result<Seq> {
        log.append(&mut storage.db, self.session_id, kind)
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

    /// Summary and blob for a JSON payload: compact text inline, pretty text
    /// in the blob. A JSON string is its own text.
    fn json_payload(&self, storage: &Storage, value: &Value) -> Result<(String, Option<BlobRef>)> {
        match value {
            Value::String(text) => self.payload(storage, text.clone(), text.clone().into_bytes()),
            _ => {
                let compact = serde_json::to_string(value)?;
                if compact.len() <= self.blob_threshold {
                    return Ok((compact, None));
                }
                self.payload(storage, compact, serde_json::to_vec_pretty(value)?)
            }
        }
    }

    /// `text` whole when it fits the threshold, else a preview of it and a
    /// blob holding `full`.
    fn payload(
        &self,
        storage: &Storage,
        text: String,
        full: Vec<u8>,
    ) -> Result<(String, Option<BlobRef>)> {
        if text.len() <= self.blob_threshold {
            return Ok((text, None));
        }
        let blob = storage.put_blob(full.as_slice())?;
        Ok((preview(&text, blob.size), Some(blob)))
    }
}

/// First non-blank line of `text`, cut at [`SUMMARY_MAX_CHARS`], followed by
/// an ellipsis and the line count and `size` of the whole.
fn preview(text: &str, size: u64) -> String {
    let first = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let mut out: String = first.chars().take(SUMMARY_MAX_CHARS).collect();
    out.push('…');
    let lines = text.lines().count();
    let unit = if lines == 1 { "line" } else { "lines" };
    out.push_str(&format!(" [{lines} {unit}, {}]", human_size(size)));
    out
}

fn human_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KIB {
        format!("{bytes} B")
    } else if b < KIB * KIB {
        format!("{:.1} KiB", b / KIB)
    } else {
        format!("{:.1} MiB", b / (KIB * KIB))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_takes_the_first_non_blank_line_and_reports_size() {
        let text = format!("\n  {}\nsecond\n", "x".repeat(200));
        let summary = preview(&text, 5000);
        assert_eq!(
            summary,
            format!("{}… [3 lines, 4.9 KiB]", "x".repeat(SUMMARY_MAX_CHARS))
        );
        assert_eq!(preview("ok", 2), "ok… [1 line, 2 B]");
    }

    #[test]
    fn preview_cuts_at_a_char_boundary() {
        let summary = preview(&"é".repeat(300), 600);
        assert!(summary.starts_with(&"é".repeat(SUMMARY_MAX_CHARS)));
        assert_eq!(human_size(3 * 1024 * 1024), "3.0 MiB");
    }
}
