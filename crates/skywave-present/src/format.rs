//! Neutral wording: sizes, durations, token counts, costs, and how a
//! command went out or why it failed. Plain strings, no key hints and no
//! styling; a head adds its own around them.

use std::time::Duration;

use chrono::{DateTime, Utc};
use skywave_core::{
    AgentSetting, AgentView, AgentWork, BackgroundTask, BlobState, CallId, ContextFill,
    ContextView, ConversationItem, ConversationPicker, ConversationsStatus,
    ConversationsUnavailable, Decision, Delivery, DeviceRef, Effort, FailReason, ImportOutcome,
    Outgoing, Pause, PendingSession, PermissionMode, RejectReason, RuleScope, RuleSuggestion,
    RunWhen, Schedule, SessionId, SessionRef, SessionUsage, SessionView, ShellEnd, ShellRun,
    ShellRunState, SubagentStatus, TaskId, TaskKind,
};

use crate::text::clean_line;

/// How an own command went out, all by this device's clock. Once answered,
/// what each step took from the queueing: `queued 14:02:11 · sent +3.2s ·
/// 2 tries · answered +8.1s`. Until then, when it was queued and where it
/// is: `queued 14:02:11 · sending, try 3`, or `… · sent once, waiting for
/// the server` (sent, so it can no longer be taken back). No running
/// count: a wait is read off the clock, not watched second by second.
pub fn delivery_label(delivery: &Delivery, outgoing: &Outgoing) -> String {
    let queued = delivery.queued_at;
    // From the queueing to `ms`; never negative, should the clock step back.
    let since =
        |ms: i64| Duration::from_millis(u64::try_from(ms - queued.timestamp_millis()).unwrap_or(0));
    let tries = delivery.tries;
    let mut parts = vec![format!("queued {}", queued.format("%H:%M:%S"))];
    match delivery.answered_at {
        Some(answered) => {
            match delivery.first_sent_at {
                Some(sent) => {
                    parts.push(format!(
                        "sent +{}",
                        took_label(since(sent.timestamp_millis()))
                    ));
                }
                // Sent before send times were recorded.
                None if tries > 0 => parts.push("sent".to_owned()),
                None => {}
            }
            if tries > 1 {
                parts.push(format!("{tries} tries"));
            }
            let verb = match outgoing {
                Outgoing::Failed(_) => "refused",
                _ => "answered",
            };
            parts.push(format!(
                "{verb} +{}",
                took_label(since(answered.timestamp_millis()))
            ));
        }
        None => match (outgoing, tries) {
            (Outgoing::Sending, 0 | 1) => parts.push("sending".to_owned()),
            (Outgoing::Sending, n) => parts.push(format!("sending, try {n}")),
            (Outgoing::Queued, 0) => {}
            (Outgoing::Queued, 1) => parts.push("sent once, waiting for the server".to_owned()),
            (Outgoing::Queued, n) => {
                parts.push(format!("sent {n} times, waiting for the server"));
            }
            (Outgoing::Accepted(_) | Outgoing::Failed(_), _) => {}
        },
    }
    parts.join(" · ")
}

/// A call's duration: tenths below ten seconds.
pub fn took_label(took: Duration) -> String {
    if took < Duration::from_secs(10) {
        format!("{:.1}s", took.as_secs_f64())
    } else {
        seconds(took)
    }
}

/// Whole seconds, minutes or hours: "7s", "2m05s", "1h02m".
pub fn seconds(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
    }
}

/// A blob's size while it is not here; a downloaded blob is its file.
pub fn blob_size(state: &BlobState) -> Option<u64> {
    match state {
        BlobState::NotDownloaded { size }
        | BlobState::Requested { size }
        | BlobState::Downloading { size, .. }
        | BlobState::Failed { size, .. } => Some(*size),
        BlobState::Available { .. } => None,
    }
}

/// Where a blob's download stands, in full: what a head shows in place of
/// content that is not here (an expanded section, the crop page, a workdir
/// file): `not downloaded · 4.0 KiB`, `requested · waiting for a slot ·
/// 4.0 KiB`, `downloading · 1.0 KiB of 4.0 KiB (25%)`, with ` · paused,
/// offline` while the link is down (or another [`pause_note`]),
/// `download failed: {reason} · 4.0 KiB`,
/// `downloaded`. A head adds its own hint (how to download, to retry).
pub fn download_state(state: &BlobState) -> String {
    match state {
        BlobState::NotDownloaded { size } => format!("not downloaded · {}", bytes(*size)),
        BlobState::Requested { size } => {
            format!("requested · waiting for a slot · {}", bytes(*size))
        }
        BlobState::Downloading { got, size, paused } => format!(
            "downloading · {} of {} ({}%){}",
            bytes(*got),
            bytes(*size),
            percent(*got, *size),
            paused_word(*paused)
        ),
        BlobState::Failed { size, reason } => format!(
            "download failed: {} · {}",
            crate::text::clean_line(reason),
            bytes(*size)
        ),
        BlobState::Available { .. } => "downloaded".to_owned(),
    }
}

/// Where a blob's download stands once it started, briefly, for a note
/// that names its size beside it: `requested`, `downloading 25%`,
/// `downloading 25% · paused, offline`, `download failed`. `None` before
/// it started and once the blob is here.
pub fn download_brief(state: &BlobState) -> Option<String> {
    match state {
        BlobState::Requested { .. } => Some("requested".to_owned()),
        BlobState::Downloading { got, size, paused } => Some(format!(
            "downloading {}%{}",
            percent(*got, *size),
            paused_word(*paused)
        )),
        BlobState::Failed { .. } => Some("download failed".to_owned()),
        BlobState::NotDownloaded { .. } | BlobState::Available { .. } => None,
    }
}

fn paused_word(paused: Option<Pause>) -> String {
    paused.map_or(String::new(), |pause| format!(" · {}", pause_note(pause)))
}

/// What a paused download waits for, as every head words it after its
/// progress: "paused, offline", "paused, device disk full".
pub fn pause_note(pause: Pause) -> &'static str {
    match pause {
        Pause::Offline => "paused, offline",
        Pause::DiskFull => "paused, device disk full",
    }
}

/// `got` of `size` as a whole percentage, at most 100; an empty size is
/// done.
pub fn percent(got: u64, size: u64) -> u64 {
    (got.saturating_mul(100))
        .checked_div(size)
        .unwrap_or(100)
        .min(100)
}

/// A byte count in words: "812 B", "12.3 KiB", "4.0 MiB", "18.6 GiB".
pub fn bytes(n: u64) -> String {
    const KIB: f64 = 1024.0;
    match n {
        0..1024 => format!("{n} B"),
        1024..1_048_576 => format!("{:.1} KiB", n as f64 / KIB),
        1_048_576..1_073_741_824 => format!("{:.1} MiB", n as f64 / (KIB * KIB)),
        _ => format!("{:.1} GiB", n as f64 / (KIB * KIB * KIB)),
    }
}

/// A turn's length: "0.8s", "12.4s", "2m 05s", "1h 02m".
pub fn turn_time(ms: u64) -> String {
    let secs = ms / 1000;
    match secs {
        0..60 => format!("{:.1}s", ms as f64 / 1000.0),
        60..3600 => format!("{}m {:02}s", secs / 60, secs % 60),
        _ => format!("{}h {:02}m", secs / 3600, secs % 3600 / 60),
    }
}

/// A token count, compact: "812", "3.1k", "153k", "1.2M".
pub fn tokens(n: u64) -> String {
    let (value, unit) = match n {
        0..1000 => return n.to_string(),
        1000..999_500 => (n as f64 / 1e3, "k"),
        _ => (n as f64 / 1e6, "M"),
    };
    if value < 9.95 {
        format!("{value:.1}{unit}")
    } else {
        format!("{value:.0}{unit}")
    }
}

/// How full the current context is, for the session footer: `ctx
/// 19.3k/1M · 2%`, `ctx 19.3k` when the window is not known, `ctx ?` when
/// the fill is not (D46: after a context starts or is switched to, or a turn
/// that reported none). Never added to a subagent's figures.
pub fn context_label(fill: Option<ContextFill>) -> String {
    let Some(ContextFill {
        tokens: used,
        window,
    }) = fill
    else {
        return "ctx ?".to_owned();
    };
    match window.filter(|&w| w > 0) {
        Some(window) => {
            let percent = (used.saturating_mul(100) + window / 2) / window;
            let percent = if percent == 0 && used > 0 {
                "<1%".to_owned()
            } else {
                format!("{percent}%")
            };
            format!("ctx {}/{} · {percent}", tokens(used), window_size(window))
        }
        None => format!("ctx {}", tokens(used)),
    }
}

/// A context window, round: "1M", "200k", "1.5M".
fn window_size(n: u64) -> String {
    tokens(n).replace(".0", "")
}

