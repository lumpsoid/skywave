//! The screen as [`ScreenUpdate`]: core's [`Screen`] with the head's own
//! state (the pairing form, the open prompts, the composer), every label
//! worded here as the TUI words it without its key hints.

use chrono::{DateTime, Local, Utc};
use skywave_core::{
    AgentView, ApprovalPrompt, ApprovalStatus, ApprovalsView, Asks, BlobPolicy, Candidate,
    CandidateSource, Completion, CompletionStatus, ConversationPicker, ConversationsStatus, Effort,
    Entry, Link, ListKind, LoginFlowView, LoginOutcome, LoginView, Outgoing, PairingStatus,
    PairingView, PendingSession, PermissionMode, QuestionPrompt, RuleSuggestion, SafetyRuleView,
    Screen, SessionId, SessionInfoView, SessionListView, SessionRow, SessionState, SessionView,
    SettingsView, Snippet, StorageStatus, StorageView, SubagentHeader, TimelineItem,
    UsageLimitsView, UsageView, ViewState, WaitingApproval, WorktreeSession, WorktreesView,
};
use skywave_present::format::{
    AMBIENT_WORD, BackgroundRow, IMPORTING_HISTORY, LOOPS_NOTE, NOTHING_RUNNING, SAVED_IN_FOLDER,
    StopAction, TRAIL_SEPARATOR, adopting_note, agent_effort_label, agent_mode_label,
    agent_model_label, answer_words, approval_origin, background_rows, blob_size, bytes,
    context_moved_label, context_row, conversation_detail, conversation_link, conversation_title,
    conversations_empty, conversations_status, delivery_label, dir_name, dollars, effort_label,
    effort_refusal, fail_reason, mode_detail, mode_label, mode_needs_confirmation, mode_refusal,
    model_refusal, move_refusal, offered_modes, on_off, outgoing_label, repeat_word,
    safety_check_label, scope_label, session_usage_fields, stop_heading, stop_warning,
    subagent_status, task_kind_word, trail, viewing_hint, work_summary,
};
use skywave_present::limits;
use skywave_present::login;
use skywave_present::policy::{WorkdirPicker, dismissable, question_unanswered, unanswered};
use skywave_present::question::reply_text;
use skywave_present::sections::{
    ItemKey, Section, approval_lost, asked_note, gated_item, sections, tool_sections,
};
use skywave_present::session_info;
use skywave_present::session_list::by_repo;
use skywave_present::storage::{self, Grouping as StorageGrouping};
use skywave_present::usage::{Grouping, NO_TURNS, Report, Span, Totals};
use skywave_present::worktrees;
use skywave_proto::{RepoListing, RepoWorktrees, WorktreeEntry};

use crate::link::count;
use crate::signals::{
    AgentLoginView, AgentSettingsView, AgentState, ApprovalPanel, ApprovalsItem, BlobPolicyView,
    CandidateView, CompletionPopup, ComposerModeView, ComposerView, ContextRow,
    ConversationRowView, ConversationsView, CreatePrompt, CreatingView, EffortOption, EffortView,
    InfoFieldView, InfoSectionView, LimitRowView, LimitsView, ListKindView, LoginFlowState,
    ModeOption, ModelOption, PairForm, PairingState, PermissionModeView, QuestionOptionView,
    QuestionPanel, QuestionView, RenamePrompt, RepairNotice, RuleOption, SafetyCheckView,
    SafetyRuleRow, ScheduleRow, ScreenUpdate, ScreenView, SessionRowView, SessionScreen,
    SnippetView, StopHeadings, StorageGroupingView, StorageKindView, StorageRowView,
    StorageSessionView, StorageTotalView, SubagentHeaderView, SubagentRowView, TaskRow,
    ToolKindView, UsageGroupingView, UsageRowView, UsageSpanView, WorktreeRemoveView,
    WorktreeRepoView, WorktreeSessionView, WorktreeView,
};
use skywave_present::text::{clean, clean_line};

use crate::timeline::{key_string, outgoing_view, preview_view, tool_kind};
use crate::{crop, workdir};

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
    pub repair_dismissed: bool,
    /// The usage screen's days and grouping.
    pub usage: (Span, Grouping),
    /// The storage screen's list.
    pub storage: StorageGrouping,
    /// The wall clock in unix-epoch millis: the usage span ends on its day.
    pub wall_ms: i64,
    /// The text of the workdir blob shown ([`workdir::WorkdirText::of`]).
    pub workdir_text: Option<&'a Result<workdir::Text, String>>,
    /// The Stay connected preference; `None` where it is not offered.
    pub stay_connected: Option<bool>,
    /// The session list shows as one plain list, not grouped by project.
    pub plain_list: bool,
    /// Only the conversation shows: tool calls are hidden.
    pub conversation_only: bool,
}

/// The new-session prompt: its input as the hub knows it and the highlight
/// in its candidate list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CreateUi {
    pub input: String,
    /// Bumped whenever the hub rewrites `input`.
    pub rev: u32,
    pub picker: WorkdirPicker,
    /// The optional outside agent conversation id the user typed. The
    /// user's alone: the hub never rewrites it, so it needs no rev.
    pub conversation: String,
    /// The optional new branch to run the session in a git worktree of. The
    /// user's alone, like `conversation`.
    pub worktree: String,
}

/// The screen for `view` with the head's `ui`.
pub fn update(view: &ViewState, ui: Ui) -> ScreenUpdate {
    let screen = match &view.screen {
        Screen::Pairing(pairing) => pairing_view(pairing, ui.form),
        Screen::SessionList(list) => list_view(list, &view.link, ui),
        Screen::Session(session) => ScreenView::Session {
            session: session_view(session, view, ui),
        },
        Screen::Settings(settings) => settings_view(settings, view, ui.stay_connected, ui.wall_ms),
        Screen::ServerAddresses(view) => ScreenView::ServerAddresses {
            current: view
                .current
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
            others: view.others.iter().map(ToString::to_string).collect(),
        },
        Screen::Usage(usage) => usage_view(usage, ui.usage, ui.wall_ms),
        Screen::Storage(view) => storage_view(view, ui.storage),
        Screen::Worktrees(view) => worktrees_view(view),
        Screen::Approvals(view) => approvals_view(view),
        Screen::UsageLimits(view) => ScreenView::UsageLimits {
            limits: limits_view(view, ui.wall_ms),
        },
        Screen::SessionInfo(view) => session_info_view(view, ui.wall_ms),
        Screen::Workdir(workdir) => workdir::screen(workdir, ui.workdir_text, ui.wall_ms),
        Screen::Crop(crop) => crop::screen(crop),
    };
    ScreenUpdate {
        screen,
        can_go_back: view.can_go_back,
        repair: repair_notice(view).filter(|_| !ui.repair_dismissed),
    }
}

/// The re-pair notice, over every screen but pairing, while the link
/// stopped for a reason only pairing again fixes.
pub fn repair_notice(view: &ViewState) -> Option<RepairNotice> {
    let Link::Rejected(rejection) = view.link else {
        return None;
    };
    if matches!(view.screen, Screen::Pairing(_)) {
        return None;
    }
    let notice = skywave_present::link::repair_notice(rejection)?;
    Some(RepairNotice {
        title: notice.title.to_owned(),
        body: notice.body.to_owned(),
        can_retry: rejection.can_retry(),
    })
}

