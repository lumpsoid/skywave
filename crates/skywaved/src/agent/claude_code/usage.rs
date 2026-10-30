//! What a Claude Code turn took, read off its `result` message as
//! agent-agnostic per-turn [`TurnUsage`] (docs/agents/claude-code.md §2.5,
//! D3).
//!
//! The figures are the turn's own, never running totals:
//! - time: `duration_ms`, the turn's wall clock;
//! - tokens: `usage`, summed over the turn's requests; read in counts the
//!   cached input too (`cache_read_input_tokens`,
//!   `cache_creation_input_tokens`), since that is most of what a turn reads;
//! - cost: the `costUSD` of each model in `model_usage_since_last_result`,
//!   summed. `total_cost_usd` is not used: it runs over the whole CLI
//!   session, and a `--resume`d process carries on from the stored total, so
//!   no delta taken within one process is safe for its first turn;
//! - model: the one `system/init` named for the turn.
//!
//! A turn the CLI did not measure (an interrupt while text streamed) reports
//! zero tokens and no models; those figures are left out, not logged as 0.

use serde_json::Value;

use crate::agent::TurnUsage;

/// The usage `result` reports for the turn it ends; `model` is the one the
/// turn's `system/init` named.
pub fn turn_usage(result: &Value, model: Option<&str>) -> TurnUsage {
    let (tokens_in, tokens_out) = tokens(result.get("usage"));
    TurnUsage {
        duration_ms: result.get("duration_ms").and_then(Value::as_u64),
        tokens_in,
        tokens_out,
        cost_usd: cost(result.get("model_usage_since_last_result")),
        model: model.filter(|m| !m.is_empty()).map(str::to_owned),
    }
}

/// Tokens read (cache included) and written; neither when the turn counted
/// none at all.
fn tokens(usage: Option<&Value>) -> (Option<u64>, Option<u64>) {
    let Some(usage) = usage.filter(|u| u.is_object()) else {
        return (None, None);
    };
    let count = |key| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    let read = [
        "input_tokens",
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
    ]
    .into_iter()
    .fold(0u64, |sum, key| sum.saturating_add(count(key)));
    let written = count("output_tokens");
    if read == 0 && written == 0 {
        return (None, None);
    }
    (Some(read), Some(written))
}

/// The turn's cost over every model it used; `None` when no model reported
/// one.
fn cost(models: Option<&Value>) -> Option<f64> {
    let costs: Vec<f64> = models?
        .as_object()?
        .values()
        .filter_map(|model| model.get("costUSD").and_then(Value::as_f64))
        .collect();
    (!costs.is_empty()).then(|| costs.iter().sum())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn reads_the_turn_figures_and_sums_models() {
        let result = json!({
            "duration_ms": 1255,
            "total_cost_usd": 0.0023,
            "usage": {
                "input_tokens": 2,
                "cache_read_input_tokens": 15834,
                "cache_creation_input_tokens": 3501,
                "output_tokens": 17
            },
            "model_usage_since_last_result": {
                "model-a": { "costUSD": 0.0008 },
                "model-b": { "costUSD": 0.0002 }
            }
        });
        let usage = turn_usage(&result, Some("model-a"));
        assert_eq!(usage.duration_ms, Some(1255));
        assert_eq!(usage.tokens_in, Some(19_337));
        assert_eq!(usage.tokens_out, Some(17));
        assert!((usage.cost_usd.unwrap() - 0.001).abs() < 1e-12);
        assert_eq!(usage.model.as_deref(), Some("model-a"));
    }

    #[test]
    fn an_unmeasured_turn_leaves_tokens_and_cost_out() {
        let result = json!({
            "duration_ms": 2590,
            "total_cost_usd": 0,
            "usage": { "input_tokens": 0, "output_tokens": 0 },
            "model_usage_since_last_result": {}
        });
        assert_eq!(
            turn_usage(&result, None),
            TurnUsage {
                duration_ms: Some(2590),
                ..TurnUsage::default()
            }
        );
        assert!(turn_usage(&json!({}), Some("")).is_empty());
    }
}
