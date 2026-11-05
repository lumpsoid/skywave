//! Full scenarios about work outside a turn: background shells, subagents,
//! schedules.

use serde_json::{Value, json};
use skywave_proto::Decision;
use skywaved::agent::claude_code::CliSession;
use skywaved::agent::{AdapterEvent, AgentCallId, AgentTask, TaskKind, TurnOutcome};

use super::Ctx;
use super::turns::{TURN, completed};
use crate::report::{Stop, ensure};
use crate::session::{Seen, Session, Setup, Workdir};

/// How long a short background task may take to end.
pub(super) const TASK: u64 = 180;
/// How long a `/loop 1m` may take to tick.
const TICK: u64 = 90;

pub fn background_bash(ctx: &mut Ctx) -> Result<(), Stop> {
    let mut s = ctx.session(Setup::default())?;
    let from = s.mark();
    s.send(
        "The baguettes need to proof, and my egg timer has been missing since the flour \
         incident. Be my timer? Run `sleep 20 && echo DING` with the Bash tool, \
         run_in_background set to true, then say only STARTED and end your turn. When it \
         dings, shout only DONE.",
    );
    let (end, _) = s.turn_end(from, TURN)?;
    let Some((listed, task)) = s.find_event(from, |e| first_task(e, TaskKind::Shell)) else {
        return Err(Stop::Inconclusive(
            "the model started no background shell".into(),
        ));
    };
    let checks = &mut ctx.checks;
    checks.contract(
        "task_started names the shell",
        ensure(
            s.find(from, |x| {
                x.is("system/task_started")
                    && x.str("/task_id") == Some(&task.id)
                    && x.str("/task_type") == Some("local_bash")
            })
            .is_some(),
            || "no task_started".into(),
        ),
    );
    checks.contract(
        "the turn ends with the shell still in the set",
        ensure(listed < end && set_at(&s, end).contains(&task.id), || {
            format!("set at the result: {:?}", set_at(&s, end))
        }),
    );
    let notified = s.wait("task_notification for the shell", TASK, end, |x| {
        x.is("system/task_notification") && x.str("/task_id") == Some(&task.id)
    })?;
    let left = s.wait_event("a set without the shell", TURN, end, |e| {
        without(e, &task.id)
    })?;
    ctx.checks.contract(
        "the shell leaves the set only after its notification",
        ensure(left.0 > notified, || {
            format!("left at {}, notified at {notified}", left.0)
        }),
    );
    self_started_turn(ctx, &mut s, notified)
}

/// A message written the moment a background shell's notification comes,
/// as the CLI starts a turn of its own for it: the race of task b26ecf.
/// The CLI takes the message up at that turn's tool round, or after its
/// `result`; either way its `command_lifecycle` `started` is the one
/// `TurnStarted { message: true }`, never the turn's `system/init`.
pub fn message_meets_own_turn(ctx: &mut Ctx) -> Result<(), Stop> {
    let mut s = ctx.session(Setup::default())?;
    let from = s.mark();
    s.send(
        "The kettle is on and I keep forgetting it. Run `sleep 6 && echo BOILED` with the \
         Bash tool, run_in_background set to true, then say only STARTED and end your turn. \
         When you are told it finished, first run `ls` with the Bash tool (not in the \
         background), then say only POURED.",
    );
    let (end, _) = s.turn_end(from, TURN)?;
    let Some((_, task)) = s.find_event(from, |e| first_task(e, TaskKind::Shell)) else {
        return Err(Stop::Inconclusive(
            "the model started no background shell".into(),
        ));
    };
    let notified = s.wait("task_notification for the shell", TASK, end, |x| {
        x.is("system/task_notification") && x.str("/task_id") == Some(&task.id)
    })?;
    // No turn is open yet, so the protocol writes the message at once.
    s.send("Also, a quick one: reply with only the word TEA.");
    let written = s.mark();
    let (took, ()) = s.wait_event("the message's turn", TURN, written, |e| {
        matches!(e, AdapterEvent::TurnStarted { message: true }).then_some(())
    })?;
    let (ended, outcome) = s.turn_end(took, TURN)?;
    let own = s.find_event(notified, |e| {
        matches!(e, AdapterEvent::TurnStarted { message: false }).then_some(())
    });
    let own_end = own.and_then(|(own, ())| {
        s.find_event(own, |e| {
            matches!(e, AdapterEvent::TurnEnded(_)).then_some(())
        })
    });
    s.note(match (own, own_end) {
        (Some((own, ())), Some((end, ()))) if own < took && end == ended => {
            "the CLI took the message up inside its own turn"
        }
        (Some((own, ())), _) if own < took => "the CLI ran the message after its own turn",
        (Some(_), _) => "the CLI ran the message before its own turn",
        (None, _) => "the CLI started no turn of its own by the message's end",
    });
    let messages = (s.seen[written..=ended].iter())
        .flat_map(|x| &x.events)
        .filter(|e| matches!(e, AdapterEvent::TurnStarted { message: true }))
        .count();
    let checks = &mut ctx.checks;
    checks.contract(
        "the message's turn is its command_lifecycle started, once",
        ensure(
            s.seen[took].is("command_lifecycle")
                && s.seen[took].str("/state") == Some("started")
                && messages == 1,
            || format!("at {}, {messages} reported", s.seen[took].msg),
        ),
    );
    checks.contract("the message's turn completes", completed(&outcome));
    Ok(())
}

