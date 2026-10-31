//! The screen as [`ScreenUpdate`]: core's [`Screen`] with the head's own
//! state (the pairing form, the open prompts, the composer), every label
//! worded here as the TUI words it without its key hints.

use skywave_core::{
    AgentView, ApprovalPrompt, ApprovalStatus, BlobPolicy, CandidateSource, Completion,
    CompletionStatus, Entry, Link, LinkRejection, ListKind, Outgoing, PairingStatus, PairingView,
    PendingSession, PermissionMode, RuleSuggestion, Screen, SessionListView, SessionRow,
    SessionState, SessionUsage, SessionView, SettingsView, TimelineItem, ViewState,
};
use skywave_present::format::{
    agent_mode_label, agent_model_label, answer_words, blob_size, bytes, delivery_label, dollars,
    fail_reason, mode_detail, mode_label, mode_needs_confirmation, mode_refusal, model_refusal,
    outgoing_label, scope_label, tokens, turn_time,
};
use skywave_present::policy::{CompletionSettle, dismissable, unanswered};
use skywave_present::sections::{ItemKey, Section, gated_item, sections, tool_sections};

use crate::link::count;
use crate::signals::{
    AgentSettingsView, AgentState, ApprovalPanel, BlobPolicyView, CandidateView, CompletionPopup,
    ComposerView, CreatePrompt, CreatingView, ListKindView, ModeOption, ModelOption, PairForm,
    PairingState, PermissionModeView, RenamePrompt, RuleOption, ScreenUpdate, ScreenView,
    SessionRowView, SessionScreen, ToolKindView,
};
use crate::timeline::{clean_line, key_string, outgoing_view, preview_view, tool_kind};

/// Most lines of the gated call's content the approval panel shows.
pub const APPROVAL_LINES: usize = 10;

/// The head's own state the screen shows.
#[derive(Debug, Clone, Copy)]
pub struct Ui<'a> {
    pub form: &'a PairForm,
    pub create: Option<&'a CreateUi>,
    pub rename: Option<&'a RenamePrompt>,
    /// The open session's composer.
    pub composer: Option<&'a ComposerView>,
    pub revoked_dismissed: bool,
}

/// The new-session prompt: its input as the hub knows it and the
/// completion settling it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CreateUi {
    pub input: String,
    /// Bumped whenever the hub rewrites `input`.
    pub rev: u32,
    pub settle: CompletionSettle,
}

/// The screen for `view` with the head's `ui`, at `wall_ms`.
pub fn update(view: &ViewState, ui: Ui, wall_ms: i64) -> ScreenUpdate {
    let screen = match &view.screen {
        Screen::Pairing(pairing) => pairing_view(pairing, ui.form),
        Screen::SessionList(list) => list_view(list, &view.link, ui),
        Screen::Session(session) => ScreenView::Session {
            session: session_view(session, &view.link, ui, wall_ms),
        },
        Screen::Settings(settings) => settings_view(settings),
    };
    ScreenUpdate {
        screen,
        can_go_back: view.can_go_back,
        revoked_full_screen: revoked(view) && !ui.revoked_dismissed,
    }
}

/// The revoked notice belongs over every screen but pairing.
pub fn revoked(view: &ViewState) -> bool {
    matches!(view.link, Link::Rejected(LinkRejection::Revoked))
        && !matches!(view.screen, Screen::Pairing(_))
}

fn pairing_view(pairing: &PairingView, form: &PairForm) -> ScreenView {
    let (state, status_label) = match &pairing.status {
        PairingStatus::Idle => (PairingState::Idle, None),
        PairingStatus::InProgress { address } => (
            PairingState::InProgress,
            Some(format!("pairing with {}…", clean_line(address))),
        ),
        PairingStatus::Failed { address, reason } => (
            PairingState::Failed,
            Some(format!(
                "pairing with {} failed: {}",
                clean_line(address),
                clean_line(reason)
            )),
        ),
    };
    ScreenView::Pairing {
        paired_device: pairing.paired.as_ref().map(|p| p.device_id.0.to_string()),
        paired_server: pairing
            .paired
            .as_ref()
            .and_then(|p| p.server_address.clone()),
        state,
        status_label,
        form: form.clone(),
    }
}

