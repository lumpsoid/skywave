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

use std::path::PathBuf;

use chrono::{DateTime, TimeDelta, Utc};
use serde_json::Value;

use crate::agent::AgentLogin;

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

/// The CLI's stored login, read from its credentials file.
#[derive(Debug, Clone)]
pub struct ClaudeLogin {
    credentials: PathBuf,
    /// Whether the environment gives the CLI another way in.
    other_login: bool,
}

impl ClaudeLogin {
    /// The login stored in `credentials`, which the CLI uses unless
    /// `other_login`.
    pub fn new(credentials: PathBuf, other_login: bool) -> ClaudeLogin {
        ClaudeLogin {
            credentials,
            other_login,
        }
    }

    /// The login the CLI uses with this process's environment; `None` with
    /// neither `CLAUDE_CONFIG_DIR` nor `HOME`.
    pub fn from_env() -> Option<ClaudeLogin> {
        let other_login = OTHER_LOGIN_ENV
            .iter()
            .any(|key| std::env::var_os(key).is_some_and(|v| !v.is_empty()));
        Some(ClaudeLogin::new(credentials_file()?, other_login))
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
        let login = ClaudeLogin::new(file.clone(), false);
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

        assert_eq!(ClaudeLogin::new(file, true).expires(), None);
    }

    #[test]
    fn a_device_login_is_offered_unless_the_environment_logs_in() {
        let file = PathBuf::from("/nonexistent/.credentials.json");
        assert!(ClaudeLogin::new(file.clone(), false).offered());
        assert!(!ClaudeLogin::new(file, true).offered());
    }
}
