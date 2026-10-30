//! The CLI's stream-json protocol as a pure state machine
//! (docs/agents/claude-code.md).
//!
//! [`Protocol`] does no I/O. The process driver feeds it every stdout line
//! with [`Protocol::on_line`] and every supervisor request with the other
//! methods; each call returns an [`Output`]: the lines to write to stdin and
//! the [`AdapterEvent`]s to report, in order. That keeps the translation
//! replayable against the recorded fixtures.
//!
//! **Turns.** The adapter, not the CLI, queues a message sent while a turn
//! runs: the next user message is written only after the turn's `result`, so
//! every written message is exactly one turn, framed by `TurnStarted` and
//! exactly one `TurnEnded`. The `result` ending a turn also gives what the
//! turn took, reported just before as `TurnUsage` ([`usage::turn_usage`]).
//!
//! **Approvals.** A `can_use_tool` request is reported as `ApprovalRequested`
//! and stays pending, with no timeout (D8), until it is answered, the CLI
//! withdraws it (`control_cancel_request`, or an error `tool_result` for its
//! tool call), or the turn ends; the last two are reported as
//! `ApprovalCancelled`. An answer to a request that is no longer pending
//! writes nothing. The request's `permission_suggestions` worth offering
//! ([`rules::offered`]) ride along as rule suggestions; an approval granting
//! one sends that update back as `updatedPermissions`, so the CLI keeps the
//! rule where it said it would. One the CLI keeps in its process only is
//! also added to the resume handle ([`rules::keep`]), reported anew, so a
//! resumed process is started with it again.

use std::collections::{HashMap, VecDeque};

use serde_json::{Value, json};
use skywave_proto::Decision;
use uuid::Uuid;

use super::{display, rules, usage};
use crate::agent::{AdapterEvent, AgentCallId, AgentRequestId, ResumeHandle, TurnOutcome};

/// The CLI version the protocol was confirmed against (T07). Another version
/// is logged as a warning, not refused.
pub const PINNED_VERSION: &str = "2.1.310";

/// The message sent with a denial; the CLI hands it to the model.
pub const DENY_MESSAGE: &str = "The user denied this action.";

/// Reason for approvals dropped because the turn was interrupted.
pub const INTERRUPT_REASON: &str = "interrupted";
/// Reason for approvals the CLI withdrew on its own.
pub const WITHDRAWN_REASON: &str = "withdrawn by the agent";
/// Reason for approvals still pending when their turn ended.
pub const TURN_ENDED_REASON: &str = "turn ended";

/// What one protocol step produces, in order.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Output {
    /// Lines for the CLI's stdin, one JSON object each.
    pub writes: Vec<Value>,
    /// Events to report to the supervisor.
    pub events: Vec<AdapterEvent>,
}

/// The resume handle naming CLI session `session_id` and the granted
/// permission updates its process keeps (`rules`, left out when empty).
pub fn resume_handle(session_id: Uuid, rules: &[Value]) -> ResumeHandle {
    let mut handle = json!({ "session_id": session_id.to_string() });
    if !rules.is_empty() {
        handle["rules"] = json!(rules);
    }
    ResumeHandle(handle)
}

/// The CLI session id and the kept permission updates in a handle made by
/// [`resume_handle`].
pub fn parse_resume_handle(handle: &ResumeHandle) -> Result<(Uuid, Vec<Value>), String> {
    let session_id = handle
        .0
        .get("session_id")
        .and_then(Value::as_str)
        .and_then(|id| Uuid::parse_str(id).ok())
        .ok_or_else(|| format!("not a resume handle of this adapter: {}", handle.0))?;
    let rules = handle
        .0
        .get("rules")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok((session_id, rules))
}

/// A user message: starts a turn.
pub fn user_message(text: &str) -> Value {
    json!({
        "type": "user",
        "message": { "role": "user", "content": text },
        "parent_tool_use_id": null,
        "session_id": "",
    })
}

/// The initialize handshake request.
pub fn initialize_request(request_id: &str) -> Value {
    json!({
        "type": "control_request",
        "request_id": request_id,
        "request": { "subtype": "initialize", "hooks": null },
    })
}

/// The request that stops the current turn.
pub fn interrupt_request(request_id: &str) -> Value {
    json!({
        "type": "control_request",
        "request_id": request_id,
        "request": { "subtype": "interrupt" },
    })
}