/// Stops a background shell, then a background subagent busy in a shell of
/// its own, then an unknown id and the stopped shell again; a second
/// process sends the only `stop_task` the CLI refuses. Its recording is
/// the fixture `stop_task.ndjson` (docs/agents/claude-code.md §3.11).
pub fn stop_task(ctx: &mut Ctx) -> Result<(), Stop> {
    let mut s = ctx.session(Setup::default())?;
    let from = s.mark();
    s.send(
        "Two ovens for the wedding cake, both at once, in ONE message as two parallel tool \
         calls: (1) the Bash tool with command `sleep 120 && echo HOT` and run_in_background \
         true; (2) the Agent tool with run_in_background true, subagent_type general-purpose, \
         prompt \"Run exactly the shell command `python3 -c \"import time; time.sleep(90); \
         print('WARM')\"` with the Bash tool in the foreground (run_in_background false), then \
         reply only WARM.\" Then say only STARTED and end your turn.",
    );
    let (end, _) = s.turn_end(from, TURN)?;
    let Some((_, shell)) = s.find_event(from, |e| first_task(e, TaskKind::Shell)) else {
        return Err(Stop::Inconclusive(
            "the model started no background shell".into(),
        ));
    };
    let (call, sub, _) = subagent_started(&s, from)?;
    // The bride calls it off once the apprentice's own shell runs.
    let (_, sub_shell) = s.wait_event("the subagent's shell", TASK, from, |e| match e {
        AdapterEvent::ToolCall {
            parent: Some(p),
            call: Some(c),
            name,
            ..
        } if *p == call && name == "Bash" => Some(c.0.clone()),
        _ => None,
    })?;
    s.wait("task_started for the subagent's shell", TURN, end, |x| {
        x.is("system/task_started") && x.str("/tool_use_id") == Some(&sub_shell)
    })?;

    s.note(&format!("stopping the shell {}", shell.id));
    let from = s.mark();
    let (ack, answer) = stop(&mut s, &shell.id)?;
    ctx.checks
        .contract("stop_task on a running shell succeeds", answer);
    let killed = s.find(from, |x| {
        x.is("system/task_updated")
            && x.str("/task_id") == Some(&shell.id)
            && x.str("/patch/status") == Some("killed")
    });
    let stopped = s.find(from, |x| {
        x.is("system/task_notification")
            && x.str("/task_id") == Some(&shell.id)
            && x.str("/status") == Some("stopped")
    });
    let left = s
        .find_event(from, |e| without(e, &shell.id))
        .map(|(at, ())| at);
    ctx.checks.contract(
        "killed, stopped, then the set without it, all before the ack",
        match (killed, stopped, left) {
            (Some(k), Some(n), Some(l)) => ensure(k < n && n < l && l < ack, || {
                format!("killed {k}, stopped {n}, left {l}, ack {ack}")
            }),
            _ => Err(format!(
                "killed {killed:?}, stopped {stopped:?}, left {left:?}, ack {ack}"
            )),
        },
    );

    s.note(&format!("stopping the subagent {sub}"));
    let from = s.mark();
    let (ack, answer) = stop(&mut s, &sub)?;
    let left = s.find_event(from, |e| without(e, &sub)).map(|(at, ())| at);
    ctx.checks.contract(
        "stop_task on a running subagent succeeds after it leaves the set",
        answer.and_then(|()| {
            ensure(left.is_some_and(|l| l < ack), || {
                format!("left {left:?}, ack {ack}")
            })
        }),
    );
    self_started_turn(ctx, &mut s, from)?;

    s.note("stopping an unknown id, then the stopped shell again");
    let (_, unknown) = stop(&mut s, "bnosuchtask")?;
    let (_, again) = stop(&mut s, &shell.id)?;
    ctx.checks.contract(
        "an unknown id and an ended task are acked with success",
        unknown.and(again),
    );
    drop(s);

    let mut s = ctx.session(Setup::default())?;
    s.note("a second process: stop_task with a non-string task_id, then with none");
    let id = s.control(json!({ "subtype": "stop_task", "task_id": 123 }));
    let answer = s.response(&id, 30)?;
    ctx.checks.contract(
        "a non-string task_id is refused",
        ensure(
            answer["subtype"] == "error"
                && answer["error"] == "stop_task: task_id must be a string",
            || answer.to_string(),
        ),
    );
    let id = s.control(json!({ "subtype": "stop_task" }));
    s.response(&id, 30)?;
    Ok(())
}

