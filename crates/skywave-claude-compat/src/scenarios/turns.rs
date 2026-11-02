//! Full scenarios about turns: messages, approvals, interrupts.

use skywave_proto::Decision;
use skywaved::agent::claude_code::protocol::INTERRUPT_REASON;
use skywaved::agent::{
    AdapterEvent, AgentCallId, AgentRequestId, RuleScope, RuleSuggestion, TurnOutcome,
};

use super::Ctx;
use crate::report::{Stop, ensure};
use crate::session::{Session, Setup};

/// How long one turn may take.
pub(super) const TURN: u64 = 120;

pub fn basic(ctx: &mut Ctx) -> Result<(), Stop> {
    let mut s = ctx.session(Setup::default())?;
    let from = s.mark();
    s.send(
        "Good morning! The bakery opens in five minutes and I'm testing the shop bell. When \
         it dings, you say PONG. Just PONG, nothing else. *ding*",
    );
    let (end, outcome) = s.turn_end(from, TURN)?;
    let checks = &mut ctx.checks;
    checks.contract("the turn completes", completed(&outcome));
    let id = s.protocol.session_id().to_string();
    let init = s.find(from, |x| x.is("system/init"));
    checks.contract(
        "system/init carries the session id",
        ensure(
            init.is_some_and(|at| s.seen[at].str("/session_id") == Some(&id)),
            || format!("init at {init:?}"),
        ),
    );
    let text = s.find_event(from, |e| match e {
        AdapterEvent::Text { parent: None, .. } => Some(()),
        _ => None,
    });
    checks.contract(
        "the model's text is read",
        ensure(text.is_some(), || "no text".into()),
    );
    let hooks: Vec<usize> = (from..end)
        .filter(|&at| s.seen[at].is_request("hook_callback"))
        .collect();
    let schedules = s
        .find_event(from, |e| {
            matches!(e, AdapterEvent::Schedules(_)).then_some(())
        })
        .is_some_and(|(at, ())| at < end);
    checks.contract(
        "the Stop hook is called once, before the result",
        ensure(hooks.len() == 1 && schedules, || {
            format!("{} hook calls, schedules read: {schedules}", hooks.len())
        }),
    );
    checks.contract(
        "the result reports usage",
        ensure(
            s.seen[end]
                .events
                .iter()
                .any(|e| matches!(e, AdapterEvent::TurnUsage(_))),
            || "no usage".into(),
        ),
    );
    let from = s.mark();
    s.send("Perfect. Now the back-door bell has a deeper voice. Say just PING. *dong*");
    let (_, outcome) = s.turn_end(from, TURN)?;
    ctx.checks.contract(
        "a second turn in the same process completes",
        completed(&outcome),
    );
    Ok(())
}

