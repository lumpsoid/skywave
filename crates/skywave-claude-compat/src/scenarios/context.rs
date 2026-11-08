//! Full scenarios that record what the CLI reports about how full a
//! context is (docs/agents/claude-code.md §2.6): the last request's usage
//! in each `result`, the window of the turn's model, a compaction made by
//! hand and one the CLI makes itself, and a subagent's own figures when it
//! ends. The main agent runs on opus and subagents on sonnet, so results
//! name two models. Each recording is the fixture of the same name.
//!
//! The CLI's own count (`get_context_usage`, the data behind `/context`) is
//! asked after each turn, written raw: the adapter does not send it, and
//! the scenarios use it only to check the figure the `result` gives.

use serde_json::{Value, json};

use super::Ctx;
use super::tasks::{TASK, self_started_turn, subagent_started};
use super::turns::{TURN, completed};
use crate::report::{Stop, ensure};
use crate::session::{Session, Setup};

/// The main agent's model.
const MAIN: &str = "claude-opus-5-5";

/// What the subagents' `resolvedModel` names.
const SUB: &str = "sonnet";

/// The auto-compact threshold, in percent of the window, that
/// [`context_autocompact`] sets: about 30k tokens of opus's 1M, which three
/// long tool outputs cross.
const AUTOCOMPACT_PCT: &str = "3";

/// How far the CLI's own count may be from the `result`'s figure. Both
/// read the same request; the CLI adds its own estimate of what came after
/// it (tens of tokens in every recording).
const SLACK: u64 = 500;

fn on_main() -> Setup {
    Setup {
        args: vec!["--model".into(), MAIN.into()],
        ..Setup::default()
    }
}

/// Three turns, a `/compact`, and a turn after it. Its recording is the
/// fixture `context_fill.ndjson`.
pub fn context_fill(ctx: &mut Ctx) -> Result<(), Stop> {
    let mut s = ctx.session(on_main())?;
    measured(
        ctx,
        &mut s,
        "Good morning! I'm testing the shop bell: when it dings, say only PONG. *ding*",
    )?;
    let (end, _) = measured(
        ctx,
        &mut s,
        "Check the ovens for me: run `echo oven one` with Bash, then in a separate Bash call \
         run `echo oven two`, then reply only CHECKED.",
    )?;
    let several = s.seen[end].msg["num_turns"].as_u64().unwrap_or(0) > 1;
    let summed = fill(&s.seen[end].msg["usage"]);
    let last = last_request(&s.seen[end].msg);
    ctx.checks.contract(
        "usage sums the turn's requests; iterations holds only the last",
        match (summed, &last) {
            (Some(summed), Ok(last)) if several => {
                ensure(summed > *last, || format!("summed {summed}, last {last}"))
            }
            _ => Err(format!(
                "num_turns {}, usage {summed:?}, last {last:?}",
                s.seen[end].msg["num_turns"]
            )),
        },
    );
    let before = last.ok();

    s.note("step: /compact");
    let from = s.mark();
    s.send("/compact");
    let (end, outcome) = s.turn_end(from, TURN)?;
    let checks = &mut ctx.checks;
    checks.contract("the /compact turn completes", completed(&outcome));
    let boundary = s.find(from, |x| x.is("system/compact_boundary"));
    checks.contract(
        "/compact sends compact_boundary, trigger manual, before its result",
        match boundary {
            Some(at) => ensure(
                at < end && s.seen[at].str("/compact_metadata/trigger") == Some("manual"),
                || format!("at {at}, result at {end}: {}", s.seen[at].msg),
            ),
            None => Err("no compact_boundary".into()),
        },
    );
    if let Some(at) = boundary {
        let pre = s.seen[at].msg["compact_metadata"]["pre_tokens"].as_u64();
        checks.contract(
            "compact_boundary's pre_tokens is the last request's fill",
            ensure(pre.is_some() && pre == before, || {
                format!("pre_tokens {pre:?}, the last result's {before:?}")
            }),
        );
    }
    let iterations = s.seen[end].msg["usage"]["iterations"]
        .as_array()
        .map(Vec::len);
    checks.contract(
        "the /compact result reports no request",
        ensure(iterations.unwrap_or(0) == 0, || {
            format!("{iterations:?} iterations")
        }),
    );

    measured(
        ctx,
        &mut s,
        "What did I ask you to test first this morning? One short sentence.",
    )?;
    every_result(ctx, &s);
    Ok(())
}