fn list_view(list: &SessionListView, link: &Link, ui: Ui) -> ScreenView {
    let rows = list.sessions.len();
    let (kind, title) = match (list.kind, list.other_count) {
        (ListKind::Active, 0) => (ListKindView::Active, format!("Sessions ({rows})")),
        (ListKind::Active, archived) => (
            ListKindView::Active,
            format!("Sessions ({rows}) · {archived} archived"),
        ),
        (ListKind::Archived, active) => (
            ListKindView::Archived,
            format!("Archived ({rows}) · {active} active"),
        ),
    };
    let empty_label = list.sessions.is_empty().then(|| {
        match list.kind {
            ListKind::Archived => "No archived sessions.",
            ListKind::Active if !list.creating.is_empty() => "No sessions yet.",
            ListKind::Active if list.other_count > 0 => "No active sessions.",
            ListKind::Active => "No sessions yet. Start one.",
        }
        .to_owned()
    });
    let archived = list.kind == ListKind::Archived;
    ScreenView::SessionList {
        kind,
        title,
        empty_label,
        rows: list
            .sessions
            .iter()
            .map(|row| row_view(row, archived))
            .collect(),
        other_count: count(list.other_count),
        creating: list.creating.iter().map(creating_view).collect(),
        dismissable: dismissable(list).map(|p| p.command_id.0.to_string()),
        create: ui
            .create
            .map(|create| create_prompt(create, list.completion.as_ref(), link)),
        rename: ui.rename.cloned(),
    }
}

fn row_view(row: &SessionRow, archived: bool) -> SessionRowView {
    let title = clean_line(&row.title);
    SessionRowView {
        session_id: row.id.0.to_string(),
        workdir: row.workdir.as_deref().map(clean_line),
        state: agent_state(&row.state),
        badge_label: badge(&row.state).to_owned(),
        unread: u32::try_from(row.unread).unwrap_or(u32::MAX),
        approvals: count(row.pending_approvals),
        approvals_label: match row.pending_approvals {
            0 => None,
            1 => Some("1 approval pending".to_owned()),
            n => Some(format!("{n} approvals pending")),
        },
        error: match &row.state {
            SessionState::Errored(info) => Some(clean_line(&info.message)),
            _ => None,
        },
        has_draft: row.has_draft,
        cost_label: row.cost_usd.map(dollars),
        last_activity_label: row.last_activity.format("%Y-%m-%d %H:%M UTC").to_string(),
        last_activity_ms: row.last_activity.timestamp_millis(),
        rename: row.rename.as_ref().map(outgoing_view),
        rename_label: rename_label(row.rename.as_ref()),
        deletable: row.deletable,
        delete_blocked_label: (archived && !row.deletable)
            .then(|| format!("cannot delete {title} yet: answer or cancel first")),
        archived,
        title,
    }
}

/// What the rename prompt starts with: the name, or empty for a default
/// title.
pub fn rename_initial(named: bool, title: &str) -> String {
    if named {
        title.to_owned()
    } else {
        String::new()
    }
}

/// A rename on its way: `queued`, `sending`; why it failed; `None` once
/// settled.
fn rename_label(rename: Option<&Outgoing>) -> Option<String> {
    match rename? {
        Outgoing::Accepted(_) => None,
        Outgoing::Failed(reason) => Some(format!("rename failed: {}", fail_reason(*reason))),
        pending => Some(outgoing_label(pending)),
    }
}

fn creating_view(pending: &PendingSession) -> CreatingView {
    CreatingView {
        command_id: pending.command_id.0.to_string(),
        workdir: clean_line(&pending.workdir),
        outgoing: outgoing_view(&pending.outgoing),
        note: match pending.outgoing {
            Outgoing::Queued => Some("waits for the server".to_owned()),
            Outgoing::Sending => None,
            Outgoing::Accepted(_) => Some("created, syncing…".to_owned()),
            Outgoing::Failed(reason) => Some(fail_reason(reason).to_owned()),
        },
        discardable: pending.discardable,
    }
}

