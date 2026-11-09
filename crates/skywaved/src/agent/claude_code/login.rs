//! When the CLI's login expires, from the credentials it stores
//! (docs/agents/claude-code.md §3.16).
//!
//! The CLI's own terminal UI warns "Your login expires in N days" from the
//! stored refresh token's expiry, not from anything stream-json says: a
//! headless process never reports it. [`ClaudeLogin`] reads the same
//! timestamp, `claudeAiOauth.refreshTokenExpiresAt` (milliseconds since the
//! epoch) in `.credentials.json`, each time it is asked. Past it the CLI can
//! no longer refresh its access token and needs `/login` again.
//!
//! A login the CLI does not use has no expiry here: one where the
//! environment the CLI inherits gives it another way in
//! ([`OTHER_LOGIN_ENV`]), and one whose access token outlives the refresh
//! token by more than [`ACCESS_OUTLIVES_REFRESH`], which the CLI's warning
//! skips too (a token made for long use).
//!
//! **Logging in from a device** ([`ClaudeLogin::begin`]) runs the CLI's
//! manual-code login (§3.17) in a process of its own, started as a usage
//! read's is: a new session id, no handshake (the CLI answers
//! `claude_authenticate` without one). The answer's `manualUrl` is the
//! attempt's URL; its `automaticUrl`, which redirects to the CLI's own
//! localhost, is never read. The process then lives inside the
//! [`ClaudeLoginAttempt`], owned by one task that keeps reading and dropping
//! its stdout (a CLI whose stdout fills up stops, and the CLI writes its
//! whole command list unasked) until the pasted code comes. The task sends
//! `claude_oauth_callback` with the code's two halves and answers with the
//! CLI's answer, however long it takes; the CLI writes `.credentials.json`
//! before it answers, so [`ClaudeLogin::expires`] sees the new login at
//! once. Any callback ends the CLI's flow, so the process is killed after
//! it, and when the attempt is dropped. The URL, the code and the account
//! the CLI answers with are never logged.

use std::path::PathBuf;
use std::time::Instant;

use chrono::{DateTime, TimeDelta, Utc};
use futures_util::future::BoxFuture;
use serde_json::Value;
use tokio::sync::oneshot;
use tracing::Instrument;
use uuid::Uuid;

use super::lines::Line;
use super::protocol::{
    Protocol, authenticate_request, manual_url, oauth_callback_request, str_field,
};
use super::{ClaudeCodeAdapter, CliSession, Process};
use crate::agent::{AgentError, AgentLogin, LoginAttempt, LoginError};

/// The request id of the one `claude_authenticate` a login process is sent.
pub const AUTHENTICATE_REQUEST_ID: &str = "skywave_login";

/// The request id of the one `claude_oauth_callback` a login process is
/// sent.
pub const CALLBACK_REQUEST_ID: &str = "skywave_login_code";

/// What [`LoginAttempt::check`] says of a paste that is not `<code>#<state>`.
pub const PASTE_WHOLE_CODE: &str = "paste the whole code shown after signing in";

/// The two halves of a pasted `<code>#<state>`, split at the first `#`,
/// the paste's surrounding whitespace left out; a refusal for anything with
/// a half missing.
pub fn split_code(pasted: &str) -> Result<(&str, &str), String> {
    match pasted.trim().split_once('#') {
        Some((code, state)) if !code.is_empty() && !state.is_empty() => Ok((code, state)),
        _ => Err(PASTE_WHOLE_CODE.to_owned()),
    }
}

/// The answer `message` (a stdout line) gives to control request
/// `request_id`: `None` for any other line, else the answer's body or the
/// CLI's refusal.
pub fn control_answer<'a>(
    message: &'a Value,
    request_id: &str,
) -> Option<Result<&'a Value, String>> {
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
            .unwrap_or("the agent refused the login");
        return Some(Err(error.to_owned()));
    }
    Some(Ok(response.get("response").unwrap_or(&Value::Null)))
}

/// Environment variables that make the CLI log in with something other than
/// its stored login (an API key, a token, or another provider).
pub const OTHER_LOGIN_ENV: [&str; 5] = [
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
];

/// An access token that outlives the refresh token by more than this is not
/// one the refresh token renews: the stored login does not expire with it.
pub const ACCESS_OUTLIVES_REFRESH: TimeDelta = TimeDelta::days(3);

