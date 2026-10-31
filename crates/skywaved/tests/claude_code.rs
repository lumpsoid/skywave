//! Claude Code adapter (T09).
//!
//! Replay tests feed the T07 recordings (`tests/fixtures/claude-code/`)
//! through the protocol state machine: every recorded stdout line goes in,
//! and every recorded stdin line must be what the adapter itself would have
//! written at that point. Process tests run the adapter against
//! `tests/support/fake-claude.sh`, a stand-in that replies only to what it
//! reads, so they are deterministic.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use skywave_proto::{Decision, SessionId};
use skywaved::agent::claude_code::protocol::{self, INTERRUPT_REASON, PINNED_VERSION, Protocol};
use skywaved::agent::claude_code::rules;
use skywaved::agent::claude_code::{ClaudeCodeAdapter, REQUIRED_ARGS};
use skywaved::agent::{
    AdapterEvent, AdapterEvents, AgentAdapter, AgentError, AgentRequestId, AgentSession,
    BodyFormat, DisplayBody, ModelChoice, PermissionMode, ResultDisplay, ResumeHandle, RuleScope,
    RuleSuggestion, StartSpec, ToolDisplay, ToolKind, TurnOutcome, TurnUsage,
};
use skywaved::config::AgentConfig;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Replay

fn fixture(name: &str) -> Vec<Value> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/claude-code")
        .join(format!("{name}.ndjson"));
    std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// Our own control requests carry ids we choose; compare them without.
fn normalized(message: &Value) -> Value {
    let mut message = message.clone();
    if message["type"] == "control_request" {
        message.as_object_mut().unwrap().remove("request_id");
    }
    message
}

/// Replays fixture `name` through a fresh protocol for CLI session
/// `session_id` and returns the events it reported. The `n`th recorded stdin
/// line (0-based) must be what the protocol writes for the matching request,
/// except those listed in `dropped`, for which it must write nothing. A
/// recorded allow that grants a rule is answered by granting the offered
/// rule that is its `updatedPermissions`. The CLI must be the pinned version.
fn replay(name: &str, session_id: Uuid, resumed: bool, dropped: &[usize]) -> Vec<AdapterEvent> {
    let (events, version) = replay_any_version(name, session_id, resumed, dropped);
    assert_eq!(version.as_deref(), Some(PINNED_VERSION));
    events
}

/// [`replay`] for a CLI of any version; returns the version it reported.
fn replay_any_version(
    name: &str,
    session_id: Uuid,
    resumed: bool,
    dropped: &[usize],
) -> (Vec<AdapterEvent>, Option<String>) {
    let mut protocol = Protocol::new(session_id, resumed);
    let start = protocol.start();
    let mut events = start.events;
    let mut written: VecDeque<Value> = start.writes.into();
    let mut n_in = 0;
    // The `permission_suggestions` of each recorded `can_use_tool`, by id.
    let mut offers: HashMap<String, Value> = HashMap::new();
    // The id of each of our control requests, by the recorded one.
    let mut our_ids: HashMap<String, String> = HashMap::new();
    for record in fixture(name) {
        let msg = &record["msg"];
        match record["dir"].as_str().unwrap() {
            "in" => {
                let out = match (
                    msg["type"].as_str().unwrap(),
                    msg["request"]["subtype"].as_str(),
                ) {
                    // Written by `start`.
                    ("control_request", Some("initialize")) => Default::default(),
                    ("control_request", Some("interrupt")) => protocol.interrupt(),
                    // A mode the adapter has no name for, or no model, it
                    // can never send: the test lists it as dropped.
                    ("control_request", Some("set_permission_mode")) => msg["request"]["mode"]
                        .as_str()
                        .and_then(protocol::parse_mode)
                        .map(|mode| protocol.set_permission_mode(mode))
                        .unwrap_or_default(),
                    ("control_request", Some("set_model")) => msg["request"]["model"]
                        .as_str()
                        .map(|model| protocol.set_model(model.to_owned()))
                        .unwrap_or_default(),
                    ("user", _) => protocol
                        .send_message(msg["message"]["content"].as_str().unwrap().to_owned()),
                    ("control_response", _) => {
                        let response = &msg["response"];
                        let decision = match response["response"]["behavior"].as_str() {
                            Some("allow") => Decision::Approve,
                            _ => Decision::Deny,
                        };
                        let request = response["request_id"].as_str().unwrap();
                        let rule = granted_rule(&offers, request, &response["response"]);
                        protocol.answer(&AgentRequestId::from(request), decision, rule)
                    }
                    other => panic!("{name}: unexpected stdin record {other:?}"),
                };
                events.extend(out.events);
                written.extend(out.writes);
                if dropped.contains(&n_in) {
                    assert!(
                        written.is_empty(),
                        "{name}: stdin #{n_in} should write nothing, wrote {written:?}"
                    );
                } else {
                    let ours = written
                        .pop_front()
                        .unwrap_or_else(|| panic!("{name}: nothing written for stdin #{n_in}"));
                    assert_eq!(normalized(&ours), normalized(msg), "{name}: stdin #{n_in}");
                    if let (Some(recorded), Some(id)) =
                        (msg["request_id"].as_str(), ours["request_id"].as_str())
                    {
                        our_ids.insert(recorded.to_owned(), id.to_owned());
                    }
                }
                n_in += 1;
            }
            "out" => {
                if msg["request"]["subtype"] == "can_use_tool" {
                    offers.insert(
                        msg["request_id"].as_str().unwrap().to_owned(),
                        msg["request"]["permission_suggestions"].clone(),
                    );
                }
                // The CLI answers our control requests under the ids we
                // chose, not the recorded driver's.
                let mut msg = msg.clone();
                if let Some(id) = msg["response"]["request_id"]
                    .as_str()
                    .and_then(|recorded| our_ids.get(recorded))
                {
                    msg["response"]["request_id"] = json!(id);
                }
                let line = serde_json::to_string(&msg).unwrap();
                let out = protocol.on_line(&line);
                assert!(
                    out.writes.is_empty(),
                    "{name}: unexpected writes {:?}",
                    out.writes
                );
                events.extend(out.events);
            }
            _ => {}
        }
    }
    assert!(written.is_empty(), "{name}: left over writes {written:?}");
    assert!(!protocol.turn_open(), "{name}: a turn is still open");
    (events, protocol.version().map(str::to_owned))
}

/// The rule a recorded allow `response` to request `request` grants: the
/// index, among the rules the request offered, of its `updatedPermissions`.
fn granted_rule(offers: &HashMap<String, Value>, request: &str, response: &Value) -> Option<u8> {
    let granted = response.get("updatedPermissions")?;
    let [update] = granted.as_array().map(Vec::as_slice).unwrap_or_default() else {
        panic!("request {request}: expected one granted update, got {granted}");
    };
    let offered = rules::offered(offers.get(request));
    let index = offered
        .iter()
        .position(|(_, u)| u == update)
        .unwrap_or_else(|| panic!("request {request}: {update} was not offered"));
    Some(u8::try_from(index).unwrap())
}