fn create_prompt(create: &CreateUi, completion: Option<&Completion>, link: &Link) -> CreatePrompt {
    let popup = completion
        .filter(|_| create.settle.open())
        .map(|completion| {
            let offline = matches!(link, Link::Reconnecting { .. } | Link::Rejected(_));
            CompletionPopup {
                candidates: completion
                    .candidates
                    .iter()
                    .map(|c| CandidateView {
                        path: clean_line(&c.path),
                        recent: c.source == CandidateSource::Recent,
                    })
                    .collect(),
                selected: count(create.settle.selected()),
                status_label: match completion.status {
                    CompletionStatus::Loading => Some("listing…"),
                    CompletionStatus::Unavailable if offline => Some("offline · recent only"),
                    CompletionStatus::Unavailable => Some("unavailable · recent only"),
                    CompletionStatus::Ready { truncated: true } => {
                        Some("more not shown · keep typing")
                    }
                    CompletionStatus::Ready { truncated: false } => None,
                }
                .map(str::to_owned),
                empty_label: (completion.candidates.is_empty()
                    && completion.status != CompletionStatus::Loading)
                    .then(|| "no matching directories".to_owned()),
            }
        });
    CreatePrompt {
        input: create.input.clone(),
        input_rev: create.rev,
        popup,
    }
}

fn session_view(session: &SessionView, link: &Link, ui: Ui, wall_ms: i64) -> SessionScreen {
    SessionScreen {
        session_id: session.id.0.to_string(),
        title: clean_line(&session.title),
        workdir: session.workdir.as_deref().map(clean_line),
        state: agent_state(&session.state),
        badge_label: badge(&session.state).to_owned(),
        rename: session.rename.as_ref().map(outgoing_view),
        rename_label: rename_label(session.rename.as_ref()),
        archived_label: session
            .archived
            .then(|| "archived · a message brings it back".to_owned()),
        agent_label: match &session.state {
            SessionState::Idle => "agent idle".to_owned(),
            SessionState::AgentWorking => "agent working".to_owned(),
            SessionState::AwaitingApproval(_) => "agent waiting for an approval".to_owned(),
            SessionState::Errored(info) => format!("agent error: {}", clean_line(&info.message)),
            SessionState::Stopped => "agent stopped · a message restarts it".to_owned(),
        },
        interrupt_label: session
            .interrupt
            .as_ref()
            .map(|o| format!("interrupt {}", outgoing_label(o))),
        stop_label: session
            .stop
            .as_ref()
            .map(|o| format!("stop {}", outgoing_label(o))),
        rules: session.rules.iter().map(rule_text).collect(),
        rules_label: match session.rules.len() {
            0 => None,
            1 => Some("1 rule".to_owned()),
            n => Some(format!("{n} rules")),
        },
        agent: agent_settings(&session.agent),
        usage: session
            .usage
            .as_ref()
            .map(|usage| usage_fields(usage, session.agent.model.is_none()))
            .unwrap_or_default(),
        approval: session
            .pending_approval
            .as_ref()
            .map(|prompt| approval_panel(prompt, session, wall_ms)),
        composer: ui.composer.cloned().unwrap_or_else(|| ComposerView {
            text: String::new(),
            cursor_utf16: 0,
            rev: 0,
            history_label: None,
            offline_label: None,
        }),
        rename_prompt: ui
            .rename
            .filter(|r| r.session_id == session.id.0.to_string())
            .cloned(),
    }
    .with_offline(link)
}

impl SessionScreen {
    /// Says on the composer that a message waits for the link.
    fn with_offline(mut self, link: &Link) -> SessionScreen {
        self.composer.offline_label = matches!(link, Link::Reconnecting { .. })
            .then(|| "offline, sends when back online".to_owned());
        self
    }
}