/// Stops `task` through the adapter: the index of its answer, and whether
/// it was a success.
fn stop(s: &mut Session, task: &str) -> Result<(usize, Result<(), String>), Stop> {
    let from = s.mark();
    let out = s.protocol.stop_task(task.to_owned());
    s.apply(out);
    s.wait_event("answer to stop_task", 30, from, |e| match e {
        AdapterEvent::TaskStopAnswered { task: t } if t == task => Some(Ok(())),
        AdapterEvent::TaskStopRefused { task: t, reason } if t == task => Some(Err(reason.clone())),
        _ => None,
    })
}

/// A background subagent writes a file; its approval comes outside any
/// turn and is answered only once the main turn is over. Its recording is
/// the fixture `subagent_approval.ndjson`.
pub fn subagent_approval(ctx: &mut Ctx) -> Result<(), Stop> {
    let mut s = ctx.session(Setup::default())?;
    s.auto_approve = false;
    let from = s.mark();
    s.send(
        "Call the Agent tool with run_in_background true, subagent_type general-purpose, \
         prompt \"Use the Write tool to create the file note.txt in the current directory \
         containing the single word HELLO. Then reply only WRITTEN.\" Then reply only LAUNCHED \
         and end your turn. If you later learn the agent finished, reply only NOTED.",
    );
    let (end, _) = s.turn_end(from, TURN)?;
    let (_, task, _) = subagent_started(&s, from)?;
    let (at, (request, asker)) =
        s.wait_event("the subagent's Write approval", TASK, from, |e| match e {
            AdapterEvent::ApprovalRequested {
                request,
                tool,
                task,
                ..
            } if tool == "Write" => Some((request.clone(), task.clone())),
            _ => None,
        })?;
    ctx.checks.contract(
        "the subagent's approval comes outside the turn, as its task's",
        ensure(at > end && asker.as_deref() == Some(&task), || {
            format!("at {at} (turn ended at {end}), task {asker:?}")
        }),
    );
    s.note("answering the held approval");
    let out = s.protocol.answer(&request, Decision::Approve, None);
    s.apply(out);
    let notified = s.wait("the subagent's task_notification", TASK, at, |x| {
        x.is("system/task_notification") && x.str("/task_id") == Some(&task)
    })?;
    self_started_turn(ctx, &mut s, notified)
}

/// A background subagent that starts a background shell of its own and
/// waits for it: the set holds both, the subagent is woken when the shell
/// ends, and the CLI starts turns of its own. Its recording is the fixture
/// `subagent_background_bash.ndjson`.
pub fn subagent_background_bash(ctx: &mut Ctx) -> Result<(), Stop> {
    let mut s = ctx.session(Setup::default())?;
    let from = s.mark();
    s.send(
        "Use the Agent tool (subagent_type general-purpose) to launch a subagent whose task \
         is: run the shell command `sleep 60 && echo SLEPT` with the Bash tool, \
         run_in_background set to true, wait until it finishes, then reply SLEPT. After \
         launching it, reply only LAUNCHED and end your turn. When you learn the subagent \
         finished, reply only DONE.",
    );
    s.turn_end(from, TURN)?;
    let (_, sub, _) = subagent_started(&s, from)?;
    let (_, shell) = s.wait_event("the subagent's shell in the set", TASK, from, |e| match e {
        AdapterEvent::BackgroundTasks(tasks) => tasks
            .iter()
            .find(|t| t.kind == TaskKind::Shell && t.parent.as_deref() == Some(&sub))
            .cloned(),
        _ => None,
    })?;
    let notified = s.wait("the shell's task_notification", 2 * TASK, from, |x| {
        x.is("system/task_notification") && x.str("/task_id") == Some(&shell.id)
    })?;
    let (empty, ()) = s.wait_event("an empty set", 2 * TASK, notified, |e| match e {
        AdapterEvent::BackgroundTasks(tasks) => tasks.is_empty().then_some(()),
        _ => None,
    })?;
    if s.protocol.turn_open() {
        s.turn_end(empty, TURN)?;
    }
    let woken = (s.seen.iter())
        .filter(|x| x.is("system/task_started") && x.str("/task_id") == Some(&sub))
        .count();
    ctx.checks.contract(
        "the subagent is woken again when its shell ends",
        ensure(woken >= 2, || format!("{woken} task_started")),
    );
    Ok(())
}