/// One line per event, cut short, for readable expectations. What the agent
/// reports unasked about its mode and model is left out ([`settings`] lists
/// it).
fn describe(events: &[AdapterEvent]) -> Vec<String> {
    fn cut(text: &str) -> String {
        let first = text.lines().next().unwrap_or("");
        first.chars().take(48).collect()
    }
    events
        .iter()
        .filter(|event| !observes_settings(event))
        .map(|event| match event {
            AdapterEvent::TurnStarted => "turn started".to_owned(),
            AdapterEvent::Text(text) => format!("text: {}", cut(text)),
            AdapterEvent::ToolCall { name, .. } => format!("tool call: {name}"),
            AdapterEvent::ToolResult { result, .. } => {
                let kind = if result.is_error { "error" } else { "ok" };
                format!("tool result {kind}: {}", cut(&result.output))
            }
            AdapterEvent::ApprovalRequested { request, tool, .. } => {
                format!("approval {}: {tool}", request.0)
            }
            AdapterEvent::ApprovalCancelled { request, reason } => {
                format!("cancelled {}: {}", request.0, cut(reason))
            }
            AdapterEvent::TurnUsage(_) => "turn usage".to_owned(),
            AdapterEvent::TurnEnded(outcome) => format!("turn ended: {outcome:?}"),
            AdapterEvent::ResumeHandle(handle) => {
                format!(
                    "resume handle: {}",
                    handle.0["session_id"].as_str().unwrap()
                )
            }
            AdapterEvent::Exited { error } => format!("exited: {error:?}"),
            AdapterEvent::PermissionMode(mode) => format!("mode: {mode:?}"),
            AdapterEvent::PermissionModeSet(mode) => format!("mode set: {mode:?}"),
            AdapterEvent::PermissionModeRefused { mode, reason } => {
                format!("mode refused {mode:?}: {}", cut(reason))
            }
            AdapterEvent::Model(model) => format!("model: {model}"),
            AdapterEvent::ModelSet(model) => format!("model set: {model}"),
            AdapterEvent::ModelRefused { model, reason } => {
                format!("model refused {model}: {}", cut(reason))
            }
            AdapterEvent::ModelsOffered(models) => format!("models offered: {}", models.len()),
        })
        .collect()
}

/// What the agent reports unasked about its mode and model.
fn observes_settings(event: &AdapterEvent) -> bool {
    matches!(
        event,
        AdapterEvent::PermissionMode(_) | AdapterEvent::Model(_) | AdapterEvent::ModelsOffered(_)
    )
}

/// `events` as [`describe`] lists them.
fn without_settings(events: Vec<AdapterEvent>) -> Vec<AdapterEvent> {
    events
        .into_iter()
        .filter(|e| !observes_settings(e))
        .collect()
}

/// One line per event about the mode or model, as [`describe`] puts it.
fn settings(events: &[AdapterEvent]) -> Vec<String> {
    let settings: Vec<AdapterEvent> = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                AdapterEvent::PermissionMode(_)
                    | AdapterEvent::PermissionModeSet(_)
                    | AdapterEvent::PermissionModeRefused { .. }
                    | AdapterEvent::Model(_)
                    | AdapterEvent::ModelSet(_)
                    | AdapterEvent::ModelRefused { .. }
                    | AdapterEvent::ModelsOffered(_)
            )
        })
        .cloned()
        .collect();
    settings
        .iter()
        .map(|event| match event {
            AdapterEvent::PermissionMode(mode) => format!("mode: {mode:?}"),
            AdapterEvent::Model(model) => format!("model: {model}"),
            AdapterEvent::ModelsOffered(models) => format!("models offered: {}", models.len()),
            other => describe(std::slice::from_ref(other)).remove(0),
        })
        .collect()
}

/// One line per tool call and result: how it is shown.
fn displays(events: &[AdapterEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            AdapterEvent::ToolCall {
                display: Some(d), ..
            } => Some(format!(
                "{:?} {:?} detail={:?} body={:?}",
                d.kind,
                d.title,
                d.detail,
                d.body.as_ref().map(|b| (b.format, b.text.lines().count())),
            )),
            AdapterEvent::ToolCall { name, .. } => Some(format!("{name}: generic")),
            AdapterEvent::ToolResult { result, .. } => Some(format!(
                "error={} status={:?} preview={:?}",
                result.is_error,
                result.status,
                result.preview.as_deref().unwrap_or(&result.output),
            )),
            _ => None,
        })
        .collect()
}

/// No result preview holds the CLI's notes for the model.
fn assert_no_model_notes(events: &[AdapterEvent]) {
    for event in events {
        if let AdapterEvent::ToolResult { result, .. } = event {
            let preview = result.preview.as_deref().unwrap_or(&result.output);
            assert!(
                !preview.contains("file state is current") && !preview.contains("STOP what"),
                "a preview with model notes: {preview:?}"
            );
        }
    }
}

fn uuid(id: &str) -> Uuid {
    Uuid::parse_str(id).unwrap()
}

const BASIC_SESSION: &str = "ce771f8e-e3a8-428f-b5d7-e2f40233a2b6";

#[test]
fn replay_basic_two_turns() {
    let events = replay("basic", uuid(BASIC_SESSION), false, &[]);
    assert_eq!(
        describe(&events),
        [
            "resume handle: ce771f8e-e3a8-428f-b5d7-e2f40233a2b6",
            "turn started",
            "text: Hi there!",
            "turn usage",
            "turn ended: Completed",
            "turn started",
            "text: You asked me to say hi in exactly two words.",
            "turn usage",
            "turn ended: Completed",
        ]
    );
}

#[test]
fn replay_resume_reports_no_new_handle() {
    let events = replay("resume", uuid(BASIC_SESSION), true, &[]);
    assert_eq!(
        describe(&events),
        [
            "turn started",
            "text: \"Say hi in exactly two words.\"",
            "turn usage",
            "turn ended: Completed",
        ]
    );
}

/// The turn usage reported, in order, costs apart: serde_json's float
/// parsing may be a bit off the recorded digits, so costs compare within
/// [`COST_EPSILON`].
fn usages(events: &[AdapterEvent]) -> (Vec<TurnUsage>, Vec<Option<f64>>) {
    events
        .iter()
        .filter_map(|event| match event {
            AdapterEvent::TurnUsage(usage) => Some((
                TurnUsage {
                    cost_usd: None,
                    ..usage.clone()
                },
                usage.cost_usd,
            )),
            _ => None,
        })
        .unzip()
}

const COST_EPSILON: f64 = 1e-12;

fn assert_costs(actual: &[Option<f64>], expected: &[f64]) {
    assert_eq!(actual.len(), expected.len(), "{actual:?}");
    for (actual, expected) in actual.iter().zip(expected) {
        let actual = actual.unwrap_or_else(|| panic!("no cost, expected {expected}"));
        assert!(
            (actual - expected).abs() < COST_EPSILON,
            "{actual} != {expected}"
        );
    }
}

/// The `total_cost_usd` of each `result` in fixture `name`.
fn recorded_totals(name: &str) -> Vec<f64> {
    fixture(name)
        .iter()
        .filter(|r| r["dir"] == "out" && r["msg"]["type"] == "result")
        .map(|r| r["msg"]["total_cost_usd"].as_f64().unwrap())
        .collect()
}

const HAIKU: &str = "claude-haiku-5-5";

/// A measured turn, cost apart.
fn haiku_turn(duration_ms: u64, tokens_in: u64, tokens_out: u64) -> TurnUsage {
    TurnUsage {
        duration_ms: Some(duration_ms),
        tokens_in: Some(tokens_in),
        tokens_out: Some(tokens_out),
        cost_usd: None,
        model: Some(HAIKU.to_owned()),
    }
}

#[test]
fn replay_usage_is_per_turn_across_turns_and_a_resume() {
    // The CLI's `total_cost_usd` runs over the session: within a process,
    // and on into a resumed one.
    let (basic_totals, resume_totals) = (recorded_totals("basic"), recorded_totals("resume"));
    let (first, second, third) = (0.00142969, 0.00086724, 0.0002183);
    assert_costs(
        &[
            Some(basic_totals[0]),
            Some(basic_totals[1]),
            Some(resume_totals[0]),
        ],
        &[first, first + second, first + second + third],
    );

    // The adapter reports each turn's own share instead.
    let (basic, costs) = usages(&replay("basic", uuid(BASIC_SESSION), false, &[]));
    assert_eq!(
        basic,
        [
            haiku_turn(1648, 2 + 12_799 + 6_490, 7),
            haiku_turn(1255, 2 + 15_834 + 3_501, 17),
        ]
    );
    assert_costs(&costs, &[first, second]);

    let (resume, costs) = usages(&replay("resume", uuid(BASIC_SESSION), true, &[]));
    assert_eq!(resume, [haiku_turn(1471, 2 + 19_310 + 90, 14)]);
    assert_costs(&costs, &[third]);
}

