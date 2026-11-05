//! Full scenarios about turns: messages, approvals, interrupts.

use serde_json::json;
use skywave_proto::Decision;
use skywaved::agent::claude_code::CliSession;
use skywaved::agent::claude_code::protocol::INTERRUPT_REASON;
use skywaved::agent::{
    AdapterEvent, AgentCallId, AgentRequestId, PermissionMode, RuleScope, RuleSuggestion,
    TurnOutcome,
};

use super::Ctx;
use super::controls::{expect_mode, set_mode};
use crate::report::{Stop, ensure};
use crate::session::{Session, Setup, Workdir};

/// How long one turn may take.
pub(super) const TURN: u64 = 120;

/// Two turns in one process, then a second process resuming the
/// conversation for a third. Its recording is the fixture `basic.ndjson`.
pub fn basic(ctx: &mut Ctx) -> Result<(), Stop> {
    let workdir = Workdir::new(None)?;
    let mut s = ctx.session(Setup {
        workdir: Some(workdir.clone()),
        ..Setup::default()
    })?;
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
    let id = s.protocol.session_id();
    drop(s);

    let mut s = ctx.session(Setup {
        session: Some(CliSession::Resume(id)),
        workdir: Some(workdir),
        ..Setup::default()
    })?;
    let from = s.mark();
    s.send(
        "Back after the lunch rush. Which two bells did we test this morning? Answer in one \
         short line.",
    );
    let (_, outcome) = s.turn_end(from, TURN)?;
    let init = s.find(from, |x| x.is("system/init"));
    let checks = &mut ctx.checks;
    checks.contract("a resumed process takes a turn", completed(&outcome));
    checks.contract(
        "a resumed process runs under the same session id",
        ensure(
            init.is_some_and(|at| s.seen[at].str("/session_id") == Some(&id.to_string())),
            || format!("init at {init:?}"),
        ),
    );
    Ok(())
}

/// With `--include-partial-messages`: an interrupt while text streams, a
/// turn whose tool runs unasked, an interrupt while idle (written raw: the
/// adapter sends none then), and a turn after it. Its recording is the
/// fixture `interrupt.ndjson` (docs/agents/claude-code.md §4).
pub fn interrupt(ctx: &mut Ctx) -> Result<(), Stop> {
    let mut s = ctx.session(Setup {
        args: vec!["--include-partial-messages".into()],
        ..Setup::default()
    })?;
    let from = s.mark();
    s.send(
        "The flour delivery is late, so let's pass the time: count from 1 to 60, one number \
         per line, each followed by a different pastry.",
    );
    s.wait("streamed text", TURN, from, |x| {
        x.is("stream_event") && x.str("/event/delta/type") == Some("text_delta")
    })?;
    s.note("interrupting while text streams");
    let out = s.protocol.interrupt();
    s.apply(out);
    let (end, outcome) = s.turn_end(from, TURN)?;
    ctx.checks.contract(
        "an interrupt while text streams ends the turn interrupted",
        ensure(outcome == TurnOutcome::Interrupted, || {
            format!("{outcome:?}")
        }),
    );
    ctx.checks.contract(
        "its result's terminal_reason is aborted_streaming",
        ensure(
            s.seen[end].str("/terminal_reason") == Some("aborted_streaming"),
            || format!("{:?}", s.seen[end].str("/terminal_reason")),
        ),
    );

    let from = s.mark();
    s.send("The delivery is here. Use the Bash tool to run exactly: echo flour-arrived");
    let (_, outcome) = s.turn_end(from, TURN)?;
    let asked = (from..s.mark()).any(|at| s.seen[at].is_request("can_use_tool"));
    ctx.checks.contract(
        "a read-only echo runs unasked",
        completed(&outcome).and_then(|()| ensure(!asked, || "it was asked".into())),
    );

    s.note("interrupting while idle");
    let id = s.control(json!({ "subtype": "interrupt" }));
    let answer = s.response(&id, 30)?;
    ctx.checks.contract(
        "an interrupt while idle is answered with success",
        ensure(answer["subtype"] == "success", || answer.to_string()),
    );

    let from = s.mark();
    s.send("All stocked up. Say just ok.");
    let (_, outcome) = s.turn_end(from, TURN)?;
    ctx.checks.contract(
        "the process takes a turn after the interrupts",
        completed(&outcome),
    );
    Ok(())
}