/// The credentials file the CLI uses with this process's environment:
/// `$CLAUDE_CONFIG_DIR/.credentials.json`, else
/// `$HOME/.claude/.credentials.json`. The CLI the adapter spawns inherits
/// the same environment.
pub fn credentials_file() -> Option<PathBuf> {
    let non_empty = |key| std::env::var_os(key).filter(|v| !v.is_empty());
    if let Some(config) = non_empty("CLAUDE_CONFIG_DIR") {
        return Some(PathBuf::from(config).join(".credentials.json"));
    }
    non_empty("HOME").map(|home| {
        PathBuf::from(home)
            .join(".claude")
            .join(".credentials.json")
    })
}

/// When the login in `credentials` (the file's JSON) expires; `None` when it
/// holds no login that expires.
pub fn login_expires(credentials: &Value) -> Option<DateTime<Utc>> {
    let oauth = credentials.get("claudeAiOauth")?;
    let millis = |key| oauth.get(key).and_then(Value::as_i64);
    let refresh = DateTime::from_timestamp_millis(millis("refreshTokenExpiresAt")?)?;
    if let Some(access) = millis("expiresAt").and_then(DateTime::from_timestamp_millis)
        && access > refresh + ACCESS_OUTLIVES_REFRESH
    {
        return None;
    }
    Some(refresh)
}

/// The CLI's stored login, read from its credentials file, and made anew by
/// a process of the CLI's own.
#[derive(Debug, Clone)]
pub struct ClaudeLogin {
    credentials: PathBuf,
    /// Whether the environment gives the CLI another way in.
    other_login: bool,
    adapter: ClaudeCodeAdapter,
    /// Where a login process runs: a configured root, as a usage read's.
    workdir: PathBuf,
}

impl ClaudeLogin {
    /// The login stored in `credentials`, which the CLI `adapter` starts
    /// uses unless `other_login`; a login process runs in `workdir`.
    pub fn new(
        credentials: PathBuf,
        other_login: bool,
        adapter: ClaudeCodeAdapter,
        workdir: PathBuf,
    ) -> ClaudeLogin {
        ClaudeLogin {
            credentials,
            other_login,
            adapter,
            workdir,
        }
    }

    /// The login the CLI uses with this process's environment; `None` with
    /// neither `CLAUDE_CONFIG_DIR` nor `HOME`.
    pub fn from_env(adapter: ClaudeCodeAdapter, workdir: PathBuf) -> Option<ClaudeLogin> {
        let other_login = OTHER_LOGIN_ENV
            .iter()
            .any(|key| std::env::var_os(key).is_some_and(|v| !v.is_empty()));
        Some(ClaudeLogin::new(
            credentials_file()?,
            other_login,
            adapter,
            workdir,
        ))
    }

    async fn begin_by(
        &self,
        deadline: tokio::time::Instant,
    ) -> Result<Box<dyn LoginAttempt>, LoginError> {
        if !self.offered() {
            return Err(LoginError::Unavailable);
        }
        let session = CliSession::New(Uuid::new_v4());
        let span = tracing::info_span!("agent_login");
        let failed = |e: AgentError| LoginError::Failed(e.to_string());
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
        let url = tokio::time::timeout_at(
            deadline,
            authenticate(&mut process, session).instrument(span.clone()),
        )
        .await
        .unwrap_or_else(|_| Err("the agent did not answer in time".to_owned()));
        match url {
            Ok(url) => {
                let (code, taken) = oneshot::channel();
                tokio::spawn(attend(process, session, taken).instrument(span));
                Ok(Box::new(ClaudeLoginAttempt { url, code }))
            }
            Err(why) => {
                // Off the reply: a CLI that ignores SIGTERM takes the term
                // grace.
                tokio::spawn(async move { process.kill().await }.instrument(span));
                Err(LoginError::Failed(why))
            }
        }
    }
}

impl AgentLogin for ClaudeLogin {
    fn expires(&self) -> Option<DateTime<Utc>> {
        if self.other_login {
            return None;
        }
        // No file is no stored login (the CLI may keep it elsewhere, such
        // as a keychain); an unreadable one tells nothing either.
        let text = std::fs::read_to_string(&self.credentials).ok()?;
        login_expires(&serde_json::from_str(&text).ok()?)
    }

    /// A login made from a device would be stored in the credentials file,
    /// which the CLI ignores while the environment gives it another way in.
    fn offered(&self) -> bool {
        !self.other_login
    }

