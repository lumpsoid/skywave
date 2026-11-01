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
//! also added to the session's grants ([`rules::keep`]), reported anew as
//! [`AdapterEvent::Grants`], so every later process of the session is
//! started with it again.
//!
//! **Handle.** The resume handle is `{"session_id"}` alone. A fresh process
//! reports it with its first `TurnStarted`, once the CLI has a conversation
//! to resume (it keeps none for a session that never got a message); a
//! resumed one reports it only if the CLI switches session ids.
//!
//! **Mode and model** (docs/agents/claude-code.md §3.7). A request is a
//! `set_permission_mode` / `set_model` control request; its
//! `control_response` is the answer: success echoes the mode, or the model
//! as the CLI resolved it, and an error keeps the old one. The CLI also says
//! what it runs with unasked: `current_permission_mode` and the offered
//! `models` in the initialize answer, `permissionMode` and `model` in each
//! `system/init`, and a `system/status` line after every mode change. The
//! status for a change the adapter asked for may come late, after a later
//! answer, so such an echo is dropped rather than reported as the mode;
//! any other status (an approved `ExitPlanMode` returning to the mode before
//! plan) is reported.
//!
//! **Effort** (docs/agents/claude-code.md §3.10). A request is an
//! `apply_flag_settings` setting `effortLevel`, but its ack proves nothing
//! (the CLI acks a level it ignores), so the answer is the `get_settings`
//! written once the ack comes: `applied.effort` is the effort in force, or
//! `null` when the running model takes none. One such chain runs at a time;
//! a request asked while one runs waits for it, so each read answers only its
//! own request. Unasked reads follow the initialize answer and every model
//! switch, which can change the effort, and report what runs; one answered
//! while a chain runs or waits is dropped, as the chain's own read is newer.
//! A chain that ends without a read (an error) writes the dropped one again.

use std::collections::{HashMap, VecDeque};

use serde_json::{Value, json};
use skywave_proto::Decision;
use uuid::Uuid;

use super::{display, rules, usage};
use crate::agent::{
    AdapterEvent, AgentCallId, AgentRequestId, AgentTask, Effort, ModelChoice, PermissionMode,
    ResumeHandle, TaskKind, TurnOutcome,
};

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

/// The resume handle naming CLI session `session_id`.
pub fn resume_handle(session_id: Uuid) -> ResumeHandle {
    ResumeHandle(json!({ "session_id": session_id.to_string() }))
}

/// The CLI session id in a handle made by [`resume_handle`]. Other keys
/// are ignored: a handle stored before grants moved out may still carry
/// `rules`, which are not granted again.
pub fn parse_resume_handle(handle: &ResumeHandle) -> Result<Uuid, String> {
    handle
        .0
        .get("session_id")
        .and_then(Value::as_str)
        .and_then(|id| Uuid::parse_str(id).ok())
        .ok_or_else(|| format!("not a resume handle of this adapter: {}", handle.0))
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

/// The request that switches the permission mode.
pub fn set_permission_mode_request(request_id: &str, mode: PermissionMode) -> Value {
    json!({
        "type": "control_request",
        "request_id": request_id,
        "request": { "subtype": "set_permission_mode", "mode": mode_name(mode) },
    })
}

/// The request that switches the model.
pub fn set_model_request(request_id: &str, model: &str) -> Value {
    json!({
        "type": "control_request",
        "request_id": request_id,
        "request": { "subtype": "set_model", "model": model },
    })
}

/// The request that sets the effort flag; the ack proves nothing.
pub fn apply_effort_request(request_id: &str, effort: Effort) -> Value {
    json!({
        "type": "control_request",
        "request_id": request_id,
        "request": {
            "subtype": "apply_flag_settings",
            "settings": { "effortLevel": effort_name(effort) },
        },
    })
}

/// The request that reads back the settings in force.
pub fn get_settings_request(request_id: &str) -> Value {
    json!({
        "type": "control_request",
        "request_id": request_id,
        "request": { "subtype": "get_settings" },
    })
}

/// What the CLI calls `mode`.
pub fn mode_name(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Ask => "default",
        PermissionMode::AcceptEdits => "acceptEdits",
        PermissionMode::Plan => "plan",
        PermissionMode::Auto => "auto",
        PermissionMode::DontAsk => "dontAsk",
        PermissionMode::Bypass => "bypassPermissions",
    }
}