/// What a session's turns took, for its footer, in order: `last turn
/// 12.4s`, `3.1k in / 812 out`, `$0.04`, the context fill
/// ([`context_label`]), `session $1.27`, `<model>` (with `model`; a head
/// that names the model elsewhere leaves it out). Figures the agent did not
/// report are left out.
pub fn session_usage_fields(usage: &SessionUsage, model: bool) -> Vec<String> {
    let turn = &usage.last_turn;
    let mut fields = Vec::new();
    if let Some(ms) = turn.duration_ms {
        fields.push(turn_time(ms));
    }
    match (turn.tokens_in, turn.tokens_out) {
        (Some(read), Some(written)) => {
            fields.push(format!("{} in / {} out", tokens(read), tokens(written)));
        }
        (Some(read), None) => fields.push(format!("{} in", tokens(read))),
        (None, Some(written)) => fields.push(format!("{} out", tokens(written))),
        (None, None) => {}
    }
    if let Some(cost) = turn.cost_usd {
        fields.push(dollars(cost));
    }
    if let Some(first) = fields.first_mut() {
        *first = format!("last turn {first}");
    }
    fields.push(context_label(usage.context));
    if let Some(cost) = usage.total.cost_usd {
        fields.push(format!("session {}", dollars(cost)));
    }
    if let Some(name) = usage.total.model.as_ref().filter(|_| model) {
        fields.push(clean_line(name));
    }
    fields
}

/// A cost in US dollars to 2–3 significant digits: "$0.0014", "$0.04",
/// "$0.87", "$1.27", "$12.3", "$140".
pub fn dollars(usd: f64) -> String {
    if usd.is_nan() || usd <= 0.0 {
        return "$0".to_owned();
    }
    if usd >= 1.0 {
        let decimals = if usd < 9.995 {
            2
        } else if usd < 99.95 {
            1
        } else {
            0
        };
        return format!("${usd:.decimals$}");
    }
    // Two significant digits, at least cents, without trailing zeros.
    let decimals = (1 - usd.log10().floor() as i32).max(2) as usize;
    let text = format!("{usd:.decimals$}");
    let (whole, fraction) = text.split_once('.').unwrap_or((&text, ""));
    let fraction = fraction.trim_end_matches('0');
    format!("${whole}.{fraction:0<2}")
}

/// A command's delivery state in words, with why it failed.
pub fn outgoing_label(outgoing: &Outgoing) -> String {
    match outgoing {
        Outgoing::Queued => "queued".to_owned(),
        Outgoing::Sending => "sending".to_owned(),
        Outgoing::Accepted(_) => "accepted".to_owned(),
        Outgoing::Failed(reason) => format!("failed: {}", fail_reason(*reason)),
    }
}

/// Why a command failed, in words.
pub fn fail_reason(reason: FailReason) -> &'static str {
    match reason {
        FailReason::Rejected(reason) => reject_reason(reason),
        FailReason::Undeliverable => "the server could not take it",
    }
}

/// Why the server refused a command, in words.
pub fn reject_reason(reason: RejectReason) -> &'static str {
    match reason {
        RejectReason::WorkdirNotAllowed => "not under a configured root on the server",
        RejectReason::WorkdirNotFound => "no such directory on the server",
        RejectReason::UnknownSession => "the server does not know this session",
        RejectReason::UnknownApproval => "the server does not know this approval",
        RejectReason::ApprovalAlreadyResolved => "already answered on another device",
        RejectReason::ApprovalCancelled => "the approval was cancelled first",
        RejectReason::NoActiveTurn => "the agent was not working",
        RejectReason::UnknownRule => "the agent did not offer that rule",
        RejectReason::BadAnswer => "that answer does not fit the question",
        RejectReason::UnknownModel => "no model named",
        RejectReason::ContextExists => "that context already exists",
        RejectReason::UnknownContext => "the session has no such context",
        RejectReason::ContextChanged => "the session moved to another context first",
        RejectReason::ContextMoved => "that context was moved to a session of its own",
        RejectReason::ContextCurrent => "that context is the current one",
        RejectReason::NoConversation => "the agent never ran in that context",
        RejectReason::BadConversation => "not an agent conversation id",
        RejectReason::UnknownTask => "that background task already ended",
        RejectReason::ShellDisabled => "shell commands are off on the server",
        RejectReason::UnknownRun => "the session has no such shell run",
        RejectReason::RunFinished => "that shell run already finished",
        RejectReason::BadShellCommand => "empty or too long shell command",
        RejectReason::BypassDisabled => "bypass mode is off on the server",
        RejectReason::MessageTooLong => "message too long",
        RejectReason::DiskLow => "the server's disk is low; free some space and send it again",
        RejectReason::ServerStopping => {
            "the server is stopping once its running work ends; send it again after"
        }
        RejectReason::UnknownBlob => "the server keeps no such content",
        RejectReason::BlobGone => "that content was already deleted or cropped",
        RejectReason::NotCroppable => "only tool and shell output can be cropped",
        RejectReason::BadCrop => "the kept lines do not fit that content",
    }
}

/// A permission mode in words, short enough for a status line: "ask",
/// "accept edits", "plan", "auto", "don't ask", "bypass".
pub fn mode_label(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Ask => "ask",
        PermissionMode::AcceptEdits => "accept edits",
        PermissionMode::Plan => "plan",
        PermissionMode::Auto => "auto",
        PermissionMode::DontAsk => "don't ask",
        PermissionMode::Bypass => "bypass",
    }
}

/// A reasoning effort in words: "low", "medium", "high", "extra high",
/// "max".
pub fn effort_label(effort: Effort) -> &'static str {
    match effort {
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
        Effort::XHigh => "extra high",
        Effort::Max => "max",
    }
}

/// What a mode lets the agent do, one line for a picker.
pub fn mode_detail(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Ask => "asks before any tool no rule allows",
        PermissionMode::AcceptEdits => "file edits run without asking",
        PermissionMode::Plan => "plans only, asks to leave with the plan",
        PermissionMode::Auto => "the agent decides what to ask about",
        PermissionMode::DontAsk => "never asks; what no rule allows is denied",
        PermissionMode::Bypass => "never asks; everything runs",
    }
}

/// Whether switching to `mode` lets every tool run unasked: a head asks
/// once more before sending it.
pub fn mode_needs_confirmation(mode: PermissionMode) -> bool {
    mode == PermissionMode::Bypass
}

/// The modes a picker offers, from the most asking to the least: every
/// mode, but bypass only where the server allows it (`bypass`, from its
/// hello); elsewhere the server refuses it, so it is not offered at all.
pub fn offered_modes(bypass: bool) -> Vec<PermissionMode> {
    PermissionMode::ALL
        .into_iter()
        .filter(|&mode| bypass || mode != PermissionMode::Bypass)
        .collect()
}

fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The activity in `work`, for where the user looks for it (a menu
/// subtitle, a picker title): `2 background tasks · 1 loop`. Ambient tasks
/// are not activity and are not counted. A schedule is always a "loop".
/// `None` when there is nothing to say.
pub fn work_summary(work: &AgentWork) -> Option<String> {
    let tasks = work.tasks.iter().filter(|task| !task.ambient).count();
    let loops = work.schedules.len();
    let parts: Vec<String> = [
        (tasks > 0).then(|| count(tasks, "background task", "background tasks")),
        (loops > 0).then(|| count(loops, "loop", "loops")),
    ]
    .into_iter()
    .flatten()
    .collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// What stopping the agent (Stop agent, clear, switch, move of the current
/// context) does to `work`, for the confirmation: background tasks end,
/// loops are paused and can come back with the next message; Skywave cannot
/// end a loop, the agent can. A loop saved with the folder says so.
/// `None` when [`stop_needs_confirmation`](crate::policy::stop_needs_confirmation)
/// is false.
pub fn stop_warning(work: &AgentWork) -> Option<String> {
    if !crate::policy::stop_needs_confirmation(work) {
        return None;
    }
    let tasks = work.tasks.iter().filter(|task| !task.ambient).count();
    let loops = work.schedules.len();
    let mut effects = Vec::new();
    if tasks > 0 {
        effects.push(format!(
            "ends {}",
            count(tasks, "background task", "background tasks")
        ));
    }
    if loops > 0 {
        effects.push(format!("pauses {}", count(loops, "loop", "loops")));
    }
    let mut text = format!("Stopping {}.", effects.join(" and "));
    if loops > 0 {
        text.push_str(
            " A paused loop can come back with your next message; to end it, ask the agent.",
        );
    }
    let durable = work
        .schedules
        .iter()
        .filter(|s| s.outlives_agent == Some(true))
        .count();
    if durable > 0 {
        let (noun, verb) = if durable == 1 {
            ("loop", "is")
        } else {
            ("loops", "are")
        };
        text.push_str(&format!(
            " {durable} {noun} {verb} saved in this folder and can fire in a later agent."
        ));
    }
    Some(text)
}

/// A row of the background work list.
pub enum BackgroundRow<'a> {
    /// A task, `depth` levels under a task that started it.
    Task(&'a BackgroundTask, usize),
    /// A loop.
    Loop(&'a Schedule),
}

/// The rows of the background work list: each task followed by the tasks it
/// started, then the loops. A task whose parent is not in the list reads as
/// a top task.
pub fn background_rows(work: &AgentWork) -> Vec<BackgroundRow<'_>> {
    fn add<'a>(
        work: &'a AgentWork,
        parent: Option<&TaskId>,
        depth: usize,
        rows: &mut Vec<BackgroundRow<'a>>,
    ) {
        for task in &work.tasks {
            let top = depth == 0
                && task
                    .parent
                    .as_ref()
                    .is_none_or(|p| !work.tasks.iter().any(|t| t.id == *p));
            if top || (depth > 0 && task.parent.as_ref() == parent) {
                rows.push(BackgroundRow::Task(task, depth));
                // Bounded: a cycle would otherwise never end.
                if depth < work.tasks.len() {
                    add(work, Some(&task.id), depth + 1, rows);
                }
            }
        }
    }
    let mut rows = Vec::new();
    add(work, None, 0, &mut rows);
    rows.extend(work.schedules.iter().map(BackgroundRow::Loop));
    rows
}

/// What kind of task a row is: `shell`, `subagent` or `task`.
pub fn task_kind_word(kind: TaskKind) -> &'static str {
    match kind {
        TaskKind::Shell => "shell",
        TaskKind::Subagent => "subagent",
        TaskKind::Other => "task",
    }
}