/// Long tool outputs with the auto-compact threshold lowered, until the
/// CLI compacts on its own, then one more turn. Its recording is the
/// fixture `context_autocompact.ndjson`.
pub fn context_autocompact(ctx: &mut Ctx) -> Result<(), Stop> {
    let mut s = ctx.session(Setup {
        env: vec![("CLAUDE_AUTOCOMPACT_PCT_OVERRIDE", AUTOCOMPACT_PCT)],
        ..on_main()
    })?;
    let mut before = None;
    let mut compacted = None;
    for (first, last) in [(1, 2500), (2501, 5000), (5001, 7500), (7501, 10000)] {
        let from = s.mark();
        let (end, fill) = measured(
            ctx,
            &mut s,
            &format!(
                "Number the loaves for the inventory: run `seq {first} {last}` with Bash, then \
                 reply only COUNTED."
            ),
        )?;
        if let Some(at) = s.find(from, |x| x.is("system/compact_boundary")) {
            compacted = Some((from, at, end, fill));
            break;
        }
        before = Some(fill);
    }
    let Some((from, at, end, after)) = compacted else {
        return Err(Stop::Inconclusive(
            "the CLI never compacted on its own".into(),
        ));
    };
    let checks = &mut ctx.checks;
    let boundary = &s.seen[at].msg["compact_metadata"];
    checks.contract(
        "the CLI's own compaction is trigger auto",
        ensure(boundary["trigger"] == "auto", || boundary.to_string()),
    );
    let init = s.find(from, |x| x.is("system/init"));
    let request = s.find(from, |x| {
        x.is("assistant") && x.msg["parent_tool_use_id"].is_null()
    });
    checks.contract(
        "it comes inside the turn, after its init and before its first request",
        ensure(
            init.is_some_and(|i| i < at) && request.is_some_and(|r| at < r && r < end),
            || format!("init {init:?}, boundary {at}, request {request:?}, result {end}"),
        ),
    );
    // It counts the new message too (tens of tokens past the last fill in
    // every recording), where /compact's equals the last fill.
    let pre = boundary["pre_tokens"].as_u64();
    checks.contract(
        "its pre_tokens is the last request's fill and the new message",
        ensure(
            matches!((pre, before), (Some(p), Some(b)) if p >= b && p - b <= SLACK),
            || format!("pre_tokens {pre:?}, the last result's {before:?}"),
        ),
    );
    checks.contract(
        "the turn's result reports the fill after the compaction",
        ensure(pre.is_some_and(|pre| after < pre), || {
            format!("pre_tokens {pre:?}, after {after}")
        }),
    );
    measured(
        ctx,
        &mut s,
        "Which numbers did you count first? One short sentence.",
    )?;
    every_result(ctx, &s);
    Ok(())
}