/// Allows permission request `request_id`, passing the tool's original
/// `input` back as `updatedInput`.
pub fn allow_response(request_id: &str, input: &Value) -> Value {
    control_success(
        request_id,
        json!({ "behavior": "allow", "updatedInput": input }),
    )
}

/// Allows permission request `request_id` as [`allow_response`] does and
/// applies `update`, one of the request's `permission_suggestions`, as its
/// `updatedPermissions`.
pub fn allow_always_response(request_id: &str, input: &Value, update: &Value) -> Value {
    control_success(
        request_id,
        json!({ "behavior": "allow", "updatedInput": input, "updatedPermissions": [update] }),
    )
}

/// Denies permission request `request_id`; the model sees `message`.
pub fn deny_response(request_id: &str, message: &str) -> Value {
    control_success(
        request_id,
        json!({ "behavior": "deny", "message": message }),
    )
}

/// Refuses a control request this adapter does not handle.
pub fn error_response(request_id: &str, error: &str) -> Value {
    json!({
        "type": "control_response",
        "response": { "subtype": "error", "request_id": request_id, "error": error },
    })
}

fn control_success(request_id: &str, response: Value) -> Value {
    json!({
        "type": "control_response",
        "response": { "subtype": "success", "request_id": request_id, "response": response },
    })
}

/// A `can_use_tool` request waiting for an answer.
#[derive(Debug)]
struct Pending {
    request: AgentRequestId,
    tool_use_id: Option<String>,
    input: Value,
    /// The permission update behind each suggestion offered, by index.
    updates: Vec<Value>,
}

/// The protocol state of one CLI process.
#[derive(Debug)]
pub struct Protocol {
    session_id: Uuid,
    resumed: bool,
    /// Granted permission updates the process keeps only in itself.
    session_rules: Vec<Value>,
    next_request: u64,
    initialize_id: Option<String>,
    ready: bool,
    startup_error: Option<String>,
    /// An error the CLI reported outside a turn after startup.
    error: Option<String>,
    /// The last stdout line that was not JSON (startup errors may land here).
    stray_output: Option<String>,
    version: Option<String>,
    /// The model the latest `system/init` named.
    model: Option<String>,
    turn_open: bool,
    interrupt_sent: bool,
    queued: VecDeque<String>,
    pending: Vec<Pending>,
    /// Tool name of each call this turn that has no result yet, by
    /// tool-use id: a structured result is read by its tool.
    tools: HashMap<String, String>,
    shutting_down: bool,
}

impl Protocol {
    /// State for a CLI process started with `--session-id session_id`, or
    /// with `--resume session_id` when `resumed`.
    pub fn new(session_id: Uuid, resumed: bool) -> Protocol {
        Protocol {
            session_id,
            resumed,
            session_rules: Vec::new(),
            next_request: 0,
            initialize_id: None,
            ready: false,
            startup_error: None,
            error: None,
            stray_output: None,
            version: None,
            model: None,
            turn_open: false,
            interrupt_sent: false,
            queued: VecDeque::new(),
            pending: Vec::new(),
            tools: HashMap::new(),
            shutting_down: false,
        }
    }

    /// The process was started with `rules`, the updates kept so far.
    pub fn with_session_rules(mut self, rules: Vec<Value>) -> Protocol {
        self.session_rules = rules;
        self
    }

    /// The first step: the initialize handshake and, for a fresh session,
    /// the resume handle (known up front from `--session-id`).
    pub fn start(&mut self) -> Output {
        let id = self.request_id();
        let mut out = Output {
            writes: vec![initialize_request(&id)],
            events: Vec::new(),
        };
        self.initialize_id = Some(id);
        if !self.resumed {
            out.events
                .push(AdapterEvent::ResumeHandle(self.resume_handle()));
        }
        out
    }

    /// The CLI answered the handshake; the session takes requests.
    pub fn ready(&self) -> bool {
        self.ready
    }

    /// Why the CLI failed before the handshake completed.
    pub fn startup_error(&self) -> Option<&str> {
        self.startup_error.as_deref()
    }

    /// The best error text the CLI gave, for an abnormal exit.
    pub fn exit_error(&self) -> Option<&str> {
        self.startup_error
            .as_deref()
            .or(self.error.as_deref())
            .or(self.stray_output.as_deref())
    }