fn pairing_view(pairing: &PairingView, form: &PairForm) -> ScreenView {
    let (state, status_label) = match &pairing.status {
        PairingStatus::Idle => (PairingState::Idle, None),
        PairingStatus::InProgress { address } => (
            PairingState::InProgress,
            Some(format!(
                "pairing with {}…",
                clean_line(&address.to_string())
            )),
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
            .map(|p| p.server_address.to_string()),
        state,
        status_label,
        form: form.clone(),
    }
}

fn list_view(list: &SessionListView, link: &Link, ui: Ui) -> ScreenView {
    // The Active/Archived toggle carries the counts and which list shows.
    let kind = match list.kind {
        ListKind::Active => ListKindView::Active,
        ListKind::Archived => ListKindView::Archived,
        ListKind::All => ListKindView::All,
    };
    let title = "Sessions".to_owned();
    let empty_label = list.sessions.is_empty().then(|| {
        match list.kind {
            ListKind::Archived => "No archived sessions.",
            ListKind::All => "No sessions yet.",
            ListKind::Active if !list.creating.is_empty() => "No sessions yet.",
            ListKind::Active if list.other_count > 0 => "No active sessions.",
            ListKind::Active => "No sessions yet. Start one.",
        }
        .to_owned()
    });
    ScreenView::SessionList {
        kind,
        title,
        empty_label,
        rows: if ui.plain_list {
            list.sessions.iter().map(|row| row_view(row, "")).collect()
        } else {
            by_repo(&list.sessions)
                .iter()
                .flat_map(|group| group.rows.iter().map(move |row| row_view(row, group.key)))
                .collect()
        },
        plain: ui.plain_list,
        other_count: count(list.other_count),
        creating: list.creating.iter().map(creating_view).collect(),
        dismissable: dismissable(list).map(|p| p.command_id.0.to_string()),
        create: ui
            .create
            .map(|create| create_prompt(create, list.completion.as_ref(), link)),
        conversations: list
            .conversations
            .as_ref()
            .map(|picker| conversations_view(picker, &list.sessions, ui.wall_ms)),
        rename: ui.rename.cloned(),
    }
}

/// The conversation picker, worded as the TUI words it without its keys.
fn conversations_view(
    picker: &ConversationPicker,
    sessions: &[SessionRow],
    wall_ms: i64,
) -> ConversationsView {
    let now = DateTime::<Utc>::from_timestamp_millis(wall_ms).unwrap_or_default();
    let title = |id: SessionId| {
        sessions
            .iter()
            .find(|row| row.id == id)
            .map(|row| row.title.clone())
    };
    ConversationsView {
        rows: picker
            .items
            .iter()
            .map(|item| ConversationRowView {
                conversation_id: item.id.clone(),
                title: clean_line(conversation_title(item)),
                detail: clean_line(&conversation_detail(item, now)),
                link_label: conversation_link(item, title).map(|link| clean_line(&link)),
                session_id: item.session.map(|id| id.0.to_string()),
            })
            .collect(),
        status_label: conversations_status(&picker.status).map(str::to_owned),
        unavailable: matches!(picker.status, ConversationsStatus::Unavailable(_)),
        empty_label: conversations_empty(picker).map(|text| clean_line(&text)),
    }
}

/// `group` is the key [`by_repo`] put the row under.
fn row_view(row: &SessionRow, group: &str) -> SessionRowView {
    let title = clean_line(&row.title);
    SessionRowView {
        session_id: row.id.0.to_string(),
        workdir: row.workdir.as_deref().map(clean_line),
        workdir_name: row.workdir.as_deref().map(|w| clean_line(dir_name(w))),
        group: group.to_owned(),
        group_name: if group.is_empty() {
            "no directory".to_owned()
        } else {
            clean_line(dir_name(group))
        },
        branch: row.worktree.as_ref().map(|w| clean_line(&w.branch)),
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
        importing_label: row.importing.then(|| IMPORTING_HISTORY.to_owned()),
        deletable: row.deletable,
        deletable_everywhere: row.deletable_everywhere,
        delete_blocked_label: (row.archived && !row.deletable)
            .then(|| format!("cannot delete {title} yet: answer or cancel first")),
        removable: row.worktree.is_some() && !row.worktree_removed,
        worktree_removed: row.worktree_removed,
        archived: row.archived,
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
        note: match (adopting_note(pending), pending.outgoing) {
            (Some(note), _) => Some(note.to_owned()),
            (None, Outgoing::Queued) => Some("waits for the server".to_owned()),
            (None, Outgoing::Sending) => None,
            (None, Outgoing::Accepted(_)) => Some("created, syncing…".to_owned()),
            (None, Outgoing::Failed(reason)) => Some(fail_reason(reason).to_owned()),
        },
        discardable: pending.discardable,
    }
}

/// The text a candidate row shows. A server listing under the input's
/// directory (everything up to its last `/`, for an absolute input) shows
/// the name past that directory; anything else shows its full path.
fn candidate_label(input: &str, candidate: &Candidate) -> String {
    let dir = input
        .rfind('/')
        .filter(|_| input.starts_with('/'))
        .map(|end| &input[..=end]);
    match dir {
        Some(dir)
            if candidate.source == CandidateSource::Server && candidate.path.starts_with(dir) =>
        {
            clean_line(&candidate.path[dir.len()..])
        }
        _ => clean_line(&candidate.path),
    }
}

fn create_prompt(create: &CreateUi, completion: Option<&Completion>, link: &Link) -> CreatePrompt {
    let popup = completion.map(|completion| {
        let offline = matches!(link, Link::Reconnecting { .. } | Link::Rejected(_));
        CompletionPopup {
            candidates: completion
                .candidates
                .iter()
                .map(|c| CandidateView {
                    path: clean_line(&c.path),
                    label: candidate_label(&completion.input, c),
                    recent: c.source == CandidateSource::Recent,
                })
                .collect(),
            selected: create.picker.highlighted().map(count),
            stale: completion.input != create.input,
            loading: completion.status == CompletionStatus::Loading,
            status_label: match completion.status {
                CompletionStatus::Loading => None,
                CompletionStatus::Unavailable if offline => Some("offline · recent only"),
                CompletionStatus::Unavailable => Some("unavailable · recent only"),
                CompletionStatus::Ready { truncated: true } => Some("more not shown · keep typing"),
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

fn session_view(session: &SessionView, view: &ViewState, ui: Ui) -> SessionScreen {
    SessionScreen {
        session_id: session.id.0.to_string(),
        title: clean_line(&session.title),
        workdir: session.workdir.as_deref().map(clean_line),
        workdir_name: session.workdir.as_deref().map(|w| clean_line(dir_name(w))),
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
            SessionState::BackgroundWork => "background work running".to_owned(),
            SessionState::AwaitingApproval(_) if session.pending_question.is_some() => {
                "agent waiting for an answer".to_owned()
            }
            SessionState::AwaitingApproval(_) => "agent waiting for an approval".to_owned(),
            SessionState::Errored(info) => format!("agent error: {}", clean_line(&info.message)),
            SessionState::Stopped => "agent stopped · a message restarts it".to_owned(),
        },
        can_interrupt: matches!(
            session.state,
            SessionState::AgentWorking | SessionState::AwaitingApproval(_)
        ),
        interrupt_busy: matches!(
            session.interrupt,
            Some(Outgoing::Queued | Outgoing::Sending | Outgoing::Accepted(_))
        ),
        interrupt_refusal: match session.interrupt {
            Some(Outgoing::Failed(reason)) => Some(fail_reason(reason).to_owned()),
            _ => None,
        },
        stop_label: session
            .stop
            .as_ref()
            .map(|o| format!("stop {}", outgoing_label(o))),
        clear_label: session
            .clear
            .as_ref()
            .map(|o| format!("new context {}", outgoing_label(o))),
        switch_label: session
            .switch
            .as_ref()
            .map(|o| format!("switch {}", outgoing_label(o))),
        can_clear: session.can_clear,
        work_label: work_summary(&session.work),
        stop_warning: stop_warning(&session.work),
        stop_headings: StopHeadings {
            stop_agent: stop_heading(StopAction::Agent).to_owned(),
            new_context: stop_heading(StopAction::NewContext).to_owned(),
            switch_context: stop_heading(StopAction::SwitchContext).to_owned(),
            move_context: stop_heading(StopAction::MoveContext).to_owned(),
            stop_task: stop_heading(StopAction::Task).to_owned(),
        },
        tasks: background_rows(&session.work)
            .into_iter()
            .filter_map(|row| match row {
                BackgroundRow::Task(task, depth) => Some(TaskRow {
                    task_id: task.id.0.clone(),
                    depth: depth as u32,
                    kind_label: task_kind_word(task.kind).to_owned(),
                    title: clean_line(&task.title),
                    ambient_label: task.ambient.then(|| AMBIENT_WORD.to_owned()),
                    stop_label: session
                        .task_stops
                        .iter()
                        .find(|(id, _)| *id == task.id)
                        .map(|(_, outgoing)| outgoing_label(outgoing)),
                    call: task.call.map(|call| call.0.to_string()),
                }),
                BackgroundRow::Loop(_) => None,
            })
            .collect(),
        schedules: session
            .work
            .schedules
            .iter()
            .map(|s| ScheduleRow {
                schedule_id: s.id.0.clone(),
                when: clean_line(&s.when),
                repeat_label: repeat_word(s.recurring).to_owned(),
                prompt: clean_line(&s.prompt),
                saved_label: (s.outlives_agent == Some(true)).then(|| SAVED_IN_FOLDER.to_owned()),
            })
            .collect(),
        loops_note: (!session.work.schedules.is_empty()).then(|| LOOPS_NOTE.to_owned()),
        work_empty_label: NOTHING_RUNNING.to_owned(),
        contexts: session
            .contexts
            .iter()
            .map(|c| ContextRow {
                context_id: c.id.0.to_string(),
                label: clean_line(&context_row(c)),
                current: c.current,
                outgoing_label: c.outgoing.as_ref().map(outgoing_label),
                moved_to_session_id: c.moved_to.as_ref().map(|to| to.id.0.to_string()),
                moved_label: context_moved_label(c).map(|l| clean_line(&l)),
                can_move: c.can_move,
                move_refusal: (!c.can_move && c.moved_to.is_none())
                    .then(|| move_refusal(c).to_owned()),
                discard_move: c.discard_move.map(|id| id.0.to_string()),
            })
            .collect(),
        context_id: session.context.0.to_string(),
        viewing_id: session.viewing.map(|v| v.0.to_string()),
        viewing_hint: viewing_hint(session).map(|h| clean_line(&h)),
        rules: session.rules.iter().map(rule_text).collect(),
        rules_label: match session.rules.len() {
            0 => None,
            1 => Some("1 rule".to_owned()),
            n => Some(format!("{n} rules")),
        },
        agent: agent_settings(&session.agent),
        safety_check: safety_check(session),
        usage: session
            .usage
            .as_ref()
            .map(|usage| session_usage_fields(usage, session.agent.model.is_none()))
            .unwrap_or_default(),
        approval: session
            .pending_approval
            .as_ref()
            .map(|prompt| approval_panel(prompt, Some(session))),
        question: session
            .pending_question
            .as_ref()
            .map(|prompt| question_panel(prompt, session)),
        subagent: session
            .subagent
            .as_ref()
            .map(|header| subagent_header(header, &session.title)),
        composer: ui.composer.cloned().unwrap_or_else(|| ComposerView {
            text: String::new(),
            cursor_utf16: 0,
            rev: 0,
            history_label: None,
            history_search: None,
            to_label: None,
            mode: ComposerModeView::Agent,
            shell_enabled: false,
            prompt: None,
            can_stop: false,
            images: Vec::new(),
            files: Vec::new(),
            preparing: 0,
        }),
        // The composer's kind only: saved prompts in agent mode, saved
        // commands in shell mode.
        snippets: view
            .snippets
            .iter()
            .filter(|s| s.kind == session.mode)
            .map(snippet_view)
            .collect(),
        rename_prompt: ui
            .rename
            .filter(|r| r.session_id == session.id.0.to_string())
            .cloned(),
        conversation_only: ui.conversation_only,
    }
    .with_recipient(session)
}

/// The header of a subagent's view in the session titled `session`.
fn subagent_header(header: &SubagentHeader, session: &str) -> SubagentHeaderView {
    let status = &header.status;
    let path = [session.to_owned(), trail(&header.trail)]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(TRAIL_SEPARATOR);
    SubagentHeaderView {
        call: header.call.0.to_string(),
        title: clean_line(&header.title),
        trail: clean_line(&path),
        status_label: clean_line(&subagent_status(status)),
        live: status.live,
        // A foreground subagent has no task: the server refuses to stop
        // one, and Interrupt ends it with the turn.
        task_id: status
            .task
            .as_ref()
            .filter(|_| status.live)
            .map(|task| task.0.clone()),
    }
}

fn snippet_view(snippet: &Snippet) -> SnippetView {
    let first = clean_line(snippet.text.trim_start());
    SnippetView {
        snippet_id: snippet.id.0.to_string(),
        label: snippet
            .name
            .as_deref()
            .map_or_else(|| first.clone(), clean_line),
        detail: snippet.name.is_some().then_some(first),
        name: snippet.name.clone(),
        text: snippet.text.clone(),
    }
}

impl SessionScreen {
    /// Says on the composer, in a subagent's view, who a message goes to.
    fn with_recipient(mut self, session: &SessionView) -> SessionScreen {
        self.composer.to_label = session
            .subagent
            .is_some()
            .then(|| "to the main agent".to_owned());
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
    let effort_failed = agent
        .effort_change
        .as_ref()
        .and_then(|c| failed(c.outgoing.as_ref()).map(|why| (effort_label(c.value), why)));
    let waiting = AgentView {
        effort_change: agent
            .effort_change
            .clone()
            .filter(|_| effort_failed.is_none()),
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
        // A bare `unknown` would not say what is unknown, alone on a chip
        // or folded with the other's; likewise the model below.
        mode_label: match (waiting.mode, &waiting.mode_change) {
            (None, None) => "mode unknown".to_owned(),
            _ => clean_line(&agent_mode_label(&waiting)),
        },
        model_label: agent_model_label(&waiting)
            .map_or_else(|| "model unknown".to_owned(), |l| clean_line(&l)),
        effort_label: agent_effort_label(&waiting)
            .map_or_else(|| "effort unknown".to_owned(), |l| clean_line(&l)),
        model_name: agent.model.as_deref().map(clean_line),
        mode_refusal: mode_failed
            .map(failure)
            .or_else(|| mode_refusal(agent))
            .map(|r| clean_line(&r)),
        model_refusal: model_failed
            .map(failure)
            .or_else(|| model_refusal(agent))
            .map(|r| clean_line(&r)),
        effort_refusal: effort_failed
            .map(failure)
            .or_else(|| effort_refusal(agent))
            .map(|r| clean_line(&r)),
        modes: offered_modes(agent.bypass_allowed)
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
        efforts: agent
            .efforts()
            .iter()
            .map(|&e| EffortOption {
                effort: effort_view(e),
                label: effort_label(e).to_owned(),
                current: agent.effort == Some(Some(e)),
                pending: agent.effort != Some(Some(e))
                    && waiting.effort_change.as_ref().is_some_and(|c| c.value == e),
            })
            .collect(),
    }
}

/// Skywave's safety check: the logged switch, and this device's switch
/// while it waits. A switch the server refused is not waiting: it shows as
/// the refusal.
fn safety_check(session: &SessionView) -> SafetyCheckView {
    let change = session.safety_check_change.as_ref();
    let refusal = change.and_then(|c| match c.outgoing {
        Some(Outgoing::Failed(reason)) => Some(format!(
            "safety check {} failed: {}",
            on_off(c.value),
            fail_reason(reason)
        )),
        _ => None,
    });
    let waiting = change.filter(|_| refusal.is_none()).map(|c| c.value);
    let on = waiting.unwrap_or(session.safety_check);
    SafetyCheckView {
        label: clean_line(&safety_check_label(session.safety_check, waiting)),
        turn_on: !on,
        on,
        pending: waiting.is_some_and(|v| v != session.safety_check),
        refusal: refusal.map(|r| clean_line(&r)),
    }
}

fn effort_view(effort: Effort) -> EffortView {
    match effort {
        Effort::Low => EffortView::Low,
        Effort::Medium => EffortView::Medium,
        Effort::High => EffortView::High,
        Effort::XHigh => EffortView::XHigh,
        Effort::Max => EffortView::Max,
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

/// The panel over the composer while the agent is blocked: the call it
/// gates as the timeline shows it, what it will do, the rules offered, and
/// this device's answer. With no `session` on show (the approvals screen),
/// no call is shown as gated.
fn approval_panel(prompt: &ApprovalPrompt, session: Option<&SessionView>) -> ApprovalPanel {
    let gated = session.and_then(gated_item);
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
        .then(|| sections.iter().filter(|s| !s.asked).find_map(Section::blob))
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
                delivery_label(delivery, &answer.outgoing)
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
        from: prompt
            .from
            .as_deref()
            .map(|from| clean_line(&approval_origin(from, gated.is_some()))),
        gated_key,
        kind,
        tool,
        title,
        detail,
        preview,
        input_label,
        asked_label: asked_note(&sections),
        lost: approval_lost(prompt, &sections).map(str::to_owned),
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
        rule_candidates: prompt
            .shell
            .as_deref()
            .map(skywave_safety::simple_commands)
            .unwrap_or_default(),
        always_picks: can_answer && prompt.suggestions.len() > 1,
        always_label,
        answer_label,
        answer: prompt.answer.as_ref().map(|a| outgoing_view(&a.outgoing)),
    }
}

fn question_panel(prompt: &QuestionPrompt, session: &SessionView) -> QuestionPanel {
    let call_key = prompt.call.and_then(|call| {
        session.timeline.iter().find_map(|item| match item {
            TimelineItem::Logged {
                entry: Entry::Tool(view),
                ..
            } if view.call == call => Some(key_string(ItemKey::of(item))),
            _ => None,
        })
    });
    let answer_label = prompt.answer.as_ref().map(|answer| {
        let label = match (&answer.outgoing, &answer.delivery) {
            (Outgoing::Queued | Outgoing::Sending, Some(delivery)) => {
                delivery_label(delivery, &answer.outgoing)
            }
            _ => outgoing_label(&answer.outgoing),
        };
        let words = clean_line(&reply_text(prompt).unwrap_or_default());
        format!("{words} · {label}")
    });
    QuestionPanel {
        approval_id: prompt.id.0.to_string(),
        from: prompt
            .from
            .as_deref()
            .map(|from| clean_line(&format!("from subagent: {from}"))),
        call_key,
        questions: prompt
            .questions
            .iter()
            .map(|q| QuestionView {
                header: clean_line(&q.header),
                text: clean(&q.text),
                options: q
                    .options
                    .iter()
                    .map(|o| QuestionOptionView {
                        label: clean_line(&o.label),
                        description: clean(&o.description),
                        preview: o.preview.as_deref().map(clean),
                    })
                    .collect(),
                multi_select: q.multi_select,
            })
            .collect(),
        can_answer: question_unanswered(prompt),
        answer_label,
        answer: prompt.answer.as_ref().map(|a| outgoing_view(&a.outgoing)),
    }
}

fn settings_view(
    settings: &SettingsView,
    view: &ViewState,
    stay_connected: Option<bool>,
    wall_ms: i64,
) -> ScreenView {
    let now = DateTime::from_timestamp_millis(wall_ms).unwrap_or_default();
    let (blob_policy, blob_policy_label) = match settings.blob_policy {
        BlobPolicy::Eager => (
            BlobPolicyView::Eager,
            "Every tool blob downloads as it syncs. Uses more data and disk.",
        ),
        BlobPolicy::OnDemand => (
            BlobPolicyView::OnDemand,
            "A tool's blob downloads when you expand it.",
        ),
    };
    ScreenView::Settings {
        blob_policy,
        blob_policy_label: blob_policy_label.to_owned(),
        server: settings
            .server_address
            .as_ref()
            .map_or_else(|| "not paired".to_owned(), ToString::to_string),
        server_addresses: settings
            .device_id
            .map(|_| server_addresses_label(settings.other_addresses.len())),
        device: settings
            .device_id
            .map_or_else(|| "not paired".to_owned(), |id| id.0.to_string()),
        fingerprint: settings.server_fingerprint.map(|f| f.to_string()),
        agent_login: settings
            .device_id
            .map(|_| login::expiry_label(view.login_expires, now)),
        login: settings
            .device_id
            .map(|_| agent_login_view(&settings.login, now)),
        stay_connected,
        safety_rules: settings.safety_rules.iter().map(safety_rule_row).collect(),
    }
}

/// A safety rule as Settings lists it, with this device's change to it while
/// it is on its way or was refused.
fn safety_rule_row(rule: &SafetyRuleView) -> SafetyRuleRow {
    let pending = rule.pending.as_ref().map(|change| match &change.outgoing {
        Outgoing::Failed(reason) => {
            let verb = if change.on { "add" } else { "remove" };
            format!("could not {verb}: {}", fail_reason(*reason))
        }
        _ if change.on => "adding".to_owned(),
        _ => "removing".to_owned(),
    });
    let discard = rule
        .pending
        .as_ref()
        .filter(|change| matches!(change.outgoing, Outgoing::Failed(_)))
        .map(|change| change.command.0.to_string());
    SafetyRuleRow {
        session_id: rule.session_id.0.to_string(),
        words: rule.words.clone(),
        label: rule.words.join(" "),
        on: rule.on,
        pending,
        discard,
    }
}

/// Logging the agent in from this device, for the settings screen.
fn agent_login_view(login: &LoginView, now: DateTime<Utc>) -> AgentLoginView {
    let (state, url) = match &login.flow {
        LoginFlowView::Idle => (LoginFlowState::Idle, None),
        LoginFlowView::Starting => (LoginFlowState::Starting, None),
        LoginFlowView::Open { url, .. } => (LoginFlowState::Open, Some(url)),
        LoginFlowView::Finishing { url } => (LoginFlowState::Finishing, Some(url)),
        LoginFlowView::NoAnswer { url, .. } => (LoginFlowState::NoAnswer, Some(url)),
        LoginFlowView::Done(LoginOutcome::LoggedIn) => (LoginFlowState::LoggedIn, None),
        LoginFlowView::Done(_) => (LoginFlowState::Failed, None),
    };
    AgentLoginView {
        can_log_in: login.offered,
        logged_out: login.logged_out,
        state,
        url: url.map(|url| clean_line(url)),
        // Server text among them (a failure's words): its breaks become
        // spaces, so each stays one line and keeps its advice at the end.
        lines: login::flow_lines(login, now)
            .iter()
            .map(|line| clean(&line.replace(['\r', '\n'], " ")))
            .collect(),
        // A start while one is on its way, or while a code is, would only
        // drop that answer.
        can_start: login.offered
            && !matches!(state, LoginFlowState::Starting | LoginFlowState::Finishing),
        can_finish: state == LoginFlowState::Open,
        can_retry: state == LoginFlowState::NoAnswer,
        can_dismiss: state != LoginFlowState::Idle,
    }
}

/// What the settings' server tile says of the other addresses it leads to.
fn server_addresses_label(others: usize) -> String {
    match others {
        0 => "Switch or add an address".to_owned(),
        1 => "1 other address".to_owned(),
        n => format!("{n} other addresses"),
    }
}

fn usage_view(view: &UsageView, (span, grouping): (Span, Grouping), wall_ms: i64) -> ScreenView {
    let now = DateTime::from_timestamp_millis(wall_ms)
        .unwrap_or_default()
        .with_timezone(&Local);
    let report = Report::new(view, span, &now);
    let row = |label: String, archived, totals: &Totals| UsageRowView {
        label,
        archived,
        cost_label: totals.cost_label(),
        tokens_label: totals.tokens_label(),
        turns_label: totals.turns_label(),
    };
    let rows: Vec<UsageRowView> = report
        .rows(grouping)
        .iter()
        .map(|r| row(clean_line(&r.label), r.archived, &r.totals))
        .collect();
    ScreenView::Usage {
        span: match span {
            Span::Today => UsageSpanView::Today,
            Span::Week => UsageSpanView::Week,
            Span::Month => UsageSpanView::Month,
            Span::All => UsageSpanView::All,
        },
        grouping: match grouping {
            Grouping::Day => UsageGroupingView::Day,
            Grouping::Session => UsageGroupingView::Session,
            Grouping::Workdir => UsageGroupingView::Workdir,
            Grouping::Model => UsageGroupingView::Model,
        },
        total: row("Total".to_owned(), false, &report.total),
        empty_label: rows.is_empty().then(|| NO_TURNS.to_owned()),
        rows,
        note: report.note,
    }
}

fn storage_view(view: &StorageView, grouping: StorageGrouping) -> ScreenView {
    let rows: Vec<StorageRowView> = storage::rows(view, grouping)
        .into_iter()
        .enumerate()
        .map(|(at, row)| StorageRowView {
            label: clean_line(&row.label),
            detail: clean_line(&row.detail),
            size: row.size,
            target: row
                .actions
                .and_then(|_| storage::row_target(view, grouping, at))
                .as_ref()
                .map(crop::target_view),
            session: row.session.map(storage_session),
        })
        .collect();
    let server_reclaiming = view.status == StorageStatus::Reclaiming;
    let server_reclaimable = view
        .report
        .as_ref()
        .filter(|report| report.reclaimable > 0 && !server_reclaiming)
        .map(|report| bytes(report.reclaimable));
    let device_reclaiming = view.device_reclaiming;
    let device_reclaimable = view
        .device
        .as_ref()
        .filter(|device| device.reclaimable > 0 && !device_reclaiming)
        .map(|device| bytes(device.reclaimable));
    ScreenView::Storage {
        grouping: match grouping {
            StorageGrouping::Sessions => StorageGroupingView::Sessions,
            StorageGrouping::Blobs => StorageGroupingView::Blobs,
            StorageGrouping::ShellRuns => StorageGroupingView::ShellRuns,
        },
        status_label: storage::status_line(view),
        totals: storage_totals(
            view.report
                .as_ref()
                .map(storage::totals)
                .unwrap_or_default(),
        ),
        empty_label: (rows.is_empty() && view.report.is_some())
            .then(|| grouping.empty_label().to_owned()),
        rows,
        server_kinds: storage_kinds(
            view.report
                .as_ref()
                .map(storage::server_kinds)
                .unwrap_or_default(),
        ),
        server_reclaimable,
        server_reclaiming,
        device_status_label: storage::device_status_line(view),
        device_totals: storage_totals(
            view.device
                .as_ref()
                .map(storage::device_totals)
                .unwrap_or_default(),
        ),
        device_kinds: storage_kinds(
            view.device
                .as_ref()
                .map(storage::device_kinds)
                .unwrap_or_default(),
        ),
        device_reclaimable,
        device_reclaiming,
    }
}

fn storage_totals(totals: Vec<storage::Total>) -> Vec<StorageTotalView> {
    totals
        .into_iter()
        .map(|t| StorageTotalView {
            label: t.label.to_owned(),
            value: t.value,
        })
        .collect()
}

/// A stacked bar's parts; the bytes cross as `i64`, clamped.
fn storage_kinds(kinds: Vec<storage::Kind>) -> Vec<StorageKindView> {
    kinds
        .into_iter()
        .map(|kind| StorageKindView {
            label: kind.label.to_owned(),
            bytes: i64::try_from(kind.bytes).unwrap_or(i64::MAX),
            size: bytes(kind.bytes),
        })
        .collect()
}

fn storage_session(facts: storage::SessionFacts) -> StorageSessionView {
    StorageSessionView {
        session_id: facts.session.0.to_string(),
        server_size: facts.server_bytes.map(bytes),
        device_size: facts.device_bytes.map(bytes),
        here: facts.here,
        archived: facts.archived,
        deletable: facts.deletable,
        deletable_everywhere: facts.deletable_everywhere,
    }
}

/// The approvals screen: each waiting approval with its panel, or the note
/// that says where a question or an unknown request is answered.
fn approvals_view(view: &ApprovalsView) -> ScreenView {
    ScreenView::Approvals {
        items: view.items.iter().map(approvals_item).collect(),
    }
}

fn approvals_item(item: &WaitingApproval) -> ApprovalsItem {
    let (panel, note) = match &item.asks {
        Asks::Approval(prompt) => (Some(approval_panel(prompt, None)), None),
        Asks::Question => (
            None,
            Some("a question · answer it in its session".to_owned()),
        ),
        Asks::Unknown => (None, Some("open its session to see it".to_owned())),
    };
    ApprovalsItem {
        session_id: item.session_id.0.to_string(),
        session_title: clean_line(&item.session_title),
        approval_id: item.id.0.to_string(),
        panel,
        note,
    }
}

/// The worktrees screen: each repository's worktrees, with the sessions
/// running in each. Repositories with a linked worktree in use come first.
fn worktrees_view(view: &WorktreesView) -> ScreenView {
    let mut repos: Vec<(u8, WorktreeRepoView)> = view
        .list
        .iter()
        .flat_map(|list| &list.repos)
        .map(|repo| (repo_rank(repo), worktree_repo_view(view, repo)))
        .collect();
    repos.sort_by_key(|(rank, _)| *rank);
    ScreenView::Worktrees {
        status_label: worktrees::status_line(view),
        repos: repos.into_iter().map(|(_, repo)| repo).collect(),
        empty_label: worktrees::empty_label(view),
    }
}

/// Where a repository sorts on the worktrees screen: 0 when it lists a linked
/// worktree, 1 when only its main one, 2 when gone or failed.
fn repo_rank(repo: &RepoWorktrees) -> u8 {
    match &repo.listing {
        RepoListing::Worktrees {
            worktrees: listed, ..
        } if listed.iter().any(|entry| !entry.main) => 0,
        RepoListing::Worktrees { .. } => 1,
        RepoListing::Gone | RepoListing::Failed { .. } => 2,
    }
}

fn worktree_repo_view(view: &WorktreesView, repo: &RepoWorktrees) -> WorktreeRepoView {
    let mut entries: Vec<WorktreeView> = match &repo.listing {
        RepoListing::Worktrees {
            worktrees: listed, ..
        } => listed
            .iter()
            .map(|entry| worktree_view(view, entry))
            .collect(),
        RepoListing::Gone | RepoListing::Failed { .. } => Vec::new(),
    };
    // Linked worktrees in use first, then the other linked ones, the main
    // working tree last (`false` sorts first).
    entries.sort_by_key(|w| (w.main, w.sessions.is_empty()));
    WorktreeRepoView {
        repo: clean_line(&repo.repo),
        note: worktrees::listing_note(&repo.listing).map(|note| clean_line(&note)),
        worktrees: entries,
    }
}

fn worktree_view(view: &WorktreesView, entry: &WorktreeEntry) -> WorktreeView {
    let (active, archived): (Vec<&WorktreeSession>, Vec<&WorktreeSession>) = view
        .sessions
        .get(&entry.path)
        .into_iter()
        .flatten()
        .partition(|session| !session.archived);
    // The first session that may be removed, archived ones last.
    let remove = active
        .iter()
        .chain(&archived)
        .find(|session| session.worktree && !session.worktree_removed)
        .map(|session| WorktreeRemoveView {
            session_id: session.id.0.to_string(),
            title: clean_line(&session.title),
        });
    let removal_label = active
        .iter()
        .chain(&archived)
        .find_map(|session| worktrees::removal_label(session.removal.as_ref()));
    WorktreeView {
        path: clean_line(&entry.path),
        branch: clean_line(&worktrees::branch_label(entry)),
        tags: worktrees::tags(entry),
        main: entry.main,
        sessions: active.into_iter().map(worktree_session_view).collect(),
        archived: archived.into_iter().map(worktree_session_view).collect(),
        remove,
        removal_label,
    }
}

fn worktree_session_view(session: &WorktreeSession) -> WorktreeSessionView {
    WorktreeSessionView {
        session_id: session.id.0.to_string(),
        title: clean_line(&session.title),
    }
}

fn session_info_view(view: &SessionInfoView, wall_ms: i64) -> ScreenView {
    let subagents: Vec<SubagentRowView> = session_info::subagents(view)
        .into_iter()
        .map(|row| SubagentRowView {
            title: clean_line(&row.title),
            detail: row.detail.as_deref().map(clean_line),
            trail: clean_line(&row.trail),
            status_label: clean_line(&row.status),
            live: row.live,
        })
        .collect();
    ScreenView::SessionInfo {
        session_id: view.id.0.to_string(),
        sections: session_info::sections(view)
            .into_iter()
            .map(|section| InfoSectionView {
                title: section.title.to_owned(),
                fields: section
                    .fields
                    .into_iter()
                    .map(|field| InfoFieldView {
                        label: clean_line(&field.label),
                        // A workdir is shown whole; nothing here is multi-line.
                        value: clean_line(&field.value),
                    })
                    .collect(),
            })
            .collect(),
        limits: limits_view(&view.limits, wall_ms),
        subagents_empty_label: subagents
            .is_empty()
            .then(|| session_info::NO_SUBAGENTS.to_owned()),
        subagents,
    }
}

fn limits_view(view: &UsageLimitsView, wall_ms: i64) -> LimitsView {
    let now = DateTime::from_timestamp_millis(wall_ms).unwrap_or_default();
    let (read_label, rows, empty_label) = match limits::body(view.answer.as_ref(), now) {
        limits::Body::Limits { read, rows } => (Some(read), rows, None),
        limits::Body::Empty(text) => (None, Vec::new(), Some(text)),
    };
    LimitsView {
        status_label: limits::status_line(view),
        read_label,
        rows: rows
            .into_iter()
            .map(|row| LimitRowView {
                label: clean_line(&row.label),
                percent_label: row.percent,
                fill_percent: row.fill.map(|fill| (fill * 100.0).round() as u32),
                warning: row.warning,
                resets_label: row.resets,
            })
            .collect(),
        empty_label,
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
        SessionState::BackgroundWork => AgentState::Working,
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
        SessionState::BackgroundWork => "working",
        SessionState::AwaitingApproval(_) => "approval",
        SessionState::Errored(_) => "error",
        SessionState::Stopped => "stopped",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use skywave_core::{
        AgentWork, Attention, BackgroundTask, FullView, Link, Outgoing, Schedule, ScheduleId, Seq,
        TaskId, TaskKind,
    };
    use skywave_present::sections::INPUT_LOST;

    use super::*;

    #[test]
    fn the_server_tile_counts_the_other_addresses() {
        assert_eq!(server_addresses_label(0), "Switch or add an address");
        assert_eq!(server_addresses_label(1), "1 other address");
        assert_eq!(server_addresses_label(3), "3 other addresses");
    }

    /// Settings say when the agent's login expires however far off, once
    /// paired.
    #[test]
    fn settings_show_the_agent_login_expiry_once_paired() {
        let now = DateTime::from_timestamp(1_794_000_000, 0).unwrap();
        let settings = |device_id, login_expires| {
            let settings = SettingsView {
                blob_policy: BlobPolicy::OnDemand,
                blobs_disk_full: false,
                server_address: None,
                other_addresses: Vec::new(),
                device_id,
                server_fingerprint: None,
                login: skywave_core::LoginView {
                    offered: false,
                    logged_out: false,
                    expires: login_expires,
                    flow: skywave_core::LoginFlowView::Idle,
                },
                safety_rules: Vec::new(),
            };
            let view = ViewState {
                link: Link::Online { latency_ms: 1 },
                queued: 0,
                attention: Attention::default(),
                screen: Screen::Settings(settings.clone()),
                can_go_back: true,
                snippets: Vec::new(),
                shell_history: Default::default(),
                login_expires,
                agent_login_offered: false,
                agent_logged_out: false,
            };
            match settings_view(&settings, &view, None, now.timestamp_millis()) {
                ScreenView::Settings { agent_login, .. } => agent_login,
                other => panic!("not settings: {other:?}"),
            }
        };
        let paired = Some(skywave_core::DeviceId::new());
        let expires = now + chrono::TimeDelta::days(40);
        assert_eq!(
            settings(paired, Some(expires)).as_deref(),
            Some("expires in 40 days · 2026-12-16")
        );
        assert_eq!(
            settings(paired, None).as_deref(),
            Some("no expiry reported")
        );
        assert_eq!(settings(None, None), None, "unpaired: no server to ask");
    }

    /// Settings list each safety rule in the order the core gives, with its
    /// logged state and this device's change to it while on its way or
    /// refused.
    #[test]
    fn settings_list_the_safety_rules_as_they_stand() {
        use skywave_core::{CommandId, FailReason, PendingSafetyRule, SafetyRuleView};
        let session_id = SessionId::new();
        let failed = CommandId::new();
        let rule = |words: &[&str], on: bool, pending: Option<PendingSafetyRule>| SafetyRuleView {
            words: words.iter().map(|word| word.to_string()).collect(),
            session_id,
            on,
            pending,
        };
        let settings = SettingsView {
            blob_policy: BlobPolicy::OnDemand,
            blobs_disk_full: false,
            server_address: None,
            other_addresses: Vec::new(),
            device_id: None,
            server_fingerprint: None,
            login: idle(false),
            safety_rules: vec![
                rule(&["git", "commit"], true, None),
                rule(
                    &["cargo", "test"],
                    true,
                    Some(PendingSafetyRule {
                        command: CommandId::new(),
                        on: false,
                        outgoing: Outgoing::Queued,
                    }),
                ),
                rule(
                    &["ls"],
                    false,
                    Some(PendingSafetyRule {
                        command: failed,
                        on: true,
                        outgoing: Outgoing::Failed(FailReason::Undeliverable),
                    }),
                ),
            ],
        };
        let view = ViewState {
            link: Link::Online { latency_ms: 1 },
            queued: 0,
            attention: Attention::default(),
            screen: Screen::Settings(settings.clone()),
            can_go_back: true,
            snippets: Vec::new(),
            shell_history: Default::default(),
            login_expires: None,
            agent_login_offered: false,
            agent_logged_out: false,
        };
        let now = DateTime::from_timestamp(1_794_000_000, 0).unwrap();
        let row = |words: &[&str], on: bool, pending: Option<&str>, discard: Option<String>| {
            SafetyRuleRow {
                session_id: session_id.0.to_string(),
                words: words.iter().map(|word| word.to_string()).collect(),
                label: words.join(" "),
                on,
                pending: pending.map(str::to_owned),
                discard,
            }
        };
        match settings_view(&settings, &view, None, now.timestamp_millis()) {
            ScreenView::Settings { safety_rules, .. } => assert_eq!(
                safety_rules,
                vec![
                    row(&["git", "commit"], true, None, None),
                    row(&["cargo", "test"], true, Some("removing"), None),
                    row(
                        &["ls"],
                        false,
                        Some("could not add: the server could not take it"),
                        Some(failed.0.to_string()),
                    ),
                ]
            ),
            other => panic!("not settings: {other:?}"),
        }
    }

    /// The settings' login tile, for `login`, paired or not.
    fn login_tile(login: LoginView, paired: bool) -> Option<AgentLoginView> {
        let settings = SettingsView {
            blob_policy: BlobPolicy::OnDemand,
            blobs_disk_full: false,
            server_address: None,
            other_addresses: Vec::new(),
            device_id: paired.then(skywave_core::DeviceId::new),
            server_fingerprint: None,
            login,
            safety_rules: Vec::new(),
        };
        let view = ViewState {
            link: Link::Online { latency_ms: 1 },
            queued: 0,
            attention: Attention::default(),
            screen: Screen::Settings(settings.clone()),
            can_go_back: true,
            snippets: Vec::new(),
            shell_history: Default::default(),
            login_expires: settings.login.expires,
            agent_login_offered: settings.login.offered,
            agent_logged_out: settings.login.logged_out,
        };
        let now = DateTime::from_timestamp(1_794_000_000, 0).unwrap();
        match settings_view(&settings, &view, None, now.timestamp_millis()) {
            ScreenView::Settings { login, .. } => login,
            other => panic!("not settings: {other:?}"),
        }
    }

    fn idle(offered: bool) -> LoginView {
        LoginView {
            offered,
            logged_out: true,
            expires: None,
            flow: LoginFlowView::Idle,
        }
    }

    #[test]
    fn the_settings_offer_log_in_only_where_the_server_does() {
        let offered = login_tile(idle(true), true).unwrap();
        assert!(offered.can_log_in && offered.can_start);
        assert!(offered.logged_out);
        assert_eq!(offered.lines, ["agent not logged in"]);
        let not = login_tile(idle(false), true).unwrap();
        assert!(!not.can_log_in && !not.can_start);
        // Offered whatever the login: a user may renew early.
        let logged_in = LoginView {
            logged_out: false,
            ..idle(true)
        };
        assert!(login_tile(logged_in, true).unwrap().can_log_in);
        assert_eq!(login_tile(idle(true), false), None, "unpaired");
    }

    #[test]
    fn every_login_flow_state_maps_to_the_tile() {
        let url = "https://example.test/login";
        let flow = |flow| login_tile(LoginView { flow, ..idle(true) }, true).unwrap();
        // (view, state, url, [start, finish, retry, dismiss], first line)
        let cases = [
            (
                LoginFlowView::Idle,
                LoginFlowState::Idle,
                None,
                [true, false, false, false],
                "agent not logged in",
            ),
            (
                LoginFlowView::Starting,
                LoginFlowState::Starting,
                None,
                [false, false, false, true],
                "starting the login…",
            ),
            (
                LoginFlowView::Open {
                    url: url.into(),
                    refused: Some(skywave_core::LoginCodeRefused::Blank),
                },
                LoginFlowState::Open,
                Some(url),
                [true, true, false, true],
                "open this URL, sign in, paste the code shown",
            ),
            (
                LoginFlowView::Finishing { url: url.into() },
                LoginFlowState::Finishing,
                Some(url),
                [false, false, false, true],
                "sending the code…",
            ),
            (
                LoginFlowView::NoAnswer {
                    url: url.into(),
                    why: skywave_core::LoginNoAnswer::Offline,
                },
                LoginFlowState::NoAnswer,
                Some(url),
                [true, false, true, true],
                "offline · the code was not sent · retry once connected",
            ),
            (
                LoginFlowView::Done(LoginOutcome::LoggedIn),
                LoginFlowState::LoggedIn,
                None,
                [true, false, false, true],
                "agent logged in",
            ),
            (
                LoginFlowView::Done(LoginOutcome::Failed("refused\nsecond line".into())),
                LoginFlowState::Failed,
                None,
                [true, false, false, true],
                "login failed: refused second line · start again",
            ),
            (
                LoginFlowView::Done(LoginOutcome::Busy),
                LoginFlowState::Failed,
                None,
                [true, false, false, true],
                "another login is under way on the server · start again",
            ),
        ];
        for (view, state, want_url, [start, finish, retry, dismiss], first) in cases {
            let tile = flow(view.clone());
            assert_eq!(tile.state, state, "{view:?}");
            assert_eq!(tile.url.as_deref(), want_url, "{view:?}");
            assert_eq!(
                [
                    tile.can_start,
                    tile.can_finish,
                    tile.can_retry,
                    tile.can_dismiss
                ],
                [start, finish, retry, dismiss],
                "{view:?}"
            );
            assert_eq!(
                tile.lines.first().map(String::as_str),
                Some(first),
                "{view:?}"
            );
        }
        let refused = flow(LoginFlowView::Open {
            url: url.into(),
            refused: Some(skywave_core::LoginCodeRefused::Blank),
        });
        assert_eq!(refused.lines[1], "no code pasted · paste the code shown");
    }

    fn task(id: &str, parent: Option<&str>, ambient: bool) -> BackgroundTask {
        BackgroundTask {
            call: None,
            id: TaskId(id.into()),
            kind: TaskKind::Shell,
            title: format!("run {id}"),
            parent: parent.map(|p| TaskId(p.into())),
            ambient,
        }
    }

    fn session_screen(work: AgentWork, task_stops: Vec<(TaskId, Outgoing)>) -> SessionScreen {
        screen_of(session_view(work, task_stops))
    }

    /// The open session `session` as the screen the hub sends.
    fn screen_of(session: SessionView) -> SessionScreen {
        let view = ViewState {
            link: Link::Connecting,
            queued: 0,
            attention: Attention::default(),
            screen: Screen::Session(Box::new(session)),
            can_go_back: true,
            snippets: Vec::new(),
            shell_history: Default::default(),
            login_expires: None,
            agent_login_offered: false,
            agent_logged_out: false,
        };
        let form = PairForm::default();
        let ui = Ui {
            form: &form,
            create: None,
            rename: None,
            composer: None,
            repair_dismissed: false,
            usage: Default::default(),
            storage: Default::default(),
            wall_ms: 0,
            workdir_text: None,
            stay_connected: None,
            plain_list: false,
            conversation_only: false,
        };
        match update(&view, ui).screen {
            ScreenView::Session { session } => session,
            other => panic!("not a session: {other:?}"),
        }
    }

    fn session_view(work: AgentWork, task_stops: Vec<(TaskId, Outgoing)>) -> SessionView {
        SessionView {
            subagent: None,
            id: skywave_core::SessionId::new(),
            title: "x".into(),
            named: false,
            rename: None,
            archived: false,
            workdir: None,
            state: SessionState::Idle,
            timeline: Vec::new(),
            pending_approval: None,
            pending_question: None,
            rules: Vec::new(),
            usage: None,
            agent: AgentView::default(),
            safety_check: false,
            safety_check_change: None,
            contexts: Vec::new(),
            context: skywave_core::ContextId::new(),
            viewing: None,
            can_clear: false,
            clear: None,
            switch: None,
            interrupt: None,
            stop: None,
            work,
            task_stops,
            mode: skywave_core::ComposerMode::Agent,
            draft: None,
            draft_images: Vec::new(),
            draft_files: Vec::new(),
            shell_draft: None,
            draft_rev: 0,
            shell: Default::default(),
            last_seq: Seq(0),
            unread_from: None,
        }
    }

    /// A question's panel carries every question and option as asked, and
    /// once this device answered, what it picked, not how it is sent.
    #[test]
    fn a_question_panel_shows_the_options_and_this_devices_answer() {
        let session = session_view(AgentWork::default(), Vec::new());
        let option = |label: &str, preview: Option<&str>| skywave_core::QuestionOption {
            label: label.into(),
            description: format!("{label} bread"),
            preview: preview.map(str::to_owned),
        };
        let mut prompt = QuestionPrompt {
            id: skywave_core::ApprovalId::new(),
            call: None,
            from: Some("Explore".into()),
            questions: vec![skywave_core::Question {
                header: "Bread".into(),
                text: "Which bread?".into(),
                options: vec![option("Rye", None), option("Spelt", Some("| LOAF |"))],
                multi_select: false,
            }],
            answer: None,
            answers: Vec::new(),
        };
        let panel = question_panel(&prompt, &session);
        assert!(panel.can_answer);
        assert_eq!(panel.from.as_deref(), Some("from subagent: Explore"));
        assert_eq!(panel.answer_label, None);
        assert_eq!(
            panel.questions,
            [QuestionView {
                header: "Bread".into(),
                text: "Which bread?".into(),
                options: vec![
                    QuestionOptionView {
                        label: "Rye".into(),
                        description: "Rye bread".into(),
                        preview: None,
                    },
                    QuestionOptionView {
                        label: "Spelt".into(),
                        description: "Spelt bread".into(),
                        preview: Some("| LOAF |".into()),
                    },
                ],
                multi_select: false,
            }]
        );

        prompt.answer = Some(skywave_core::Answer {
            decision: skywave_core::Decision::Approve,
            rule: None,
            outgoing: Outgoing::Sending,
            delivery: None,
            retryable: false,
        });
        prompt.answers = vec![skywave_core::QuestionAnswer {
            picked: vec![1],
            other: None,
        }];
        let panel = question_panel(&prompt, &session);
        assert!(!panel.can_answer);
        let label = panel.answer_label.unwrap();
        assert!(label.starts_with("you answered: Spelt · "), "{label}");
    }

    #[test]
    fn an_approval_whose_input_was_lost_says_so_up_front() {
        let session = session_view(AgentWork::default(), Vec::new());
        let prompt = |input| ApprovalPrompt {
            id: skywave_core::ApprovalId::new(),
            call: None,
            from: None,
            tool: "write".into(),
            summary: "a.txt".into(),
            input: Some(input),
            suggestions: Vec::new(),
            shell: None,
            answer: None,
        };
        let panel = approval_panel(&prompt(FullView::Lost), Some(&session));
        assert_eq!(panel.lost.as_deref(), Some(INPUT_LOST));
        assert_eq!(panel.input_label, None, "no blob to offer");

        // Control: a kept input is offered and nothing is said.
        let kept = FullView::Blob(skywave_core::BlobView {
            id: skywave_core::BlobId::new(),
            state: skywave_core::BlobState::NotDownloaded { size: 2048 },
            change: None,
            pending: None,
        });
        let panel = approval_panel(&prompt(kept), Some(&session));
        assert_eq!(panel.lost, None);
        assert_eq!(panel.input_label.as_deref(), Some("full input 2.0 KiB"));
    }

    /// The panel shows the gated call, but the request's own input (other
    /// than the call's) is what was lost; and a call whose body was kept
    /// but whose input was lost warns too.
    #[test]
    fn an_approval_of_a_call_warns_of_its_own_lost_input() {
        let kept = || {
            Some(FullView::Blob(skywave_core::BlobView {
                id: skywave_core::BlobId::new(),
                state: skywave_core::BlobState::NotDownloaded { size: 2048 },
                change: None,
                pending: None,
            }))
        };
        let panel = |call_input: Option<FullView>, asked: Option<FullView>| {
            let call = skywave_core::CallId::new();
            let mut session = session_view(AgentWork::default(), Vec::new());
            session.timeline = vec![TimelineItem::Logged {
                parent: None,
                seq: Seq(1),
                at: chrono::Utc::now(),
                context: session.context,
                entry: Entry::Tool(Box::new(skywave_core::ToolView {
                    call,
                    name: "Edit".into(),
                    kind: skywave_core::ToolKind::FileEdit,
                    title: "src/lib.rs".into(),
                    detail: None,
                    body: Some(skywave_core::BodyView {
                        format: skywave_core::BodyFormat::Diff,
                        preview: "-a\n+b".into(),
                        lines: 400,
                        full: kept(),
                    }),
                    input: call_input,
                    approval: None,
                    safety_check: None,
                    safety_feedback: None,
                    shell: None,
                    outcome: skywave_core::ToolOutcome::Running {
                        since: chrono::Utc::now(),
                    },
                    subagent: None,
                })),
            }];
            let prompt = ApprovalPrompt {
                id: skywave_core::ApprovalId::new(),
                call: Some(call),
                from: None,
                tool: "Edit".into(),
                summary: "src/lib.rs".into(),
                input: asked,
                suggestions: Vec::new(),
                shell: None,
                answer: None,
            };
            // The gated call is found through the session's pending prompt.
            session.pending_approval = Some(prompt.clone());
            let panel = approval_panel(&prompt, Some(&session));
            assert_eq!(panel.title, "src/lib.rs", "the gated call shows");
            assert!(panel.gated_key.is_some(), "the gated call shows");
            panel.lost
        };
        assert_eq!(
            panel(kept(), Some(FullView::Lost)).as_deref(),
            Some(INPUT_LOST)
        );
        assert_eq!(
            panel(Some(FullView::Lost), None).as_deref(),
            Some(INPUT_LOST)
        );
        // Control: all kept, nothing said.
        assert_eq!(panel(kept(), kept()), None);
    }

    /// A gated call whose approval asked about an input other than the
    /// call's names that input in the panel; one asking what the call
    /// holds names nothing extra.
    #[test]
    fn an_approval_names_its_own_input_when_it_differs() {
        let panel = |asked: Option<FullView>| {
            let call = skywave_core::CallId::new();
            let id = skywave_core::ApprovalId::new();
            let mut session = session_view(AgentWork::default(), Vec::new());
            session.timeline = vec![TimelineItem::Logged {
                parent: None,
                seq: Seq(1),
                at: chrono::Utc::now(),
                context: session.context,
                entry: Entry::Tool(Box::new(skywave_core::ToolView {
                    call,
                    name: "Bash".into(),
                    kind: skywave_core::ToolKind::Shell,
                    title: "$ make".into(),
                    detail: None,
                    body: None,
                    input: None,
                    approval: Some(skywave_core::ToolApproval {
                        id,
                        question: false,
                        status: ApprovalStatus::Pending,
                        answer: None,
                        input: asked.clone(),
                    }),
                    safety_check: None,
                    safety_feedback: None,
                    shell: None,
                    outcome: skywave_core::ToolOutcome::Running {
                        since: chrono::Utc::now(),
                    },
                    subagent: None,
                })),
            }];
            let prompt = ApprovalPrompt {
                id,
                call: Some(call),
                from: None,
                tool: "Bash".into(),
                summary: "$ make".into(),
                input: asked,
                suggestions: Vec::new(),
                shell: None,
                answer: None,
            };
            session.pending_approval = Some(prompt.clone());
            approval_panel(&prompt, Some(&session))
        };
        let asked = FullView::Blob(skywave_core::BlobView {
            id: skywave_core::BlobId::new(),
            state: skywave_core::BlobState::NotDownloaded { size: 2048 },
            change: None,
            pending: None,
        });
        let shown = panel(Some(asked));
        assert_eq!(
            shown.asked_label.as_deref(),
            Some("approval input: differs from the call's · 2.0 KiB")
        );
        // Not offered as the call's own whole input.
        assert_eq!(shown.input_label, None);
        // Control: nothing asked beyond the call, nothing named.
        assert_eq!(panel(None).asked_label, None);
    }

    #[test]
    fn no_work_says_nothing() {
        let screen = session_screen(AgentWork::default(), Vec::new());
        assert_eq!(screen.work_label, None);
        assert_eq!(screen.stop_warning, None);
        assert!(screen.tasks.is_empty() && screen.schedules.is_empty());
        assert_eq!(screen.loops_note, None);
    }

    /// Background work on an idle turn shows as working, and offers no
    /// interrupt; a working turn is the control.
    #[test]
    fn background_work_shows_working_without_interrupt() {
        let mut session = session_view(AgentWork::default(), Vec::new());
        session.state = SessionState::BackgroundWork;
        let screen = screen_of(session.clone());
        assert_eq!(screen.state, AgentState::Working);
        assert_eq!(screen.badge_label, "working");
        assert_eq!(screen.agent_label, "background work running");
        assert!(!screen.can_interrupt);

        session.state = SessionState::AgentWorking;
        let screen = screen_of(session);
        assert_eq!(screen.agent_label, "agent working");
        assert!(screen.can_interrupt);
    }

    #[test]
    fn work_rows_carry_depth_labels_and_this_devices_stop() {
        let work = AgentWork {
            tasks: vec![
                task("child", Some("top"), false),
                task("top", None, false),
                task("watch", None, true),
            ],
            schedules: vec![
                Schedule {
                    id: ScheduleId("s1".into()),
                    when: "*/5 * * * *".into(),
                    recurring: true,
                    prompt: "check".into(),
                    outlives_agent: Some(true),
                },
                Schedule {
                    id: ScheduleId("s2".into()),
                    when: "in 1h".into(),
                    recurring: false,
                    prompt: "later".into(),
                    outlives_agent: None,
                },
            ],
        };
        let screen = session_screen(work, vec![(TaskId("top".into()), Outgoing::Queued)]);
        assert_eq!(
            screen.work_label.as_deref(),
            Some("2 background tasks · 2 loops")
        );
        assert!(screen.stop_warning.as_deref().is_some_and(|w| {
            w.starts_with("Stopping ends 2 background tasks and pauses 2 loops.")
        }));
        let shape: Vec<(&str, u32)> = screen
            .tasks
            .iter()
            .map(|t| (t.task_id.as_str(), t.depth))
            .collect();
        assert_eq!(shape, [("top", 0), ("child", 1), ("watch", 0)]);
        assert_eq!(screen.tasks[0].kind_label, "shell");
        assert_eq!(screen.tasks[0].stop_label.as_deref(), Some("queued"));
        assert_eq!(screen.tasks[1].stop_label, None);
        assert_eq!(screen.tasks[2].ambient_label.as_deref(), Some("watcher"));
        assert_eq!(screen.tasks[0].ambient_label, None);
        assert_eq!(screen.schedules[0].repeat_label, "repeats");
        assert_eq!(
            screen.schedules[0].saved_label.as_deref(),
            Some("saved in this folder")
        );
        assert_eq!(screen.schedules[1].repeat_label, "once");
        assert_eq!(screen.schedules[1].saved_label, None);
        assert!(screen.loops_note.is_some());
        assert_eq!(screen.stop_headings.stop_agent, "Stop the agent?");
    }

    #[test]
    fn a_watcher_alone_needs_no_confirmation() {
        let work = AgentWork {
            tasks: vec![task("watch", None, true)],
            schedules: Vec::new(),
        };
        let screen = session_screen(work, Vec::new());
        assert_eq!(screen.stop_warning, None);
        assert_eq!(screen.work_label, None);
        assert_eq!(screen.tasks.len(), 1, "still listed");
    }
}
