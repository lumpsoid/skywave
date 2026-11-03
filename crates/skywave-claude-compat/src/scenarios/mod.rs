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
mod tasks;
mod turns;

use crate::report::{Checks, Stop};
use crate::session::{Session, Setup, Shared};

/// How costly a scenario is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Control requests only, no user message: seconds, no tokens.
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
        name: "basic",
        tier: Tier::Full,
        about: "two turns: handle, text, Stop hook before result, usage",
        run: turns::basic,
    },
    Scenario {
        name: "permissions",
        tier: Tier::Full,
        about: "can_use_tool allow and deny, and whether a granted rule applies (d29758)",
        run: turns::permissions,
    },
    Scenario {
        name: "interrupt_pending",
        tier: Tier::Full,
        about: "interrupt while an approval waits: cancel, an aborted result, no Stop hook",
        run: turns::interrupt_pending,
    },
    Scenario {
        name: "background_bash",
        tier: Tier::Full,
        about: "a background shell: level set, notification, a turn the CLI starts (~40s)",
        run: tasks::background_bash,
    },
    Scenario {
        name: "stop_task",
        tier: Tier::Full,
        about: "stop_task on a background shell: event order before the ack",
        run: tasks::stop_task,
    },
    Scenario {
        name: "subagent",
        tier: Tier::Full,
        about: "a background subagent: its call, its lines, agent_id on its approval",
        run: tasks::subagent,
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
        name: "loop",
        tier: Tier::Full,
        about: "/loop: schedules in the Stop hook, a tick the CLI starts, durable crons (~80s)",
        run: tasks::loop_tick,
    },
];