pub fn permissions(ctx: &mut Ctx) -> Result<(), Stop> {
    // `local`: the CLI keeps a project rule in .claude/settings.local.json
    // (docs/agents/claude-code.md §3.5).
    let mut s = ctx.session(Setup {
        args: vec!["--setting-sources".into(), "project,local".into()],
        ..Setup::default()
    })?;
    s.auto_approve = false;

    let from = s.mark();
    s.send(
        "Could you stick a note on the fridge for the morning baker? Use the Write tool to \
         create fridge-note.txt containing exactly: the croissants are in the left oven",
    );
    let write = asked(&mut s, from, "Write")?;
    ctx.checks.contract(
        "can_use_tool names its tool call",
        ensure(write.call.is_some(), || "no tool_use_id".into()),
    );
    answer(&mut s, &write.request, Decision::Approve, None);
    s.turn_end(from, TURN)?;
    ctx.checks.contract(
        "an allowed call runs",
        ran(&s, from, &write.call, false).and_then(|()| {
            ensure(s.workdir().join("fridge-note.txt").exists(), || {
                "fridge-note.txt was not written".into()
            })
        }),
    );

    // The user says no to their own idea: the approval is denied.
    let from = s.mark();
    s.send(
        "Hmm, the cat keeps sitting on that fridge. Take the note down: use the Bash tool to \
         run exactly: rm fridge-note.txt",
    );
    let rm = asked(&mut s, from, "Bash")?;
    answer(&mut s, &rm.request, Decision::Deny, None);
    let (_, outcome) = s.turn_end(from, TURN)?;
    ctx.checks.contract(
        "a denied call ends in an error result, and the turn goes on",
        ran(&s, from, &rm.call, true)
            .and_then(|()| completed(&outcome))
            .and_then(|()| {
                ensure(s.workdir().join("fridge-note.txt").exists(), || {
                    "fridge-note.txt was removed".into()
                })
            }),
    );

    let from = s.mark();
    s.send(
        "Never mind, the cat can share. Different job: the oven timer counts by little \
         marker files. Use the Bash tool to run exactly: touch oven-timer",
    );
    let touch = asked(&mut s, from, "Bash")?;
    let Some(rule) = touch
        .suggestions
        .iter()
        .position(|r| r.scope != RuleScope::User && r.label.starts_with("Bash("))
    else {
        answer(&mut s, &touch.request, Decision::Approve, None);
        return Err(Stop::Inconclusive(format!(
            "no Bash rule offered: {:?}",
            touch.suggestions
        )));
    };
    s.note(&format!("granting {:?}", touch.suggestions[rule]));
    answer(
        &mut s,
        &touch.request,
        Decision::Approve,
        Some(u8::try_from(rule).unwrap_or(u8::MAX)),
    );
    s.turn_end(from, TURN)?;

    let from = s.mark();
    s.send(
        "The scones want another round. Use the Bash tool to run exactly the same command \
         again: touch oven-timer",
    );
    let again = next_approval(&mut s, from)?;
    if let Some(again) = &again {
        answer(&mut s, &again.request, Decision::Approve, None);
    }
    let (end, _) = s.turn_end(from, TURN)?;
    let called = s
        .find_event(from, |e| match e {
            AdapterEvent::ToolCall { name, .. } if name == "Bash" => Some(()),
            _ => None,
        })
        .is_some_and(|(at, ())| at <= end);
    if !called {
        return Err(Stop::Inconclusive(
            "the model did not run Bash again".into(),
        ));
    }
    ctx.checks.contract(
        "a granted rule spares the next call its approval",
        ensure(again.is_none(), || "the call was asked again".into()),
    );
    if again.is_none() {
        ctx.checks.watch(
            "the always-allow reply shape is unconfirmed",
            "d29758: record rules_granted.ndjson and mark §3.2 [C] (docs/agents/claude-code.md §3.5)",
            Err("the CLI applied the granted rule".into()),
        );
    }
    Ok(())
}