/// What an ambient task is called beside its title.
pub const AMBIENT_WORD: &str = "watcher";

/// Whether a loop runs again: `repeats` or `once`.
pub fn repeat_word(recurring: bool) -> &'static str {
    if recurring { "repeats" } else { "once" }
}

/// Beside a loop that is saved with the folder and can fire in a later agent.
pub const SAVED_IN_FOLDER: &str = "saved in this folder";

/// Beneath the loops: Skywave lists them but cannot end them.
pub const LOOPS_NOTE: &str = "Loops as of the last turn; ask the agent to end one.";

/// The list when there is nothing to show.
pub const NOTHING_RUNNING: &str = "Nothing is running in the background.";

/// An action that stops the agent, and so needs confirming when
/// [`stop_warning`] has something to say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopAction {
    /// Stop the agent.
    Agent,
    /// Start a new context.
    NewContext,
    /// Switch to another context.
    SwitchContext,
    /// Move the current context to a session of its own.
    MoveContext,
    /// Stop the task a subagent runs.
    Task,
}

/// The question a confirmation of `action` asks.
pub fn stop_heading(action: StopAction) -> &'static str {
    match action {
        StopAction::Agent => "Stop the agent?",
        StopAction::NewContext => "Start a new context?",
        StopAction::SwitchContext => "Switch context?",
        StopAction::MoveContext => "Move this context to a new session?",
        StopAction::Task => "Stop this subagent?",
    }
}

/// The mode for a status line: the one in effect, `→` a switch not yet
/// answered (`ask → plan`), `unknown` while the agent has not said
/// (`unknown → plan`).
pub fn agent_mode_label(agent: &AgentView) -> String {
    let current = agent.mode.map_or("unknown", mode_label);
    match &agent.mode_change {
        Some(change) if Some(change.value) != agent.mode => {
            format!("{current} → {}", mode_label(change.value))
        }
        _ => current.to_owned(),
    }
}

/// The model for a status line, as [`agent_mode_label`] does for the mode:
/// its offered label, else its name; `unknown → Opus` while one is asked
/// for before the agent said, and `None` while neither.
pub fn agent_model_label(agent: &AgentView) -> Option<String> {
    let current = agent.model_label();
    let change = agent.model_change.as_ref().map(|change| {
        agent
            .choice(&change.value)
            .map_or(change.value.as_str(), |m| m.label.as_str())
    });
    match (current, change) {
        (Some(current), Some(change)) if current != change => Some(format!("{current} → {change}")),
        (None, Some(change)) => Some(format!("unknown → {change}")),
        (current, _) => current.map(str::to_owned),
    }
}

/// The effort for a status line: `effort high` in effect, `high → max` with
/// a switch not yet answered, `unknown → max` before the agent said,
/// `no effort` when its model takes none (`no effort → high` once one is
/// asked for), and `None` while neither is known.
pub fn agent_effort_label(agent: &AgentView) -> Option<String> {
    let change = agent.effort_change.as_ref().map(|change| change.value);
    match (agent.effort, change) {
        (Some(current), Some(change)) if current != Some(change) => Some(format!(
            "{} → {}",
            current.map_or("no effort", effort_label),
            effort_label(change)
        )),
        (Some(Some(current)), _) => Some(format!("effort {}", effort_label(current))),
        (Some(None), _) => Some("no effort".to_owned()),
        (None, Some(change)) => Some(format!("unknown → {}", effort_label(change))),
        (None, None) => None,
    }
}

/// The agent's newest refusal of a mode, in words: `bypass refused: …`.
pub fn mode_refusal(agent: &AgentView) -> Option<String> {
    let refused = agent.mode_refused.as_ref()?;
    Some(format!(
        "{} refused: {}",
        mode_label(refused.value),
        refused.reason
    ))
}

/// The agent's newest refusal of a model, in words: `bogus refused: …`.
pub fn model_refusal(agent: &AgentView) -> Option<String> {
    let refused = agent.model_refused.as_ref()?;
    Some(format!("{} refused: {}", refused.value, refused.reason))
}

/// The agent's newest refusal of an effort, in words: `effort max refused: …`.
pub fn effort_refusal(agent: &AgentView) -> Option<String> {
    let refused = agent.effort_refused.as_ref()?;
    Some(format!(
        "effort {} refused: {}",
        effort_label(refused.value),
        refused.reason
    ))
}

/// All refusals, mode then model then effort, for a head that shows them
/// together.
pub fn agent_refusal(agent: &AgentView) -> Vec<String> {
    mode_refusal(agent)
        .into_iter()
        .chain(model_refusal(agent))
        .chain(effort_refusal(agent))
        .collect()
}

/// A timeline line for a mode or model change or refusal: `mode → plan`,
/// `model → claude-sonnet-5-5`, `mode bypass refused: …`. `who` names the
/// device that asked, when one did; the head words it.
pub fn agent_setting_line(setting: &AgentSetting, refused: Option<&str>) -> String {
    let (what, value) = match setting {
        AgentSetting::Mode(mode) => ("mode", mode_label(*mode).to_owned()),
        AgentSetting::Model(model) => ("model", model.clone()),
        AgentSetting::Effort(effort) => ("effort", effort.map_or("none", effort_label).to_owned()),
    };
    match refused {
        Some(reason) => format!("{what} {value} refused: {reason}"),
        None => format!("{what} → {value}"),
    }
}

/// A timeline warning that the agent runs a version the server was not
/// checked against: `agent 2.2.0 is not the version the server was checked
/// against (2.1.310) · turns may misbehave`. Heads show it as a warning.
pub fn agent_version_line(version: &str, confirmed: &str) -> String {
    format!(
        "agent {version} is not the version the server was checked against ({confirmed}) · \
         turns may misbehave"
    )
}

/// Who asked for a setting change, for after [`agent_setting_line`]: `by
/// laptop`, `by this device`, or nothing when the agent changed on its own.
pub fn asked_by(by: Option<&DeviceRef>) -> Option<String> {
    let by = by?;
    Some(if by.me {
        "by this device".to_owned()
    } else {
        format!("by {}", by.name.as_deref().unwrap_or("another device"))
    })
}

/// A decision in words: "approved", "denied".
pub fn decision_label(decision: Decision) -> &'static str {
    match decision {
        Decision::Approve => "approved",
        Decision::Deny => "denied",
    }
}

/// An answer in words: `approved always` when it granted a rule.
pub fn answer_words(decision: Decision, rule: Option<&RuleSuggestion>) -> &'static str {
    match rule {
        Some(_) => "approved always",
        None => decision_label(decision),
    }
}

/// How far a rule reaches, in words.
pub fn scope_label(scope: RuleScope) -> &'static str {
    match scope {
        RuleScope::Session => "this session",
        RuleScope::Project => "project",
        RuleScope::User => "user",
    }
}

/// Who did something the timeline notes: "you" for this device, else its
/// [label](DeviceRef::label).
pub fn device_word(by: &DeviceRef) -> String {
    if by.me { "you".to_owned() } else { by.label() }
}

/// The divider where a context was cleared: `New context · velvet walrus ·
/// by laptop`.
pub fn context_started_line(name: &str, by: &DeviceRef) -> String {
    format!("New context · {name} · by {}", device_word(by))
}

/// The divider where an earlier context was made current again: `Back to
/// start · by you`.
pub fn context_switched_line(name: &str, by: &DeviceRef) -> String {
    format!("Back to {name} · by {}", device_word(by))
}

/// A session the timeline names: its title quoted (`“fix the flaky
/// test”`), or `another session` for one this device deleted or has not
/// pulled yet.
pub fn session_word(session: &SessionRef) -> String {
    match &session.title {
        Some(title) => format!("“{title}”"),
        None => "another session".to_owned(),
    }
}

/// The divider where a context was moved into a session of its own:
/// `velvet walrus went on in “fix the flaky test” · by you`.
pub fn context_moved_line(name: &str, to: &SessionRef, by: &DeviceRef) -> String {
    format!(
        "{name} went on in {} · by {}",
        session_word(to),
        device_word(by)
    )
}

