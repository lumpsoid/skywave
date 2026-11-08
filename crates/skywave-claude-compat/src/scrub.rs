//! The fixtures' scrubbing (tests/fixtures/claude-code/README.md), applied
//! to a recording before it is written: what `record-claude.sh --scrub`
//! does, plus trimming `get_settings` answers to `response.applied` and
//! `get_usage` answers to the limits the adapter reads.

use std::collections::HashSet;

use serde_json::{Map, Value};

/// `records` (fixture records: `{dir, msg}` or `{dir: "note", text}`) with
/// the account, pid, `home` and thinking signatures scrubbed, every
/// `get_settings` answer cut to its `applied` settings, and every
/// `get_usage` answer to `rate_limits_available`, `rate_limits.limits` and
/// `behaviors` (the plan, spend and per-product shares go).
pub fn scrub(records: &[Value], home: &str) -> Vec<Value> {
    let asked = |subtype: &str| -> HashSet<&str> {
        records
            .iter()
            .filter(|r| r["dir"] == "in" && r["msg"]["request"]["subtype"] == subtype)
            .filter_map(|r| r["msg"]["request_id"].as_str())
            .collect()
    };
    let reads = asked("get_settings");
    let usage = asked("get_usage");
    records
        .iter()
        .map(|record| {
            let mut record = record.clone();
            if let Some(msg) = record.get_mut("msg")
                && msg["type"] == "control_response"
                && msg["response"]["request_id"]
                    .as_str()
                    .is_some_and(|id| reads.contains(id))
                && let Some(body) = msg["response"].get_mut("response")
                && let Some(applied) = body.get("applied").cloned()
            {
                *body = Value::Object(Map::from_iter([("applied".to_owned(), applied)]));
            }
            if let Some(msg) = record.get_mut("msg")
                && msg["type"] == "control_response"
                && msg["response"]["request_id"]
                    .as_str()
                    .is_some_and(|id| usage.contains(id))
                && let Some(body) = msg["response"].get_mut("response")
                && body.is_object()
            {
                let mut kept = Map::new();
                for key in ["rate_limits_available", "behaviors"] {
                    if let Some(value) = body.get(key) {
                        kept.insert(key.to_owned(), value.clone());
                    }
                }
                if let Some(limits) = body.pointer("/rate_limits/limits") {
                    let limits = Map::from_iter([("limits".to_owned(), limits.clone())]);
                    kept.insert("rate_limits".to_owned(), Value::Object(limits));
                }
                *body = Value::Object(kept);
            }
            walk(&mut record, home);
            record
        })
        .collect()
}

fn walk(value: &mut Value, home: &str) {
    match value {
        Value::String(text) if !home.is_empty() && text.contains(home) => {
            *text = text.replace(home, "$HOME");
        }
        Value::Array(items) => items.iter_mut().for_each(|v| walk(v, home)),
        Value::Object(fields) => {
            if fields.contains_key("signature") {
                fields.insert("signature".into(), "<scrubbed-signature>".into());
            }
            if fields.contains_key("pid") {
                fields.insert("pid".into(), 12345.into());
            }
            if let Some(Value::Object(account)) = fields.get_mut("account") {
                if account.contains_key("email") {
                    account.insert("email".into(), "user@example.com".into());
                }
                if account.contains_key("organization") {
                    account.insert("organization".into(), "<scrubbed-organization>".into());
                }
            }
            fields.values_mut().for_each(|v| walk(v, home));
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn scrubs_the_account_home_pid_signatures_and_settings() {
        let records = vec![
            json!({"dir": "in", "msg": {"type": "control_request", "request_id": "r1", "request": {"subtype": "get_settings"}}}),
            json!({"dir": "out", "msg": {"type": "control_response", "response": {"subtype": "success", "request_id": "r1", "response": {"applied": {"effort": "high"}, "effective": {"env": {"TOKEN": "x"}}, "sources": []}}}}),
            json!({"dir": "out", "msg": {"type": "control_response", "response": {"subtype": "success", "request_id": "r0", "response": {"pid": 77, "account": {"email": "a@b.c", "organization": "Org"}}}}}),
            json!({"dir": "out", "msg": {"type": "assistant", "message": {"content": [{"type": "thinking", "signature": "c2ln"}, {"type": "text", "text": "in /home/me/x"}]}}}),
            json!({"dir": "note", "text": "cwd /home/me/x"}),
        ];
        let scrubbed = scrub(&records, "/home/me");
        assert_eq!(
            scrubbed[1]["msg"]["response"]["response"],
            json!({"applied": {"effort": "high"}})
        );
        assert_eq!(
            scrubbed[2]["msg"]["response"]["response"],
            json!({"pid": 12345, "account": {"email": "user@example.com", "organization": "<scrubbed-organization>"}})
        );
        let content = &scrubbed[3]["msg"]["message"]["content"];
        assert_eq!(content[0]["signature"], "<scrubbed-signature>");
        assert_eq!(content[1]["text"], "in $HOME/x");
        assert_eq!(scrubbed[4], json!({"dir": "note", "text": "cwd $HOME/x"}));
    }

    #[test]
    fn keeps_only_the_limits_of_a_usage_answer() {
        let limits = json!([{"group": "session", "percent": 75}]);
        let records = vec![
            json!({"dir": "in", "msg": {"type": "control_request", "request_id": "u1", "request": {"subtype": "get_usage", "skip_behaviors": true}}}),
            json!({"dir": "out", "msg": {"type": "control_response", "response": {"subtype": "success", "request_id": "u1", "response": {
                "session": {"total_cost_usd": 0}, "subscription_type": "max", "rate_limits_available": true,
                "rate_limits": {"five_hour": {"utilization": 75}, "limits": limits.clone(), "spend": {"used": 1}},
                "behaviors": null,
            }}}}),
        ];
        assert_eq!(
            scrub(&records, "/home/me")[1]["msg"]["response"]["response"],
            json!({"rate_limits_available": true, "behaviors": null, "rate_limits": {"limits": limits}})
        );
    }
}
