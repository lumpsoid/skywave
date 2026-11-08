//! The account's usage limits, from the CLI's `get_usage` control request
//! (docs/agents/claude-code.md §3.15).
//!
//! The CLI answers `get_usage` without a handshake and without a turn,
//! within about a second of starting, so [`ClaudeUsageLimits`] runs a
//! process of its own for each read rather than borrowing a session's: one
//! as the adapter starts any, under a new session id, which writes the
//! request, reads until its answer and is killed. A process that is never
//! sent a message keeps no conversation, so nothing is left behind.
//!
//! The answer's `rate_limits.limits` is the list read: each limit's
//! `group` is its window, `percent` its use (0–100, not the 0–1 fraction
//! `rate_limit_event` uses), `severity`, `resets_at` and `scope`. A group
//! or severity this adapter does not know is left out, as is any figure
//! missing; the rest of the answer (the codename keys, spend, the process's
//! own `session` cost) is never read.

use std::path::PathBuf;
use std::time::Instant;

use chrono::{DateTime, Utc};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tracing::Instrument;
use uuid::Uuid;

use super::lines::Line;
use super::protocol::{Protocol, str_field};
use super::{ClaudeCodeAdapter, CliSession, Process};
use crate::agent::{LimitSeverity, LimitWindow, LimitsError, UsageLimit, UsageLimitsSource};

/// The request id of the one `get_usage` a usage process is sent.
pub const USAGE_REQUEST_ID: &str = "skywave_usage";

/// The request that reads the account's usage. `skip_behaviors` leaves out
/// the CLI's advice on how to spend what is left, which is never read.
pub fn get_usage_request(request_id: &str) -> Value {
    json!({
        "type": "control_request",
        "request_id": request_id,
        "request": { "subtype": "get_usage", "skip_behaviors": true },
    })
}

/// The answer `message` (a stdout line) gives to request `request_id`:
/// `None` for any other line, else the limits or the CLI's refusal.
pub fn usage_answer(message: &Value, request_id: &str) -> Option<Result<Vec<UsageLimit>, String>> {
    if message.get("type").and_then(Value::as_str) != Some("control_response") {
        return None;
    }
    let response = message.get("response")?;
    if str_field(response, "request_id") != request_id {
        return None;
    }
    if str_field(response, "subtype") != "success" {
        let error = response
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("get_usage failed");
        return Some(Err(error.to_owned()));
    }
    Some(usage_limits(
        response.get("response").unwrap_or(&Value::Null),
    ))
}

/// The limits a successful `get_usage` answer's body lists: none when the
/// account has no rate limits (`rate_limits_available` false), an error
/// when it has some and no list the adapter reads.
pub fn usage_limits(body: &Value) -> Result<Vec<UsageLimit>, String> {
    if body.get("rate_limits_available").and_then(Value::as_bool) == Some(false) {
        return Ok(Vec::new());
    }
    let limits = body
        .pointer("/rate_limits/limits")
        .and_then(Value::as_array)
        .ok_or("the agent's answer lists no usage limits")?;
    Ok(limits.iter().filter_map(usage_limit).collect())
}

/// One entry of `rate_limits.limits`; `None` for a window the adapter does
/// not know.
fn usage_limit(limit: &Value) -> Option<UsageLimit> {
    let window = parse_window(limit.get("group")?.as_str()?)?;
    let percent = limit
        .get("percent")
        .and_then(Value::as_f64)
        .filter(|p| p.is_finite() && *p >= 0.0)
        .map(|p| p.round().min(f64::from(u32::MAX)) as u32);
    let severity = limit
        .get("severity")
        .and_then(Value::as_str)
        .and_then(parse_severity);
    let resets_at = limit
        .get("resets_at")
        .and_then(Value::as_str)
        .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
        .map(|at| at.with_timezone(&Utc));
    Some(UsageLimit {
        window,
        scope: scope(limit.get("scope")),
        percent,
        severity,
        resets_at,
    })
}

/// The window the CLI calls `group`; `None` for one it added since.
pub fn parse_window(group: &str) -> Option<LimitWindow> {
    match group {
        "session" => Some(LimitWindow::Session),
        "weekly" => Some(LimitWindow::Weekly),
        _ => None,
    }
}

/// The severity the CLI calls `name`; `None` for one it added since.
pub fn parse_severity(name: &str) -> Option<LimitSeverity> {
    match name {
        "normal" => Some(LimitSeverity::Normal),
        "warning" => Some(LimitSeverity::Warning),
        _ => None,
    }
}

/// What a limit's `scope` covers, named: its model's `display_name`, else
/// its `surface` when that is text. `null` (all use) is `None`.
fn scope(scope: Option<&Value>) -> Option<String> {
    let scope = scope?;
    let named = |value: Option<&Value>| {
        value
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
    };
    named(scope.pointer("/model/display_name")).or_else(|| named(scope.get("surface")))
}

/// Reads the usage limits with a process of their own, one read at a time:
/// two devices asking at once start one process after the other.
pub struct ClaudeUsageLimits {
    adapter: ClaudeCodeAdapter,
    /// Where the process runs: a configured root, so the CLI runs where
    /// it is trusted to.
    workdir: PathBuf,
    one_at_a_time: Mutex<()>,
}

impl ClaudeUsageLimits {
    pub fn new(adapter: ClaudeCodeAdapter, workdir: PathBuf) -> ClaudeUsageLimits {
        ClaudeUsageLimits {
            adapter,
            workdir,
            one_at_a_time: Mutex::new(()),
        }
    }