pub fn subagent(ctx: &mut Ctx) -> Result<(), Stop> {
    let mut s = ctx.session(Setup::default())?;
    let from = s.mark();
    s.send(
        "Could you send the apprentice out to chalk today's special on the board? Call the \
         Agent tool with run_in_background true, subagent_type general-purpose, prompt \"Use \
         the Write tool to create the file chalkboard.txt in the current directory containing \
         the single word CROISSANTS. Then reply only WRITTEN.\" Then say only LAUNCHED and end \
         your turn. When the apprentice is back, say only NOTED.",
    );
    s.turn_end(from, TURN)?;
    let (call, task, backgrounded) = subagent_started(&s, from)?;
    let checks = &mut ctx.checks;
    checks.contract(
        "task_started marks the subagent backgrounded",
        ensure(backgrounded, || "is_backgrounded is not true".into()),
    );
    // The CLI lists the task before its task_started names the call; the
    // adapter then reports the set again with it.
    let listed = s.find_event(from, |e| match e {
        AdapterEvent::BackgroundTasks(tasks) => tasks
            .iter()
            .find(|t| t.id == task && t.kind == TaskKind::Subagent && t.call.is_some())
            .and_then(|t| t.call.clone()),
        _ => None,
    });
    checks.contract(
        "the set lists the subagent with its Agent call",
        match listed {
            Some((_, c)) => ensure(c == call, || format!("call {}", c.0)),
            None => Err("never listed with its call".into()),
        },
    );
    let (at, asker) = s.wait_event("the subagent's Write approval", TASK, from, |e| match e {
        AdapterEvent::ApprovalRequested { tool, task, .. } if tool == "Write" => Some(task.clone()),
        _ => None,
    })?;
    ctx.checks.contract(
        "the subagent's can_use_tool carries its task id as agent_id",
        ensure(
            asker.as_deref() == Some(&task) && s.seen[at].str("/request/agent_id") == Some(&task),
            || format!("the adapter read the task {asker:?}"),
        ),
    );
    let notified = s.wait("the subagent's task_notification", TASK, from, |x| {
        x.is("system/task_notification") && x.str("/task_id") == Some(&task)
    })?;
    let parented = s
        .find_event(from, |e| match e {
            AdapterEvent::Text { parent, .. } | AdapterEvent::ToolCall { parent, .. }
                if parent.as_ref() == Some(&call) =>
            {
                Some(())
            }
            _ => None,
        })
        .is_some();
    ctx.checks.contract(
        "the subagent's own lines carry its call as parent_tool_use_id",
        ensure(parented, || "no line of its own".into()),
    );
    ctx.checks.contract(
        "the subagent's Write ran",
        ensure(s.workdir().join("chalkboard.txt").exists(), || {
            "chalkboard.txt was not written".into()
        }),
    );
    s.wait_event("a set without the subagent", TURN, notified, |e| {
        without(e, &task)
    })?;
    self_started_turn(ctx, &mut s, notified)
}