/// A foreground subagent on sonnet, then two background ones at once.
/// Its recording is the fixture `subagent_context.ndjson`.
pub fn subagent_context(ctx: &mut Ctx) -> Result<(), Stop> {
    let mut s = ctx.session(on_main())?;
    let from = s.mark();
    let (end, _) = measured(
        ctx,
        &mut s,
        "Could a helper check the flour? Call the Agent tool in the foreground \
         (run_in_background false), subagent_type general-purpose, model sonnet, prompt \
         \"Run `echo flour ok` with Bash and reply with its output.\" I'll wait; then reply \
         only CHECKED.",
    )?;
    let (call, task, backgrounded) = subagent_started(&s, from)?;
    if backgrounded {
        return Err(Stop::Inconclusive(
            "the model ran the helper in the background".into(),
        ));
    }
    let result = agent_result(&s, from, &call.0)
        .ok_or_else(|| Stop::Stalled("no tool_result for the Agent call".into()))?;
    if !resolved_sub(result) {
        return Err(Stop::Inconclusive(format!(
            "the helper ran on {}",
            result["resolvedModel"]
        )));
    }
    let checks = &mut ctx.checks;
    let iterations = &result["usage"]["iterations"];
    let last = iterations
        .as_array()
        .filter(|i| i.len() == 1)
        .and_then(|i| fill(&i[0]));
    checks.contract(
        "a foreground Agent result's totalTokens is its last request's fill",
        ensure(
            last.is_some() && result["totalTokens"].as_u64() == last,
            || {
                format!(
                    "totalTokens {}, iterations {iterations}",
                    result["totalTokens"]
                )
            },
        ),
    );
    checks.contract(
        "a foreground Agent result carries totalDurationMs and totalToolUseCount",
        ensure(
            result["totalDurationMs"].is_u64() && result["totalToolUseCount"].is_u64(),
            || result.to_string(),
        ),
    );
    let notified = s.find(from, |x| {
        x.is("system/task_notification") && x.str("/task_id") == Some(&task)
    });
    checks.contract(
        "a completed foreground subagent's task_notification carries its usage",
        match notified {
            Some(at) => run_usage(&s.seen[at].msg),
            None => Err("no task_notification".into()),
        },
    );
    let models = &s.seen[end].msg["model_usage_since_last_result"];
    checks.contract(
        "the turn's result names the subagent's model beside the main one",
        ensure(
            models.get(MAIN).is_some()
                && models
                    .as_object()
                    .is_some_and(|m| m.keys().any(|k| k.contains(SUB))),
            || models.to_string(),
        ),
    );

    s.note("step: two background helpers at once");
    let from = s.mark();
    s.send(
        "Now two helpers at once, please. In one message make two Agent tool calls, both with \
         run_in_background true, subagent_type general-purpose, model sonnet: the first with \
         prompt \"Run `echo left oven ok` with Bash and reply with its output.\", the second \
         with prompt \"Run `echo right oven ok` with Bash and reply with its output.\" Then \
         reply only LAUNCHED. When they are back, say only NOTED.",
    );
    s.turn_end(from, TURN)?;
    let tasks: Vec<(String, String)> = s.seen[from..]
        .iter()
        .filter(|x| x.is("system/task_started") && x.str("/task_type") == Some("local_agent"))
        .filter_map(|x| {
            Some((
                x.str("/tool_use_id")?.to_owned(),
                x.str("/task_id")?.to_owned(),
            ))
        })
        .collect();
    if tasks.len() != 2 {
        return Err(Stop::Inconclusive(format!(
            "the model started {} helpers",
            tasks.len()
        )));
    }
    for (call, _) in &tasks {
        let launched = agent_result(&s, from, call).cloned().unwrap_or_default();
        if launched["status"] != "async_launched" || !resolved_sub(&launched) {
            return Err(Stop::Inconclusive(format!(
                "a helper was not launched in the background on {SUB}: {launched}"
            )));
        }
    }
    ctx.checks.contract(
        "a background Agent result names its model and carries no figures",
        ensure(
            tasks.iter().all(|(call, _)| {
                agent_result(&s, from, call)
                    .is_some_and(|r| r.get("totalTokens").is_none() && r.get("usage").is_none())
            }),
            || "a launch result carries figures".into(),
        ),
    );
    let mut first = None;
    for (_, task) in &tasks {
        let at = s.wait("a helper's task_notification", TASK, from, |x| {
            x.is("system/task_notification") && x.str("/task_id") == Some(task.as_str())
        })?;
        first = Some(first.map_or(at, |f: usize| f.min(at)));
        ctx.checks.contract(
            "each background subagent's task_notification carries its own usage",
            run_usage(&s.seen[at].msg),
        );
    }
    self_started_turn(ctx, &mut s, first.expect("two tasks"))?;
    if s.protocol.turn_open() {
        s.turn_end(s.mark(), TURN)?;
    }
    every_result(ctx, &s);
    Ok(())
}