/// A moved context's new session's first item: `Continues velvet walrus
/// from “fix the flaky test”`.
pub fn moved_from_line(name: &str, from: &SessionRef) -> String {
    format!("Continues {name} from {}", session_word(from))
}

/// An adopted session's first item: `Continues agent conversation 3f2a from
/// outside Skywave`. Its imported history follows, up to the
/// [`import_finished_line`].
pub fn adopted_line(conversation: &str) -> String {
    format!("Continues agent conversation {conversation} from outside Skywave")
}

/// What a head marks a row of imported history with: it happened outside
/// Skywave, before the session adopted the conversation.
pub const IMPORTED_MARK: &str = "imported";

/// The divider where an adopted conversation's imported history ends and
/// the session's own begins. `Unavailable` says so too: with no history
/// above it, the timeline would otherwise not say why there is none.
pub fn import_finished_line(outcome: &ImportOutcome) -> String {
    match outcome {
        ImportOutcome::Imported { .. } => "Continued in Skywave from here".to_owned(),
        ImportOutcome::NotFound => {
            "history not found · continued in Skywave from here".to_owned()
        }
        ImportOutcome::Failed { reason } => {
            format!("history could not be read: {reason} · continued in Skywave from here")
        }
        ImportOutcome::Unavailable => {
            "history not imported: the server's agent cannot read it · continued in Skywave from here"
                .to_owned()
        }
    }
}

/// What a session row adds while an adopted conversation's history is
/// still arriving ([`SessionRow::importing`](skywave_core::SessionRow::importing)).
pub const IMPORTING_HISTORY: &str = "importing history…";

/// What a pending creation that adopts a conversation adds while it is not
/// on the server yet: the server imports the history as it creates the
/// session. `None` for one that adopts nothing, or once answered.
pub fn adopting_note(pending: &PendingSession) -> Option<&'static str> {
    pending.conversation.as_ref()?;
    match pending.outgoing {
        Outgoing::Sending => Some("Importing conversation history…"),
        Outgoing::Queued => Some("will import when online"),
        Outgoing::Accepted(_) | Outgoing::Failed(_) => None,
    }
}

/// How long ago `at` was at `now`, coarsely: `just now`, `12 min ago`,
/// `3 h ago`, `5 d ago`, then the date (`2026-10-01`). A time ahead of
/// `now` (another clock) reads `just now`.
pub fn ago(at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = (now - at).num_seconds();
    match secs {
        ..60 => "just now".to_owned(),
        60..3_600 => format!("{} min ago", secs / 60),
        3_600..86_400 => format!("{} h ago", secs / 3_600),
        86_400..2_592_000 => format!("{} d ago", secs / 86_400),
        _ => at.format("%Y-%m-%d").to_string(),
    }
}

/// A server path's last two components, `…/personal/skywave`; a shorter
/// path whole.
pub fn short_path(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() <= 2 {
        return path.to_owned();
    }
    format!("…/{}", parts[parts.len() - 2..].join("/"))
}

/// What a conversation picker row is called: the agent's title, else its
/// first prompt, else its id.
pub fn conversation_title(item: &ConversationItem) -> &str {
    item.title
        .as_deref()
        .or(item.first_prompt.as_deref())
        .unwrap_or(&item.id)
}

/// A conversation picker row's detail: `…/personal/skywave · 3 h ago · 42
/// messages`.
pub fn conversation_detail(item: &ConversationItem, now: DateTime<Utc>) -> String {
    format!(
        "{} · {} · {}",
        short_path(&item.workdir),
        ago(item.last_active, now),
        count(item.messages as usize, "message", "messages")
    )
}

/// Where a listed conversation already is in Skywave: `already in Skywave:
/// “fix the flaky test”` when a session runs it now, else `continued in
/// “fix the flaky test”` (`… and 2 more`) for sessions that adopted it
/// before. `title` names a session of the replica (`None`: not on this
/// list). `None` when Skywave has it nowhere.
pub fn conversation_link(
    item: &ConversationItem,
    title: impl Fn(SessionId) -> Option<String>,
) -> Option<String> {
    let word = |id: SessionId| {
        session_word(&SessionRef {
            id,
            title: title(id),
        })
    };
    if let Some(session) = item.session {
        return Some(format!("already in Skywave: {}", word(session)));
    }
    let (first, rest) = item.continued_in.split_first()?;
    Some(match rest.len() {
        0 => format!("continued in {}", word(*first)),
        more => format!("continued in {} and {more} more", word(*first)),
    })
}

/// Why the conversation picker lists nothing. A failure carries the
/// transport's words: heads sanitise it before writing it raw.
pub fn conversations_unavailable(why: &ConversationsUnavailable) -> String {
    match why {
        ConversationsUnavailable::Offline => {
            "offline · the server lists its conversations once connected".to_owned()
        }
        ConversationsUnavailable::AgentCannotList => {
            "can't list conversations: the server's agent cannot list its conversations".to_owned()
        }
        ConversationsUnavailable::Failed(reason) => {
            format!("can't list conversations: {reason}")
        }
    }
}

/// The conversation picker's footnote: still listing, or listed only the
/// newest. `None` for a full listing or none.
pub fn conversations_status(status: &ConversationsStatus) -> Option<&'static str> {
    match status {
        ConversationsStatus::Loading => Some("listing…"),
        ConversationsStatus::Ready { truncated: true } => Some("only the newest are listed"),
        ConversationsStatus::Ready { truncated: false } | ConversationsStatus::Unavailable(_) => {
            None
        }
    }
}

/// What the conversation picker shows in place of its rows while it has
/// none: that it is asking, that there are none, or why it has no listing
/// ([`conversations_unavailable`]). `None` while it has rows.
pub fn conversations_empty(picker: &ConversationPicker) -> Option<String> {
    if !picker.items.is_empty() {
        return None;
    }
    Some(match &picker.status {
        ConversationsStatus::Loading => "Asking the server for its agent's conversations…".into(),
        ConversationsStatus::Ready { .. } => {
            "The server's agent has no conversations under the roots.".into()
        }
        ConversationsStatus::Unavailable(why) => conversations_unavailable(why),
    })
}

/// What a picker adds to a moved context's row: `moved → “fix the flaky
/// test”`. `None` for one not moved.
pub fn context_moved_label(context: &ContextView) -> Option<String> {
    let to = context.moved_to.as_ref()?;
    Some(format!("moved → {}", session_word(to)))
}

/// Why a picker does not move a row the core does not offer to move. The core
/// says only yes or no, so a context the agent never answered in and one a
/// message of this device is still on its way to share the general words.
pub fn move_refusal(context: &ContextView) -> &'static str {
    if context.moved_to.is_some() {
        "already moved to another session"
    } else if context.outgoing.is_some() {
        "a move or message of yours to it is on its way"
    } else {
        "can't move yet: it needs an answered turn and no message of yours on its way"
    }
}

/// A context as a picker row reads: its name, then its first prompt
/// (`velvet walrus · fix the flaky test`), or the name alone while it has
/// no message.
pub fn context_row(context: &ContextView) -> String {
    match &context.first_prompt {
        Some(prompt) => format!("{} · {prompt}", context.name),
        None => context.name.clone(),
    }
}

/// The hint while the timeline shows a context other than the one messages
/// go to: `Viewing start — messages go to velvet walrus`. `None` while the
/// whole session or the current context shows.
pub fn viewing_hint(view: &SessionView) -> Option<String> {
    let viewing = view.viewing.filter(|viewing| *viewing != view.context)?;
    let name = |id| {
        view.contexts
            .iter()
            .find(|c| c.id == id)
            .map(|c| c.name.as_str())
    };
    let target = name(view.context)?;
    Some(match name(viewing) {
        Some(shown) => format!("Viewing {shown} — messages go to {target}"),
        None => format!("Viewing an earlier context — messages go to {target}"),
    })
}

/// How a subagent stands, for the row of its `Agent` call and the header of
/// its view: `running · 14 steps · Write src/sync.rs`, then `finished · 14
/// steps`. What it did last is said only while it runs. Once its agent
/// reported what it ran, what it reported: `finished · 1 step · 4.3s · ctx
/// 11.9k`, its own count of tool calls for the steps and its context at
/// its end (D46), never "tokens used".
pub fn subagent_status(status: &SubagentStatus) -> String {
    let usage = status.usage.as_ref().filter(|_| !status.live);
    let steps = usage
        .and_then(|u| u.tool_uses)
        .map_or(status.steps as usize, |n| n as usize);
    let steps = count(steps, "step", "steps");
    match (&status.last, status.live) {
        (Some(last), true) => format!("running · {steps} · {last}"),
        (None, true) => format!("running · {steps}"),
        (_, false) => {
            let mut line = format!("finished · {steps}");
            if let Some(ms) = usage.and_then(|u| u.duration_ms) {
                line.push_str(&format!(" · {}", turn_time(ms)));
            }
            if let Some(context) = usage.and_then(|u| u.context_tokens) {
                line.push_str(&format!(" · ctx {}", tokens(context)));
            }
            line
        }
    }
}