    fn begin(&self, deadline: Instant) -> BoxFuture<'_, Result<Box<dyn LoginAttempt>, LoginError>> {
        Box::pin(self.begin_by(tokio::time::Instant::from_std(deadline)))
    }
}

/// An open login: its URL, and the way to the task that owns its process.
/// No `Debug`: the URL carries the flow's state.
pub struct ClaudeLoginAttempt {
    url: String,
    /// Dropped unused, it tells the task to kill the process.
    code: oneshot::Sender<Pasted>,
}

/// The pasted code's halves, for the task to send, and where its answer
/// goes.
struct Pasted {
    code: String,
    state: String,
    answer: oneshot::Sender<Result<(), String>>,
}

impl LoginAttempt for ClaudeLoginAttempt {
    fn url(&self) -> &str {
        &self.url
    }

    fn check(&self, code: &str) -> Result<(), String> {
        split_code(code).map(|_| ())
    }

    fn finish(self: Box<Self>, code: String) -> BoxFuture<'static, Result<(), String>> {
        Box::pin(async move {
            let (code, state) = split_code(&code)?;
            let (answer, answered) = oneshot::channel();
            let pasted = Pasted {
                code: code.to_owned(),
                state: state.to_owned(),
                answer,
            };
            if self.code.send(pasted).is_err() {
                return Err(TASK_GONE.to_owned());
            }
            answered.await.unwrap_or_else(|_| Err(TASK_GONE.to_owned()))
        })
    }
}

/// The task owning the process is gone: it panicked.
const TASK_GONE: &str = "the agent's login stopped";

/// Writes the `claude_authenticate` and reads until its answer: the manual
/// URL, or the CLI's refusal, or the exit's error once stdout ends.
async fn authenticate(process: &mut Process, session: CliSession) -> Result<String, String> {
    if let Err(e) = process
        .write(&[authenticate_request(AUTHENTICATE_REQUEST_ID)])
        .await
    {
        // A CLI that already exited shows up as EOF below, with its error.
        tracing::debug!("cannot write to the agent: {e}");
    }
    let body = answer(process, session, AUTHENTICATE_REQUEST_ID).await?;
    manual_url(&body)
        .map(str::to_owned)
        .ok_or_else(|| "the agent gave no URL to sign in at".to_owned())
}

/// Owns an open attempt's process: reads and drops its stdout until the
/// pasted code comes, then sends it and passes the CLI's answer on. Every
/// way out kills the process: the attempt dropped before a code, the
/// answer's receiver dropped before the answer, or the answer given.
async fn attend(mut process: Process, session: CliSession, mut code: oneshot::Receiver<Pasted>) {
    let mut ended = false;
    let pasted = loop {
        tokio::select! {
            pasted = &mut code => break pasted.ok(),
            line = process.next_line(), if !ended => ended = !matches!(line, Ok(Some(_))),
        }
    };
    if let Some(Pasted {
        code,
        state,
        mut answer,
    }) = pasted
    {
        let outcome = tokio::select! {
            outcome = callback(&mut process, session, &code, &state) => Some(outcome),
            () = answer.closed() => None,
        };
        if let Some(outcome) = outcome {
            let _ = answer.send(outcome);
        }
    }
    process.kill().await;
}

/// Writes the `claude_oauth_callback` and reads until its answer, with no
/// deadline: the CLI's exchange with its token endpoint is the outcome.
async fn callback(
    process: &mut Process,
    session: CliSession,
    code: &str,
    state: &str,
) -> Result<(), String> {
    let request = oauth_callback_request(CALLBACK_REQUEST_ID, code, state);
    if let Err(e) = process.write(&[request]).await {
        tracing::debug!("cannot write to the agent: {e}");
    }
    // The body names the account; it is not read.
    answer(process, session, CALLBACK_REQUEST_ID)
        .await
        .map(|_| ())
}