pub fn subagent_foreground(ctx: &mut Ctx) -> Result<(), Stop> {
    let mut s = ctx.session(Setup {
        files: RECIPES.to_vec(),
        ..Setup::default()
    })?;
    let from = s.mark();
    s.send(
        "A vampire just walked in and asked, very politely, what to avoid. Could a helper \
         check my recipe box? Call the Agent tool in the foreground (run_in_background false), \
         subagent_type general-purpose, prompt \"Read every file under recipes/, then use the \
         Write tool to create garlic.txt listing the recipes that use garlic, one per line, \
         and reply with their names.\" I'll wait right here with my tea, then tell me what it \
         found.",
    );
    let (end, outcome) = s.turn_end(from, 2 * TURN)?;
    let (call, task, backgrounded) = subagent_started(&s, from)?;
    if backgrounded {
        return Err(Stop::Inconclusive(
            "the model ran the helper in the background".into(),
        ));
    }
    let checks = &mut ctx.checks;
    checks.contract("the turn completes", completed(&outcome));
    checks.contract(
        "a foreground subagent never enters the set",
        ensure(
            s.find_event(0, |e| match e {
                AdapterEvent::BackgroundTasks(tasks) => {
                    tasks.iter().any(|t| t.id == task).then_some(())
                }
                _ => None,
            })
            .is_none(),
            || "listed".into(),
        ),
    );
    let parented = s
        .find_event(from, |e| match e {
            AdapterEvent::ToolCall { parent, .. } | AdapterEvent::Text { parent, .. }
                if parent.as_ref() == Some(&call) =>
            {
                Some(())
            }
            _ => None,
        })
        .is_some_and(|(at, ())| at < end);
    checks.contract(
        "its lines carry its call as parent_tool_use_id, inside the turn",
        ensure(parented, || "none".into()),
    );
    let notified = s.find(from, |x| {
        x.is("system/task_notification") && x.str("/task_id") == Some(&task)
    });
    let result = s
        .find_event(from, |e| match e {
            AdapterEvent::ToolResult { call: Some(c), .. } if *c == call => Some(()),
            _ => None,
        })
        .map(|(at, ())| at);
    checks.contract(
        "the Agent call's result comes after the task_notification, inside the turn",
        match (notified, result) {
            (Some(n), Some(r)) => ensure(n < r && r < end, || {
                format!("notification {n}, result {r}, end {end}")
            }),
            _ => Err(format!("notification {notified:?}, result {result:?}")),
        },
    );
    // Its approvals name it as agent_id, which is no live task, so they
    // are the turn's. The helper may need none (Read and Grep are asked
    // for nothing).
    for (at, seen) in s.seen.iter().enumerate().skip(from) {
        if seen.is_request("can_use_tool") && seen.str("/request/agent_id") == Some(&task) {
            let theirs = seen
                .events
                .iter()
                .any(|e| matches!(e, AdapterEvent::ApprovalRequested { task: None, .. }));
            ctx.checks.contract(
                "a foreground subagent's approval is the turn's",
                ensure(theirs, || format!("line {at} was tied to a task")),
            );
            break;
        }
    }
    Ok(())
}

/// The main turn runs a foreground shell while a background subagent runs
/// one of its own; the turn is interrupted mid-shell. The session, the
/// subagent's task, and the indices of the first message and the turn's
/// end.
fn interrupt_beside_a_subagent(
    ctx: &mut Ctx,
    setup: Setup,
) -> Result<(Session, AgentTask, usize, usize), Stop> {
    let mut s = ctx.session(setup)?;
    let start = s.mark();
    s.send(
        "Two jobs at once, it's the Saturday rush! First send the apprentice to watch the \
         dough rise: call the Agent tool with run_in_background true, subagent_type \
         general-purpose, prompt \"Run exactly the shell command `python3 -c \"import time; \
         time.sleep(45); print('RISEN')\"` with the Bash tool in the foreground, then reply \
         only RISEN.\" Once the apprentice is off, knead the rye yourself: run exactly the \
         shell command `python3 -c \"import time; time.sleep(30); print('KNEADED')\"` with the \
         Bash tool in the foreground (run_in_background false). Then say only DONE. When the \
         apprentice reports back, say only NOTED.",
    );
    // Mid-knead the user shouts "stop!": the turn is interrupted, and the
    // apprentice should keep watching the dough.
    // The main turn's 30 s shell runs once its task_started comes.
    let at = s.wait(
        "the main turn's shell or the end of the turn",
        TURN,
        start,
        |x| {
            x.events
                .iter()
                .any(|e| matches!(e, AdapterEvent::TurnEnded(_)))
                || x.events.iter().any(|e| {
                    matches!(e, AdapterEvent::ToolCall { parent: None, name, input, .. }
                    if name == "Bash" && command(input).contains("sleep(30)"))
                })
        },
    )?;
    let Some(main_call) = s.seen[at].events.iter().find_map(|e| match e {
        AdapterEvent::ToolCall {
            parent: None,
            call: Some(call),
            name,
            input,
            ..
        } if name == "Bash" && command(input).contains("sleep(30)") => Some(call.0.clone()),
        _ => None,
    }) else {
        return Err(Stop::Inconclusive(
            "the turn ended without the foreground shell".into(),
        ));
    };
    s.wait("task_started for the main turn's shell", TURN, at, |x| {
        x.is("system/task_started") && x.str("/tool_use_id") == Some(&main_call)
    })?;
    let Some((_, task)) = s
        .find_event(start, |e| first_task(e, TaskKind::Subagent))
        .filter(|(_, t)| set_at(&s, s.mark() - 1).contains(&t.id))
    else {
        return Err(Stop::Inconclusive(
            "no background subagent runs at the interrupt".into(),
        ));
    };
    let from = s.mark();
    s.note("interrupting the turn");
    let out = s.protocol.interrupt();
    s.apply(out);
    let (end, outcome) = s.turn_end(from, TURN)?;
    ctx.checks.contract(
        "the turn ends interrupted, terminal_reason aborted*",
        ensure(
            outcome == TurnOutcome::Interrupted
                && s.seen[end]
                    .str("/terminal_reason")
                    .is_some_and(|r| r.starts_with("aborted")),
            || format!("{outcome:?}, {:?}", s.seen[end].str("/terminal_reason")),
        ),
    );
    Ok((s, task, start, end))
}