/// The mode the CLI calls `name`; `None` for one it added since.
pub fn parse_mode(name: &str) -> Option<PermissionMode> {
    PermissionMode::ALL
        .into_iter()
        .find(|mode| mode_name(*mode) == name)
}

/// What the CLI calls `effort`.
pub fn effort_name(effort: Effort) -> &'static str {
    match effort {
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
        Effort::XHigh => "xhigh",
        Effort::Max => "max",
    }
}

/// The effort the CLI calls `name`; `None` for one it added since.
pub fn parse_effort(name: &str) -> Option<Effort> {
    Effort::ALL
        .into_iter()
        .find(|effort| effort_name(*effort) == name)
}

/// The models an initialize answer offers, each with the effort levels it
/// takes (`supportedEffortLevels`, when it `supportsEffort`).
pub fn offered_models(models: Option<&Value>) -> Vec<ModelChoice> {
    let Some(models) = models.and_then(Value::as_array) else {
        return Vec::new();
    };
    models
        .iter()
        .filter_map(|model| {
            let id = model.get("value")?.as_str()?.to_owned();
            let text = |key| {
                model
                    .get(key)
                    .and_then(Value::as_str)
                    .filter(|t| !t.is_empty())
                    .map(str::to_owned)
            };
            let efforts = match model.get("supportsEffort").and_then(Value::as_bool) {
                Some(true) => model
                    .get("supportedEffortLevels")
                    .and_then(Value::as_array)
                    .map(|levels| {
                        levels
                            .iter()
                            .filter_map(Value::as_str)
                            .filter_map(parse_effort)
                            .collect()
                    })
                    .unwrap_or_default(),
                _ => Vec::new(),
            };
            Some(ModelChoice {
                label: text("displayName").unwrap_or_else(|| id.clone()),
                detail: text("description"),
                resolved: text("resolvedModel"),
                id,
                efforts,
            })
        })
        .collect()
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
    /// The handle naming `session_id` has been reported, or was stored
    /// already (a resumed process). A fresh process reports it with its
    /// first turn.
    handle_reported: bool,
    /// Granted permission updates the process keeps only in itself: the
    /// session's grants.
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
    /// Mode, model and effort requests and effort reads the CLI has not
    /// answered, by request id.
    settings: HashMap<String, Asked>,
    /// Effort requests waiting for the chain in flight, oldest first.
    efforts: VecDeque<Effort>,
    /// An effort chain (apply, then its read) has not ended.
    effort_in_flight: bool,
    /// An unasked read was dropped while a chain ran, and no read has
    /// answered since: what it saw is not in the log.
    effort_unread: bool,
    /// Modes switched to on request whose `system/status` echo has not
    /// come yet, oldest first.
    echoes: VecDeque<PermissionMode>,
}

/// A control request waiting for its `control_response`.
#[derive(Debug, Clone)]
enum Asked {
    Mode(PermissionMode),
    Model(String),
    /// The `apply_flag_settings` that starts an effort chain.
    EffortApply(Effort),
    /// A `get_settings`: the answer to the effort `asked`, or, for `None`,
    /// a look at what runs.
    EffortRead {
        asked: Option<Effort>,
    },
}

impl Protocol {
    /// State for a CLI process started with `--session-id session_id`, or
    /// with `--resume session_id` when `resumed`.
    pub fn new(session_id: Uuid, resumed: bool) -> Protocol {
        Protocol {
            session_id,
            handle_reported: resumed,
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
            settings: HashMap::new(),
            efforts: VecDeque::new(),
            effort_in_flight: false,
            effort_unread: false,
            echoes: VecDeque::new(),
        }
    }

    /// The process was started with `rules`, the session's grants so far.
    pub fn with_session_rules(mut self, rules: Vec<Value>) -> Protocol {
        self.session_rules = rules;
        self
    }

