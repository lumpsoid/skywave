//! Full scenarios about work outside a turn: background shells, subagents,
//! schedules.

use serde_json::Value;
use skywaved::agent::{AdapterEvent, AgentCallId, AgentTask, TaskKind, TurnOutcome};

use super::Ctx;
use super::turns::{TURN, completed};
use crate::report::{Stop, ensure};
use crate::session::{Seen, Session, Setup};

/// How long a short background task may take to end.
const TASK: u64 = 180;
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

pub fn stop_task(ctx: &mut Ctx) -> Result<(), Stop> {
    let mut s = ctx.session(Setup::default())?;
    let from = s.mark();
    s.send(
        "Preheat the big oven nice and slow for the wedding cake: run `sleep 120` with the \
         Bash tool, run_in_background set to true. Then say only STARTED and end your turn.",
    );
    // The bride calls it off: the user switches the oven off themselves.
    let (end, _) = s.turn_end(from, TURN)?;
    let Some(task) = set_at(&s, end).into_iter().next() else {
        return Err(Stop::Inconclusive(
            "no background shell runs after the turn".into(),
        ));
    };
    let from = s.mark();
    let out = s.protocol.stop_task(task.clone());
    s.apply(out);
    let (ack, answer) = s.wait_event("answer to stop_task", 30, from, |e| match e {
        AdapterEvent::TaskStopAnswered { task: t } if *t == task => Some(Ok(())),
        AdapterEvent::TaskStopRefused { task: t, reason } if *t == task => {
            Some(Err(reason.clone()))
        }
        _ => None,
    })?;
    ctx.checks
        .contract("stop_task on a running shell succeeds", answer);
    let killed = s.find(from, |x| {
        x.is("system/task_updated")
            && x.str("/task_id") == Some(&task)
            && x.str("/patch/status") == Some("killed")
    });
    let stopped = s.find(from, |x| {
        x.is("system/task_notification")
            && x.str("/task_id") == Some(&task)
            && x.str("/status") == Some("stopped")
    });
    let left = s.find_event(from, |e| without(e, &task)).map(|(at, ())| at);
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
         subagent_type general-purpose, prompt \"Read every file under recipes/ and reply with \
         the names of the recipes that use garlic.\" I'll wait right here with my tea, then \
         tell me what it found.",
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

pub fn interrupt_spares_tasks(ctx: &mut Ctx) -> Result<(), Stop> {
    let mut s = ctx.session(Setup::default())?;
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
    let checks = &mut ctx.checks;
    checks.contract(
        "the turn ends interrupted, terminal_reason aborted*",
        ensure(
            outcome == TurnOutcome::Interrupted
                && s.seen[end]
                    .str("/terminal_reason")
                    .is_some_and(|r| r.starts_with("aborted")),
            || format!("{outcome:?}, {:?}", s.seen[end].str("/terminal_reason")),
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

    let lifecycle = |state: &'static str| {
        move |x: &Seen| x.is("command_lifecycle") && x.str("/state") == Some(state)
    };
    let started = s.wait("a tick", TICK, end, lifecycle("started"))?;
    let (opened, ()) = s.wait_event("the tick's turn", 30, started, |e| {
        matches!(e, AdapterEvent::TurnStarted { message: false }).then_some(())
    })?;
    let (tick_end, _) = s.turn_end(opened, TURN)?;
    let done = s.wait(
        "the tick's completed lifecycle",
        30,
        tick_end,
        lifecycle("completed"),
    )?;
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

/// The recipe box the foreground subagent reads.
const RECIPES: &[(&str, &str)] = &[
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
fn self_started_turn(ctx: &mut Ctx, s: &mut Session, from: usize) -> Result<(), Stop> {
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
fn first_task(e: &AdapterEvent, kind: TaskKind) -> Option<AgentTask> {
    match e {
        AdapterEvent::BackgroundTasks(tasks) => tasks.iter().find(|t| t.kind == kind).cloned(),
        _ => None,
    }
}

/// A set event without task `id`.
fn without(e: &AdapterEvent, id: &str) -> Option<()> {
    match e {
        AdapterEvent::BackgroundTasks(tasks) => (!tasks.iter().any(|t| t.id == id)).then_some(()),
        _ => None,
    }
}

/// The ids in the last set reported at or before index `at`.
fn set_at(s: &Session, at: usize) -> Vec<String> {
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
fn subagent_started(s: &Session, from: usize) -> Result<(AgentCallId, String, bool), Stop> {
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

fn command(input: &Value) -> &str {
    input.get("command").and_then(Value::as_str).unwrap_or("")
}
