//! Does a stdout line still decode the way the adapter reads it?
//!
//! The adapter (`skywaved::agent::claude_code::protocol`) reads JSON values
//! leniently: a field it reads that went missing or changed type is skipped
//! in silence, and so is a message kind it does not know. This module names
//! both. A field the adapter reads that no longer decodes is a broken
//! contract; a kind it has never seen is a note, since the CLI may have
//! added something worth handling.

use serde_json::Value;
use skywaved::agent::claude_code::protocol::{parse_effort, parse_mode};

/// What [`check`] found in one line.
#[derive(Debug, PartialEq, Eq)]
pub enum Shape {
    /// Every field the adapter reads decodes.
    Fits,
    /// A kind or variant the adapter has never seen, by name.
    New(String),
    /// A field the adapter reads is missing or of another type.
    Broken(String),
}

/// Kinds the adapter knows and ignores: progress signals only.
const IGNORED: &[&str] = &[
    "rate_limit_event",
    "stream_event",
    "command_lifecycle",
    "system/thinking_tokens",
    "system/task_progress",
    "system/task_updated",
    "system/task_notification",
    "system/permission_denied",
];

/// Checks one stdout message.
pub fn check(msg: &Value) -> Shape {
    let kind = kind(msg);
    match kind.as_str() {
        "system/init" => all(&[
            string(msg, "/claude_code_version"),
            uuid(msg, "/session_id"),
            string(msg, "/model"),
            mode(msg, "/permissionMode", true),
        ]),
        "system/status" => mode(msg, "/permissionMode", false),
        "system/background_tasks_changed" => background_tasks(msg),
        "system/task_started" => {
            let agent = msg.pointer("/task_type").and_then(Value::as_str) == Some("local_agent");
            all(&[
                string(msg, "/task_id"),
                string(msg, "/task_type"),
                if agent {
                    string(msg, "/tool_use_id")
                } else {
                    Shape::Fits
                },
            ])
        }
        "assistant" => assistant(msg),
        "user" => user(msg),
        "control_request" => control_request(msg),
        "control_cancel_request" => string(msg, "/request_id"),
        "control_response" => control_response(msg),
        "result" => all(&[string(msg, "/subtype"), boolean(msg, "/is_error")]),
        _ if IGNORED.contains(&kind.as_str()) => Shape::Fits,
        _ => Shape::New(kind),
    }
}

/// `type`, or `type/subtype` for a `system` line.
fn kind(msg: &Value) -> String {
    let field = |key| msg.get(key).and_then(Value::as_str).unwrap_or("?");
    match field("type") {
        "system" => format!("system/{}", field("subtype")),
        other => other.to_owned(),
    }
}

/// The first finding that is not [`Shape::Fits`], a broken one first.
fn all(shapes: &[Shape]) -> Shape {
    shapes
        .iter()
        .find(|s| matches!(s, Shape::Broken(_)))
        .or_else(|| shapes.iter().find(|s| matches!(s, Shape::New(_))))
        .map_or(Shape::Fits, |s| match s {
            Shape::Broken(why) => Shape::Broken(why.clone()),
            Shape::New(what) => Shape::New(what.clone()),
            Shape::Fits => Shape::Fits,
        })
}

fn broken(msg: &Value, pointer: &str, want: &str) -> Shape {
    let got = msg
        .pointer(pointer)
        .map_or("nothing".to_owned(), |v| clip(&v.to_string()));
    Shape::Broken(format!("{} {pointer}: want {want}, got {got}", kind(msg)))
}

fn string(msg: &Value, pointer: &str) -> Shape {
    match msg.pointer(pointer) {
        Some(Value::String(_)) => Shape::Fits,
        _ => broken(msg, pointer, "a string"),
    }
}

fn boolean(msg: &Value, pointer: &str) -> Shape {
    match msg.pointer(pointer) {
        Some(Value::Bool(_)) => Shape::Fits,
        _ => broken(msg, pointer, "a bool"),
    }
}

fn uuid(msg: &Value, pointer: &str) -> Shape {
    match msg.pointer(pointer).and_then(Value::as_str) {
        Some(id) if uuid::Uuid::parse_str(id).is_ok() => Shape::Fits,
        _ => broken(msg, pointer, "a uuid"),
    }
}

/// A permission mode the adapter knows; absent is fine unless `required`.
fn mode(msg: &Value, pointer: &str, required: bool) -> Shape {
    match msg.pointer(pointer) {
        None if !required => Shape::Fits,
        Some(Value::String(name)) if parse_mode(name).is_some() => Shape::Fits,
        Some(Value::String(name)) => Shape::New(format!("permission mode {name}")),
        _ => broken(msg, pointer, "a permission mode"),
    }
}

fn background_tasks(msg: &Value) -> Shape {
    let Some(tasks) = msg.get("tasks").and_then(Value::as_array) else {
        return broken(msg, "/tasks", "an array");
    };
    let mut shapes = Vec::new();
    for (i, task) in tasks.iter().enumerate() {
        shapes.push(string(msg, &format!("/tasks/{i}/task_id")));
        match task.get("task_type").and_then(Value::as_str) {
            Some("local_bash" | "local_agent") => {}
            Some(other) => shapes.push(Shape::New(format!("background task type {other}"))),
            None => shapes.push(broken(msg, &format!("/tasks/{i}/task_type"), "a string")),
        }
    }
    all(&shapes)
}