    /// The first step: the initialize handshake. A fresh session reports
    /// its handle with its first turn, not here: the CLI keeps no
    /// conversation for a session that never got a message.
    pub fn start(&mut self) -> Output {
        let id = self.request_id();
        let out = Output {
            writes: vec![initialize_request(&id)],
            events: Vec::new(),
        };
        self.initialize_id = Some(id);
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
                        .push(AdapterEvent::Grants(rules::grants(&self.session_rules)));
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

    /// Asks the CLI to switch to `mode`; answered by `PermissionModeSet` or
    /// `PermissionModeRefused` when its `control_response` comes.
    pub fn set_permission_mode(&mut self, mode: PermissionMode) -> Output {
        let id = self.request_id();
        let write = set_permission_mode_request(&id, mode);
        self.settings.insert(id, Asked::Mode(mode));
        Output {
            writes: vec![write],
            events: Vec::new(),
        }
    }

    /// Asks the CLI to switch to `model`; answered by `ModelSet` or
    /// `ModelRefused`.
    pub fn set_model(&mut self, model: String) -> Output {
        let id = self.request_id();
        let write = set_model_request(&id, &model);
        self.settings.insert(id, Asked::Model(model));
        Output {
            writes: vec![write],
            events: Vec::new(),
        }
    }

    /// Asks the CLI to run at `effort`; answered by `EffortSet` or
    /// `EffortRefused` once the `get_settings` after its apply comes back.
    /// Asked while another effort chain runs, it waits for that one to end.
    pub fn set_effort(&mut self, effort: Effort) -> Output {
        self.efforts.push_back(effort);
        let mut out = Output::default();
        self.next_effort(&mut out);
        out
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
            "system" if str_field(message, "subtype") == "status" => {
                self.on_status(message, &mut out);
            }
            "system" if str_field(message, "subtype") == "background_tasks_changed" => {
                self.on_background_tasks(message, &mut out);
            }
            "assistant" => self.on_assistant(message, &mut out),
            "user" => self.on_user(message, &mut out),
            "control_request" => self.on_control_request(message, &mut out),
            "control_cancel_request" => self.on_cancel(message, &mut out),
            "control_response" => self.on_control_response(message, &mut out),
            "result" => self.on_result(message, &mut out),
            // rate_limit_event, stream_event, other system subtypes: progress
            // signals only. That includes the edge events of tasks
            // (task_started, task_updated, task_progress, task_notification):
            // the task set comes from background_tasks_changed alone.
            _ => {}
        }
        out
    }

    /// Reports `TurnStarted`, after the resume handle when it is the first
    /// turn of a fresh process: from that message on the CLI has a
    /// conversation to resume.
    fn open_turn(&mut self, out: &mut Output) {
        self.turn_open = true;
        self.interrupt_sent = false;
        if !self.handle_reported {
            self.handle_reported = true;
            out.events
                .push(AdapterEvent::ResumeHandle(resume_handle(self.session_id)));
        }
        out.events.push(AdapterEvent::TurnStarted);
    }

    fn request_id(&mut self) -> String {
        self.next_request += 1;
        format!("skywave_{}", self.next_request)
    }

    /// Starts the oldest waiting effort chain, unless one is in flight.
    fn next_effort(&mut self, out: &mut Output) {
        if self.effort_in_flight {
            return;
        }
        let Some(effort) = self.efforts.pop_front() else {
            return;
        };
        self.effort_in_flight = true;
        let id = self.request_id();
        out.writes.push(apply_effort_request(&id, effort));
        self.settings.insert(id, Asked::EffortApply(effort));
    }

    /// The effort chain in flight ended: the next one starts. With none
    /// waiting, a read dropped during the chain that nothing replaced (the
    /// chain ended without reading) is written again.
    fn end_effort(&mut self, out: &mut Output) {
        self.effort_in_flight = false;
        self.next_effort(out);
        if self.effort_unread && !self.effort_busy() {
            self.effort_unread = false;
            self.read_effort(None, out);
        }
    }

    /// An effort chain runs or waits, so a read that is not its own is
    /// older than the read that will end it.
    fn effort_busy(&self) -> bool {
        self.effort_in_flight || !self.efforts.is_empty()
    }

    /// Writes a `get_settings` answering the effort `asked`, or, for `None`,
    /// looking at what runs.
    fn read_effort(&mut self, asked: Option<Effort>, out: &mut Output) {
        let id = self.request_id();
        out.writes.push(get_settings_request(&id));
        self.settings.insert(id, Asked::EffortRead { asked });
    }

