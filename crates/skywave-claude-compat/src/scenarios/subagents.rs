//! Full scenarios that record how subagents behave beyond the basics
//! (docs/agents/claude-code.md §3.12, §3.13): a foreground helper moved to
//! the background or stopped, and each way of trying to message a
//! background one. Each recording is the fixture of the same name.

use serde_json::json;
use skywaved::agent::{AdapterEvent, AgentCallId};

use super::Ctx;
use super::tasks::{RECIPES, TASK, self_started_turn, subagent_started};
use super::turns::TURN;
use crate::report::{Stop, ensure};
use crate::session::{Session, Setup};

/// What every message to a subagent says; its prompt asks it to repeat it.
const WORD: &str = "The code word is ZEBRA.";

/// The end of each prompt to a subagent that may be sent [`WORD`].
const ANSWER: &str = "Then reply with exactly the code word somebody sent you while you \
                      worked, or NONE if nobody did.";

/// The ask that makes the model run a helper in the foreground on a chore
/// of a few steps.
const TASTING: &str = "Could you ask a helper agent to read my recipes one at a time and \
                       write a two-line tasting note for each? Call the Agent tool in the \
                       foreground (run_in_background false), subagent_type general-purpose. \
                       I'll wait for it.";

/// A foreground helper sent to the background once it has taken two steps
/// (Ctrl+B in the TUI: a `background_tasks` request naming its call).
pub fn subagent_foreground_backgrounded(ctx: &mut Ctx) -> Result<(), Stop> {
    let (mut s, call, task) = busy_helper(ctx)?;
    s.note("pressing Ctrl+B on the helper");
    // The set listing it comes before the request's answer.
    let from = s.mark();
    let id = s.control(json!({ "subtype": "background_tasks", "tool_use_id": call.0 }));
    s.response(&id, 30)?;
    let (end, _) = s.turn_end(from, TURN)?;
    let listed = s
        .find_event(from, |e| match e {
            AdapterEvent::BackgroundTasks(tasks) => tasks
                .iter()
                .any(|t| t.id == task && t.call.as_ref() == Some(&call))
                .then_some(()),
            _ => None,
        })
        .is_some();
    ctx.checks.contract(
        "a helper sent to the background enters the set with its call",
        ensure(listed, || "never listed".into()),
    );
    let notified = s.wait("the helper's task_notification", TASK, end, |x| {
        x.is("system/task_notification") && x.str("/task_id") == Some(&task)
    })?;
    self_started_turn(ctx, &mut s, notified)
}

/// A foreground helper stopped once it has taken two steps: its `Agent`
/// call fails and the turn goes on.
pub fn subagent_foreground_stop(ctx: &mut Ctx) -> Result<(), Stop> {
    let (mut s, call, task) = busy_helper(ctx)?;
    s.note("stopping the helper");
    let from = s.mark();
    let out = s.protocol.stop_task(task.clone());
    s.apply(out);
    let (end, _) = s.turn_end(from, TURN)?;
    let failed = s
        .find_event(from, |e| match e {
            AdapterEvent::ToolResult {
                call: Some(c),
                result,
            } if *c == call => Some(result.is_error),
            _ => None,
        })
        .is_some_and(|(at, error)| error && at < end);
    ctx.checks.contract(
        "a stopped foreground helper fails its Agent call",
        ensure(failed, || "no failed result for the Agent call".into()),
    );
    Ok(())
}

/// Starts the tasting chore and waits until its foreground helper has made
/// two tool calls: the session, the helper's call and its task id.
fn busy_helper(ctx: &mut Ctx) -> Result<(Session, AgentCallId, String), Stop> {
    let mut s = ctx.session(Setup {
        files: RECIPES.to_vec(),
        ..Setup::default()
    })?;
    let from = s.mark();
    s.send(TASTING);
    s.wait("the helper's task_started", TURN, from, |x| {
        x.is("system/task_started") && x.str("/task_type") == Some("local_agent")
    })?;
    let (call, task, backgrounded) = subagent_started(&s, from)?;
    if backgrounded {
        return Err(Stop::Inconclusive(
            "the model ran the helper in the background".into(),
        ));
    }
    let mut steps = 0;
    while steps < 2 {
        let at = s.mark();
        s.wait_event("a step of the helper", TURN, at, |e| match e {
            AdapterEvent::ToolCall {
                parent: Some(p), ..
            } if *p == call => Some(()),
            _ => None,
        })?;
        steps += 1;
    }
    Ok((s, call, task))
}

/// `send_task_message` while a background subagent works.
pub fn subagent_message_ctl(ctx: &mut Ctx) -> Result<(), Stop> {
    let (mut s, _, task, end) = background_subagent(ctx, &working())?;
    let answer = send_task_message(&mut s, &task)?;
    ctx.checks.contract(
        "send_task_message is refused while the subagent runs",
        ensure(answer["subtype"] == "error", || answer.to_string()),
    );
    to_the_end(ctx, &mut s, &task, end)
}