/// Sends `message`, waits for its turn's end and checks the context figures
/// of its `result`: the turn's end index and the last request's fill.
fn measured(ctx: &mut Ctx, s: &mut Session, message: &str) -> Result<(usize, u64), Stop> {
    let from = s.mark();
    s.send(message);
    let (end, outcome) = s.turn_end(from, 2 * TURN)?;
    let checks = &mut ctx.checks;
    checks.contract("the turn completes", completed(&outcome));
    let model = init_model(s, end);
    checks.contract(
        "the turn runs on the main model",
        ensure(model.as_deref() == Some(MAIN), || format!("{model:?}")),
    );
    let result = &s.seen[end].msg;
    let last = last_request(result);
    checks.contract(
        "a result's iterations hold one request",
        last.clone().map(|_| ()),
    );
    let window = model.as_deref().and_then(|m| window(result, m));
    checks.contract(
        "a result gives the window of the turn's model",
        ensure(window.is_some(), || {
            format!("{}", result["model_usage_since_last_result"])
        }),
    );
    let fill = last.map_err(Stop::Stalled)?;
    let id = s.control(json!({ "subtype": "get_context_usage" }));
    let answer = s.response(&id, 60)?;
    let total = answer
        .pointer("/response/totalTokens")
        .and_then(Value::as_u64);
    let max = answer
        .pointer("/response/maxTokens")
        .and_then(Value::as_u64);
    ctx.checks.contract(
        "the CLI's own count agrees with the last request's fill",
        ensure(total.is_some_and(|t| t.abs_diff(fill) <= SLACK), || {
            format!("get_context_usage {total:?}, the result's {fill}")
        }),
    );
    ctx.checks.contract(
        "the CLI's own maximum is the result's window",
        ensure(max.is_some() && max == window, || {
            format!("maxTokens {max:?}, contextWindow {window:?}")
        }),
    );
    Ok((end, fill))
}

/// Every `result` that reports a request gives the window of its turn's
/// model, turns the CLI starts itself included: their
/// `model_usage_since_last_result` may list a subagent's model first.
fn every_result(ctx: &mut Ctx, s: &Session) {
    let missing: Vec<String> = (0..s.seen.len())
        .filter(|&at| s.seen[at].is("result") && last_request(&s.seen[at].msg).is_ok())
        .filter_map(|at| {
            let model = init_model(s, at);
            let found = model.as_deref().and_then(|m| window(&s.seen[at].msg, m));
            found.is_none().then(|| format!("result {at} on {model:?}"))
        })
        .collect();
    ctx.checks.contract(
        "every result's window is found under its turn's system/init model",
        ensure(missing.is_empty(), || missing.join(", ")),
    );
}

/// The `model` of the last `system/init` before index `at`.
fn init_model(s: &Session, at: usize) -> Option<String> {
    s.seen[..at]
        .iter()
        .rev()
        .find(|x| x.is("system/init"))
        .and_then(|x| x.str("/model"))
        .map(str::to_owned)
}

/// The fill of a `result`'s single request in `usage.iterations`.
fn last_request(result: &Value) -> Result<u64, String> {
    match result["usage"]["iterations"].as_array().map(Vec::as_slice) {
        Some([one]) => fill(one).ok_or_else(|| format!("unreadable iteration {one}")),
        other => Err(format!("iterations {other:?}")),
    }
}

/// `contextWindow` of `model` in a result's `model_usage_since_last_result`.
fn window(result: &Value, model: &str) -> Option<u64> {
    result["model_usage_since_last_result"][model]["contextWindow"].as_u64()
}

/// What a request's `usage` adds up to: everything the model read, and
/// what it wrote, which the next request reads.
fn fill(usage: &Value) -> Option<u64> {
    [
        "input_tokens",
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
        "output_tokens",
    ]
    .iter()
    .map(|k| usage[k].as_u64())
    .sum()
}

/// The `tool_use_result` of the main thread's result for call `call`.
fn agent_result<'a>(s: &'a Session, from: usize, call: &str) -> Option<&'a Value> {
    s.seen[from..].iter().find_map(|x| {
        let answers = x.is("user")
            && x.msg["parent_tool_use_id"].is_null()
            && x.msg["message"]["content"]
                .as_array()
                .is_some_and(|c| c.iter().any(|b| b["tool_use_id"] == call));
        answers.then_some(&x.msg["tool_use_result"])
    })
}

fn resolved_sub(result: &Value) -> bool {
    result["resolvedModel"]
        .as_str()
        .is_some_and(|m| m.contains(SUB))
}

/// A completed `local_agent` notification's `usage`.
fn run_usage(msg: &Value) -> Result<(), String> {
    let usage = &msg["usage"];
    ensure(
        msg["status"] == "completed"
            && ["total_tokens", "tool_uses", "duration_ms"]
                .iter()
                .all(|k| usage[k].is_u64()),
        || msg.to_string(),
    )
}