    /// A message has been written and its `result` not yet read.
    pub fn turn_open(&self) -> bool {
        self.turn_open
    }

    /// The open turn has been asked to stop and its `result` not yet read.
    pub fn interrupting(&self) -> bool {
        self.turn_open && self.interrupt_sent
    }

    /// The CLI version reported by the handshake or `system/init`.
    pub fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    /// The CLI session id this process runs.
    pub fn session_id(&self) -> Uuid {
        self.session_id
    }

    /// Sends `text` now if idle, else queues it for the next turn.
    pub fn send_message(&mut self, text: String) -> Output {
        if self.turn_open {
            self.queued.push_back(text);
            Output::default()
        } else {
            self.begin_turn(&text)
        }
    }

    /// Answers approval `request`; nothing if it is no longer pending. An
    /// approval granting `rule` applies the update offered at that index; an
    /// index the request did not offer is only an approval.
    pub fn answer(
        &mut self,
        request: &AgentRequestId,
        decision: Decision,
        rule: Option<u8>,
    ) -> Output {
        let Some(index) = self.pending.iter().position(|p| p.request == *request) else {
            tracing::debug!(request = %request.0, "dropping an answer to an approval that is not pending");
            return Output::default();
        };
        let pending = self.pending.remove(index);
        let update = rule.and_then(|rule| pending.updates.get(usize::from(rule)));
        let mut out = Output::default();
        let write = match (decision, update) {
            (Decision::Approve, Some(update)) => {
                if rules::keep(&mut self.session_rules, update) {
                    out.events
                        .push(AdapterEvent::ResumeHandle(self.resume_handle()));
                }
                allow_always_response(&pending.request.0, &pending.input, update)
            }
            (Decision::Approve, None) => allow_response(&pending.request.0, &pending.input),
            (Decision::Deny, _) => deny_response(&pending.request.0, DENY_MESSAGE),
        };
        out.writes.push(write);
        out
    }

    /// Stops the current turn; nothing when idle or already interrupting.
    pub fn interrupt(&mut self) -> Output {
        if !self.turn_open || self.interrupt_sent {
            return Output::default();
        }
        self.interrupt_sent = true;
        let id = self.request_id();
        Output {
            writes: vec![interrupt_request(&id)],
            events: Vec::new(),
        }
    }

    /// The session is stopping: queued messages are dropped and no new turn
    /// starts.
    pub fn begin_shutdown(&mut self) {
        self.shutting_down = true;
        self.queued.clear();
    }

    /// Translates one stdout line.
    pub fn on_line(&mut self, line: &str) -> Output {
        let line = line.trim();
        if line.is_empty() {
            return Output::default();
        }
        match serde_json::from_str::<Value>(line) {
            Ok(message) => self.on_message(&message),
            Err(_) => {
                tracing::warn!("agent wrote a non-JSON line: {line}");
                self.stray_output = Some(line.to_owned());
                Output::default()
            }
        }
    }

    /// Translates one stdout message. Unknown types and fields are ignored.
    pub fn on_message(&mut self, message: &Value) -> Output {
        let mut out = Output::default();
        match str_field(message, "type") {
            "system" if str_field(message, "subtype") == "init" => self.on_init(message, &mut out),
            "assistant" => self.on_assistant(message, &mut out),
            "user" => self.on_user(message, &mut out),
            "control_request" => self.on_control_request(message, &mut out),
            "control_cancel_request" => self.on_cancel(message, &mut out),
            "control_response" => self.on_control_response(message),
            "result" => self.on_result(message, &mut out),
            // rate_limit_event, stream_event, other system subtypes: progress
            // signals only.
            _ => {}
        }
        out
    }

    fn resume_handle(&self) -> ResumeHandle {
        resume_handle(self.session_id, &self.session_rules)
    }

    fn request_id(&mut self) -> String {
        self.next_request += 1;
        format!("skywave_{}", self.next_request)
    }

    fn begin_turn(&mut self, text: &str) -> Output {
        self.turn_open = true;
        self.interrupt_sent = false;
        Output {
            writes: vec![user_message(text)],
            events: vec![AdapterEvent::TurnStarted],
        }
    }