    async fn read_by(
        &self,
        deadline: tokio::time::Instant,
    ) -> Result<Vec<UsageLimit>, LimitsError> {
        let _turn = tokio::time::timeout_at(deadline, self.one_at_a_time.lock())
            .await
            .map_err(|_| timed_out())?;
        let session = CliSession::New(Uuid::new_v4());
        let span = tracing::info_span!("usage_limits");
        let failed = |e: crate::agent::AgentError| LimitsError::Failed(e.to_string());
        let child = self
            .adapter
            .spawn(&self.workdir, session, &[], None)
            .map_err(failed)?;
        let mut process = Process::new(
            child,
            &span,
            self.adapter.write_timeout,
            self.adapter.term_grace,
        )
        .map_err(failed)?;
        let read = tokio::time::timeout_at(
            deadline,
            ask(&mut process, session).instrument(span.clone()),
        )
        .await
        .unwrap_or_else(|_| Err(timed_out()));
        // Off the reply: a CLI that ignores SIGTERM takes the term grace.
        tokio::spawn(async move { process.kill().await }.instrument(span));
        read
    }
}

impl UsageLimitsSource for ClaudeUsageLimits {
    fn read(&self, deadline: Instant) -> BoxFuture<'_, Result<Vec<UsageLimit>, LimitsError>> {
        Box::pin(self.read_by(tokio::time::Instant::from_std(deadline)))
    }
}

fn timed_out() -> LimitsError {
    LimitsError::Failed("the agent did not answer in time".to_owned())
}

/// Writes the `get_usage` and reads until its answer, or until stdout ends,
/// with the exit's error then.
async fn ask(process: &mut Process, session: CliSession) -> Result<Vec<UsageLimit>, LimitsError> {
    if let Err(e) = process.write(&[get_usage_request(USAGE_REQUEST_ID)]).await {
        // A CLI that already exited shows up as EOF below, with its error.
        tracing::debug!("cannot write to the agent: {e}");
    }
    loop {
        match process.next_line().await {
            Ok(Some(Line::Text(text))) => {
                let Ok(message) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                if let Some(answer) = usage_answer(&message, USAGE_REQUEST_ID) {
                    return answer.map_err(LimitsError::Failed);
                }
            }
            Ok(Some(Line::TooLong(_))) => {}
            Ok(None) | Err(_) => {
                let status = process.wait_exit().await;
                let protocol = Protocol::new(session.id(), false);
                let error = process.exit_error(&protocol, status).await;
                return Err(LimitsError::Failed(error.unwrap_or_else(|| {
                    "the agent exited without answering".to_owned()
                })));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_each_limit_and_leaves_out_what_it_does_not_know() {
        let body = json!({
            "rate_limits_available": true,
            "rate_limits": { "limits": [
                { "kind": "session", "group": "session", "percent": 75, "severity": "warning",
                  "resets_at": "2026-11-08T20:20:00.259325+00:00", "scope": null, "is_active": true },
                { "kind": "weekly_scoped", "group": "weekly", "percent": 0.4, "severity": "normal",
                  "resets_at": "2026-11-15T00:00:00+00:00",
                  "scope": { "model": { "id": null, "display_name": "Fable" }, "surface": null } },
                { "kind": "monthly", "group": "monthly", "percent": 3 },
                { "group": "weekly", "severity": "dire", "resets_at": "soon" },
            ] },
        });
        let at = |s: &str| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc);
        assert_eq!(
            usage_limits(&body).unwrap(),
            [
                UsageLimit {
                    window: LimitWindow::Session,
                    scope: None,
                    percent: Some(75),
                    severity: Some(LimitSeverity::Warning),
                    resets_at: Some(at("2026-11-08T20:20:00.259325+00:00")),
                },
                UsageLimit {
                    window: LimitWindow::Weekly,
                    scope: Some("Fable".into()),
                    percent: Some(0),
                    severity: Some(LimitSeverity::Normal),
                    resets_at: Some(at("2026-11-15T00:00:00+00:00")),
                },
                UsageLimit {
                    window: LimitWindow::Weekly,
                    scope: None,
                    percent: None,
                    severity: None,
                    resets_at: None,
                },
            ]
        );
    }

    #[test]
    fn an_account_without_limits_has_none_and_a_missing_list_is_an_error() {
        let none = json!({ "rate_limits_available": false, "rate_limits": null });
        assert_eq!(usage_limits(&none).unwrap(), []);
        assert!(usage_limits(&json!({ "rate_limits_available": true })).is_err());
    }

    #[test]
    fn only_the_answer_to_the_request_counts() {
        let answer = |id: &str, body: Value| {
            json!({ "type": "control_response", "response": body.as_object().unwrap().clone().into_iter()
                .chain([("request_id".to_owned(), json!(id))]).collect::<serde_json::Map<_, _>>() })
        };
        let ok = answer(
            "u1",
            json!({ "subtype": "success", "response": { "rate_limits_available": false } }),
        );
        assert_eq!(usage_answer(&ok, "u1"), Some(Ok(Vec::new())));
        assert_eq!(usage_answer(&ok, "u2"), None);
        assert_eq!(usage_answer(&json!({ "type": "system" }), "u1"), None);
        let refused = answer(
            "u1",
            json!({ "subtype": "error", "error": "Not logged in" }),
        );
        assert_eq!(
            usage_answer(&refused, "u1"),
            Some(Err("Not logged in".into()))
        );
    }

    #[test]
    fn the_request_skips_the_advice() {
        assert_eq!(
            get_usage_request("r"),
            json!({
                "type": "control_request",
                "request_id": "r",
                "request": { "subtype": "get_usage", "skip_behaviors": true },
            })
        );
    }
}