/// A `set_permission_mode` before turns, walking every mode, with each
/// tool call approved; the last switch comes while an approval waits. Its
/// recording is the fixture `permission_modes.ndjson`
/// (docs/agents/claude-code.md §3.7); each turn starts at a `step:` note.
pub fn permission_modes(ctx: &mut Ctx) -> Result<(), Stop> {
    let mut s = ctx.session(Setup::default())?;
    let write = |file: &str, text: &str| {
        format!("Use the Write tool to create {file} containing exactly: {text}")
    };
    let rm = |file: &str| format!("Use the Bash tool to run exactly: rm {file}");
    mode_step(&mut s, "default: Write", &write("menu-1.txt", "rye bread"))?;
    switch_mode(ctx, &mut s, PermissionMode::AcceptEdits)?;
    mode_step(
        &mut s,
        "acceptEdits: Write",
        &write("menu-2.txt", "sourdough"),
    )?;
    mode_step(&mut s, "acceptEdits: Bash rm", &rm("menu-2.txt"))?;
    switch_mode(ctx, &mut s, PermissionMode::Plan)?;
    mode_step(
        &mut s,
        "plan: Write",
        &format!(
            "{} When your plan is ready, ask to leave plan mode and then do it.",
            write("menu-3.txt", "baguette")
        ),
    )?;
    switch_mode(ctx, &mut s, PermissionMode::Auto)?;
    mode_step(&mut s, "auto: Write", &write("menu-4.txt", "brioche"))?;
    mode_step(&mut s, "auto: Bash rm", &rm("menu-4.txt"))?;
    switch_mode(ctx, &mut s, PermissionMode::DontAsk)?;
    mode_step(&mut s, "dontAsk: Write", &write("menu-5.txt", "focaccia"))?;
    switch_mode(ctx, &mut s, PermissionMode::Ask)?;
    mode_step(
        &mut s,
        "default again: Write",
        &write("menu-6.txt", "challah"),
    )?;

    // The switch comes while the Write's approval waits; the approval is
    // answered only after it.
    s.note("step: pending + switch to acceptEdits");
    s.auto_approve = false;
    let from = s.mark();
    s.send(&write("menu-7.txt", "pretzel"));
    let pending = asked(&mut s, from, "Write")?;
    switch_mode(ctx, &mut s, PermissionMode::AcceptEdits)?;
    let cancelled = (from..s.mark()).any(|at| s.seen[at].is("control_cancel_request"));
    ctx.checks.contract(
        "a mode switch leaves a waiting approval waiting",
        ensure(!cancelled, || "the CLI cancelled it".into()),
    );
    answer(&mut s, &pending.request, Decision::Approve, None);
    s.turn_end(from, TURN)?;
    Ok(())
}

/// Plan from `default`: the approved `ExitPlanMode` returns to `default`.
/// Its recording is the fixture `permission_modes_plan.ndjson`.
pub fn permission_modes_plan(ctx: &mut Ctx) -> Result<(), Stop> {
    let mut s = ctx.session(Setup::default())?;
    switch_mode(ctx, &mut s, PermissionMode::Plan)?;
    mode_step(
        &mut s,
        "plan (from default): Write",
        "Use the Write tool to create specials.txt containing exactly: plum tart. When your \
         plan is ready, ask to leave plan mode and then do it.",
    )?;
    mode_step(
        &mut s,
        "after plan: Write",
        "Use the Write tool to create specials-2.txt containing exactly: pear tart",
    )?;
    Ok(())
}

/// Notes `step` and runs a turn of `message`, every approval allowed.
fn mode_step(s: &mut Session, step: &str, message: &str) -> Result<(), Stop> {
    s.note(&format!("step: {step}"));
    let from = s.mark();
    s.send(message);
    s.turn_end(from, TURN)?;
    Ok(())
}

fn switch_mode(ctx: &mut Ctx, s: &mut Session, mode: PermissionMode) -> Result<(), Stop> {
    let answer = set_mode(s, mode);
    if let Err(stop) = answer {
        return Err(stop);
    }
    ctx.checks
        .contract("each mode switch succeeds", expect_mode(answer, mode));
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
    Ok(())
}

