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
    fn the_update_is_kept_verbatim() {
        let update = json!({"type":"setMode","mode":"acceptEdits","destination":"session"});
        let offered = offered(Some(&json!([update.clone()])));
        assert_eq!(offered[0].1, update);
    }
}
