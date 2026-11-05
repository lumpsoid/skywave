//! The scenarios, each a scene played against a real CLI process.
//!
//! Every scene is a small play: a user runs a tiny bakery in the scratch
//! folder and chats with the agent as a shop assistant (fridge notes, oven
//! timers, an apprentice sent out to chalk the board). Each line of the
//! play still asks for one exact step, naming the tool and the command,
//! because the checks need that step to happen. Interrupts, denials and
//! stops are the user changing their mind.
//!
//! A scenario asserts on structure (message kinds, their order, fields and
//! ids), never on what the model writes. When the model does something
//! else than asked, the scenario stops as inconclusive rather than failing.

mod controls;
mod conversations;
mod subagents;
mod tasks;
mod turns;

use crate::report::{Checks, Stop};
use crate::session::{Session, Setup, Shared};

/// How costly a scenario is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// No model call: control requests, and local slash commands
    /// (`/rename`) where a conversation must exist. Seconds, no tokens.
    Quick,
    /// Real turns on the cheap model; some wait on timers.
    Full,
}

pub struct Scenario {
    pub name: &'static str,
    pub tier: Tier,
    pub about: &'static str,
    pub run: fn(&mut Ctx) -> Result<(), Stop>,
}

/// What a running scenario has: where the CLI is, its recording and its
/// checks.
pub struct Ctx {
    pub claude: String,
    pub recorder: Shared,
    pub checks: Checks,
}

impl Ctx {
    /// Starts a CLI process; it ends when dropped.
    pub fn session(&self, setup: Setup) -> Result<Session, Stop> {
        Session::start(&self.claude, self.recorder.clone(), setup)
    }

    /// Starts a CLI process that may fail before the handshake ends
    /// ([`Session::start_or_refusal`]).
    pub fn session_or_refusal(&self, setup: Setup) -> Result<Result<Session, String>, Stop> {
        Session::start_or_refusal(&self.claude, self.recorder.clone(), setup)
    }
}