    fn check_version(&mut self, version: Option<&str>) {
        let Some(version) = version else { return };
        if self.version.as_deref() == Some(version) {
            return;
        }
        if version != PINNED_VERSION {
            tracing::warn!(
                version,
                pinned = PINNED_VERSION,
                "agent CLI version differs from the one the adapter was confirmed against"
            );
        }
        self.version = Some(version.to_owned());
    }

    fn on_init(&mut self, message: &Value, out: &mut Output) {
        self.check_version(message.get("claude_code_version").and_then(Value::as_str));
        if let Some(model) = message.get("model").and_then(Value::as_str) {
            self.model = Some(model.to_owned());
        }
        if !self.turn_open {
            // A turn the CLI started on its own; frame it all the same.
            self.turn_open = true;
            self.interrupt_sent = false;
            out.events.push(AdapterEvent::TurnStarted);
        }
        let reported = message
            .get("session_id")
            .and_then(Value::as_str)
            .and_then(|id| Uuid::parse_str(id).ok());
        if let Some(id) = reported
            && id != self.session_id
        {
            tracing::info!(session_id = %id, "agent runs under another session id; keeping it");
            self.session_id = id;
            out.events
                .push(AdapterEvent::ResumeHandle(self.resume_handle()));
        }
    }

    fn on_assistant(&mut self, message: &Value, out: &mut Output) {
        let Some(blocks) = content_blocks(message) else {
            return;
        };
        for block in blocks {
            match str_field(block, "type") {
                "text" => {
                    let text = str_field(block, "text");
                    if !text.is_empty() {
                        out.events.push(AdapterEvent::Text(text.to_owned()));
                    }
                }
                "tool_use" => {
                    let name = str_field(block, "name").to_owned();
                    let input = block.get("input").cloned().unwrap_or(Value::Null);
                    let id = block.get("id").and_then(Value::as_str);
                    if let Some(id) = id {
                        self.tools.insert(id.to_owned(), name.clone());
                    }
                    out.events.push(AdapterEvent::ToolCall {
                        call: id.map(AgentCallId::from),
                        display: display::call_display(&name, &input),
                        name,
                        input,
                    });
                }
                // thinking blocks are opaque.
                _ => {}
            }
        }
    }

