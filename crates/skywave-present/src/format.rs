//! Neutral wording: sizes, durations, token counts, costs, and how a
//! command went out or why it failed. Plain strings, no key hints and no
//! styling; a head adds its own around them.

use std::time::Duration;

use skywave_core::{
    AgentSetting, AgentView, BlobState, ContextView, Decision, Delivery, DeviceRef, Effort,
    FailReason, Outgoing, PermissionMode, RejectReason, RuleScope, RuleSuggestion, SessionRef,
    SessionView,
};

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

/// `got` of `size` as a whole percentage, at most 100; an empty size is
/// done.
pub fn percent(got: u64, size: u64) -> u64 {
    (got.saturating_mul(100))
        .checked_div(size)
        .unwrap_or(100)
        .min(100)
}

/// A byte count in words: "812 B", "12.3 KiB", "4.0 MiB".
pub fn bytes(n: u64) -> String {
    const KIB: f64 = 1024.0;
    match n {
        0..1024 => format!("{n} B"),
        1024..1_048_576 => format!("{:.1} KiB", n as f64 / KIB),
        _ => format!("{:.1} MiB", n as f64 / (KIB * KIB)),
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
        RejectReason::UnknownModel => "no model named",
        RejectReason::ContextExists => "that context already exists",
        RejectReason::UnknownContext => "the session has no such context",
        RejectReason::ContextChanged => "the session moved to another context first",
        RejectReason::ContextMoved => "that context was moved to a session of its own",
        RejectReason::ContextCurrent => "that context is the current one",
        RejectReason::NoConversation => "the agent never ran in that context",
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

#[cfg(test)]
mod tests {
    use skywave_core::{ModelChoice, Refusal, SettingChange};

    use super::*;

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

        let mut view = SessionView {
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
            rules: Vec::new(),
            usage: None,
            agent: AgentView::default(),
            interrupt: None,
            stop: None,
            draft: None,
            draft_rev: 0,
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
}