/// Reads stdout until the answer to `request_id`: its body, or the CLI's
/// refusal, or the exit's error once stdout ends.
async fn answer(
    process: &mut Process,
    session: CliSession,
    request_id: &str,
) -> Result<Value, String> {
    loop {
        match process.next_line().await {
            Ok(Some(Line::Text(text))) => {
                let Ok(message) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                if let Some(answer) = control_answer(&message, request_id) {
                    return answer.cloned();
                }
            }
            Ok(Some(Line::TooLong(_))) => {}
            Ok(None) | Err(_) => {
                let status = process.wait_exit().await;
                let protocol = Protocol::new(session.id(), false);
                let error = process.exit_error(&protocol, status).await;
                return Err(
                    error.unwrap_or_else(|| "the agent exited without answering".to_owned())
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const REFRESH: i64 = 1_794_445_707_628;

    #[test]
    fn the_refresh_token_expiry_is_the_login_expiry() {
        let credentials = json!({"claudeAiOauth": {
            "expiresAt": REFRESH - 2 * 86_400_000,
            "refreshTokenExpiresAt": REFRESH,
        }});
        assert_eq!(
            login_expires(&credentials),
            DateTime::from_timestamp_millis(REFRESH)
        );
    }

    #[test]
    fn an_access_token_outliving_the_refresh_token_does_not_expire_with_it() {
        let just = REFRESH + ACCESS_OUTLIVES_REFRESH.num_milliseconds();
        let at = |access: i64| {
            login_expires(&json!({"claudeAiOauth": {
                "expiresAt": access,
                "refreshTokenExpiresAt": REFRESH,
            }}))
        };
        assert_eq!(at(just), DateTime::from_timestamp_millis(REFRESH));
        assert_eq!(at(just + 1), None);
    }

    #[test]
    fn no_oauth_login_no_expiry() {
        assert_eq!(login_expires(&json!({})), None);
        assert_eq!(login_expires(&json!({"claudeAiOauth": {}})), None);
        assert_eq!(
            login_expires(&json!({"claudeAiOauth": {"refreshTokenExpiresAt": "soon"}})),
            None
        );
    }

    #[test]
    fn the_file_is_read_each_time_and_another_login_hides_it() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(".credentials.json");
        let login = stored(file.clone(), false);
        assert_eq!(login.expires(), None);

        let write = |millis: i64| {
            let body = json!({"claudeAiOauth": {"refreshTokenExpiresAt": millis}});
            std::fs::write(&file, body.to_string()).unwrap();
        };
        write(REFRESH);
        assert_eq!(login.expires(), DateTime::from_timestamp_millis(REFRESH));
        write(REFRESH + 1);
        assert_eq!(
            login.expires(),
            DateTime::from_timestamp_millis(REFRESH + 1)
        );

        assert_eq!(stored(file, true).expires(), None);
    }

    #[test]
    fn a_device_login_is_offered_unless_the_environment_logs_in() {
        let file = PathBuf::from("/nonexistent/.credentials.json");
        assert!(stored(file.clone(), false).offered());
        assert!(!stored(file, true).offered());
    }

    #[test]
    fn a_pasted_code_splits_at_its_first_hash() {
        assert_eq!(split_code("a#b#c"), Ok(("a", "b#c")));
        assert_eq!(split_code(" code#state\n"), Ok(("code", "state")));
        for refused in ["nohash", "#state", "code#", "#", ""] {
            assert_eq!(
                split_code(refused),
                Err(PASTE_WHOLE_CODE.to_owned()),
                "{refused:?}"
            );
        }
    }

    #[test]
    fn only_the_answer_to_the_request_counts() {
        let answer = |id: &str, subtype: &str| {
            json!({ "type": "control_response", "response": {
                "subtype": subtype, "request_id": id,
                "response": { "manualUrl": "https://m" }, "error": "Request failed with status code 400",
            }})
        };
        let ok = answer("l1", "success");
        assert_eq!(
            control_answer(&ok, "l1"),
            Some(Ok(&json!({ "manualUrl": "https://m" })))
        );
        assert_eq!(control_answer(&ok, "l2"), None);
        assert_eq!(control_answer(&json!({ "type": "system" }), "l1"), None);
        assert_eq!(
            control_answer(&answer("l1", "error"), "l1"),
            Some(Err("Request failed with status code 400".to_owned()))
        );
    }

    /// A login in `file`, whose CLI is never started.
    fn stored(file: PathBuf, other_login: bool) -> ClaudeLogin {
        let adapter = ClaudeCodeAdapter::new(crate::config::AgentConfig {
            command: PathBuf::from("claude"),
            args: Vec::new(),
            allow_bypass: false,
        });
        ClaudeLogin::new(file, other_login, adapter, PathBuf::from("/nonexistent"))
    }
}