    fn begin_turn(&mut self, text: &str) -> Output {
        let mut out = Output {
            writes: vec![user_message(text)],
            events: Vec::new(),
        };
        self.open_turn(&mut out);
        out
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
            out.events.push(AdapterEvent::Model(model.to_owned()));
        }
        if let Some(mode) = message
            .get("permissionMode")
            .and_then(Value::as_str)
            .and_then(parse_mode)
        {
            out.events.push(AdapterEvent::PermissionMode(mode));
        }
        let reported = message
            .get("session_id")
            .and_then(Value::as_str)
            .and_then(|id| Uuid::parse_str(id).ok());
        // A handle reported with the old id is reported anew; one not
        // reported yet goes out once, with the new id, as the turn opens.
        let mut report_again = false;
        if let Some(id) = reported
            && id != self.session_id
        {
            tracing::info!(session_id = %id, "agent runs under another session id; keeping it");
            self.session_id = id;
            report_again = self.handle_reported;
        }
        if !self.turn_open {
            // A turn the CLI started on its own; frame it all the same.
            self.open_turn(out);
        }
        if report_again {
            out.events
                .push(AdapterEvent::ResumeHandle(resume_handle(self.session_id)));
        }
    }

    /// `system/background_tasks_changed`: every live task, replacing the
    /// last report.
    fn on_background_tasks(&mut self, message: &Value, out: &mut Output) {
        let tasks = message
            .get("tasks")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter_map(|task| {
                let id = task.get("task_id").and_then(Value::as_str)?;
                let kind = match str_field(task, "task_type") {
                    "local_bash" => TaskKind::Shell,
                    "local_agent" => TaskKind::Subagent,
                    _ => TaskKind::Other,
                };
                Some(AgentTask {
                    id: id.to_owned(),
                    kind,
                    title: str_field(task, "description").to_owned(),
                    parent: task
                        .get("parent_task_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    ambient: task
                        .get("ambient")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                })
            })
            .collect();
        out.events.push(AdapterEvent::BackgroundTasks(tasks));
    }

    fn on_assistant(&mut self, message: &Value, out: &mut Output) {
        if is_subagent_message(message) {
            return;
        }
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
        if is_subagent_message(message) {
            return;
        }
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

    /// A `system/status`: the mode now, unless it echoes a switch the
    /// adapter asked for.
    fn on_status(&mut self, message: &Value, out: &mut Output) {
        let Some(mode) = message
            .get("permissionMode")
            .and_then(Value::as_str)
            .and_then(parse_mode)
        else {
            return;
        };
        if let Some(index) = self.echoes.iter().position(|echo| *echo == mode) {
            // Echoes come in order: older ones that never came are gone.
            self.echoes.drain(..=index);
            return;
        }
        out.events.push(AdapterEvent::PermissionMode(mode));
    }

    /// The answer to a mode, model or effort request, or to an effort read.
    fn on_setting_response(&mut self, asked: Asked, response: &Value, out: &mut Output) {
        let success = str_field(response, "subtype") == "success";
        let reason = || {
            response
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("refused")
                .to_owned()
        };
        let body = response.get("response").unwrap_or(&Value::Null);
        match asked {
            Asked::Mode(mode) if success => {
                let mode = body
                    .get("mode")
                    .and_then(Value::as_str)
                    .and_then(parse_mode)
                    .unwrap_or(mode);
                self.echoes.push_back(mode);
                out.events.push(AdapterEvent::PermissionModeSet(mode));
            }
            Asked::Mode(mode) => out.events.push(AdapterEvent::PermissionModeRefused {
                mode,
                reason: reason(),
            }),
            Asked::Model(model) if success => {
                let resolved = body
                    .get("model")
                    .and_then(Value::as_str)
                    .filter(|m| !m.is_empty())
                    .map_or(model, str::to_owned);
                self.model = Some(resolved.clone());
                out.events.push(AdapterEvent::ModelSet(resolved));
                // Another model may take another effort, or none.
                self.read_effort(None, out);
            }
            Asked::Model(model) => out.events.push(AdapterEvent::ModelRefused {
                model,
                reason: reason(),
            }),
            // Written only now: a read sent with the apply could be answered
            // before the apply is.
            Asked::EffortApply(effort) if success => self.read_effort(Some(effort), out),
            Asked::EffortApply(effort) => {
                out.events.push(AdapterEvent::EffortRefused {
                    effort,
                    reason: reason(),
                });
                self.end_effort(out);
            }
            Asked::EffortRead { asked } => self.on_effort_read(asked, response, out),
        }
    }

    /// The answer to a `get_settings`. The body also holds the merged
    /// settings (env, hooks, permission rules): never log it.
    fn on_effort_read(&mut self, asked: Option<Effort>, response: &Value, out: &mut Output) {
        let success = str_field(response, "subtype") == "success";
        let applied = response
            .get("response")
            .and_then(|body| body.get("applied"));
        let raw = applied.and_then(|applied| applied.get("effort"));
        let in_force = if success { applied_effort(raw) } else { None };
        let raw = match raw {
            Some(Value::String(name)) => name.clone(),
            Some(other) => other.to_string(),
            None => "nothing".to_owned(),
        };
        if success && in_force.is_none() {
            tracing::warn!(effort = %raw, "agent reports an effort the adapter does not know");
        }
        let Some(effort) = asked else {
            match in_force {
                Some(in_force) if !self.effort_busy() => {
                    out.events.push(AdapterEvent::Effort(in_force));
                }
                Some(_) => self.effort_unread = true,
                None if !success => {
                    tracing::warn!(
                        "cannot read the agent's settings: {}",
                        str_field(response, "error")
                    );
                }
                None => {}
            }
            return;
        };
        match in_force {
            Some(in_force) if in_force == Some(effort) => {
                out.events.push(AdapterEvent::EffortSet(in_force));
            }
            Some(in_force) => {
                let reason = match in_force {
                    Some(kept) => format!("kept {}", effort_name(kept)),
                    None => {
                        let model = applied
                            .and_then(|applied| applied.get("model"))
                            .and_then(Value::as_str)
                            .or(self.model.as_deref())
                            .unwrap_or("the model");
                        format!("{model} takes no effort level")
                    }
                };
                out.events
                    .push(AdapterEvent::EffortRefused { effort, reason });
                out.events.push(AdapterEvent::Effort(in_force));
            }
            None => {
                let reason = if success {
                    format!("agent reports effort {raw}")
                } else {
                    response
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("refused")
                        .to_owned()
                };
                out.events
                    .push(AdapterEvent::EffortRefused { effort, reason });
            }
        }
        if success {
            // This read is newer than any dropped one.
            self.effort_unread = false;
        }
        self.end_effort(out);
    }

    fn on_control_response(&mut self, message: &Value, out: &mut Output) {
        let response = message.get("response").unwrap_or(&Value::Null);
        let request_id = str_field(response, "request_id");
        if let Some(asked) = self.settings.remove(request_id) {
            self.on_setting_response(asked, response, out);
            return;
        }
        if self.initialize_id.as_deref() != Some(request_id) || self.ready {
            // Interrupt acknowledgements carry nothing the adapter needs.
            return;
        }
        if str_field(response, "subtype") == "success" {
            // The reply also holds the account (email, org): never log it.
            let body = response.get("response").unwrap_or(&Value::Null);
            let version = body.get("claude_code_version").and_then(Value::as_str);
            self.check_version(version);
            if let Some(mode) = body
                .get("current_permission_mode")
                .and_then(Value::as_str)
                .and_then(parse_mode)
            {
                out.events.push(AdapterEvent::PermissionMode(mode));
            }
            let models = offered_models(body.get("models"));
            if !models.is_empty() {
                out.events.push(AdapterEvent::ModelsOffered(models));
            }
            self.ready = true;
            // The CLI reports no task set at start: nothing is running.
            out.events.push(AdapterEvent::BackgroundTasks(Vec::new()));
            // Neither this answer nor `system/init` names the effort.
            self.read_effort(None, out);
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

/// The effort a `get_settings` answer's `applied.effort` names: `Some(None)`
/// for `null` (the running model takes none), `None` for a level this
/// adapter does not know or no effort at all.
fn applied_effort(raw: Option<&Value>) -> Option<Option<Effort>> {
    match raw? {
        Value::Null => Some(None),
        Value::String(name) => parse_effort(name).map(Some),
        _ => None,
    }
}

fn str_field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

/// A subagent's own message, which comes after the main turn's `result` and
/// outside any turn. Not reported: the main agent's turn quotes what the
/// subagent found.
fn is_subagent_message(message: &Value) -> bool {
    message
        .get("parent_tool_use_id")
        .is_some_and(|id| !id.is_null())
}
