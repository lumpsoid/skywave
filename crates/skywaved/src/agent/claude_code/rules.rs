//! Claude Code's permission suggestions as agent-agnostic rule suggestions
//! (docs/agents/claude-code.md §3.1, D3).
//!
//! A `can_use_tool` request may carry `permission_suggestions`: permission
//! updates the CLI would apply to stop asking. [`offered`] keeps the ones
//! that widen what is allowed (`addRules` with `behavior: "allow"`,
//! `setMode` to `acceptEdits` or `bypassPermissions`, `addDirectories`),
//! labels each for the user and maps its `destination` to a [`RuleScope`];
//! an approval that grants one sends that update back verbatim as the allow
//! response's `updatedPermissions`. Anything else (removals, rules that deny
//! or ask, modes that allow less, unknown shapes or destinations) is not
//! offered.
//!
//! Labels read after "always allow": `Bash(cargo test)`, `file edits`,
//! `access to /srv/data`. They are handed over uncut; the mapper applies the
//! protocol's caps.
//!
//! **Session rules outlive the process.** The CLI keeps a granted update
//! with destination `session` or `cliArg` in its process only, but the rule
//! was granted for the Skywave session, which outlives it (an idle shutdown,
//! a daemon restart). [`keep`] collects those updates for the resume handle;
//! [`cli_args`] grants them again to the resumed process as flags.

use serde_json::Value;

use crate::agent::{RuleScope, RuleSuggestion};

/// The suggestions of a request worth offering, each with the update to
/// send back when it is granted, in the request's order.
pub fn offered(suggestions: Option<&Value>) -> Vec<(RuleSuggestion, Value)> {
    let Some(suggestions) = suggestions.and_then(Value::as_array) else {
        return Vec::new();
    };
    suggestions
        .iter()
        .filter_map(|update| {
            let suggestion = RuleSuggestion {
                label: label(update)?,
                scope: scope(update)?,
            };
            Some((suggestion, update.clone()))
        })
        .collect()
}

/// Adds granted `update` to `kept`, the updates that die with the process,
/// when it is one of them; a later mode replaces an earlier one. Whether
/// `kept` changed.
pub fn keep(kept: &mut Vec<Value>, update: &Value) -> bool {
    if scope(update) != Some(RuleScope::Session) || kept.contains(update) {
        return false;
    }
    if is_mode(update) {
        kept.retain(|k| !is_mode(k));
    }
    kept.push(update.clone());
    true
}

/// The CLI flags that grant `kept` again, in order. Updates that allow
/// nothing more are skipped, so a stored handle cannot narrow the process.
pub fn cli_args(kept: &[Value]) -> Vec<String> {
    let mut args = Vec::new();
    for update in kept.iter().filter(|u| label(u).is_some()) {
        let (flag, values): (&str, Vec<String>) = match update["type"].as_str() {
            Some("setMode") => ("--permission-mode", strings(&update["mode"])),
            Some("addRules") => (
                "--allowedTools",
                items(update, "rules").filter_map(rule).collect(),
            ),
            Some("addDirectories") => (
                "--add-dir",
                items(update, "directories").flat_map(strings).collect(),
            ),
            _ => continue,
        };
        for value in values {
            args.extend([flag.to_owned(), value]);
        }
    }
    args
}

fn is_mode(update: &Value) -> bool {
    update["type"] == "setMode"
}

fn items<'a>(update: &'a Value, key: &str) -> impl Iterator<Item = &'a Value> {
    update[key].as_array().into_iter().flatten()
}

fn strings(value: &Value) -> Vec<String> {
    value.as_str().map(str::to_owned).into_iter().collect()
}

/// What granting `update` allows, or `None` when it allows nothing more.
fn label(update: &Value) -> Option<String> {
    match update.get("type")?.as_str()? {
        "addRules" if update.get("behavior")?.as_str()? == "allow" => {
            let rules: Vec<String> = update
                .get("rules")?
                .as_array()?
                .iter()
                .map(rule)
                .collect::<Option<_>>()?;
            (!rules.is_empty()).then(|| rules.join(", "))
        }
        "setMode" => match update.get("mode")?.as_str()? {
            "acceptEdits" => Some("file edits".to_owned()),
            "bypassPermissions" => Some("every tool".to_owned()),
            _ => None,
        },
        "addDirectories" => {
            let directories: Vec<&str> = update
                .get("directories")?
                .as_array()?
                .iter()
                .map(Value::as_str)
                .collect::<Option<_>>()?;
            (!directories.is_empty()).then(|| format!("access to {}", directories.join(", ")))
        }
        _ => None,
    }
}