/// `send_task_message` after the background subagent finished.
pub fn subagent_message_ctl_done(ctx: &mut Ctx) -> Result<(), Stop> {
    let (mut s, _, task, end) = background_subagent(ctx, "Reply with exactly the word PONG.")?;
    let notified = s.wait("the subagent's task_notification", TASK, end, |x| {
        x.is("system/task_notification") && x.str("/task_id") == Some(&task)
    })?;
    self_started_turn(ctx, &mut s, notified)?;
    let answer = send_task_message(&mut s, &task)?;
    ctx.checks.contract(
        "send_task_message is refused after the subagent finished",
        ensure(answer["subtype"] == "error", || answer.to_string()),
    );
    Ok(())
}

/// A `user` line naming the subagent's `Agent` call as its
/// `parent_tool_use_id`, written raw: the main agent gets it.
pub fn subagent_message_ptu(ctx: &mut Ctx) -> Result<(), Stop> {
    let (mut s, call, task, end) = background_subagent(ctx, &working())?;
    s.note("writing a user line under the subagent's call");
    let from = s.mark();
    s.raw(json!({
        "type": "user",
        "message": { "role": "user", "content": WORD },
        "parent_tool_use_id": call.0,
        "session_id": "",
    }));
    let (opened, ()) = s.wait_event("the turn the line starts", TURN, from, |e| {
        matches!(e, AdapterEvent::TurnStarted { message: false }).then_some(())
    })?;
    s.turn_end(opened, TURN)?;
    to_the_end(ctx, &mut s, &task, end)
}

/// The relay to a background subagent that works by sleeping, so it may
/// finish first and be resumed by the message.
pub fn subagent_message_relay(ctx: &mut Ctx) -> Result<(), Stop> {
    relay(ctx, &working())
}

/// The relay to a background subagent busy with quick tool calls, so the
/// message lands mid-run.
pub fn subagent_message_relay_midrun(ctx: &mut Ctx) -> Result<(), Stop> {
    relay(
        ctx,
        &format!(
            "Run the Bash command `date +%N` ten times, one call at a time, each as its own \
             tool call. {ANSWER}"
        ),
    )
}

/// A main-turn message asking the main agent to `SendMessage` the
/// background subagent [`WORD`], then the run to the subagent's last
/// report.
fn relay(ctx: &mut Ctx, work: &str) -> Result<(), Stop> {
    let (mut s, _, task, _) = background_subagent(ctx, work)?;
    let from = s.mark();
    s.send(&format!(
        "Use SendMessage to tell the background agent {task} exactly: '{WORD}' Then reply SENT."
    ));
    let (end, _) = s.turn_end(from, TURN)?;
    to_the_end(ctx, &mut s, &task, end)
}

/// The prompt of a subagent that works for a while, in shells the CLI
/// lets it run (it refuses a bare `sleep`).
fn working() -> String {
    format!(
        "Run exactly the shell command `python3 -c \"import time; time.sleep(20)\"` with the \
         Bash tool in the foreground, then run it once more. {ANSWER}"
    )
}

/// Starts a background subagent with prompt `work` and waits for the turn
/// that started it: the session, its call, its task id and the index of
/// the turn's end.
fn background_subagent(
    ctx: &mut Ctx,
    work: &str,
) -> Result<(Session, AgentCallId, String, usize), Stop> {
    let mut s = ctx.session(Setup::default())?;
    let from = s.mark();
    s.send(&format!(
        "Use the Agent tool (subagent_type general-purpose, in the background) with this exact \
         prompt: '{work}' Do not wait for it. Reply STARTED."
    ));
    let (end, _) = s.turn_end(from, TURN)?;
    let (call, task, backgrounded) = subagent_started(&s, from)?;
    if !backgrounded {
        return Err(Stop::Inconclusive(
            "the model ran the subagent in the foreground".into(),
        ));
    }
    Ok((s, call, task, end))
}

/// Writes `send_task_message` to `task`, raw (the adapter never sends it):
/// the CLI's `response`.
fn send_task_message(s: &mut Session, task: &str) -> Result<serde_json::Value, Stop> {
    let id = s.control(json!({ "subtype": "send_task_message", "task_id": task, "message": WORD }));
    s.response(&id, 30)
}

/// Waits from index `from` for the subagent's last report: its
/// `task_notification` once no run of it is left, then the turn the CLI
/// starts for it. The subagent's answer is noted.
fn to_the_end(ctx: &mut Ctx, s: &mut Session, task: &str, from: usize) -> Result<(), Stop> {
    let notified = s.wait("the subagent's task_notification", 2 * TASK, from, |x| {
        x.is("system/task_notification") && x.str("/task_id") == Some(task)
    })?;
    let summary = s.seen[notified].str("/summary").unwrap_or("").to_owned();
    s.note(&format!("the subagent reported: {summary}"));
    self_started_turn(ctx, s, notified)
}