#[test]
fn replay_an_unmeasured_interrupted_turn_reports_only_its_time() {
    let ours = uuid("11111111-1111-4111-8111-111111111111");
    let (usages, costs) = usages(&replay("interrupt", ours, false, &[4]));
    // Interrupted while text streamed, the CLI counts nothing for the turn
    // (zero tokens, `total_cost_usd: 0`): only its time is reported.
    assert_eq!(
        usages[0],
        TurnUsage {
            duration_ms: Some(2590),
            model: Some(HAIKU.to_owned()),
            ..TurnUsage::default()
        }
    );
    assert_eq!(costs[0], None);
    assert_costs(&costs[1..], &[0.00055378, 0.0002098]);
}

#[test]
fn replay_permissions_allow_then_deny() {
    let events = replay(
        "permissions",
        uuid("7d7945cf-3afb-478c-b0e7-a1e6b4dff356"),
        false,
        &[],
    );
    // Only the events `describe` shows, so indices match it.
    let events = without_settings(events);
    assert_eq!(
        describe(&events),
        [
            "resume handle: 7d7945cf-3afb-478c-b0e7-a1e6b4dff356",
            "turn started",
            "tool call: Write",
            "approval 6587ec1f-4dd9-4cd1-b704-51fcb4f5ec34: Write",
            "tool result ok: File created successfully at: /tmp/skywave-t07-s",
            "text: I created `/tmp/skywave-t07-scratch/hello.txt` c",
            "turn usage",
            "turn ended: Completed",
            "turn started",
            "tool call: Bash",
            "approval 5446b928-31cd-4fb3-b499-1f4fccf50f2a: Bash",
            "tool result error: The user denied this action.",
            "text: The `rm` was denied, so I haven't deleted `hello",
            "turn usage",
            "turn ended: Completed",
        ]
    );
    // The approval carries the tool's input, the same as its tool call.
    let write = json!({ "file_path": "/tmp/skywave-t07-scratch/hello.txt", "content": "hi" });
    assert!(matches!(&events[2], AdapterEvent::ToolCall { input, .. } if *input == write));
    assert!(matches!(&events[3], AdapterEvent::ApprovalRequested { input, .. } if *input == write));
    // Call, approval and result name the same tool-use id.
    let AdapterEvent::ToolCall {
        call: Some(call), ..
    } = &events[2]
    else {
        panic!("expected a tool call with an id, got {:?}", events[2]);
    };
    assert!(
        matches!(&events[3], AdapterEvent::ApprovalRequested { call: Some(c), .. } if c == call)
    );
    assert!(matches!(&events[4], AdapterEvent::ToolResult { call: Some(c), .. } if c == call));

    // Displays come from the inputs and the structured results.
    assert_eq!(
        displays(&events),
        [
            r#"FileWrite "/tmp/skywave-t07-scratch/hello.txt" detail=None body=Some((Text, 1))"#,
            r#"error=false status=Some("created") preview="""#,
            concat!(
                r#"Shell "$ cat /tmp/skywave-t07-scratch/hello.txt && rm /tmp/skywave-t07-scratch/hello.txt && ls -la /tmp/skywave-t07-scratch/" "#,
                r#"detail=Some("Show hello.txt contents, delete it, and list the directory") body=None"#,
            ),
            r#"error=true status=Some("denied") preview="The user denied this action.""#,
        ]
    );
    assert_no_model_notes(&events);
    // The model-facing text, notes and all, stays the output.
    let AdapterEvent::ToolResult { result, .. } = &events[4] else {
        panic!("expected a tool result, got {:?}", events[4]);
    };
    assert!(
        result
            .output
            .contains("file state is current in your context")
    );
}

/// The recorded `can_use_tool` requests of fixture `name`, in order.
fn permission_requests(name: &str) -> Vec<Value> {
    fixture(name)
        .into_iter()
        .filter(|r| r["dir"] == "out" && r["msg"]["request"]["subtype"] == "can_use_tool")
        .map(|r| r["msg"].clone())
        .collect()
}

#[test]
fn replay_permission_suggestions_are_offered_as_rules() {
    let events = replay(
        "permissions",
        uuid("7d7945cf-3afb-478c-b0e7-a1e6b4dff356"),
        false,
        &[],
    );
    let suggestions: Vec<&Vec<RuleSuggestion>> = events
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::ApprovalRequested { suggestions, .. } => Some(suggestions),
            _ => None,
        })
        .collect();
    assert_eq!(
        suggestions,
        [
            &vec![RuleSuggestion {
                label: "file edits".to_owned(),
                scope: RuleScope::Session,
            }],
            &vec![
                RuleSuggestion {
                    label: "Bash(cat /tmp/skywave-t07-scratch/hello.txt && rm /tmp/skywave-t07-scratch/hello.txt && ls -la /tmp/skywave-t07-scratch/)".to_owned(),
                    scope: RuleScope::Project,
                },
                RuleSuggestion {
                    label: "access to /tmp/skywave-t07-scratch".to_owned(),
                    scope: RuleScope::Session,
                },
                RuleSuggestion {
                    label: "file edits".to_owned(),
                    scope: RuleScope::Session,
                },
            ],
        ]
    );
}

#[test]
fn granting_a_rule_sends_its_suggestion_as_updated_permissions() {
    for (n, request) in permission_requests("permissions").into_iter().enumerate() {
        let mut protocol = Protocol::new(Uuid::new_v4(), false);
        let request_id = AgentRequestId::from(request["request_id"].as_str().unwrap());
        let out = protocol.on_line(&serde_json::to_string(&request).unwrap());
        let [AdapterEvent::ApprovalRequested { suggestions, .. }] = &out.events[..] else {
            panic!("request #{n}: {:?}", out.events);
        };
        // The last one offered: every recorded suggestion is offered.
        let last = suggestions.len() - 1;
        let out = protocol.answer(&request_id, Decision::Approve, Some(last as u8));
        let suggestion = &request["request"]["permission_suggestions"][last];
        assert_eq!(
            out.writes,
            [json!({
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": request_id.0,
                    "response": {
                        "behavior": "allow",
                        "updatedInput": request["request"]["input"],
                        "updatedPermissions": [suggestion],
                    },
                },
            })],
            "request #{n}"
        );
    }
}

#[test]
fn a_rule_the_request_did_not_offer_or_a_denial_grants_nothing() {
    let request = permission_requests("permissions").remove(0);
    let request_id = AgentRequestId::from(request["request_id"].as_str().unwrap());
    let input = &request["request"]["input"];
    let line = serde_json::to_string(&request).unwrap();

    let mut protocol = Protocol::new(Uuid::new_v4(), false);
    protocol.on_line(&line);
    let out = protocol.answer(&request_id, Decision::Approve, Some(5));
    assert_eq!(out.writes, [protocol::allow_response(&request_id.0, input)]);

    let mut protocol = Protocol::new(Uuid::new_v4(), false);
    protocol.on_line(&line);
    let out = protocol.answer(&request_id, Decision::Deny, Some(0));
    assert_eq!(
        out.writes,
        [protocol::deny_response(
            &request_id.0,
            protocol::DENY_MESSAGE
        )]
    );
}