/// One permission rule as the CLI writes it: `Tool` or `Tool(content)`.
fn rule(rule: &Value) -> Option<String> {
    let tool = rule.get("toolName")?.as_str()?;
    Some(match rule.get("ruleContent").and_then(Value::as_str) {
        Some(content) => format!("{tool}({content})"),
        None => tool.to_owned(),
    })
}

/// Where the CLI keeps the update: this process, the project's settings, or
/// the user's.
fn scope(update: &Value) -> Option<RuleScope> {
    match update.get("destination")?.as_str()? {
        "session" | "cliArg" => Some(RuleScope::Session),
        "localSettings" | "projectSettings" => Some(RuleScope::Project),
        "userSettings" => Some(RuleScope::User),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn labels(suggestions: Value) -> Vec<(String, RuleScope)> {
        offered(Some(&suggestions))
            .into_iter()
            .map(|(s, _)| (s.label, s.scope))
            .collect()
    }

    #[test]
    fn widening_updates_are_offered_with_their_scope() {
        let suggestions = json!([
            {"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"cargo test"},{"toolName":"Read"}],
             "behavior":"allow","destination":"localSettings"},
            {"type":"setMode","mode":"acceptEdits","destination":"session"},
            {"type":"setMode","mode":"bypassPermissions","destination":"cliArg"},
            {"type":"addDirectories","directories":["/srv/data"],"destination":"userSettings"},
            {"type":"addRules","rules":[{"toolName":"Write"}],"behavior":"allow","destination":"projectSettings"},
        ]);
        assert_eq!(
            labels(suggestions),
            [
                ("Bash(cargo test), Read".to_owned(), RuleScope::Project),
                ("file edits".to_owned(), RuleScope::Session),
                ("every tool".to_owned(), RuleScope::Session),
                ("access to /srv/data".to_owned(), RuleScope::User),
                ("Write".to_owned(), RuleScope::Project),
            ]
        );
    }

    #[test]
    fn narrowing_or_unknown_updates_are_not_offered() {
        let suggestions = json!([
            {"type":"addRules","rules":[{"toolName":"Bash"}],"behavior":"deny","destination":"session"},
            {"type":"addRules","rules":[],"behavior":"allow","destination":"session"},
            {"type":"removeRules","rules":[{"toolName":"Bash"}],"behavior":"allow","destination":"session"},
            {"type":"setMode","mode":"plan","destination":"session"},
            {"type":"setMode","mode":"acceptEdits","destination":"somewhereElse"},
            {"type":"addDirectories","directories":[],"destination":"session"},
            {"type":"somethingNew","destination":"session"},
            "not an object",
        ]);
        assert!(labels(suggestions).is_empty());
        assert!(offered(None).is_empty());
        assert!(offered(Some(&json!({"type":"setMode"}))).is_empty());
    }

    #[test]
    fn only_updates_the_process_keeps_are_kept_once_and_the_latest_mode_wins() {
        let edits = json!({"type":"setMode","mode":"acceptEdits","destination":"session"});
        let every = json!({"type":"setMode","mode":"bypassPermissions","destination":"cliArg"});
        let dir =
            json!({"type":"addDirectories","directories":["/srv/data"],"destination":"session"});
        let project = json!({"type":"addRules","rules":[{"toolName":"Read"}],"behavior":"allow","destination":"localSettings"});
        let mut kept = Vec::new();
        assert!(keep(&mut kept, &edits));
        assert!(keep(&mut kept, &dir));
        assert!(!keep(&mut kept, &edits));
        assert!(!keep(&mut kept, &project));
        assert!(keep(&mut kept, &every));
        assert_eq!(kept, [dir, every]);
    }

    #[test]
    fn kept_updates_become_cli_flags() {
        let kept = [
            json!({"type":"setMode","mode":"acceptEdits","destination":"session"}),
            json!({"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"cargo test"},{"toolName":"Read"}],
                   "behavior":"allow","destination":"session"}),
            json!({"type":"addDirectories","directories":["/srv/a","/srv/b"],"destination":"session"}),
            // Allows less: never granted again.
            json!({"type":"setMode","mode":"plan","destination":"session"}),
            json!({"type":"addRules","rules":[{"toolName":"Bash"}],"behavior":"deny","destination":"session"}),
        ];
        assert_eq!(
            cli_args(&kept),
            [
                "--permission-mode",
                "acceptEdits",
                "--allowedTools",
                "Bash(cargo test)",
                "--allowedTools",
                "Read",
                "--add-dir",
                "/srv/a",
                "--add-dir",
                "/srv/b",
            ]
        );
    }

    #[test]
    fn the_update_is_kept_verbatim() {
        let update = json!({"type":"setMode","mode":"acceptEdits","destination":"session"});
        let offered = offered(Some(&json!([update.clone()])));
        assert_eq!(offered[0].1, update);
    }
}