/// Every scenario, cheapest first.
pub const ALL: &[Scenario] = &[
    Scenario {
        name: "handshake",
        tier: Tier::Quick,
        about: "initialize: version, mode, offered models, effort read",
        run: controls::handshake,
    },
    Scenario {
        name: "permission_mode_errors",
        tier: Tier::Quick,
        about: "set_permission_mode: success echoes, refusals, the CLI's mode list",
        run: controls::permission_modes,
    },
    Scenario {
        name: "bypass_allowed",
        tier: Tier::Quick,
        about: "--allow-dangerously-skip-permissions: starts asking, may switch to bypass",
        run: controls::bypass_allowed,
    },
    Scenario {
        name: "model_switch",
        tier: Tier::Quick,
        about: "set_model: resolved model, refusal, default",
        run: controls::model_switch,
    },
    Scenario {
        name: "effort_switch",
        tier: Tier::Quick,
        about: "apply_flag_settings + get_settings: levels read back, a model with none",
        run: controls::effort_switch,
    },
    Scenario {
        name: "stop_task_unknown",
        tier: Tier::Quick,
        about: "stop_task on an unknown id gets exactly one answer",
        run: controls::stop_task_unknown,
    },
    Scenario {
        name: "send_task_message",
        tier: Tier::Quick,
        about: "is send_task_message still a stub (D33, D34)",
        run: controls::send_task_message,
    },
    Scenario {
        name: "resume",
        tier: Tier::Quick,
        about: "--resume: same session id, the transcript's branch continued",
        run: conversations::resume,
    },
    Scenario {
        name: "fork",
        tier: Tier::Quick,
        about: "--resume --fork-session --session-id: our id, the history copied, parent untouched",
        run: conversations::fork,
    },
    Scenario {
        name: "resume_unknown",
        tier: Tier::Quick,
        about: "resume or fork of an unknown id fails at start with UNKNOWN_SESSION's text",
        run: conversations::resume_unknown,
    },
    Scenario {
        name: "transcript",
        tier: Tier::Quick,
        about: "the transcript: where project_dir says (long cwd hashed), the fields transcripts.rs reads",
        run: conversations::transcript,
    },
    Scenario {
        name: "basic",
        tier: Tier::Full,
        about: "two turns: handle, text, Stop hook before result, usage; then a resume",
        run: turns::basic,
    },
    Scenario {
        name: "fork_turns",
        tier: Tier::Full,
        about: "a fork with model turns; resuming parent and fork shows the parent untouched",
        run: conversations::fork_turns,
    },
    Scenario {
        name: "interrupt",
        tier: Tier::Full,
        about: "interrupt while text streams and while idle; an echo runs unasked",
        run: turns::interrupt,
    },
    Scenario {
        name: "permission_modes",
        tier: Tier::Full,
        about: "every mode in turn, and a switch while an approval waits",
        run: turns::permission_modes,
    },
    Scenario {
        name: "permission_modes_plan",
        tier: Tier::Full,
        about: "plan from default: the approved ExitPlanMode returns to default",
        run: turns::permission_modes_plan,
    },
    Scenario {
        name: "permissions",
        tier: Tier::Full,
        about: "can_use_tool allow and deny, and a granted rule spares the next call",
        run: turns::permissions,
    },
    Scenario {
        name: "rules_granted",
        tier: Tier::Full,
        about: "a tool rule, a directory and file edits, each granted, then applied by the CLI",
        run: turns::rules_granted,
    },
    Scenario {
        name: "interrupt_pending",
        tier: Tier::Full,
        about: "interrupt while an approval waits, and stdin closed while one waits",
        run: turns::interrupt_pending,
    },
    Scenario {
        name: "background_bash",
        tier: Tier::Full,
        about: "a background shell: level set, notification, a turn the CLI starts (~40s)",
        run: tasks::background_bash,
    },
    Scenario {
        name: "message_meets_own_turn",
        tier: Tier::Full,
        about: "a message written as the CLI starts a turn of its own: its started is its turn (~30s)",
        run: tasks::message_meets_own_turn,
    },
    Scenario {
        name: "stop_task",
        tier: Tier::Full,
        about: "stop_task on a shell, a subagent, an unknown and an ended id; a refused one (~40s)",
        run: tasks::stop_task,
    },
    Scenario {
        name: "subagent",
        tier: Tier::Full,
        about: "a background subagent: its call, its lines, agent_id on its approval",
        run: tasks::subagent,
    },
    Scenario {
        name: "subagent_approval",
        tier: Tier::Full,
        about: "a background subagent's approval, outside any turn, answered later",
        run: tasks::subagent_approval,
    },
    Scenario {
        name: "subagent_background_bash",
        tier: Tier::Full,
        about: "a subagent's own background shell: nested in the set, wakes it (~90s)",
        run: tasks::subagent_background_bash,
    },
    Scenario {
        name: "subagent_foreground",
        tier: Tier::Full,
        about: "a foreground subagent: never in the level set, its approval is the turn's",
        run: tasks::subagent_foreground,
    },
    Scenario {
        name: "interrupt_spares_tasks",
        tier: Tier::Full,
        about: "with perTaskStopAffordance an interrupt spares a background subagent (~60s)",
        run: tasks::interrupt_spares_tasks,
    },
    Scenario {
        name: "interrupt_kills_tasks",
        tier: Tier::Full,
        about: "without perTaskStopAffordance an interrupt kills a background subagent",
        run: tasks::interrupt_kills_tasks,
    },
    Scenario {
        name: "subagent_foreground_backgrounded",
        tier: Tier::Full,
        about: "a foreground helper sent to the background (Ctrl+B) enters the set",
        run: subagents::subagent_foreground_backgrounded,
    },
    Scenario {
        name: "subagent_foreground_stop",
        tier: Tier::Full,
        about: "a stopped foreground helper fails its Agent call",
        run: subagents::subagent_foreground_stop,
    },
    Scenario {
        name: "subagent_message_ctl",
        tier: Tier::Full,
        about: "send_task_message to a running subagent is refused (~60s)",
        run: subagents::subagent_message_ctl,
    },
    Scenario {
        name: "subagent_message_ctl_done",
        tier: Tier::Full,
        about: "send_task_message to a finished subagent is refused",
        run: subagents::subagent_message_ctl_done,
    },
    Scenario {
        name: "subagent_message_ptu",
        tier: Tier::Full,
        about: "a user line under a subagent's call goes to the main agent (~60s)",
        run: subagents::subagent_message_ptu,
    },
    Scenario {
        name: "subagent_message_relay",
        tier: Tier::Full,
        about: "the relay: the main agent SendMessages a working subagent (~60s)",
        run: subagents::subagent_message_relay,
    },
    Scenario {
        name: "subagent_message_relay_midrun",
        tier: Tier::Full,
        about: "the relay to a subagent busy with quick calls",
        run: subagents::subagent_message_relay_midrun,
    },
    Scenario {
        name: "loop",
        tier: Tier::Full,
        about: "/loop: schedules in the Stop hook, a tick the CLI starts, durable crons (~80s)",
        run: tasks::loop_tick,
    },
    Scenario {
        name: "stop_hook",
        tier: Tier::Full,
        about: "the Stop hook lists the jobs after a tick, a delete, a durable create (~80s)",
        run: tasks::stop_hook,
    },
    Scenario {
        name: "loop_resume",
        tier: Tier::Full,
        about: "/loop, a tick, a resume: the job is listed and ticks again (~3min)",
        run: tasks::loop_resume,
    },
];