/// The fixture `rules_granted.ndjson` is to be recorded with the runbook in
/// docs/agents/claude-code.md §3.5: one CLI process in which a tool rule
/// (`addRules`), a directory (`addDirectories`) and file edits (`setMode`
/// `acceptEdits`) are each granted with an approval, each followed by a
/// call of the same tool that the grant covers.
#[test]
#[ignore = "needs real capture, task d29758"]
fn replay_granted_rules_are_applied_by_the_cli() {
    const NAME: &str = "rules_granted";
    let records = fixture(NAME);
    let argv: Vec<&str> = records
        .iter()
        .filter_map(|r| r["text"].as_str()?.strip_prefix("argv: "))
        .collect();
    let [argv] = argv[..] else {
        panic!("{NAME}: expected one CLI process, got {argv:?}");
    };
    let session = argv
        .split(' ')
        .skip_while(|a| *a != "--session-id")
        .nth(1)
        .unwrap_or_else(|| panic!("{NAME}: no --session-id in {argv}"));

    // (a) Every stdin line is what the adapter writes, a rule grant
    // included: the update offered, unchanged, as `updatedPermissions`.
    let (events, version) = replay_any_version(NAME, uuid(session), false, &[]);
    assert!(version.is_some(), "{NAME}: no claude_code_version");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AdapterEvent::ApprovalRequested { suggestions, .. } if !suggestions.is_empty()))
    );

    let requests: HashMap<&str, &Value> = records
        .iter()
        .filter(|r| r["dir"] == "out" && r["msg"]["request"]["subtype"] == "can_use_tool")
        .map(|r| {
            (
                r["msg"]["request_id"].as_str().unwrap(),
                &r["msg"]["request"],
            )
        })
        .collect();
    let mut kinds = Vec::new();
    for (at, record) in records.iter().enumerate() {
        let response = &record["msg"]["response"];
        let Some(granted) = response["response"].get("updatedPermissions") else {
            continue;
        };
        let request = requests[response["request_id"].as_str().unwrap()];
        let update = &granted[0];
        assert!(
            request["permission_suggestions"]
                .as_array()
                .unwrap()
                .contains(update),
            "{NAME}: {update} is not one of the request's suggestions"
        );
        kinds.push(match update["type"].as_str().unwrap() {
            "setMode" => format!("setMode {}", update["mode"].as_str().unwrap()),
            other => other.to_owned(),
        });

        // (b) The next call of the same tool runs without a prompt.
        let tool = request["tool_name"].as_str().unwrap();
        let next = records[at + 1..]
            .iter()
            .filter(|r| r["dir"] == "out" && r["msg"]["type"] == "assistant")
            .flat_map(|r| r["msg"]["message"]["content"].as_array().unwrap())
            .find(|c| c["type"] == "tool_use" && c["name"] == tool)
            .unwrap_or_else(|| panic!("{NAME}: no {tool} call after granting {update}"));
        assert!(
            !requests.values().any(|r| r["tool_use_id"] == next["id"]),
            "{NAME}: {tool} call {} asked again after granting {update}",
            next["id"]
        );
    }
    kinds.sort();
    assert_eq!(kinds, ["addDirectories", "addRules", "setMode acceptEdits"]);
}

/// One recorded turn of a `permission_modes*` fixture, from its `step:` note
/// to the next: the mode its `system/init` reported, the tools the CLI asked
/// for, every tool the model called, the modes of the `system/status` lines
/// sent between its `system/init` and its `result`, and whether each tool
/// result was an error. A status that answers a `set_permission_mode`
/// between turns can land on either side of the next `system/init`, so
/// only those inside the turn are counted.
#[derive(Debug, PartialEq)]
struct ModeStep {
    name: String,
    mode: String,
    asked: Vec<String>,
    called: Vec<String>,
    statuses: Vec<String>,
    errors: Vec<bool>,
}