/// The agent's mode and model with every choice to switch to. A switch
/// the server refused is not waiting: it shows as the refusal.
fn agent_settings(agent: &AgentView) -> AgentSettingsView {
    let failed = |outgoing: Option<&Outgoing>| match outgoing {
        Some(Outgoing::Failed(reason)) => Some(fail_reason(*reason)),
        _ => None,
    };
    let mode_failed = agent
        .mode_change
        .as_ref()
        .and_then(|c| failed(c.outgoing.as_ref()).map(|why| (mode_label(c.value), why)));
    let model_failed = agent.model_change.as_ref().and_then(|c| {
        let label = agent
            .choice(&c.value)
            .map_or(c.value.as_str(), |m| m.label.as_str());
        failed(c.outgoing.as_ref()).map(|why| (label, why))
    });
    let waiting = AgentView {
        mode_change: agent.mode_change.clone().filter(|_| mode_failed.is_none()),
        model_change: agent
            .model_change
            .clone()
            .filter(|_| model_failed.is_none()),
        ..agent.clone()
    };
    let failure = |(what, why): (&str, &str)| format!("{what} failed: {why}");
    let current_model = agent
        .model
        .as_deref()
        .and_then(|m| agent.choice(m))
        .map(|m| m.id.as_str());
    let pending_model = waiting.model_change.as_ref().map(|c| {
        agent
            .choice(&c.value)
            .map_or(c.value.as_str(), |m| m.id.as_str())
    });
    AgentSettingsView {
        mode_label: clean_line(&agent_mode_label(&waiting)),
        model_label: agent_model_label(&waiting).map(|l| clean_line(&l)),
        model_name: agent.model.as_deref().map(clean_line),
        mode_refusal: mode_failed
            .map(failure)
            .or_else(|| mode_refusal(agent))
            .map(|r| clean_line(&r)),
        model_refusal: model_failed
            .map(failure)
            .or_else(|| model_refusal(agent))
            .map(|r| clean_line(&r)),
        modes: PermissionMode::ALL
            .into_iter()
            .map(|mode| ModeOption {
                mode: mode_view(mode),
                label: mode_label(mode).to_owned(),
                detail: mode_detail(mode).to_owned(),
                needs_confirmation: mode_needs_confirmation(mode),
                current: agent.mode == Some(mode),
                pending: agent.mode != Some(mode)
                    && waiting
                        .mode_change
                        .as_ref()
                        .is_some_and(|c| c.value == mode),
            })
            .collect(),
        models: agent
            .models
            .iter()
            .map(|m| ModelOption {
                id: m.id.clone(),
                label: clean_line(&m.label),
                detail: m.detail.as_deref().map(clean_line),
                current: current_model == Some(m.id.as_str()),
                pending: pending_model != current_model && pending_model == Some(m.id.as_str()),
            })
            .collect(),
    }
}

fn mode_view(mode: PermissionMode) -> PermissionModeView {
    match mode {
        PermissionMode::Ask => PermissionModeView::Ask,
        PermissionMode::AcceptEdits => PermissionModeView::AcceptEdits,
        PermissionMode::Plan => PermissionModeView::Plan,
        PermissionMode::Auto => PermissionModeView::Auto,
        PermissionMode::DontAsk => PermissionModeView::DontAsk,
        PermissionMode::Bypass => PermissionModeView::Bypass,
    }
}

/// `last turn 12.4s`, `3.1k in / 812 out`, `$0.04`, `session $1.27`,
/// `<model>` (with `model`: the model chip names it once the agent has),
/// with the figures the agent did not report left out.
fn usage_fields(usage: &SessionUsage, model: bool) -> Vec<String> {
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
    if let Some(cost) = usage.total.cost_usd {
        fields.push(format!("session {}", dollars(cost)));
    }
    if let Some(name) = usage.total.model.as_ref().filter(|_| model) {
        fields.push(clean_line(name));
    }
    fields
}