pub fn interrupt_pending(ctx: &mut Ctx) -> Result<(), Stop> {
    let mut s = ctx.session(Setup::default())?;
    s.auto_approve = false;
    let start = s.mark();
    s.send(
        "Let's start a diary for the sourdough starter, he deserves one. Use the Write tool \
         to create starter-diary.txt containing exactly: day one, very bubbly",
    );
    let write = asked(&mut s, start, "Write")?;
    // The user wanders off to serve a customer: the turn is interrupted
    // while the approval waits.
    let from = s.mark();
    let out = s.protocol.interrupt();
    s.apply(out);
    let (end, outcome) = s.turn_end(from, TURN)?;
    let checks = &mut ctx.checks;
    checks.contract(
        "the turn ends interrupted",
        ensure(outcome == TurnOutcome::Interrupted, || {
            format!("{outcome:?}")
        }),
    );
    let cancel = s.find(from, |x| {
        x.is("control_cancel_request") && x.str("/request_id") == Some(&write.request.0)
    });
    checks.contract(
        "the waiting can_use_tool gets a control_cancel_request",
        ensure(cancel.is_some_and(|at| at < end), || {
            "none before the result".into()
        }),
    );
    let cancelled = s.find_event(from, |e| match e {
        AdapterEvent::ApprovalCancelled { request, reason } if *request == write.request => {
            Some(reason.clone())
        }
        _ => None,
    });
    checks.contract(
        "the approval is cancelled as interrupted",
        match cancelled {
            Some((_, reason)) => ensure(reason == INTERRUPT_REASON, || reason),
            None => Err("not cancelled".into()),
        },
    );
    checks.contract(
        "the result's terminal_reason starts with aborted",
        ensure(
            s.seen[end]
                .str("/terminal_reason")
                .is_some_and(|r| r.starts_with("aborted")),
            || format!("{:?}", s.seen[end].str("/terminal_reason")),
        ),
    );
    checks.contract(
        "an interrupted turn calls no Stop hook",
        ensure(
            !(start..end).any(|at| s.seen[at].is_request("hook_callback")),
            || "a hook_callback came".into(),
        ),
    );
    checks.contract(
        "the cancelled call wrote nothing",
        ensure(!s.workdir().join("starter-diary.txt").exists(), || {
            "starter-diary.txt exists".into()
        }),
    );
    let from = s.mark();
    s.send(
        "Sorry, a customer wanted forty-two macarons. Forget the diary for now. Are you \
         still there? Say just PONG.",
    );
    let (_, outcome) = s.turn_end(from, TURN)?;
    ctx.checks.contract(
        "the process takes a turn after the interrupt",
        completed(&outcome),
    );
    Ok(())
}

/// An approval as the adapter reported it.
struct Approval {
    pub request: AgentRequestId,
    pub call: Option<AgentCallId>,
    pub tool: String,
    pub suggestions: Vec<RuleSuggestion>,
}

/// The next approval from index `from` on, or `None` when the turn ends
/// first.
fn next_approval(s: &mut Session, from: usize) -> Result<Option<Approval>, Stop> {
    let (_, approval) = s.wait_event("approval or end of the turn", TURN, from, |e| match e {
        AdapterEvent::ApprovalRequested {
            request,
            call,
            tool,
            suggestions,
            ..
        } => Some(Some(Approval {
            request: request.clone(),
            call: call.clone(),
            tool: tool.clone(),
            suggestions: suggestions.clone(),
        })),
        AdapterEvent::TurnEnded(_) => Some(None),
        _ => None,
    })?;
    Ok(approval)
}

/// The next approval, which must be for `tool`; the scenario is
/// inconclusive when the model did something else.
fn asked(s: &mut Session, from: usize, tool: &str) -> Result<Approval, Stop> {
    match next_approval(s, from)? {
        Some(approval) if approval.tool == tool => Ok(approval),
        Some(other) => {
            answer(s, &other.request, Decision::Deny, None);
            Err(Stop::Inconclusive(format!(
                "the model asked for {} instead of {tool}",
                other.tool
            )))
        }
        None => Err(Stop::Inconclusive(format!(
            "the turn ended without a {tool} call to approve"
        ))),
    }
}

fn answer(s: &mut Session, request: &AgentRequestId, decision: Decision, rule: Option<u8>) {
    let out = s.protocol.answer(request, decision, rule);
    s.apply(out);
}

/// Whether call `call`'s result came after index `from`, an error or not
/// as `error` says.
fn ran(s: &Session, from: usize, call: &Option<AgentCallId>, error: bool) -> Result<(), String> {
    let result = s.find_event(from, |e| match e {
        AdapterEvent::ToolResult { call: c, result } if c == call => Some(result.is_error),
        _ => None,
    });
    match result {
        Some((_, is_error)) => ensure(is_error == error, || {
            format!("the result's is_error is {is_error}")
        }),
        None => Err("no tool_result for the call".into()),
    }
}

pub(super) fn completed(outcome: &TurnOutcome) -> Result<(), String> {
    ensure(*outcome == TurnOutcome::Completed, || {
        format!("{outcome:?}")
    })
}