/// The subagents a view is inside, outermost first, as a breadcrumb:
/// `outer › inner`. Empty for a subagent the main agent started.
pub fn trail(trail: &[(CallId, String)]) -> String {
    let titles: Vec<&str> = trail.iter().map(|(_, title)| title.as_str()).collect();
    titles.join(TRAIL_SEPARATOR)
}

/// Between the names of a breadcrumb.
pub const TRAIL_SEPARATOR: &str = " › ";

/// Where an approval comes from when a subagent asked for it: `from
/// subagent: Explore`. `shown` is whether the view has the gated call; in
/// the main view it has not, and the label says where to look.
pub fn approval_origin(from: &str, shown: bool) -> String {
    if shown {
        format!("from subagent: {from}")
    } else {
        format!("from subagent: {from} · open it to see the call")
    }
}

/// How a shell run ended: `exit 0`, `exit 1`, `killed by signal 9`,
/// `stopped`, `skipped: previous failed` (an `if ok` run) or `skipped:
/// previous succeeded` (an `if failed` run), `lost: daemon restarted`,
/// `could not start: <reason>`. `when` is the run's condition, which is
/// what a skip went by.
pub fn shell_end_label(end: &ShellEnd, when: RunWhen) -> String {
    match end {
        ShellEnd::Exited { code } => format!("exit {code}"),
        ShellEnd::Signalled { signal } => format!("killed by signal {signal}"),
        ShellEnd::Stopped => "stopped".to_owned(),
        ShellEnd::Skipped => match when {
            RunWhen::OnSuccess => "skipped: previous failed",
            RunWhen::OnFailure => "skipped: previous succeeded",
            RunWhen::Always => "skipped",
        }
        .to_owned(),
        ShellEnd::Lost => "lost: daemon restarted".to_owned(),
        ShellEnd::NotStarted { reason } => format!("could not start: {reason}"),
    }
}

/// The marker of a run's condition, in a run's row and beside the shell
/// prompt: `if ok ✓`, `if failed ✗`; `None` for one that always runs.
/// Words, never `&&`, `||` or `;`: the command text is never parsed, and
/// the marker must not read as part of it.
pub fn when_marker(when: RunWhen) -> Option<&'static str> {
    match when {
        RunWhen::Always => None,
        RunWhen::OnSuccess => Some("if ok ✓"),
        RunWhen::OnFailure => Some("if failed ✗"),
    }
}

/// The shell composer's prompt, in parts so a head can style or wire each
/// one (the GUI's marker is a chip that steps the condition). Its
/// [`Display`](std::fmt::Display) is the whole prompt: `skywave [if ok ✓] $`,
/// `/ $`, or a bare `$` before any cwd is known. The marker is never part
/// of the command written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShellPrompt<'a> {
    /// The last part of the session's last known cwd: `skywave`, `/`;
    /// `None` before any is known. Unsanitized: a head cleans it.
    pub cwd: Option<&'a str>,
    /// The condition the next command runs on, as [`when_marker`].
    pub marker: Option<&'static str>,
    /// What closes the prompt: `$`.
    pub sigil: &'static str,
}

impl std::fmt::Display for ShellPrompt<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(cwd) = self.cwd {
            write!(f, "{cwd} ")?;
        }
        if let Some(marker) = self.marker {
            write!(f, "[{marker}] ")?;
        }
        f.write_str(self.sigil)
    }
}

/// The shell composer's prompt for a session last seen in `cwd`, with
/// the condition `when` the next command runs on.
pub fn shell_prompt(cwd: Option<&str>, when: RunWhen) -> ShellPrompt<'_> {
    let tail = cwd.filter(|cwd| !cwd.is_empty()).map(|cwd| {
        let trimmed = cwd.trim_end_matches('/');
        match trimmed.rsplit_once('/') {
            _ if trimmed.is_empty() => "/",
            Some((_, tail)) => tail,
            None => trimmed,
        }
    });
    ShellPrompt {
        cwd: tail,
        marker: when_marker(when),
        sigil: "$",
    }
}

/// What a shell run took: from its start to its end, or to `now` while it
/// runs, as [`took_label`]. `None` for one that never started.
pub fn shell_took(run: &ShellRun, now: DateTime<Utc>) -> Option<String> {
    let started = run.started_at?;
    let end = run.finished_at.unwrap_or(now);
    Some(took_label((end - started).to_std().unwrap_or_default()))
}

/// When a run not started yet was queued: `queued 14:02:11`. From its
/// logged request (the server's clock), or for this device's run not in
/// the log yet from when it was queued here. No running count, as
/// [`delivery_label`]. `None` once started, for a
/// [`refused`](ShellRun::refused) run (it is not queued: it will never
/// run), or for a run whose id carries no time.
pub fn queued_label(run: &ShellRun) -> Option<String> {
    if !matches!(run.state, ShellRunState::Pending | ShellRunState::Queued) || run.refused() {
        return None;
    }
    let since = run
        .requested_at
        .or(run.delivery.map(|delivery| delivery.queued_at))?;
    Some(format!("queued {}", since.format("%H:%M:%S")))
}

#[cfg(test)]
mod tests {
    use skywave_core::{
        BackgroundTask, CommandId, ModelChoice, Refusal, Schedule, ScheduleId, SettingChange,
        TaskId, TaskKind,
    };

    use super::*;

    fn conversation(title: Option<&str>, first_prompt: Option<&str>) -> ConversationItem {
        ConversationItem {
            id: "3f2a".into(),
            workdir: "/home/me/Projects/personal/skywave".into(),
            title: title.map(Into::into),
            first_prompt: first_prompt.map(Into::into),
            last_active: DateTime::parse_from_rfc3339("2026-11-03T09:00:00Z")
                .unwrap()
                .to_utc(),
            messages: 42,
            session: None,
            continued_in: Vec::new(),
        }
    }

    #[test]
    fn a_picker_row_is_called_by_its_title_else_its_first_prompt_else_its_id() {
        assert_eq!(
            conversation_title(&conversation(Some("Fix the test"), Some("fix it"))),
            "Fix the test"
        );
        assert_eq!(
            conversation_title(&conversation(None, Some("fix it"))),
            "fix it"
        );
        assert_eq!(conversation_title(&conversation(None, None)), "3f2a");
    }

    #[test]
    fn a_picker_row_says_where_how_long_ago_and_how_long() {
        let item = conversation(None, None);
        let at = |rfc: &str| DateTime::parse_from_rfc3339(rfc).unwrap().to_utc();
        assert_eq!(
            conversation_detail(&item, at("2026-11-03T12:10:00Z")),
            "…/personal/skywave · 3 h ago · 42 messages"
        );
        let one = ConversationItem {
            messages: 1,
            workdir: "/srv/app".into(),
            ..item.clone()
        };
        assert_eq!(
            conversation_detail(&one, at("2026-11-03T09:00:30Z")),
            "/srv/app · just now · 1 message"
        );
        // Every step of the scale, and a clock behind the agent's.
        let ago_at = |now: &str| ago(item.last_active, at(now));
        assert_eq!(ago_at("2026-11-03T08:59:00Z"), "just now");
        assert_eq!(ago_at("2026-11-03T09:12:00Z"), "12 min ago");
        assert_eq!(ago_at("2026-11-08T10:00:00Z"), "5 d ago");
        assert_eq!(ago_at("2026-12-25T09:00:00Z"), "2026-11-03");
    }

    #[test]
    fn a_picker_row_says_where_skywave_has_it() {
        let item = conversation(None, None);
        let (here, other, gone) = (SessionId::new(), SessionId::new(), SessionId::new());
        let title = |id: SessionId| (id == here).then(|| "fix the flaky test".to_owned());
        assert_eq!(conversation_link(&item, title), None);
        let continued = ConversationItem {
            continued_in: vec![here],
            ..item.clone()
        };
        assert_eq!(
            conversation_link(&continued, title).as_deref(),
            Some("continued in “fix the flaky test”")
        );
        let twice = ConversationItem {
            continued_in: vec![gone, here, other],
            ..item.clone()
        };
        assert_eq!(
            conversation_link(&twice, title).as_deref(),
            Some("continued in another session and 2 more")
        );
        // Running in a session now wins over earlier adoptions.
        let running = ConversationItem {
            session: Some(here),
            ..twice
        };
        assert_eq!(
            conversation_link(&running, title).as_deref(),
            Some("already in Skywave: “fix the flaky test”")
        );
    }

    #[test]
    fn an_unavailable_listing_says_offline_or_why() {
        assert_eq!(
            conversations_unavailable(&ConversationsUnavailable::Offline),
            "offline · the server lists its conversations once connected"
        );
        assert_eq!(
            conversations_unavailable(&ConversationsUnavailable::AgentCannotList),
            "can't list conversations: the server's agent cannot list its conversations"
        );
        assert_eq!(
            conversations_unavailable(&ConversationsUnavailable::Failed(
                "unexpected HTTP status 503".into()
            )),
            "can't list conversations: unexpected HTTP status 503"
        );
    }