/// One process in which each kind of update is granted with an approval and
/// then spares the next call of the same tool: a tool rule, a directory
/// outside the workdir, and file edits. Its recording is the fixture
/// `rules_granted.ndjson` (docs/agents/claude-code.md §3.5).
pub fn rules_granted(ctx: &mut Ctx) -> Result<(), Stop> {
    let workdir = Workdir::new(Some("kitchen"))?;
    let pantry = workdir.root().join("pantry");
    std::fs::create_dir_all(&pantry)
        .and_then(|()| std::fs::write(pantry.join("flour.txt"), "rye\n"))
        .map_err(|e| Stop::Stalled(format!("cannot stock the pantry: {e}")))?;
    let pantry = std::fs::canonicalize(&pantry)
        .map_err(|e| Stop::Stalled(format!("cannot find the pantry: {e}")))?;
    let pantry = pantry.display();
    // `local`: the CLI keeps a project rule in .claude/settings.local.json.
    let mut s = ctx.session(Setup {
        args: vec!["--setting-sources".into(), "project,local".into()],
        workdir: Some(workdir),
        ..Setup::default()
    })?;
    s.auto_approve = false;

    grant_then_repeat(
        ctx,
        &mut s,
        "Bash",
        "addRules",
        |r| r.scope != RuleScope::User && r.label.starts_with("Bash("),
        [
            "The oven timer counts by little marker files. Use the Bash tool to run exactly: \
             touch oven-timer"
                .to_owned(),
            "The scones want another round. Use the Bash tool to run exactly the same command \
             again: touch oven-timer"
                .to_owned(),
        ],
    )?;
    grant_then_repeat(
        ctx,
        &mut s,
        "Bash",
        "addDirectories",
        |r| r.label.starts_with("access to "),
        // The directory is offered for a write there; a read outside the
        // workdir is offered a `Read(…)` rule instead. Once it is a working
        // directory, a read-only command there runs unasked.
        [
            format!(
                "Mark the pantry as checked: use the Bash tool to run exactly: touch \
                 {pantry}/checked"
            ),
            format!("What flour is left? Use the Bash tool to run exactly: cat {pantry}/flour.txt"),
        ],
    )?;
    grant_then_repeat(
        ctx,
        &mut s,
        "Write",
        "setMode acceptEdits",
        |r| r.label == "file edits",
        [
            "Leave a note for the morning baker: use the Write tool to create note-one.txt \
             containing exactly: rye is low"
                .to_owned(),
            "And a second one: use the Write tool to create note-two.txt containing exactly: \
             sugar is fine"
                .to_owned(),
        ],
    )
}

/// Sends `messages[0]`, grants the first update `pick` takes from its
/// `tool` approval, then sends `messages[1]` and checks that its `tool` call
/// is not asked; `kind` names the update in the check.
fn grant_then_repeat(
    ctx: &mut Ctx,
    s: &mut Session,
    tool: &str,
    kind: &str,
    pick: fn(&RuleSuggestion) -> bool,
    messages: [String; 2],
) -> Result<(), Stop> {
    let from = s.mark();
    s.send(&messages[0]);
    let first = asked(s, from, tool)?;
    let Some(rule) = first.suggestions.iter().position(pick) else {
        answer(s, &first.request, Decision::Deny, None);
        return Err(Stop::Inconclusive(format!(
            "no {kind} offered for {tool}: {:?}",
            first.suggestions
        )));
    };
    s.note(&format!("granting {:?}", first.suggestions[rule]));
    answer(
        s,
        &first.request,
        Decision::Approve,
        Some(u8::try_from(rule).unwrap_or(u8::MAX)),
    );
    s.turn_end(from, TURN)?;

    let from = s.mark();
    s.send(&messages[1]);
    let again = next_approval(s, from)?;
    if let Some(again) = &again {
        answer(s, &again.request, Decision::Approve, None);
    }
    let (end, _) = s.turn_end(from, TURN)?;
    let called = s
        .find_event(from, |e| match e {
            AdapterEvent::ToolCall { name, .. } if name == tool => Some(()),
            _ => None,
        })
        .is_some_and(|(at, ())| at <= end);
    if !called {
        return Err(Stop::Inconclusive(format!(
            "the model did not call {tool} again"
        )));
    }
    ctx.checks.contract(
        &format!("a granted {kind} spares the next {tool} call its approval"),
        ensure(again.is_none(), || {
            format!("the {tool} call was asked again")
        }),
    );
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

    // The user closes the laptop while another approval waits: stdin
    // closes, the CLI fails the approval and the turn goes on without it.
    let from = s.mark();
    s.send(
        "Fine, one more try at the diary: use the Write tool to create starter-diary.txt \
         containing exactly: day two, still bubbly",
    );
    let write = asked(&mut s, from, "Write")?;
    s.close_stdin();
    s.wait_closed(TURN)?;
    let cancelled = s.find_event(from, |e| match e {
        AdapterEvent::ApprovalCancelled { request, reason } if *request == write.request => {
            Some(reason.clone())
        }
        _ => None,
    });
    ctx.checks.contract(
        "closing stdin fails a waiting approval",
        ensure(cancelled.is_some(), || "not cancelled".into()),
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