fn assistant(msg: &Value) -> Shape {
    let Some(blocks) = msg.pointer("/message/content").and_then(Value::as_array) else {
        return broken(msg, "/message/content", "an array");
    };
    let mut shapes = Vec::new();
    for (i, block) in blocks.iter().enumerate() {
        let at = |field: &str| format!("/message/content/{i}/{field}");
        match block.get("type").and_then(Value::as_str) {
            Some("text") => shapes.push(string(msg, &at("text"))),
            Some("tool_use") => {
                shapes.push(string(msg, &at("id")));
                shapes.push(string(msg, &at("name")));
                if !msg.pointer(&at("input")).is_some_and(Value::is_object) {
                    shapes.push(broken(msg, &at("input"), "an object"));
                }
            }
            Some("thinking" | "redacted_thinking") => {}
            Some(other) => shapes.push(Shape::New(format!("assistant block {other}"))),
            None => shapes.push(broken(msg, &at("type"), "a string")),
        }
    }
    all(&shapes)
}

fn user(msg: &Value) -> Shape {
    let blocks = match msg.pointer("/message/content") {
        Some(Value::Array(blocks)) => blocks,
        // A plain prompt or marker: the adapter reads only tool results.
        Some(Value::String(_)) => return Shape::Fits,
        _ => return broken(msg, "/message/content", "an array or a string"),
    };
    let mut shapes = Vec::new();
    for (i, block) in blocks.iter().enumerate() {
        if block.get("type").and_then(Value::as_str) != Some("tool_result") {
            continue;
        }
        let at = |field: &str| format!("/message/content/{i}/{field}");
        shapes.push(string(msg, &at("tool_use_id")));
        match block.get("content") {
            None | Some(Value::Null | Value::String(_) | Value::Array(_)) => {}
            Some(_) => shapes.push(broken(msg, &at("content"), "text or blocks")),
        }
        match block.get("is_error") {
            None | Some(Value::Bool(_)) => {}
            Some(_) => shapes.push(broken(msg, &at("is_error"), "a bool")),
        }
    }
    all(&shapes)
}

fn control_request(msg: &Value) -> Shape {
    let base = all(&[string(msg, "/request_id"), string(msg, "/request/subtype")]);
    if base != Shape::Fits {
        return base;
    }
    match msg.pointer("/request/subtype").and_then(Value::as_str) {
        Some("can_use_tool") => {
            let input = if msg.pointer("/request/input").is_some_and(Value::is_object) {
                Shape::Fits
            } else {
                broken(msg, "/request/input", "an object")
            };
            all(&[
                string(msg, "/request/tool_name"),
                string(msg, "/request/tool_use_id"),
                input,
            ])
        }
        Some("hook_callback") => string(msg, "/request/callback_id"),
        // The adapter refuses any other request with an error.
        Some(other) => Shape::New(format!("control_request/{other}")),
        None => Shape::Fits,
    }
}

fn control_response(msg: &Value) -> Shape {
    let base = string(msg, "/response/request_id");
    if base != Shape::Fits {
        return base;
    }
    match msg.pointer("/response/subtype").and_then(Value::as_str) {
        Some("success") => {
            // An effort read: `applied.effort` must still name a level.
            match msg.pointer("/response/response/applied/effort") {
                None | Some(Value::Null) => Shape::Fits,
                Some(Value::String(name)) if parse_effort(name).is_some() => Shape::Fits,
                Some(Value::String(name)) => Shape::New(format!("effort level {name}")),
                Some(_) => broken(msg, "/response/response/applied/effort", "a level"),
            }
        }
        Some("error") => string(msg, "/response/error"),
        _ => broken(msg, "/response/subtype", "success or error"),
    }
}

/// `text`, cut to one short line for a report.
pub fn clip(text: &str) -> String {
    const MAX: usize = 160;
    let line = text.replace('\n', " ");
    match line.char_indices().nth(MAX) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;

    /// Every line of every recorded fixture fits: the shape table agrees
    /// with what the adapter's replay tests already accept.
    #[test]
    fn every_recorded_line_fits() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../skywaved/tests/fixtures/claude-code");
        let mut checked = 0;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|e| e != "ndjson") {
                continue;
            }
            for line in std::fs::read_to_string(&path).unwrap().lines() {
                let record: Value = serde_json::from_str(line).unwrap();
                if record["dir"] != "out" {
                    continue;
                }
                let shape = check(&record["msg"]);
                assert_eq!(shape, Shape::Fits, "{}: {line}", path.display());
                checked += 1;
            }
        }
        assert!(checked > 1000, "only {checked} lines checked");
    }

    #[test]
    fn a_missing_field_the_adapter_reads_is_broken() {
        let shape = check(
            &json!({"type": "system", "subtype": "task_started", "task_type": "local_agent", "task_id": "a"}),
        );
        assert!(
            matches!(&shape, Shape::Broken(why) if why.contains("/tool_use_id")),
            "{shape:?}"
        );
    }

    #[test]
    fn an_unknown_kind_is_new() {
        assert_eq!(
            check(&json!({"type": "system", "subtype": "teleport"})),
            Shape::New("system/teleport".to_owned())
        );
        assert_eq!(
            check(
                &json!({"type": "control_request", "request_id": "r", "request": {"subtype": "elicit"}})
            ),
            Shape::New("control_request/elicit".to_owned())
        );
    }
}