    #[test]
    fn the_picker_footnote_and_empty_text_follow_its_status() {
        let picker = |status| ConversationPicker {
            status,
            items: Vec::new(),
        };
        let loading = picker(ConversationsStatus::Loading);
        assert_eq!(conversations_status(&loading.status), Some("listing…"));
        assert_eq!(
            conversations_empty(&loading).as_deref(),
            Some("Asking the server for its agent's conversations…")
        );
        let none = picker(ConversationsStatus::Ready { truncated: false });
        assert_eq!(conversations_status(&none.status), None);
        assert_eq!(
            conversations_empty(&none).as_deref(),
            Some("The server's agent has no conversations under the roots.")
        );
        let cut = ConversationsStatus::Ready { truncated: true };
        assert_eq!(
            conversations_status(&cut),
            Some("only the newest are listed")
        );
        let offline = picker(ConversationsStatus::Unavailable(
            ConversationsUnavailable::Offline,
        ));
        assert_eq!(conversations_status(&offline.status), None);
        assert_eq!(
            conversations_empty(&offline),
            Some(conversations_unavailable(
                &ConversationsUnavailable::Offline
            ))
        );
        // With rows there is nothing to say in their place.
        let listed = ConversationPicker {
            items: vec![conversation(Some("fix it"), None)],
            ..none
        };
        assert_eq!(conversations_empty(&listed), None);
    }

    #[test]
    fn every_import_outcome_draws_its_divider() {
        assert_eq!(
            import_finished_line(&ImportOutcome::Imported { items: 12 }),
            "Continued in Skywave from here"
        );
        assert_eq!(
            import_finished_line(&ImportOutcome::NotFound),
            "history not found · continued in Skywave from here"
        );
        assert_eq!(
            import_finished_line(&ImportOutcome::Failed {
                reason: "bad line 3".into()
            }),
            "history could not be read: bad line 3 · continued in Skywave from here"
        );
        assert_eq!(
            import_finished_line(&ImportOutcome::Unavailable),
            "history not imported: the server's agent cannot read it · continued in Skywave from here"
        );
    }

    #[test]
    fn a_pending_adopting_creation_says_it_imports() {
        let pending = |conversation: Option<&str>, outgoing| PendingSession {
            command_id: CommandId::new(),
            workdir: "/srv/app".into(),
            conversation: conversation.map(Into::into),
            outgoing,
            discardable: false,
        };
        assert_eq!(
            adopting_note(&pending(Some("3f2a"), Outgoing::Sending)),
            Some("Importing conversation history…")
        );
        assert_eq!(
            adopting_note(&pending(Some("3f2a"), Outgoing::Queued)),
            Some("will import when online")
        );
        assert_eq!(
            adopting_note(&pending(
                Some("3f2a"),
                Outgoing::Failed(FailReason::Rejected(RejectReason::BadShellCommand))
            )),
            None
        );
        assert_eq!(adopting_note(&pending(None, Outgoing::Sending)), None);
        assert_eq!(adopting_note(&pending(None, Outgoing::Queued)), None);
    }

    #[test]
    fn every_shell_end_is_worded() {
        let always = RunWhen::Always;
        let cases = [
            (ShellEnd::Exited { code: 0 }, always, "exit 0"),
            (ShellEnd::Exited { code: 1 }, always, "exit 1"),
            (
                ShellEnd::Signalled { signal: 9 },
                always,
                "killed by signal 9",
            ),
            (ShellEnd::Stopped, always, "stopped"),
            (
                ShellEnd::Skipped,
                RunWhen::OnSuccess,
                "skipped: previous failed",
            ),
            (
                ShellEnd::Skipped,
                RunWhen::OnFailure,
                "skipped: previous succeeded",
            ),
            (ShellEnd::Lost, always, "lost: daemon restarted"),
            (
                ShellEnd::NotStarted {
                    reason: "shell disabled".into(),
                },
                always,
                "could not start: shell disabled",
            ),
        ];
        for (end, when, text) in cases {
            assert_eq!(shell_end_label(&end, when), text, "{end:?}");
        }
    }

    #[test]
    fn run_conditions_read_as_words_not_shell_operators() {
        assert_eq!(when_marker(RunWhen::Always), None);
        assert_eq!(when_marker(RunWhen::OnSuccess), Some("if ok ✓"));
        assert_eq!(when_marker(RunWhen::OnFailure), Some("if failed ✗"));
        for when in [RunWhen::OnSuccess, RunWhen::OnFailure] {
            let marker = when_marker(when).unwrap();
            assert!(!["&&", "||", ";"].iter().any(|op| marker.contains(op)));
        }
    }

    #[test]
    fn the_shell_prompt_names_the_cwd_tail() {
        let line = |cwd| shell_prompt(cwd, RunWhen::Always).to_string();
        assert_eq!(line(Some("/srv/skywave")), "skywave $");
        assert_eq!(line(Some("/srv/skywave/")), "skywave $");
        assert_eq!(line(Some("/")), "/ $");
        assert_eq!(line(Some("relative")), "relative $");
        assert_eq!(line(Some("")), "$");
        assert_eq!(line(None), "$");
    }

    #[test]
    fn the_shell_prompt_puts_the_condition_before_the_sigil() {
        let prompt = shell_prompt(Some("/srv/skywave"), RunWhen::OnSuccess);
        assert_eq!(
            prompt,
            ShellPrompt {
                cwd: Some("skywave"),
                marker: Some("if ok ✓"),
                sigil: "$",
            }
        );
        assert_eq!(prompt.to_string(), "skywave [if ok ✓] $");
        assert_eq!(
            shell_prompt(None, RunWhen::OnFailure).to_string(),
            "[if failed ✗] $"
        );
        let always = shell_prompt(Some("/srv/skywave"), RunWhen::Always);
        assert_eq!((always.cwd, always.marker), (Some("skywave"), None));
    }

    fn shell_run(state: ShellRunState) -> ShellRun {
        ShellRun {
            run: skywave_core::CommandId::new(),
            command: "make".into(),
            when: RunWhen::Always,
            by: DeviceRef {
                id: skywave_core::DeviceId::new(),
                name: None,
                me: true,
            },
            state,
            requested_at: None,
            started_at: None,
            finished_at: None,
            tail_or_preview: String::new(),
            lines: 0,
            output: None,
            truncated: false,
            outgoing: None,
            discardable: false,
            stopping: false,
            delivery: None,
        }
    }

    #[test]
    fn a_run_takes_from_its_start_to_its_end_or_now() {
        use chrono::TimeZone;
        let at = |s: i64| Utc.timestamp_opt(1_700_000_000 + s, 0).unwrap();
        let mut run = shell_run(ShellRunState::Running);
        assert_eq!(shell_took(&run, at(5)), None, "not started");
        run.started_at = Some(at(0));
        assert_eq!(shell_took(&run, at(3)).as_deref(), Some("3.0s"));
        run.state = ShellRunState::Finished(ShellEnd::Exited { code: 0 });
        run.finished_at = Some(at(125));
        assert_eq!(shell_took(&run, at(9000)).as_deref(), Some("2m05s"));
    }

    #[test]
    fn a_waiting_run_says_when_it_was_queued() {
        use chrono::TimeZone;
        // 2023-11-14 22:13:20 UTC.
        let at = |s: i64| Utc.timestamp_opt(1_700_000_000 + s, 0).unwrap();
        let mut run = shell_run(ShellRunState::Queued);
        assert_eq!(queued_label(&run), None, "no time known");
        run.requested_at = Some(at(0));
        assert_eq!(queued_label(&run).as_deref(), Some("queued 22:13:20"));
        // This device's run not logged yet goes by when it was queued here.
        let mut pending = shell_run(ShellRunState::Pending);
        pending.delivery = Some(Delivery {
            queued_at: at(90),
            first_sent_at: None,
            answered_at: None,
            tries: 0,
        });
        assert_eq!(queued_label(&pending).as_deref(), Some("queued 22:14:50"));
        // Refused, it is not queued at all.
        pending.outgoing = Some(Outgoing::Failed(FailReason::Rejected(
            RejectReason::BadShellCommand,
        )));
        assert_eq!(queued_label(&pending), None, "refused");
        run.state = ShellRunState::Running;
        assert_eq!(queued_label(&run), None, "started");
    }

    fn task(ambient: bool) -> BackgroundTask {
        BackgroundTask {
            call: None,
            id: TaskId("t".into()),
            kind: TaskKind::Shell,
            title: "t".into(),
            parent: None,
            ambient,
        }
    }

    fn status(live: bool, steps: u32, last: Option<&str>) -> SubagentStatus {
        SubagentStatus {
            task: None,
            live,
            steps,
            last: last.map(str::to_owned),
            usage: None,
        }
    }