fn mode_steps(records: &[Value]) -> Vec<ModeStep> {
    let mut steps: Vec<ModeStep> = Vec::new();
    let mut in_turn = false;
    for record in records {
        if let Some(name) = record["text"]
            .as_str()
            .and_then(|t| t.strip_prefix("step: "))
        {
            steps.push(ModeStep {
                name: name.to_owned(),
                mode: String::new(),
                asked: vec![],
                called: vec![],
                statuses: vec![],
                errors: vec![],
            });
            continue;
        }
        let (Some(step), "out") = (steps.last_mut(), record["dir"].as_str().unwrap_or("")) else {
            continue;
        };
        let msg = &record["msg"];
        match (msg["type"].as_str().unwrap(), msg["subtype"].as_str()) {
            ("system", Some("init")) => {
                step.mode = msg["permissionMode"].as_str().unwrap().into();
                in_turn = true;
            }
            ("result", _) => in_turn = false,
            ("system", Some("status")) if in_turn => step
                .statuses
                .push(msg["permissionMode"].as_str().unwrap().into()),
            ("control_request", _) if msg["request"]["subtype"] == "can_use_tool" => step
                .asked
                .push(msg["request"]["tool_name"].as_str().unwrap().into()),
            ("assistant", _) => step.called.extend(
                msg["message"]["content"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|c| c["type"] == "tool_use")
                    .map(|c| c["name"].as_str().unwrap().to_owned()),
            ),
            ("user", _) => step.errors.extend(
                msg["message"]["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|c| c["type"] == "tool_result")
                    .map(|c| c["is_error"] == true),
            ),
            _ => {}
        }
    }
    steps
}

/// Every `set_permission_mode` in fixture `name`, in order: the mode asked
/// for, the CLI's `control_response.response`, and the mode of the first
/// `system/status` line after that response, if any.
fn mode_changes(records: &[Value]) -> Vec<(String, Value, Option<String>)> {
    let mut changes: Vec<(String, Value, Option<String>)> = Vec::new();
    let mut pending: HashMap<&str, String> = HashMap::new();
    for record in records {
        let msg = &record["msg"];
        match record["dir"].as_str().unwrap_or("") {
            "in" if msg["request"]["subtype"] == "set_permission_mode" => {
                pending.insert(
                    msg["request_id"].as_str().unwrap(),
                    msg["request"]["mode"].as_str().unwrap().into(),
                );
            }
            "out" if msg["type"] == "control_response" => {
                if let Some(mode) = pending.remove(msg["response"]["request_id"].as_str().unwrap())
                {
                    changes.push((mode, msg["response"].clone(), None));
                }
            }
            // Later status lines come from elsewhere (an approved
            // ExitPlanMode); `mode_steps` reports those.
            "out" if msg["type"] == "system" && msg["subtype"] == "status" => {
                if let Some(last @ (_, _, None)) = changes.last_mut() {
                    last.2 = Some(msg["permissionMode"].as_str().unwrap().into());
                }
            }
            _ => {}
        }
    }
    assert!(pending.is_empty(), "unanswered mode changes {pending:?}");
    changes
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

/// The CLI side of a mode change (docs/agents/claude-code.md §3.7): a
/// `set_permission_mode` is answered with `{"mode"}` and a `system/status`
/// line, takes effect from the next tool call in the same process, and
/// leaves an approval that is already pending waiting for its answer.
#[test]
fn recorded_permission_modes_change_what_the_cli_asks() {
    let records = fixture("permission_modes");
    for (mode, response, status) in mode_changes(&records) {
        assert_eq!(response["subtype"], "success", "{mode}");
        assert_eq!(response["response"], json!({ "mode": mode }));
        assert_eq!(status.as_deref(), Some(mode.as_str()));
    }

    let step =
        |name: &str, mode: &str, asked: &[&str], called: &[&str], statuses: &[&str]| ModeStep {
            name: name.into(),
            mode: mode.into(),
            asked: strings(asked),
            called: strings(called),
            statuses: strings(statuses),
            errors: vec![false; called.len()],
        };
    let mut expected = vec![
        step("default: Write", "default", &["Write"], &["Write"], &[]),
        step("acceptEdits: Write", "acceptEdits", &[], &["Write"], &[]),
        step("acceptEdits: Bash rm", "acceptEdits", &[], &["Bash"], &[]),
        // Plan writes its plan file unasked, then asks to leave plan mode;
        // approving it returns to the mode before plan.
        step(
            "plan: Write",
            "plan",
            &["ExitPlanMode"],
            &["Write", "ToolSearch", "ExitPlanMode", "Write"],
            &["acceptEdits"],
        ),
        step("auto: Write", "auto", &[], &["Write"], &[]),
        step("auto: Bash rm", "auto", &[], &["Bash"], &[]),
        step("dontAsk: Write", "dontAsk", &[], &["Write"], &[]),
        step(
            "default again: Write",
            "default",
            &["Write"],
            &["Write"],
            &[],
        ),
        // The switch to acceptEdits does not settle the pending Write: the
        // CLI sends no cancel and waits for the answer.
        step(
            "pending + switch to acceptEdits",
            "default",
            &["Write"],
            &["Write"],
            &["acceptEdits"],
        ),
    ];
    // dontAsk refuses instead of asking.
    expected[6].errors = vec![true];
    assert_eq!(mode_steps(&records), expected);
    assert!(
        !records
            .iter()
            .any(|r| r["msg"]["type"] == "control_cancel_request")
    );
}

/// Approving `ExitPlanMode` returns to the mode the session was in before
/// plan: `default` here, `acceptEdits` in `permission_modes`.
#[test]
fn recorded_plan_approval_returns_to_the_previous_mode() {
    let records = fixture("permission_modes_plan");
    // The only set_permission_mode; the switch back comes from the approval.
    let [(mode, response, status)] = &mode_changes(&records)[..] else {
        panic!("expected one mode change");
    };
    assert_eq!(
        (mode.as_str(), &response["response"], status.as_deref()),
        ("plan", &json!({"mode": "plan"}), Some("plan"))
    );

    let exit_plan = records
        .iter()
        .find(|r| r["dir"] == "out" && r["msg"]["request"]["tool_name"] == "ExitPlanMode")
        .map(|r| &r["msg"]["request"])
        .unwrap();
    assert_eq!(exit_plan["permission_suggestions"], Value::Null);
    assert!(exit_plan["input"]["plan"].as_str().is_some());

    let steps = mode_steps(&records);
    assert_eq!(
        steps
            .iter()
            .map(|s| (s.name.as_str(), s.mode.as_str(), &s.asked, &s.statuses))
            .collect::<Vec<_>>(),
        [
            (
                "plan (from default): Write",
                "plan",
                &strings(&["ExitPlanMode", "Write"]),
                &strings(&["default"])
            ),
            (
                "after plan: Write",
                "default",
                &strings(&["Write"]),
                &vec![]
            ),
        ]
    );
}

/// A mode the CLI rejects is answered with an error `control_response`
/// carrying an `error_code`, and the mode stays as it was.
#[test]
fn recorded_rejected_permission_modes_keep_the_mode() {
    let records = fixture("permission_mode_errors");
    let changes: Vec<(String, Value, Option<String>)> = mode_changes(&records);
    let summary: Vec<(&str, &str, Option<&str>, Option<&str>)> = changes
        .iter()
        .map(|(mode, response, status)| {
            (
                mode.as_str(),
                response["subtype"].as_str().unwrap(),
                response["error_code"].as_str(),
                status.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            ("plan", "success", None, Some("plan")),
            (
                "bypassPermissions",
                "error",
                Some("bypass_not_launched"),
                None
            ),
            ("bogus", "error", Some("invalid_mode"), None),
        ]
    );
    // `initialize` reads the mode back: still plan after both rejections.
    let current: Vec<&str> = records
        .iter()
        .filter_map(|r| r["msg"]["response"]["response"]["current_permission_mode"].as_str())
        .collect();
    assert_eq!(current, ["default", "plan", "plan", "plan"]);
}

#[test]
fn replay_mode_switches_answer_requests_and_report_the_mode() {
    let events = replay("permission_modes", Uuid::new_v4(), false, &[]);
    let haiku = "model: claude-haiku-5-5";
    assert_eq!(
        settings(&events),
        strings(&[
            "mode: Ask",
            "models offered: 13",
            // default: Write
            haiku,
            "mode: Ask",
            "mode set: AcceptEdits",
            // acceptEdits: Write; its status echo is dropped.
            haiku,
            "mode: AcceptEdits",
            // acceptEdits: Bash rm
            haiku,
            "mode: AcceptEdits",
            "mode set: Plan",
            // plan: Write; the approved plan returns to acceptEdits.
            haiku,
            "mode: Plan",
            "mode: AcceptEdits",
            "mode set: Auto",
            haiku,
            "mode: Auto",
            haiku,
            "mode: Auto",
            "mode set: DontAsk",
            haiku,
            "mode: DontAsk",
            "mode set: Ask",
            haiku,
            "mode: Ask",
            // A switch while an approval waits.
            haiku,
            "mode: Ask",
            "mode set: AcceptEdits",
        ])
    );
}

#[test]
fn replay_an_approved_plan_reports_the_mode_before_it() {
    let events = replay("permission_modes_plan", Uuid::new_v4(), false, &[]);
    let haiku = "model: claude-haiku-5-5";
    assert_eq!(
        settings(&events),
        strings(&[
            "mode: Ask",
            "models offered: 13",
            "mode set: Plan",
            haiku,
            "mode: Plan",
            "mode: Ask",
            haiku,
            "mode: Ask",
        ])
    );
}

#[test]
fn replay_refused_modes_and_models_keep_what_the_agent_had() {
    let events = replay(
        "permission_mode_errors",
        Uuid::new_v4(),
        false,
        &[2, 4, 5, 6],
    );
    assert_eq!(
        settings(&events),
        strings(&[
            "mode: Ask",
            "models offered: 13",
            "mode set: Plan",
            "mode refused Bypass: Cannot set permission mode to bypassPermissions ",
        ])
    );

    let events = replay("model_switch", Uuid::new_v4(), false, &[2, 6, 7]);
    assert_eq!(
        settings(&events),
        strings(&[
            "mode: Ask",
            "models offered: 13",
            // Set answers with the model the CLI resolved.
            "model set: claude-sonnet-5-5",
            "model set: claude-opus-4-7",
            "model refused bogus-model: Model 'bogus-model' not found",
            "model set: claude-opus-5-5",
        ])
    );
}

#[test]
fn the_initialize_answer_offers_its_models() {
    let events = replay("model_switch", Uuid::new_v4(), false, &[2, 6, 7]);
    let Some(AdapterEvent::ModelsOffered(models)) = events
        .iter()
        .find(|e| matches!(e, AdapterEvent::ModelsOffered(_)))
    else {
        panic!("no models offered");
    };
    assert_eq!(
        models[0],
        ModelChoice {
            id: "default".into(),
            label: "Default (recommended)".into(),
            detail: Some("Opus 5.5 · Best for everyday, complex tasks".into()),
            resolved: Some("claude-opus-5-5".into()),
            efforts: Vec::new(),
        }
    );
    assert!(models.iter().any(|m| m.id == "haiku"));
}

#[test]
fn replay_interrupt_while_streaming_and_while_idle() {
    // This run had no --session-id: the CLI's own id is adopted.
    let ours = uuid("11111111-1111-4111-8111-111111111111");
    // stdin #4 is an interrupt sent while idle: the adapter writes nothing.
    let events = replay("interrupt", ours, false, &[4]);
    let cli = fixture("interrupt")
        .into_iter()
        .find(|r| r["msg"]["subtype"] == "init")
        .unwrap()["msg"]["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        describe(&events),
        [
            format!("resume handle: {ours}"),
            "turn started".to_owned(),
            format!("resume handle: {cli}"),
            "text: 1".to_owned(),
            "turn usage".to_owned(),
            "turn ended: Interrupted".to_owned(),
            "turn started".to_owned(),
            "tool call: Bash".to_owned(),
            "tool result ok: pending-test".to_owned(),
            "text: pending-test".to_owned(),
            "turn usage".to_owned(),
            "turn ended: Completed".to_owned(),
            "turn started".to_owned(),
            "text: ok".to_owned(),
            "turn usage".to_owned(),
            "turn ended: Completed".to_owned(),
        ]
    );
    assert_eq!(
        displays(&events),
        [
            r#"Shell "$ echo pending-test" detail=Some("Print pending-test string") body=None"#,
            r#"error=false status=None preview="pending-test""#,
        ]
    );
}

#[test]
fn replay_interrupt_with_a_pending_approval() {
    let cli = "5be4083d-8321-408c-a950-44c6e8a7f067";
    // stdin #3 is the late allow for the cancelled request: dropped.
    let events = replay("interrupt_pending", uuid(cli), false, &[3]);
    assert_eq!(
        describe(&events),
        [
            format!("resume handle: {cli}"),
            "turn started".to_owned(),
            "tool call: Write".to_owned(),
            "approval aa219e26-965b-44bc-ad3e-0f57fede7ee4: Write".to_owned(),
            format!("cancelled aa219e26-965b-44bc-ad3e-0f57fede7ee4: {INTERRUPT_REASON}"),
            "tool result error: The user doesn't want to proceed with this tool ".to_owned(),
            "turn usage".to_owned(),
            "turn ended: Interrupted".to_owned(),
            // Then stdin closes while a second approval waits: the CLI fails
            // the approval itself and carries on.
            "turn started".to_owned(),
            "tool call: Write".to_owned(),
            "approval 071d7172-762d-450e-8cca-676210e9b4c4: Write".to_owned(),
            "cancelled 071d7172-762d-450e-8cca-676210e9b4c4: Tool permission request failed: AbortError: Tool"
                .to_owned(),
            "tool result error: Tool permission request failed: AbortError: Tool".to_owned(),
            "tool call: Write".to_owned(),
            "tool result error: Tool permission request failed: AbortError: Stre".to_owned(),
            "text: I couldn't create `pending2.txt`. The Write call".to_owned(),
            "turn usage".to_owned(),
            "turn ended: Completed".to_owned(),
        ]
    );
    let write = |name: &str| {
        format!(r#"FileWrite "/tmp/skywave-t07-scratch/{name}" detail=None body=Some((Text, 1))"#)
    };
    assert_eq!(
        displays(&events),
        [
            write("pending.txt"),
            r#"error=true status=Some("cancelled") preview="User rejected tool use""#.to_owned(),
            write("pending2.txt"),
            concat!(
                r#"error=true status=Some("cancelled") preview="Tool permission request failed: "#,
                r#"AbortError: Tool permission stream closed before response received""#,
            )
            .to_owned(),
            write("pending2.txt"),
            concat!(
                r#"error=true status=Some("cancelled") preview="Tool permission request failed: "#,
                r#"AbortError: Stream closed""#,
            )
            .to_owned(),
        ]
    );
    assert_no_model_notes(&events);
}

#[test]
fn a_message_during_a_turn_waits_for_its_result() {
    let mut protocol = Protocol::new(Uuid::new_v4(), true);
    protocol.start();
    protocol.on_line(r#"{"type":"control_response","response":{"subtype":"success","request_id":"skywave_1","response":{}}}"#);
    assert!(protocol.ready());
    let first = protocol.send_message("one".to_owned());
    assert_eq!(first.events, [AdapterEvent::TurnStarted]);
    let queued = protocol.send_message("two".to_owned());
    assert_eq!(queued, Default::default());
    let end =
        protocol.on_line(r#"{"type":"result","subtype":"success","is_error":false,"result":"ok"}"#);
    assert_eq!(
        end.events,
        [
            AdapterEvent::TurnEnded(TurnOutcome::Completed),
            AdapterEvent::TurnStarted
        ]
    );
    assert_eq!(end.writes, [protocol::user_message("two")]);
}

#[test]
fn a_failed_turn_and_unknown_messages() {
    let mut protocol = Protocol::new(Uuid::new_v4(), true);
    protocol.start();
    protocol.send_message("go".to_owned());
    for line in [
        r#"{"type":"something_new","x":1}"#,
        r#"{"type":"system","subtype":"status","status":"requesting"}"#,
        "",
    ] {
        assert_eq!(protocol.on_line(line), Default::default());
    }
    // A control request the adapter does not handle is refused, not ignored.
    let hook = protocol.on_line(
        r#"{"type":"control_request","request_id":"h1","request":{"subtype":"hook_callback"}}"#,
    );
    assert_eq!(hook.writes[0]["response"]["subtype"], "error");
    assert_eq!(hook.writes[0]["response"]["request_id"], "h1");
    let end = protocol.on_line(
        r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":null,"errors":["API down"]}"#,
    );
    assert_eq!(
        end.events,
        [AdapterEvent::TurnEnded(TurnOutcome::Failed(
            "API down".to_owned()
        ))]
    );
}

#[test]
fn resume_handles_round_trip() {
    let id = Uuid::new_v4();
    let handle = protocol::resume_handle(id, &[]);
    assert_eq!(handle.0, json!({ "session_id": id.to_string() }));
    assert_eq!(protocol::parse_resume_handle(&handle), Ok((id, Vec::new())));
    let rules = [json!({"type":"setMode","mode":"acceptEdits","destination":"session"})];
    let handle = protocol::resume_handle(id, &rules);
    assert_eq!(
        handle.0,
        json!({ "session_id": id.to_string(), "rules": rules })
    );
    assert_eq!(
        protocol::parse_resume_handle(&handle),
        Ok((id, rules.to_vec()))
    );
    assert!(protocol::parse_resume_handle(&ResumeHandle(json!({ "other": 1 }))).is_err());
}

#[test]
fn a_granted_session_rule_is_added_to_the_resume_handle() {
    let requests = permission_requests("permissions");
    // Request #1 offers a project rule (0), then session ones (1, 2).
    let request = &requests[1];
    let request_id = AgentRequestId::from(request["request_id"].as_str().unwrap());
    let line = serde_json::to_string(request).unwrap();
    let offered = &request["request"]["permission_suggestions"];
    let id = Uuid::new_v4();
    let earlier = json!({"type":"addDirectories","directories":["/srv"],"destination":"session"});

    // A project rule is kept by the CLI's settings: no new handle.
    let mut protocol = Protocol::new(id, true).with_session_rules(vec![earlier.clone()]);
    protocol.on_line(&line);
    let out = protocol.answer(&request_id, Decision::Approve, Some(0));
    assert!(out.events.is_empty(), "{:?}", out.events);

    // A session rule joins the ones the process was started with.
    let mut protocol = Protocol::new(id, true).with_session_rules(vec![earlier.clone()]);
    protocol.on_line(&line);
    let out = protocol.answer(&request_id, Decision::Approve, Some(2));
    assert_eq!(
        out.events,
        [AdapterEvent::ResumeHandle(protocol::resume_handle(
            id,
            &[earlier, offered[2].clone()]
        ))]
    );

    // A denial grants nothing.
    let mut protocol = Protocol::new(id, true);
    protocol.on_line(&line);
    let out = protocol.answer(&request_id, Decision::Deny, Some(2));
    assert!(out.events.is_empty());
}

// ---------------------------------------------------------------------------
// Process

const WAIT: Duration = Duration::from_secs(10);

fn fake_cli() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/fake-claude.sh")
}

fn adapter() -> ClaudeCodeAdapter {
    ClaudeCodeAdapter::new(AgentConfig {
        command: fake_cli(),
        args: vec!["--model".to_owned(), "haiku".to_owned()],
    })
    .with_grace_period(Duration::from_secs(5))
}

fn spec(workdir: &Path, resume: Option<ResumeHandle>) -> StartSpec {
    StartSpec {
        session_id: SessionId::new(),
        workdir: workdir.to_owned(),
        resume,
        mode: None,
        model: None,
    }
}

/// The next event, skipping what the agent reports unasked about its mode
/// and model (`settings_test` reads those with [`next_raw`]).
async fn next(events: &mut AdapterEvents) -> AdapterEvent {
    loop {
        let event = next_raw(events).await;
        if !observes_settings(&event) {
            return event;
        }
    }
}

async fn next_raw(events: &mut AdapterEvents) -> AdapterEvent {
    tokio::time::timeout(WAIT, events.recv())
        .await
        .expect("no adapter event in time")
        .expect("adapter event stream ended")
}

async fn take(events: &mut AdapterEvents, n: usize) -> Vec<AdapterEvent> {
    let mut out = Vec::new();
    for _ in 0..n {
        out.push(next(events).await);
    }
    out
}

fn argv(workdir: &Path) -> Vec<String> {
    std::fs::read_to_string(workdir.join("argv.txt"))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The fake CLI's process id, from ./pid.txt in its workdir.
fn fake_pid(workdir: &Path) -> u32 {
    std::fs::read_to_string(workdir.join("pid.txt"))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

/// Whether process `pid` exists, a zombie included (`kill -0`).
fn process_exists(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

fn text(text: &str) -> AdapterEvent {
    AdapterEvent::Text(text.to_owned())
}

/// The fake CLI's one tool call id.
const CALL: &str = "toolu_1";

/// A result as the fake CLI sends it: no structured `tool_use_result`, so
/// only a failure's text is read, for `status`.
fn tool_result(output: &str, is_error: bool, status: Option<&str>) -> AdapterEvent {
    AdapterEvent::ToolResult {
        call: Some(CALL.into()),
        result: ResultDisplay {
            is_error,
            status: status.map(str::to_owned),
            preview: None,
            output: output.to_owned(),
        },
    }
}

fn write_call() -> AdapterEvent {
    AdapterEvent::ToolCall {
        call: Some(CALL.into()),
        name: "Write".to_owned(),
        input: json!({ "file_path": "a.txt", "content": "hi" }),
        display: Some(ToolDisplay {
            kind: ToolKind::FileWrite,
            title: "a.txt".to_owned(),
            detail: None,
            body: Some(DisplayBody {
                format: BodyFormat::Text,
                text: "hi".to_owned(),
            }),
            shows_all_input: true,
        }),
    }
}

fn write_approval() -> AdapterEvent {
    AdapterEvent::ApprovalRequested {
        request: "perm-1".into(),
        call: Some(CALL.into()),
        tool: "Write".to_owned(),
        input: json!({ "file_path": "a.txt", "content": "hi" }),
        suggestions: Vec::new(),
    }
}

const COMPLETED: AdapterEvent = AdapterEvent::TurnEnded(TurnOutcome::Completed);
const INTERRUPTED: AdapterEvent = AdapterEvent::TurnEnded(TurnOutcome::Interrupted);

#[tokio::test]
async fn a_session_runs_turns_approvals_and_interrupts() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut session, mut events) = adapter().start(spec(tmp.path(), None)).await.unwrap();

    // The handle is known before any turn, and names the --session-id.
    let AdapterEvent::ResumeHandle(handle) = next(&mut events).await else {
        panic!("expected the resume handle first");
    };
    let (id, _) = protocol::parse_resume_handle(&handle).unwrap();
    let mut expected: Vec<String> = REQUIRED_ARGS.iter().map(|a| (*a).to_owned()).collect();
    expected.extend(["--session-id".to_owned(), id.to_string()]);
    expected.extend(["--model".to_owned(), "haiku".to_owned()]);
    assert_eq!(argv(tmp.path()), expected);

    session.send_message("hello".to_owned()).await.unwrap();
    assert_eq!(
        take(&mut events, 3).await,
        [AdapterEvent::TurnStarted, text("echo"), COMPLETED]
    );

    // Allow, with a message queued behind the approval and a stale answer.
    session
        .send_message("please approve".to_owned())
        .await
        .unwrap();
    assert_eq!(
        take(&mut events, 3).await,
        [AdapterEvent::TurnStarted, write_call(), write_approval()]
    );
    session.send_message("queued".to_owned()).await.unwrap();
    session
        .answer_approval("not-pending".into(), Decision::Approve, None)
        .await
        .unwrap();
    session
        .answer_approval("perm-1".into(), Decision::Approve, None)
        .await
        .unwrap();
    assert_eq!(
        take(&mut events, 6).await,
        [
            tool_result("written", false, None),
            text("done writing"),
            COMPLETED,
            AdapterEvent::TurnStarted,
            text("echo"),
            COMPLETED,
        ]
    );

    // Deny.
    session
        .send_message("approve again".to_owned())
        .await
        .unwrap();
    assert_eq!(
        take(&mut events, 3).await,
        [AdapterEvent::TurnStarted, write_call(), write_approval()]
    );
    session
        .answer_approval("perm-1".into(), Decision::Deny, None)
        .await
        .unwrap();
    assert_eq!(
        take(&mut events, 3).await,
        [
            tool_result("The user denied this action.", true, Some("denied")),
            text("not writing"),
            COMPLETED,
        ]
    );

    // Interrupt while the approval waits: cancelled, then interrupted.
    session
        .send_message("approve, then stop".to_owned())
        .await
        .unwrap();
    assert_eq!(
        take(&mut events, 3).await,
        [AdapterEvent::TurnStarted, write_call(), write_approval()]
    );
    session.interrupt().await.unwrap();
    assert_eq!(
        take(&mut events, 3).await,
        [
            AdapterEvent::ApprovalCancelled {
                request: "perm-1".into(),
                reason: INTERRUPT_REASON.to_owned(),
            },
            tool_result("rejected", true, None),
            INTERRUPTED,
        ]
    );

    // Interrupt while text streams; the session stays usable.
    session.send_message("hang".to_owned()).await.unwrap();
    assert_eq!(
        take(&mut events, 2).await,
        [AdapterEvent::TurnStarted, text("working")]
    );
    session.interrupt().await.unwrap();
    session.interrupt().await.unwrap();
    assert_eq!(next(&mut events).await, INTERRUPTED);

    // Idle: an interrupt does nothing, a message runs a normal turn.
    session.interrupt().await.unwrap();
    session
        .send_message("still there?".to_owned())
        .await
        .unwrap();
    assert_eq!(
        take(&mut events, 3).await,
        [AdapterEvent::TurnStarted, text("echo"), COMPLETED]
    );

    session.shutdown().await.unwrap();
    assert_eq!(
        next(&mut events).await,
        AdapterEvent::Exited { error: None }
    );
    assert!(events.recv().await.is_none());
}

#[tokio::test]
async fn a_restart_resumes_with_the_handle_and_its_session_rules() {
    let tmp = tempfile::tempdir().unwrap();
    let id = Uuid::new_v4();
    let rules = [
        json!({"type":"setMode","mode":"acceptEdits","destination":"session"}),
        json!({"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"cargo test"}],
               "behavior":"allow","destination":"session"}),
    ];
    let handle = protocol::resume_handle(id, &rules);
    let (mut session, mut events) = adapter()
        .start(spec(tmp.path(), Some(handle)))
        .await
        .unwrap();
    let mut expected: Vec<String> = REQUIRED_ARGS.iter().map(|a| (*a).to_owned()).collect();
    expected.extend(
        [
            "--permission-mode",
            "acceptEdits",
            "--allowedTools",
            "Bash(cargo test)",
            "--resume",
            &id.to_string(),
            "--model",
            "haiku",
        ]
        .map(str::to_owned),
    );
    assert_eq!(argv(tmp.path()), expected);

    // No new handle: the first event is the turn.
    session
        .send_message("hello again".to_owned())
        .await
        .unwrap();
    assert_eq!(
        take(&mut events, 3).await,
        [AdapterEvent::TurnStarted, text("echo"), COMPLETED]
    );
    session.shutdown().await.unwrap();
    assert_eq!(
        next(&mut events).await,
        AdapterEvent::Exited { error: None }
    );
}

#[tokio::test]
async fn resuming_an_unknown_session_is_a_resume_error() {
    let tmp = tempfile::tempdir().unwrap();
    let handle = protocol::resume_handle(Uuid::nil(), &[]);
    let error = adapter()
        .start(spec(tmp.path(), Some(handle)))
        .await
        .unwrap_err();
    match error {
        AgentError::Resume(message) => {
            assert!(message.contains("No conversation found"), "{message}")
        }
        other => panic!("expected a resume error, got {other:?}"),
    }
}

#[tokio::test]
async fn a_foreign_resume_handle_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let handle = ResumeHandle(json!({ "fake_session": "x" }));
    let error = adapter()
        .start(spec(tmp.path(), Some(handle)))
        .await
        .unwrap_err();
    assert!(matches!(error, AgentError::Resume(_)), "{error:?}");
}

#[tokio::test]
async fn a_missing_command_is_a_start_error() {
    let tmp = tempfile::tempdir().unwrap();
    let adapter = ClaudeCodeAdapter::new(AgentConfig {
        command: tmp.path().join("no-such-agent"),
        args: Vec::new(),
    });
    let error = adapter.start(spec(tmp.path(), None)).await.unwrap_err();
    assert!(matches!(error, AgentError::Start(_)), "{error:?}");
}

#[tokio::test]
async fn shutdown_after_an_interrupt_waits_for_the_result() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut session, mut events) = adapter().start(spec(tmp.path(), None)).await.unwrap();
    assert!(matches!(
        next(&mut events).await,
        AdapterEvent::ResumeHandle(_)
    ));
    session.send_message("hang".to_owned()).await.unwrap();
    assert_eq!(
        take(&mut events, 2).await,
        [AdapterEvent::TurnStarted, text("working")]
    );
    // A message queued behind the running turn is dropped by the shutdown.
    session.send_message("never sent".to_owned()).await.unwrap();
    session.interrupt().await.unwrap();
    session.shutdown().await.unwrap();
    assert_eq!(
        take(&mut events, 2).await,
        [INTERRUPTED, AdapterEvent::Exited { error: None }]
    );
    assert!(events.recv().await.is_none());
}

#[tokio::test]
async fn shutdown_mid_turn_abandons_the_turn_and_its_approval() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut session, mut events) = adapter().start(spec(tmp.path(), None)).await.unwrap();
    assert!(matches!(
        next(&mut events).await,
        AdapterEvent::ResumeHandle(_)
    ));
    session.send_message("approve".to_owned()).await.unwrap();
    assert_eq!(
        take(&mut events, 3).await,
        [AdapterEvent::TurnStarted, write_call(), write_approval()]
    );
    let pid = fake_pid(tmp.path());
    session.shutdown().await.unwrap();
    // Neither cancelled nor ended: the process was killed, not interrupted.
    assert_eq!(
        next(&mut events).await,
        AdapterEvent::Exited { error: None }
    );
    assert!(events.recv().await.is_none());
    assert!(!process_exists(pid), "agent process {pid} still exists");
}

#[tokio::test]
async fn a_crash_is_reported_with_stderr_and_closes_the_session() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut session, mut events) = adapter().start(spec(tmp.path(), None)).await.unwrap();
    assert!(matches!(
        next(&mut events).await,
        AdapterEvent::ResumeHandle(_)
    ));
    session.send_message("crash".to_owned()).await.unwrap();
    assert_eq!(
        take(&mut events, 2).await,
        [
            AdapterEvent::TurnStarted,
            AdapterEvent::Exited {
                error: Some("boom: the agent fell over".to_owned())
            },
        ]
    );
    assert!(events.recv().await.is_none());
    assert!(matches!(
        session.send_message("anyone?".to_owned()).await,
        Err(AgentError::Closed)
    ));
}