/// The panel over the composer while the agent is blocked: the call it
/// gates as the timeline shows it, what it will do, the rules offered, and
/// this device's answer.
fn approval_panel(prompt: &ApprovalPrompt, session: &SessionView, wall_ms: i64) -> ApprovalPanel {
    let gated = gated_item(session);
    let gated_key = gated.map(|(_, item)| key_string(ItemKey::of(item)));
    // The legacy entry an approval with no logged call stands for.
    let standalone = Entry::Approval {
        id: prompt.id,
        tool: prompt.tool.clone(),
        summary: prompt.summary.clone(),
        input: prompt.input.clone(),
        status: ApprovalStatus::Pending,
        answer: None,
    };
    let (kind, tool, title, detail, sections): (_, _, _, _, Vec<Section>) = match gated {
        Some((
            _,
            TimelineItem::Logged {
                entry: Entry::Tool(view),
                ..
            },
        )) => (
            tool_kind(view.kind),
            clean_line(&view.name),
            clean_line(&view.title),
            view.detail.as_deref().map(clean_line),
            tool_sections(view),
        ),
        _ => (
            ToolKindView::Other,
            clean_line(&prompt.tool),
            clean_line(&prompt.summary),
            None,
            sections(&standalone),
        ),
    };
    let preview = sections
        .iter()
        .find(|s| !s.hidden)
        .map(|s| preview_view(s, APPROVAL_LINES));
    let input_label = preview
        .is_none()
        .then(|| sections.iter().find_map(|s| s.blob))
        .flatten()
        .map(|blob| {
            blob_size(&blob.state).map_or_else(
                || "full input".to_owned(),
                |size| format!("full input {}", bytes(size)),
            )
        });
    let can_answer = unanswered(prompt);
    let always_label = match prompt.suggestions.as_slice() {
        _ if !can_answer => None,
        [] => None,
        [rule] => Some(format!("always allow · {}", rule_text(rule))),
        rules => Some(format!(
            "always allow · choose one of {} rules",
            rules.len()
        )),
    };
    let answer_label = prompt.answer.as_ref().map(|answer| {
        // On its way: how long it has waited says more than "queued".
        let label = match (&answer.outgoing, &answer.delivery) {
            (Outgoing::Queued | Outgoing::Sending, Some(delivery)) => {
                delivery_label(delivery, &answer.outgoing, wall_ms)
            }
            _ => outgoing_label(&answer.outgoing),
        };
        let rule = answer
            .rule
            .as_ref()
            .map_or_else(String::new, |rule| format!(" · {}", rule_text(rule)));
        format!(
            "you {}{rule} · {label}",
            answer_words(answer.decision, answer.rule.as_ref())
        )
    });
    ApprovalPanel {
        approval_id: prompt.id.0.to_string(),
        gated_key,
        kind,
        tool,
        title,
        detail,
        preview,
        input_label,
        can_answer,
        suggestions: prompt
            .suggestions
            .iter()
            .enumerate()
            .map(|(index, rule)| RuleOption {
                index: count(index),
                label: rule_text(rule),
            })
            .collect(),
        always_picks: can_answer && prompt.suggestions.len() > 1,
        always_label,
        answer_label,
        answer: prompt.answer.as_ref().map(|a| outgoing_view(&a.outgoing)),
    }
}

fn settings_view(settings: &SettingsView) -> ScreenView {
    let (blob_policy, blob_policy_label) = match settings.blob_policy {
        BlobPolicy::Eager => (BlobPolicyView::Eager, "eager (download every blob)"),
        BlobPolicy::OnDemand => (BlobPolicyView::OnDemand, "on demand (download when opened)"),
    };
    ScreenView::Settings {
        blob_policy,
        blob_policy_label: blob_policy_label.to_owned(),
        server: settings
            .server_address
            .clone()
            .unwrap_or_else(|| "not paired".to_owned()),
        device: settings
            .device_id
            .map_or_else(|| "not paired".to_owned(), |id| id.0.to_string()),
        fingerprint: settings.server_fingerprint.map(|f| f.to_string()),
    }
}

/// A rule as listed: its label, then how far it reaches.
fn rule_text(rule: &RuleSuggestion) -> String {
    format!("{} ({})", clean_line(&rule.label), scope_label(rule.scope))
}

fn agent_state(state: &SessionState) -> AgentState {
    match state {
        SessionState::Idle => AgentState::Idle,
        SessionState::AgentWorking => AgentState::Working,
        SessionState::AwaitingApproval(_) => AgentState::AwaitingApproval,
        SessionState::Errored(_) => AgentState::Errored,
        SessionState::Stopped => AgentState::Stopped,
    }
}

/// A session state's short word.
fn badge(state: &SessionState) -> &'static str {
    match state {
        SessionState::Idle => "idle",
        SessionState::AgentWorking => "working",
        SessionState::AwaitingApproval(_) => "approval",
        SessionState::Errored(_) => "error",
        SessionState::Stopped => "stopped",
    }
}