pub fn interrupt_spares_tasks(ctx: &mut Ctx) -> Result<(), Stop> {
    let (mut s, task, start, end) = interrupt_beside_a_subagent(ctx, Setup::default())?;
    let checks = &mut ctx.checks;
    checks.contract(
        "an interrupted turn calls no Stop hook",
        ensure(
            !(start..end).any(|at| s.seen[at].is_request("hook_callback")),
            || "a hook_callback came".into(),
        ),
    );
    checks.contract(
        "the interrupt spares the background subagent",
        ensure(set_at(&s, end).contains(&task.id), || {
            format!("set at the result: {:?}", set_at(&s, end))
        }),
    );
    let notified = s.wait("the subagent's task_notification", TASK, end, |x| {
        x.is("system/task_notification") && x.str("/task_id") == Some(&task.id)
    })?;
    ctx.checks.contract(
        "the spared subagent completes",
        ensure(s.seen[notified].str("/status") == Some("completed"), || {
            format!("{:?}", s.seen[notified].str("/status"))
        }),
    );
    self_started_turn(ctx, &mut s, notified)
}

/// The same with the `initialize` skywaved sent before
/// `perTaskStopAffordance` (and no hooks): the interrupt kills the
/// subagent. Its recording is the fixture `interrupt_kills_tasks.ndjson`
/// (docs/agents/claude-code.md §4.4).
pub fn interrupt_kills_tasks(ctx: &mut Ctx) -> Result<(), Stop> {
    let setup = Setup {
        initialize: Some(|line: &mut Value| {
            line["request"]["hooks"] = Value::Null;
            if let Some(request) = line["request"].as_object_mut() {
                request.remove("perTaskStopAffordance");
            }
        }),
        ..Setup::default()
    };
    let (s, task, _, end) = interrupt_beside_a_subagent(ctx, setup)?;
    ctx.checks.contract(
        "without perTaskStopAffordance the interrupt kills the background subagent",
        ensure(!set_at(&s, end).contains(&task.id), || {
            format!("set at the result: {:?}", set_at(&s, end))
        }),
    );
    Ok(())
}

pub fn loop_tick(ctx: &mut Ctx) -> Result<(), Stop> {
    let mut s = ctx.session(Setup::default())?;
    let from = s.mark();
    s.send("/loop 1m peek at the oven and say only TICK");
    let (end, _) = s.turn_end(from, TURN)?;
    let created = s
        .find_event(from, |e| match e {
            AdapterEvent::ToolCall { name, .. } if name == "CronCreate" => Some(()),
            _ => None,
        })
        .is_some();
    if !created {
        return Err(Stop::Inconclusive("/loop made no CronCreate call".into()));
    }
    let job = last_schedules(&s, from, end).into_iter().next();
    let Some(job) = job else {
        ctx.checks.contract(
            "the Stop hook lists the new job in session_crons",
            Err("no job listed".into()),
        );
        return Ok(());
    };
    ctx.checks
        .contract("the Stop hook lists the new job in session_crons", Ok(()));

    let (started, opened, tick_end, done) = next_tick(&mut s, end)?;
    let checks = &mut ctx.checks;
    checks.contract(
        "a tick is a turn the CLI starts, framed by command_lifecycle",
        ensure(
            s.seen[started].str("/wakeup_source") == Some("schedule_wakeup") && done > tick_end,
            || format!("{}", s.seen[started].msg),
        ),
    );
    checks.contract(
        "the tick's Stop hook lists the job",
        ensure(last_schedules(&s, opened, tick_end).contains(&job), || {
            "not listed".into()
        }),
    );

    let from = s.mark();
    s.send(
        "One more thing: the sourdough starter gets grumpy if nobody feeds him at 4 in the \
         morning, even when I'm away. Use the CronCreate tool to create a DURABLE (durable: \
         true) recurring cron job, schedule `0 4 * * *`, prompt `feed the sourdough starter`. \
         Then say only CREATED.",
    );
    s.turn_end(from, TURN)?;
    let durable = s
        .seen
        .iter()
        .skip(from)
        .filter_map(|x| x.msg.pointer("/tool_use_result/durable"))
        .find_map(Value::as_bool);
    let Some(durable) = durable else {
        return Err(Stop::Inconclusive("no CronCreate result".into()));
    };
    let file = s.workdir().join(".claude/scheduled_tasks.json").exists();
    ctx.checks.watch(
        "durable: true is ignored",
        "D31 deferred: durable schedules across sessions",
        ensure(!durable && !file, || {
            format!("the result says durable {durable}, scheduled_tasks.json written: {file}")
        }),
    );
    Ok(())
}