    fn on_user(&mut self, message: &Value, out: &mut Output) {
        // Text blocks here are the CLI's own markers ("[Request interrupted
        // by user]"); only tool results matter.
        let Some(blocks) = content_blocks(message) else {
            return;
        };
        let results: Vec<&Value> = blocks
            .iter()
            .filter(|b| str_field(b, "type") == "tool_result")
            .collect();
        // The structured result sits beside the content, so it is only
        // unambiguous for a message with one result (as the CLI sends them).
        let structured = match results.as_slice() {
            [_] => message.get("tool_use_result"),
            _ => None,
        };
        for block in results {
            let output = tool_output(block.get("content"));
            let is_error = block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let tool_use_id = block.get("tool_use_id").and_then(Value::as_str);
            // A result for a call still awaiting approval: the CLI gave up on
            // the approval (stdin closed, or a timeout of its own).
            if let Some(tool_use_id) = tool_use_id
                && let Some(index) = self
                    .pending
                    .iter()
                    .position(|p| p.tool_use_id.as_deref() == Some(tool_use_id))
            {
                let pending = self.pending.remove(index);
                let reason = output
                    .lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty())
                    .unwrap_or(WITHDRAWN_REASON)
                    .to_owned();
                out.events.push(AdapterEvent::ApprovalCancelled {
                    request: pending.request,
                    reason,
                });
            }
            let tool = tool_use_id.and_then(|id| self.tools.remove(id));
            out.events.push(AdapterEvent::ToolResult {
                call: tool_use_id.map(AgentCallId::from),
                result: display::result_display(tool.as_deref(), structured, output, is_error),
            });
        }
    }

    fn on_control_request(&mut self, message: &Value, out: &mut Output) {
        let request_id = str_field(message, "request_id");
        let request = message.get("request").unwrap_or(&Value::Null);
        let subtype = str_field(request, "subtype");
        if subtype != "can_use_tool" {
            tracing::warn!(
                subtype,
                "refusing an agent control request the adapter does not handle"
            );
            out.writes.push(error_response(
                request_id,
                &format!("unsupported control request: {subtype}"),
            ));
            return;
        }
        let tool = str_field(request, "tool_name").to_owned();
        let input = request.get("input").cloned().unwrap_or(Value::Null);
        let tool_use_id = request
            .get("tool_use_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let (suggestions, updates) = rules::offered(request.get("permission_suggestions"))
            .into_iter()
            .unzip();
        let request = AgentRequestId(request_id.to_owned());
        let call = tool_use_id.as_deref().map(AgentCallId::from);
        self.pending.push(Pending {
            request: request.clone(),
            tool_use_id,
            input: input.clone(),
            updates,
        });
        out.events.push(AdapterEvent::ApprovalRequested {
            request,
            call,
            tool,
            input,
            suggestions,
        });
    }

    fn on_cancel(&mut self, message: &Value, out: &mut Output) {
        let request_id = str_field(message, "request_id");
        let Some(index) = self.pending.iter().position(|p| p.request.0 == request_id) else {
            return;
        };
        let pending = self.pending.remove(index);
        let reason = if self.interrupt_sent {
            INTERRUPT_REASON
        } else {
            WITHDRAWN_REASON
        };
        out.events.push(AdapterEvent::ApprovalCancelled {
            request: pending.request,
            reason: reason.to_owned(),
        });
    }

    fn on_control_response(&mut self, message: &Value) {
        let response = message.get("response").unwrap_or(&Value::Null);
        let request_id = str_field(response, "request_id");
        if self.initialize_id.as_deref() != Some(request_id) || self.ready {
            // Interrupt acknowledgements carry nothing the adapter needs.
            return;
        }
        if str_field(response, "subtype") == "success" {
            // The reply also holds the account (email, org): never log it.
            let version = response
                .get("response")
                .and_then(|r| r.get("claude_code_version"))
                .and_then(Value::as_str);
            self.check_version(version);
            self.ready = true;
        } else {
            let error = response
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("initialize failed");
            self.startup_error = Some(error.to_owned());
        }
    }

    fn on_result(&mut self, message: &Value, out: &mut Output) {
        let is_error = message
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let subtype = str_field(message, "subtype");
        if !self.turn_open {
            if is_error {
                let error = result_error(message);
                if self.ready {
                    tracing::warn!("agent reported an error outside a turn: {error}");
                    self.error = Some(error);
                } else {
                    self.startup_error = Some(error);
                }
            }
            return;
        }
        let aborted = str_field(message, "terminal_reason").starts_with("aborted");
        let outcome = if !is_error && subtype == "success" {
            TurnOutcome::Completed
        } else if self.interrupt_sent || aborted {
            TurnOutcome::Interrupted
        } else {
            TurnOutcome::Failed(result_error(message))
        };
        let reason = if outcome == TurnOutcome::Interrupted {
            INTERRUPT_REASON
        } else {
            TURN_ENDED_REASON
        };
        for pending in self.pending.drain(..) {
            out.events.push(AdapterEvent::ApprovalCancelled {
                request: pending.request,
                reason: reason.to_owned(),
            });
        }
        let usage = usage::turn_usage(message, self.model.as_deref());
        if !usage.is_empty() {
            out.events.push(AdapterEvent::TurnUsage(usage));
        }
        out.events.push(AdapterEvent::TurnEnded(outcome));
        self.tools.clear();
        self.turn_open = false;
        self.interrupt_sent = false;
        if !self.shutting_down
            && let Some(next) = self.queued.pop_front()
        {
            let next = self.begin_turn(&next);
            out.writes.extend(next.writes);
            out.events.extend(next.events);
        }
    }
}

/// `message.content` when it is an array of blocks.
fn content_blocks(message: &Value) -> Option<&Vec<Value>> {
    message
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
}

/// The text of a `tool_result`'s content: a string, or an array of blocks
/// whose text blocks are joined (other blocks become `[<type>]`).
fn tool_output(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .map(|block| match str_field(block, "type") {
                "text" => str_field(block, "text").to_owned(),
                other => format!("[{other}]"),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

/// The error a failed `result` gives: its `errors`, else its `result` text.
fn result_error(message: &Value) -> String {
    let errors: Vec<&str> = message
        .get("errors")
        .and_then(Value::as_array)
        .map(|e| e.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if !errors.is_empty() {
        return errors.join("; ");
    }
    match message.get("result").and_then(Value::as_str) {
        Some(text) if !text.is_empty() => text.to_owned(),
        _ => format!("turn failed ({})", str_field(message, "subtype")),
    }
}

fn str_field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}