    #[test]
    fn a_finished_subagent_says_what_its_agent_reported_it_ran() {
        let figures = |tool_uses, duration_ms, context_tokens| SubagentStatus {
            usage: Some(skywave_core::SubagentUsage {
                call: CallId::new(),
                duration_ms,
                tool_uses,
                context_tokens,
            }),
            ..status(false, 3, None)
        };
        assert_eq!(
            subagent_status(&figures(Some(1), Some(4_284), Some(11_850))),
            "finished · 1 step · 4.3s · ctx 12k"
        );
        assert_eq!(
            subagent_status(&figures(Some(6), Some(61_000), Some(13_097))),
            "finished · 6 steps · 1m 01s · ctx 13k"
        );
        // A figure not reported is left out; the steps are then its own.
        assert_eq!(
            subagent_status(&figures(None, None, Some(9_936))),
            "finished · 3 steps · ctx 9.9k"
        );
        // Stopped: none reported.
        assert_eq!(
            subagent_status(&status(false, 3, None)),
            "finished · 3 steps"
        );
        // While it runs, none is shown.
        let running = SubagentStatus {
            live: true,
            ..figures(Some(1), Some(4_284), Some(11_850))
        };
        assert_eq!(subagent_status(&running), "running · 3 steps");
    }

    #[test]
    fn the_context_label_says_the_fill_of_the_window_or_unknown() {
        let fill = |tokens, window| Some(ContextFill { tokens, window });
        assert_eq!(
            context_label(fill(19_337, Some(1_000_000))),
            "ctx 19k/1M · 2%"
        );
        assert_eq!(
            context_label(fill(150_000, Some(200_000))),
            "ctx 150k/200k · 75%"
        );
        assert_eq!(
            context_label(fill(3_010, Some(1_000_000))),
            "ctx 3.0k/1M · <1%"
        );
        assert_eq!(context_label(fill(812, None)), "ctx 812");
        assert_eq!(context_label(fill(812, Some(0))), "ctx 812");
        assert_eq!(context_label(None), "ctx ?");
    }

    #[test]
    fn the_session_usage_fields_put_the_fill_after_the_last_turn() {
        let turn = skywave_core::TurnUsage {
            duration_ms: Some(12_400),
            tokens_in: Some(3_100),
            tokens_out: Some(812),
            cost_usd: Some(0.04),
            model: Some("model-a".into()),
            context_tokens: Some(19_337),
            context_window: Some(1_000_000),
        };
        let mut usage = SessionUsage::new(&turn);
        assert_eq!(
            session_usage_fields(&usage, true),
            [
                "last turn 12.4s",
                "3.1k in / 812 out",
                "$0.04",
                "ctx 19k/1M · 2%",
                "session $0.04",
                "model-a",
            ]
        );
        // A turn that reported no fill: unknown.
        usage.add(&skywave_core::TurnUsage {
            duration_ms: Some(30_000),
            ..Default::default()
        });
        assert_eq!(
            session_usage_fields(&usage, false),
            ["last turn 30.0s", "ctx ?", "session $0.04"]
        );
    }

    #[test]
    fn a_subagent_status_says_running_with_what_it_did_last_then_finished() {
        assert_eq!(
            subagent_status(&status(true, 14, Some("Write src/sync.rs"))),
            "running · 14 steps · Write src/sync.rs"
        );
        assert_eq!(
            subagent_status(&status(true, 3, Some("Read a.rs"))),
            "running · 3 steps · Read a.rs"
        );
        assert_eq!(subagent_status(&status(true, 1, None)), "running · 1 step");
        assert_eq!(subagent_status(&status(true, 0, None)), "running · 0 steps");
        assert_eq!(
            subagent_status(&status(false, 14, Some("Write src/sync.rs"))),
            "finished · 14 steps"
        );
        assert_eq!(
            subagent_status(&status(false, 3, None)),
            "finished · 3 steps"
        );
    }

    #[test]
    fn a_trail_joins_the_outer_subagents_and_is_empty_for_a_direct_one() {
        assert_eq!(trail(&[]), "");
        let outer = (CallId::new(), "Explore".to_owned());
        let middle = (CallId::new(), "Plan".to_owned());
        assert_eq!(trail(std::slice::from_ref(&outer)), "Explore");
        assert_eq!(trail(&[outer, middle]), "Explore › Plan");
    }

    #[test]
    fn an_approval_from_a_subagent_says_where_to_look_when_its_call_is_not_shown() {
        assert_eq!(approval_origin("Explore", true), "from subagent: Explore");
        assert_eq!(
            approval_origin("Explore", false),
            "from subagent: Explore · open it to see the call"
        );
    }

    fn schedule(outlives_agent: Option<bool>) -> Schedule {
        Schedule {
            id: ScheduleId("s".into()),
            when: "*/5 * * * *".into(),
            recurring: true,
            prompt: "check".into(),
            outlives_agent,
        }
    }

    fn work(tasks: Vec<BackgroundTask>, schedules: Vec<Schedule>) -> AgentWork {
        AgentWork { tasks, schedules }
    }

    #[test]
    fn the_work_summary_counts_activity_only() {
        assert_eq!(work_summary(&work(vec![], vec![])), None);
        assert_eq!(work_summary(&work(vec![task(true)], vec![])), None);
        assert_eq!(
            work_summary(&work(vec![task(false)], vec![])).as_deref(),
            Some("1 background task")
        );
        assert_eq!(
            work_summary(&work(
                vec![task(false), task(false), task(true)],
                vec![schedule(None)]
            ))
            .as_deref(),
            Some("2 background tasks · 1 loop")
        );
        assert_eq!(
            work_summary(&work(vec![], vec![schedule(None), schedule(None)])).as_deref(),
            Some("2 loops")
        );
    }

    #[test]
    fn the_stop_warning_pauses_loops_and_never_ends_them() {
        assert_eq!(stop_warning(&work(vec![], vec![])), None);
        assert_eq!(stop_warning(&work(vec![task(true)], vec![])), None);
        assert_eq!(
            stop_warning(&work(vec![task(false)], vec![])).as_deref(),
            Some("Stopping ends 1 background task.")
        );
        let both = stop_warning(&work(
            vec![task(false), task(false)],
            vec![schedule(Some(false))],
        ))
        .unwrap();
        assert!(both.starts_with("Stopping ends 2 background tasks and pauses 1 loop."));
        assert!(both.contains("to end it, ask the agent"));
        assert!(!both.contains("saved in this folder"));
        for text in [
            both,
            stop_warning(&work(vec![], vec![schedule(None), schedule(None)])).unwrap(),
        ] {
            assert!(text.contains("pauses"));
            assert!(!text.contains("ends 1 loop") && !text.contains("ends 2 loops"));
            assert!(!text.contains("and ends"));
        }
    }

    #[test]
    fn the_stop_warning_names_durable_loops_only_when_there_are_some() {
        let one = stop_warning(&work(vec![], vec![schedule(Some(true)), schedule(None)])).unwrap();
        assert!(one.contains("1 loop is saved in this folder and can fire in a later agent."));
        let two = stop_warning(&work(
            vec![],
            vec![schedule(Some(true)), schedule(Some(true))],
        ))
        .unwrap();
        assert!(two.contains("2 loops are saved in this folder"));
        let none = stop_warning(&work(vec![], vec![schedule(None)])).unwrap();
        assert!(!none.contains("saved in this folder"));
    }

    #[test]
    fn background_rows_put_children_under_parents_and_loops_last() {
        let mut parent = task(false);
        parent.id = TaskId("p".into());
        let mut child = task(false);
        child.id = TaskId("c".into());
        child.parent = Some(TaskId("p".into()));
        let mut orphan = task(false);
        orphan.id = TaskId("o".into());
        orphan.parent = Some(TaskId("gone".into()));
        // The child is listed first: it still follows its parent.
        let w = work(vec![child, parent, orphan], vec![schedule(None)]);
        let shape: Vec<String> = background_rows(&w)
            .iter()
            .map(|row| match row {
                BackgroundRow::Task(t, depth) => format!("{}{}", depth, t.id.0),
                BackgroundRow::Loop(_) => "loop".to_owned(),
            })
            .collect();
        assert_eq!(shape, ["0p", "1c", "0o", "loop"]);
    }

    #[test]
    fn background_rows_survive_a_parent_cycle() {
        let mut a = task(false);
        a.id = TaskId("a".into());
        a.parent = Some(TaskId("b".into()));
        let mut b = task(false);
        b.id = TaskId("b".into());
        b.parent = Some(TaskId("a".into()));
        // Neither is a top task, so neither is listed; what matters is that
        // it ends.
        assert!(background_rows(&work(vec![a, b], vec![])).is_empty());
    }

    #[test]
    fn the_work_words() {
        assert_eq!(task_kind_word(TaskKind::Shell), "shell");
        assert_eq!(task_kind_word(TaskKind::Subagent), "subagent");
        assert_eq!(task_kind_word(TaskKind::Other), "task");
        assert_eq!(repeat_word(true), "repeats");
        assert_eq!(repeat_word(false), "once");
        assert_eq!(stop_heading(StopAction::Agent), "Stop the agent?");
        assert_eq!(stop_heading(StopAction::NewContext), "Start a new context?");
        assert_eq!(stop_heading(StopAction::SwitchContext), "Switch context?");
        assert_eq!(
            stop_heading(StopAction::MoveContext),
            "Move this context to a new session?"
        );
    }