/// `/loop`, one tick, then a second process resuming the session: the job
/// is listed again and its ticks come back. Its recording is the fixture
/// `loop_resume.ndjson` (docs/agents/claude-code.md §3.11).
pub fn loop_resume(ctx: &mut Ctx) -> Result<(), Stop> {
    let workdir = Workdir::new(None)?;
    let mut s = ctx.session(Setup {
        workdir: Some(workdir.clone()),
        ..Setup::default()
    })?;
    let (job, end) = start_loop(&mut s)?;
    next_tick(&mut s, end)?;
    let id = s.protocol.session_id();
    drop(s);

    let mut s = ctx.session(Setup {
        session: Some(CliSession::Resume(id)),
        workdir: Some(workdir),
        ..Setup::default()
    })?;
    let from = s.mark();
    s.send("Back from the market. Reply only HELLO.");
    let (end, _) = s.turn_end(from, TURN)?;
    ctx.checks.contract(
        "a resumed process lists the job in its first Stop hook",
        ensure(last_schedules(&s, from, end).contains(&job), || {
            format!("listed {:?}", last_schedules(&s, from, end))
        }),
    );
    let (_, opened, tick_end, _) = next_tick(&mut s, end)?;
    ctx.checks.contract(
        "the job ticks again after a resume",
        ensure(last_schedules(&s, opened, tick_end).contains(&job), || {
            "the tick's hook does not list it".into()
        }),
    );
    Ok(())
}

/// `/loop`, one tick, then the job deleted, a durable job created and that
/// deleted too: each completed turn's Stop hook lists the jobs then. Its
/// recording is the fixture `stop_hook.ndjson`.
pub fn stop_hook(ctx: &mut Ctx) -> Result<(), Stop> {
    let mut s = ctx.session(Setup::default())?;
    let (_, end) = start_loop(&mut s)?;
    next_tick(&mut s, end)?;
    let steps = [
        "Delete the recurring cron job you created for the loop, using the CronDelete tool. \
         Then reply only DELETED.",
        "Use the CronCreate tool to create a DURABLE (durable: true) recurring cron job, \
         schedule `0 4 * * *`, prompt `reply only DURABLE`. Then reply only CREATED.",
        "Delete that durable cron job with the CronDelete tool. Then reply only GONE.",
    ];
    let mut listed = Vec::new();
    for step in steps {
        let from = s.mark();
        s.send(step);
        let (end, _) = s.turn_end(from, TURN)?;
        listed.push(last_schedules(&s, from, end).len());
    }
    ctx.checks.contract(
        "the Stop hook lists the jobs left after each turn",
        ensure(listed == [0, 1, 0], || format!("jobs listed: {listed:?}")),
    );
    let hooks = (s.seen.iter())
        .filter(|x| x.is_request("hook_callback"))
        .count();
    let results = (s.seen.iter()).filter(|x| x.is("result")).count();
    ctx.checks.contract(
        "every completed turn calls the Stop hook once",
        ensure(hooks == results, || {
            format!("{hooks} hooks, {results} results")
        }),
    );
    Ok(())
}

/// Sends `/loop` and waits for its turn: the job its Stop hook lists, and
/// the index of the turn's end.
fn start_loop(s: &mut Session) -> Result<(String, usize), Stop> {
    let from = s.mark();
    s.send("/loop 1m peek at the oven and say only TICK");
    let (end, _) = s.turn_end(from, TURN)?;
    let job = last_schedules(s, from, end).into_iter().next();
    let job = job.ok_or_else(|| Stop::Inconclusive("/loop listed no job".into()))?;
    Ok((job, end))
}

/// The next tick from index `from` on: the indices of its lifecycle
/// `started`, its `TurnStarted`, its end and its lifecycle `completed`.
fn next_tick(s: &mut Session, from: usize) -> Result<(usize, usize, usize, usize), Stop> {
    let lifecycle = |state: &'static str| {
        move |x: &Seen| x.is("command_lifecycle") && x.str("/state") == Some(state)
    };
    let started = s.wait("a tick", TICK, from, lifecycle("started"))?;
    let (opened, ()) = s.wait_event("the tick's turn", 30, started, |e| {
        matches!(e, AdapterEvent::TurnStarted { message: false }).then_some(())
    })?;
    let (end, _) = s.turn_end(opened, TURN)?;
    let done = s.wait(
        "the tick's completed lifecycle",
        30,
        end,
        lifecycle("completed"),
    )?;
    Ok((started, opened, end, done))
}