#[tokio::test]
async fn dropping_the_session_stops_the_agent() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut session, mut events) = adapter().start(spec(tmp.path(), None)).await.unwrap();
    assert!(matches!(
        next(&mut events).await,
        AdapterEvent::ResumeHandle(_)
    ));
    session.send_message("hang".to_owned()).await.unwrap();
    assert_eq!(
        take(&mut events, 2).await,
        [AdapterEvent::TurnStarted, text("working")]
    );
    let pid = fake_pid(tmp.path());
    drop(session);
    assert_eq!(
        next(&mut events).await,
        AdapterEvent::Exited { error: None }
    );
    assert!(events.recv().await.is_none());
    assert!(!process_exists(pid), "agent process {pid} still exists");
}

#[tokio::test]
async fn a_session_starts_with_and_switches_its_mode_and_model() {
    let tmp = tempfile::tempdir().unwrap();
    let mut spec = spec(tmp.path(), None);
    spec.mode = Some(PermissionMode::Plan);
    spec.model = Some("sonnet".to_owned());
    let (mut session, mut events) = adapter().start(spec).await.unwrap();
    assert!(matches!(
        next_raw(&mut events).await,
        AdapterEvent::ResumeHandle(_)
    ));
    // Asked for after the handshake, answered like any request; the
    // status echo of the switch is not reported.
    assert_eq!(
        take_raw(&mut events, 2).await,
        [
            AdapterEvent::PermissionModeSet(PermissionMode::Plan),
            AdapterEvent::ModelSet("resolved-sonnet".to_owned()),
        ]
    );

    session.send_message("hello".to_owned()).await.unwrap();
    let usage = AdapterEvent::TurnUsage(TurnUsage {
        model: Some("resolved-sonnet".to_owned()),
        ..TurnUsage::default()
    });
    assert_eq!(
        take_raw(&mut events, 6).await,
        [
            AdapterEvent::TurnStarted,
            AdapterEvent::Model("resolved-sonnet".to_owned()),
            AdapterEvent::PermissionMode(PermissionMode::Plan),
            text("echo"),
            usage,
            COMPLETED,
        ]
    );

    session
        .set_permission_mode(PermissionMode::Bypass)
        .await
        .unwrap();
    session.set_model("bogus".to_owned()).await.unwrap();
    session
        .set_permission_mode(PermissionMode::AcceptEdits)
        .await
        .unwrap();
    let events_now = take_raw(&mut events, 3).await;
    assert!(matches!(
        &events_now[0],
        AdapterEvent::PermissionModeRefused { mode: PermissionMode::Bypass, reason }
            if reason.contains("--dangerously-skip-permissions")
    ));
    assert_eq!(
        events_now[1..],
        [
            AdapterEvent::ModelRefused {
                model: "bogus".to_owned(),
                reason: "Model 'bogus' not found".to_owned(),
            },
            AdapterEvent::PermissionModeSet(PermissionMode::AcceptEdits),
        ]
    );
    session.shutdown().await.unwrap();
    assert_eq!(
        next_raw(&mut events).await,
        AdapterEvent::Exited { error: None }
    );
}

async fn take_raw(events: &mut AdapterEvents, n: usize) -> Vec<AdapterEvent> {
    let mut out = Vec::new();
    for _ in 0..n {
        out.push(next_raw(events).await);
    }
    out
}