    #[test]
    fn agent_settings_read_as_current_then_switch() {
        let mut agent = AgentView {
            mode: Some(PermissionMode::Ask),
            model: Some("model-s".into()),
            models: vec![ModelChoice {
                id: "sonnet".into(),
                label: "Sonnet".into(),
                detail: None,
                resolved: Some("model-s".into()),
                efforts: Vec::new(),
            }],
            ..AgentView::default()
        };
        assert_eq!(agent_mode_label(&agent), "ask");
        assert_eq!(agent_model_label(&agent).as_deref(), Some("Sonnet"));
        agent.mode_change = Some(SettingChange {
            value: PermissionMode::Plan,
            outgoing: None,
        });
        agent.model_change = Some(SettingChange {
            value: "opus".into(),
            outgoing: None,
        });
        assert_eq!(agent_mode_label(&agent), "ask → plan");
        assert_eq!(agent_model_label(&agent).as_deref(), Some("Sonnet → opus"));
        assert_eq!(agent_mode_label(&AgentView::default()), "unknown");
        assert_eq!(agent_model_label(&AgentView::default()), None);
        let asked = AgentView {
            model_change: Some(SettingChange {
                value: "opus".into(),
                outgoing: None,
            }),
            ..AgentView::default()
        };
        assert_eq!(agent_model_label(&asked).as_deref(), Some("unknown → opus"));
        assert_eq!(
            agent_setting_line(&AgentSetting::Mode(PermissionMode::Bypass), Some("no")),
            "mode bypass refused: no"
        );
    }

    #[test]
    fn bypass_is_offered_only_where_the_server_allows_it() {
        use PermissionMode::*;
        let safe = [Ask, AcceptEdits, Plan, Auto, DontAsk];
        assert_eq!(offered_modes(false), safe);
        assert_eq!(offered_modes(true), PermissionMode::ALL);
        // Offered, it still asks once more; no other mode does.
        assert!(mode_needs_confirmation(Bypass));
        assert!(!safe.into_iter().any(mode_needs_confirmation));
    }

    #[test]
    fn effort_reads_as_current_then_switch() {
        let change = |effort| {
            Some(SettingChange {
                value: effort,
                outgoing: None,
            })
        };
        let label = |effort, effort_change| {
            agent_effort_label(&AgentView {
                effort,
                effort_change,
                ..AgentView::default()
            })
        };
        let words = [
            (Effort::Low, "low"),
            (Effort::Medium, "medium"),
            (Effort::High, "high"),
            (Effort::XHigh, "extra high"),
            (Effort::Max, "max"),
        ];
        for (effort, word) in words {
            assert_eq!(effort_label(effort), word);
        }
        assert_eq!(label(None, None), None);
        assert_eq!(
            label(Some(Some(Effort::High)), None).as_deref(),
            Some("effort high")
        );
        assert_eq!(
            label(Some(Some(Effort::High)), change(Effort::Max)).as_deref(),
            Some("high → max")
        );
        assert_eq!(
            label(Some(Some(Effort::High)), change(Effort::High)).as_deref(),
            Some("effort high"),
            "a switch to what already runs"
        );
        assert_eq!(
            label(None, change(Effort::Max)).as_deref(),
            Some("unknown → max")
        );
        assert_eq!(label(Some(None), None).as_deref(), Some("no effort"));
        assert_eq!(
            label(Some(None), change(Effort::High)).as_deref(),
            Some("no effort → high")
        );
    }

    #[test]
    fn effort_refusals_and_timeline_lines() {
        let agent = AgentView {
            effort_refused: Some(Refusal {
                value: Effort::High,
                reason: "kept medium".into(),
            }),
            model_refused: Some(Refusal {
                value: "bogus".into(),
                reason: "no".into(),
            }),
            ..AgentView::default()
        };
        assert_eq!(
            effort_refusal(&agent).as_deref(),
            Some("effort high refused: kept medium")
        );
        assert_eq!(effort_refusal(&AgentView::default()), None);
        assert_eq!(
            agent_refusal(&agent),
            ["bogus refused: no", "effort high refused: kept medium"]
        );
        assert_eq!(
            agent_setting_line(&AgentSetting::Effort(Some(Effort::XHigh)), None),
            "effort → extra high"
        );
        assert_eq!(
            agent_setting_line(&AgentSetting::Effort(None), None),
            "effort → none"
        );
        assert_eq!(
            agent_setting_line(
                &AgentSetting::Effort(Some(Effort::High)),
                Some("kept medium")
            ),
            "effort high refused: kept medium"
        );
    }

    #[test]
    fn usage_figures_are_compact() {
        let times = [(800, "0.8s"), (12_400, "12.4s"), (125_000, "2m 05s")];
        for (ms, text) in times.into_iter().chain([(3_720_000, "1h 02m")]) {
            assert_eq!(turn_time(ms), text);
        }
        for (n, text) in [
            (812, "812"),
            (3_100, "3.1k"),
            (19_337, "19k"),
            (153_400, "153k"),
            (999_999, "1.0M"),
            (1_234_567, "1.2M"),
            (45_000_000, "45M"),
        ] {
            assert_eq!(tokens(n), text, "{n}");
        }
        for (usd, text) in [
            (0.0, "$0"),
            (0.00142969, "$0.0014"),
            (0.04, "$0.04"),
            (0.1, "$0.10"),
            (0.86724, "$0.87"),
            (1.27, "$1.27"),
            (12.34, "$12.3"),
            (140.2, "$140"),
        ] {
            assert_eq!(dollars(usd), text, "{usd}");
        }
    }

    #[test]
    fn context_dividers_rows_and_the_viewing_hint() {
        use skywave_core::{ContextId, DeviceId, SessionId, SessionState};

        let laptop = DeviceRef {
            id: DeviceId::new(),
            name: Some("laptop".into()),
            me: false,
        };
        let me = DeviceRef {
            me: true,
            ..laptop.clone()
        };
        assert_eq!(
            context_started_line("velvet walrus", &laptop),
            "New context · velvet walrus · by laptop"
        );
        assert_eq!(
            context_switched_line("start", &me),
            "Back to start · by you"
        );

        let sid = SessionId::new();
        let (start, walrus) = (ContextId::first_of(sid), ContextId::new());
        let context = |id, name: &str, prompt: Option<&str>, current| ContextView {
            id,
            name: name.into(),
            first_prompt: prompt.map(str::to_owned),
            messages: usize::from(prompt.is_some()),
            last_activity: None,
            current,
            outgoing: None,
            moved_to: None,
            discard_move: None,
            can_move: false,
        };
        let contexts = vec![
            context(start, "start", Some("fix the flaky test"), false),
            context(walrus, "velvet walrus", None, true),
        ];
        assert_eq!(context_row(&contexts[0]), "start · fix the flaky test");
        assert_eq!(context_row(&contexts[1]), "velvet walrus");
        assert_eq!(context_moved_label(&contexts[0]), None);
        let titled = SessionRef {
            id: SessionId::new(),
            title: Some("fix the flaky test".into()),
        };
        let unknown = SessionRef {
            title: None,
            ..titled.clone()
        };
        let moved = ContextView {
            moved_to: Some(titled.clone()),
            ..contexts[0].clone()
        };
        assert_eq!(
            context_moved_label(&moved).as_deref(),
            Some("moved → “fix the flaky test”")
        );
        assert_eq!(
            context_moved_line("velvet walrus", &titled, &me),
            "velvet walrus went on in “fix the flaky test” · by you"
        );
        assert_eq!(
            context_moved_line("start", &unknown, &laptop),
            "start went on in another session · by laptop"
        );
        assert_eq!(
            moved_from_line("velvet walrus", &titled),
            "Continues velvet walrus from “fix the flaky test”"
        );
        assert_eq!(
            moved_from_line("velvet walrus", &unknown),
            "Continues velvet walrus from another session"
        );
        assert_eq!(
            adopted_line("3f2a"),
            "Continues agent conversation 3f2a from outside Skywave"
        );

        let mut view = SessionView {
            subagent: None,
            id: sid,
            title: "x".into(),
            named: false,
            rename: None,
            archived: false,
            workdir: None,
            state: SessionState::Idle,
            timeline: Vec::new(),
            contexts,
            context: walrus,
            viewing: None,
            can_clear: false,
            clear: None,
            switch: None,
            pending_approval: None,
            pending_question: None,
            rules: Vec::new(),
            usage: None,
            agent: AgentView::default(),
            interrupt: None,
            stop: None,
            work: Default::default(),
            task_stops: Vec::new(),
            mode: skywave_core::ComposerMode::Agent,
            draft: None,
            shell_draft: None,
            draft_rev: 0,
            shell: Default::default(),
            last_seq: skywave_core::Seq(1),
            unread_from: None,
        };
        assert_eq!(viewing_hint(&view), None, "the whole session");
        view.viewing = Some(walrus);
        assert_eq!(viewing_hint(&view), None, "the current context");
        view.viewing = Some(start);
        assert_eq!(
            viewing_hint(&view).as_deref(),
            Some("Viewing start — messages go to velvet walrus")
        );
    }

    #[test]
    fn a_bad_conversation_id_is_named() {
        assert_eq!(
            reject_reason(RejectReason::BadConversation),
            "not an agent conversation id"
        );
    }
}