/// The recipe box the foreground subagents read.
pub(super) const RECIPES: &[(&str, &str)] = &[
    (
        "recipes/pancakes.md",
        "# Pancakes\n\nflour, milk, eggs, butter, a pinch of salt\n",
    ),
    (
        "recipes/garlic-bread.md",
        "# Garlic bread\n\nbaguette, butter, garlic, parsley\n",
    ),
    (
        "recipes/tomato-soup.md",
        "# Tomato soup\n\ntomatoes, onion, garlic, olive oil, basil\n",
    ),
    (
        "recipes/lemon-cake.md",
        "# Lemon cake\n\nflour, sugar, eggs, butter, lemons\n",
    ),
];

/// A turn the CLI starts on its own after index `from` (its `system/init`
/// with no message written), which must complete.
pub(super) fn self_started_turn(ctx: &mut Ctx, s: &mut Session, from: usize) -> Result<(), Stop> {
    let (opened, ()) = s.wait_event("a turn the CLI starts", TURN, from, |e| {
        matches!(e, AdapterEvent::TurnStarted { message: false }).then_some(())
    })?;
    let (_, outcome) = s.turn_end(opened, TURN)?;
    ctx.checks.contract(
        "the CLI starts a turn on its own, which completes",
        completed(&outcome),
    );
    Ok(())
}

/// The first task of `kind` in a set event.
pub(super) fn first_task(e: &AdapterEvent, kind: TaskKind) -> Option<AgentTask> {
    match e {
        AdapterEvent::BackgroundTasks(tasks) => tasks.iter().find(|t| t.kind == kind).cloned(),
        _ => None,
    }
}

/// A set event without task `id`.
pub(super) fn without(e: &AdapterEvent, id: &str) -> Option<()> {
    match e {
        AdapterEvent::BackgroundTasks(tasks) => (!tasks.iter().any(|t| t.id == id)).then_some(()),
        _ => None,
    }
}

/// The ids in the last set reported at or before index `at`.
pub(super) fn set_at(s: &Session, at: usize) -> Vec<String> {
    s.seen[..=at]
        .iter()
        .rev()
        .find_map(|x| {
            x.events.iter().rev().find_map(|e| match e {
                AdapterEvent::BackgroundTasks(tasks) => {
                    Some(tasks.iter().map(|t| t.id.clone()).collect())
                }
                _ => None,
            })
        })
        .unwrap_or_default()
}

/// The job ids of the last schedules read between `from` and `to`.
fn last_schedules(s: &Session, from: usize, to: usize) -> Vec<String> {
    s.seen[from..=to]
        .iter()
        .rev()
        .find_map(|x| {
            x.events.iter().find_map(|e| match e {
                AdapterEvent::Schedules(jobs) => Some(jobs.iter().map(|j| j.id.clone()).collect()),
                _ => None,
            })
        })
        .unwrap_or_default()
}

/// The first subagent's `task_started` from index `from` on: its `Agent`
/// call, task id and whether it runs in the background.
pub(super) fn subagent_started(
    s: &Session,
    from: usize,
) -> Result<(AgentCallId, String, bool), Stop> {
    let at = s
        .find(from, |x| {
            x.is("system/task_started") && x.str("/task_type") == Some("local_agent")
        })
        .ok_or_else(|| Stop::Inconclusive("the model started no subagent".into()))?;
    let seen = &s.seen[at];
    let (Some(call), Some(task)) = (seen.str("/tool_use_id"), seen.str("/task_id")) else {
        return Err(Stop::Stalled("task_started without its ids".into()));
    };
    let agent_call = s.find_event(from, |e| match e {
        AdapterEvent::ToolCall {
            name,
            call: Some(c),
            parent: None,
            ..
        } if name == "Agent" => Some(c.0.clone()),
        _ => None,
    });
    if agent_call.as_ref().map(|(_, c)| c.as_str()) != Some(call) {
        return Err(Stop::Stalled(format!(
            "task_started names call {call}, the Agent call is {agent_call:?}"
        )));
    }
    let backgrounded = seen.msg["is_backgrounded"] == true;
    Ok((AgentCallId::from(call), task.to_owned(), backgrounded))
}

pub(super) fn command(input: &Value) -> &str {
    input.get("command").and_then(Value::as_str).unwrap_or("")
}
