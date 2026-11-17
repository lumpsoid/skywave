use rinf::{RustSignal, SignalPiece};
use serde::{Deserialize, Serialize};

use super::crop::{BlobTargetView, CropScreen};
use super::search::Utf16Range;
use super::timeline::{OutgoingView, SectionView, ToolKindView};
use super::workdir::WorkdirScreen;

/// The screen to show, with everything on it but the timeline (its own
/// signal) and the banner. Sent whole whenever it changes; Dart replaces
/// it, and the navigator follows `screen` (D18).
#[derive(Debug, Clone, PartialEq, Serialize, RustSignal)]
pub struct ScreenUpdate {
    pub screen: ScreenView,
    /// `Back` goes somewhere; when false, system back leaves the app.
    pub can_go_back: bool,
    /// The link stopped for a reason only pairing again fixes (revoked, the
    /// server's CA or names changed, a damaged identity): a full-screen
    /// notice covers `screen` until `RepairDismiss` (browse local data) or
    /// `Navigate(Pairing)`. `None` otherwise.
    pub repair: Option<RepairNotice>,
}

/// The full-screen re-pair notice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct RepairNotice {
    /// A sentence: `This device was revoked.`
    pub title: String,
    /// What happened and what to do.
    pub body: String,
    /// `RetryNow` connects again: a fix on the server may clear it.
    pub can_retry: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, SignalPiece)]
#[expect(
    clippy::large_enum_variant,
    reason = "one screen at a time, sent whole; boxing gains nothing"
)]
pub enum ScreenView {
    /// Pairing, first or again.
    Pairing {
        /// This device's id when re-pairing; `None` before the first.
        paired_device: Option<String>,
        /// The server it is paired with, when re-pairing.
        paired_server: Option<String>,
        state: PairingState,
        /// `pairing with host:7443…`, `pairing failed: <reason>`.
        status_label: Option<String>,
        /// What the form's fields hold when `form.rev` changes.
        form: PairForm,
    },
    SessionList {
        kind: ListKindView,
        /// `Sessions`; the scope control says which list shows.
        title: String,
        /// What an empty list says; `None` when it has rows.
        empty_label: Option<String>,
        rows: Vec<SessionRowView>,
        /// Show `rows` as one plain list in recency order: no project headers.
        /// Otherwise rows come grouped by project, a header wherever `group`
        /// changes.
        plain: bool,
        /// Sessions this device asked for that are not in the replica yet,
        /// or that the server refused; oldest first.
        creating: Vec<CreatingView>,
        /// The refused creation `Discard` takes off the list, if any.
        dismissable: Option<String>,
        /// The new-session prompt, while open (`CreateOpen`); it closes
        /// once the core took the session.
        create: Option<CreatePrompt>,
        /// The picker of the server agent's conversations, while open
        /// (`ConversationsOpen`); it closes on `ConversationsClose`, once
        /// one is picked, or on leaving the list.
        conversations: Option<ConversationsView>,
        /// The rename prompt, while open (`RenameOpen`).
        rename: Option<RenamePrompt>,
    },
    Session {
        session: SessionScreen,
    },
    Settings {
        blob_policy: BlobPolicyView,
        /// What the selected policy does, shown under the toggle.
        blob_policy_label: String,
        /// The server address, or `not paired`.
        server: String,
        /// Under the address, on the tile that opens the server addresses
        /// page, e.g. `2 other addresses`; `None` while unpaired, when
        /// there is no server to move.
        server_addresses: Option<String>,
        /// This device's id, or `not paired`.
        device: String,
        /// The pinned server CA fingerprint, to compare with what
        /// `skywaved` prints.
        fingerprint: Option<String>,
        /// When the agent's login on the server expires, however far off:
        /// `expires in 12 days · 2026-11-21`, `expired · 2026-11-08`, `no
        /// expiry reported`; `None` while unpaired.
        agent_login: Option<String>,
        /// Logging the server's agent in from this device; `None` while
        /// unpaired.
        login: Option<AgentLoginView>,
        /// Whether the link stays open with the app in the background
        /// (the head's own preference, not the core's); `None` where it
        /// is not offered: everywhere but Android.
        stay_connected: Option<bool>,
        /// This device's notification switches (the head's own preference,
        /// not the core's).
        notifications: NotificationsView,
        /// The safety rules: the ones the server's log holds, and the ones
        /// with a change of this device still on its way; in the core's
        /// order.
        safety_rules: Vec<SafetyRuleRow>,
        /// This build's version and commit, `0.1.0 (2391c1d)`.
        version: String,
    },
    /// Cost and tokens across sessions, from the replica alone.
    Usage {
        /// The days counted; `UsageShow` picks them.
        span: UsageSpanView,
        /// What the rows are; `UsageShow` picks it.
        grouping: UsageGroupingView,
        /// Every turn in the span.
        total: UsageRowView,
        /// Newest day first, else costliest first.
        rows: Vec<UsageRowView>,
        /// What an empty span says; `None` when it has rows.
        empty_label: Option<String>,
        /// What the totals leave out: sessions deleted from this device.
        note: String,
    },
    /// What the server's data takes on its disk, asked for on opening.
    Storage {
        /// Which list the rows are; `StorageShow` picks it.
        grouping: StorageGroupingView,
        /// Asking, offline or failed; `None` once the report is fresh.
        status_label: Option<String>,
        /// Free space first; empty until a report arrives.
        totals: Vec<StorageTotalView>,
        /// Largest first.
        rows: Vec<StorageRowView>,
        /// What an empty list says; `None` when it has rows or there is
        /// no report.
        empty_label: Option<String>,
        /// The server's disk by kind, for a stacked bar; empty without a
        /// report.
        server_kinds: Vec<StorageKindView>,
        /// What the server's reclaim would give back, as `2.0 MiB`; `None`
        /// when there is nothing to reclaim, or a reclaim is running.
        server_reclaimable: Option<String>,
        /// A reclaim on the server is running.
        server_reclaiming: bool,
        /// What the screen says above this device's storage while it has no
        /// fresh answer; `None` once it has one.
        device_status_label: Option<String>,
        /// This device's totals, `Used` first; empty until its report.
        device_totals: Vec<StorageTotalView>,
        /// This device's disk by kind, for a stacked bar; empty until its
        /// report.
        device_kinds: Vec<StorageKindView>,
        /// What this device's reclaim would give back, as `2.0 MiB`; `None`
        /// when there is nothing to reclaim, or a reclaim is running.
        device_reclaimable: Option<String>,
        /// A reclaim of this device's database is running.
        device_reclaiming: bool,
    },
    /// Every git worktree of the repositories sessions' worktrees came
    /// from, asked for on opening; `RefreshWorktrees` asks again.
    Worktrees {
        /// Asking, offline or failed; `None` once the list is fresh.
        status_label: Option<String>,
        /// One per repository: those with a linked worktree that is in use
        /// first, then the other listed ones, then those gone or failed; the
        /// server's path order within each group.
        repos: Vec<WorktreeRepoView>,
        /// What an empty list says; `None` when it has repos or there is no
        /// list.
        empty_label: Option<String>,
    },
    /// Every approval this device can answer, from every session, oldest
    /// first.
    Approvals {
        items: Vec<ApprovalsItem>,
    },
    /// The paired server's addresses: the one in use, then the others to
    /// switch to (`SetServerAddress`, also for a new one) or forget
    /// (`ForgetServerAddress`), in the order they were left.
    ServerAddresses {
        current: String,
        others: Vec<String>,
    },
    /// The usage limits of the account the server's agent runs as: the
    /// stored ones, asked for on opening only when none are stored.
    UsageLimits {
        limits: LimitsView,
    },
    /// A session's particulars: what it is, the account's usage limits
    /// (as the usage limits page's), what its turns took, its agent's
    /// settings, its models and every subagent it ran.
    SessionInfo {
        session_id: String,
        /// The session, its usage, its agent and its models (left out
        /// while no turn reported usage), in that order.
        sections: Vec<InfoSectionView>,
        limits: LimitsView,
        /// In the order they were launched, nested ones included.
        subagents: Vec<SubagentRowView>,
        /// What an empty subagent list says; `None` when it has rows.
        subagents_empty_label: Option<String>,
    },
    /// A session's workdir, read on request and kept for offline reading.
    Workdir {
        workdir: WorkdirScreen,
    },
    /// Which lines of an output to keep (`OpenCrop`); back returns to the
    /// session or storage screen it was opened from.
    Crop {
        crop: CropScreen,
    },
}

/// One approval or question on the approvals screen.
#[derive(Debug, Clone, PartialEq, Serialize, SignalPiece)]
pub struct ApprovalsItem {
    pub session_id: String,
    pub session_title: String,
    pub approval_id: String,
    /// The approval to answer here; `None` for a question or a request the
    /// replica lacks, which open in their session.
    pub panel: Option<ApprovalPanel>,
    /// What to say instead of a panel, e.g. "a question · answer it in its
    /// session".
    pub note: Option<String>,
}

/// Logging the server's agent in from this device, on the settings screen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct AgentLoginView {
    /// The server offers a login from a device: the tile shows `Log in`
    /// (`StartAgentLogin`), always, so the login can be renewed early.
    pub can_log_in: bool,
    /// The server says its agent is logged out.
    pub logged_out: bool,
    pub state: LoginFlowState,
    /// The page to log in at, to open or copy, while a login is open, its
    /// code on its way, or unanswered; `None` otherwise.
    pub url: Option<String>,
    /// Where the login stands, in lines: the steps, why a code was not
    /// sent, why it got no answer, how the login ended. Empty when there is
    /// nothing to say.
    pub lines: Vec<String>,
    /// `StartAgentLogin` would start one: offered, and none is under way
    /// (idle, ended, or open, which a new start joins).
    pub can_start: bool,
    /// `FinishAgentLogin` takes a pasted code: a login is open.
    pub can_finish: bool,
    /// `RetryAgentLogin` sends the unanswered code again.
    pub can_retry: bool,
    /// `DismissAgentLogin` forgets the login: one is started or ended.
    pub can_dismiss: bool,
}

/// Where this device's login of the server's agent stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, SignalPiece)]
pub enum LoginFlowState {
    /// None started, or the last one dismissed.
    Idle,
    /// Asked the server to start one.
    Starting,
    /// Open at `url`: waiting for the pasted code.
    Open,
    /// The pasted code is on its way.
    Finishing,
    /// The pasted code got no answer; `RetryAgentLogin` sends it again.
    NoAnswer,
    /// The agent is logged in.
    LoggedIn,
    /// The login ended without logging in; `lines` say why.
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SignalPiece)]
pub enum UsageSpanView {
    Today,
    Week,
    Month,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SignalPiece)]
pub enum UsageGroupingView {
    Day,
    Session,
    Workdir,
    Model,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SignalPiece)]
pub enum StorageGroupingView {
    Sessions,
    Blobs,
    ShellRuns,
}

/// One total of the storage screen: `Free`, `Database`, ….
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct StorageTotalView {
    pub label: String,
    /// `18.6 GiB`, `none (logs to stderr)`.
    pub value: String,
}

/// One row of the storage screen: a session, a blob or a shell run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct StorageRowView {
    /// A session's title, or `$ command`.
    pub label: String,
    /// What makes up the size.
    pub detail: String,
    /// `2.0 MiB`.
    pub size: String,
    /// The blob a blob or shell run row stands for, and what may be done
    /// to it; `None` for a session row and a run kept as no blob.
    pub target: Option<BlobTargetView>,
    /// The session a Sessions row stands for, for selecting and acting on
    /// it; `None` for other rows.
    pub session: Option<StorageSessionView>,
}

/// One part of a stacked bar of the storage screen: `Database`, `Blobs`, ….
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct StorageKindView {
    pub label: String,
    /// The bytes, for the bar's fractions. `i64` because Dart reads big
    /// integers as one; Rust clamps to `i64::MAX`.
    pub bytes: i64,
    /// `18.6 GiB`.
    pub size: String,
}

/// What a Sessions row of the storage screen stands for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct StorageSessionView {
    pub session_id: String,
    /// What the server keeps for the session, `2.0 MiB`; `None` when the
    /// server's report does not list it.
    pub server_size: Option<String>,
    /// What this device keeps for the session; `None` when this device's
    /// report does not list it.
    pub device_size: Option<String>,
    /// The session is in this replica.
    pub here: bool,
    /// On the archived list.
    pub archived: bool,
    /// Removable from this device, as the session list says.
    pub deletable: bool,
    /// Deletable everywhere, as the session list says.
    pub deletable_everywhere: bool,
}

/// One repository of the worktrees screen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct WorktreeRepoView {
    /// The repository's main working tree's path.
    pub repo: String,
    /// Why the list is short or missing (more, gone, git failed).
    pub note: Option<String>,
    /// Linked worktrees with a session in use first, then the other linked
    /// ones, the main working tree last; git's order within each group.
    pub worktrees: Vec<WorktreeView>,
}

/// One git worktree of a repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct WorktreeView {
    pub path: String,
    /// The branch, `detached at <sha>` when none is checked out.
    pub branch: String,
    /// `main`, `missing`, `uncommitted changes`, `merged`, `not merged`….
    pub tags: Vec<String>,
    /// The repository's main working tree, which can't be removed.
    pub main: bool,
    /// Its sessions that are not archived, latest message first.
    pub sessions: Vec<WorktreeSessionView>,
    /// Its archived sessions, latest message first.
    pub archived: Vec<WorktreeSessionView>,
    /// How to remove it; `None` when no session of it may send `RemoveWorktree`.
    pub remove: Option<WorktreeRemoveView>,
    /// A removal on its way or failed; `None` when none or settled.
    pub removal_label: Option<String>,
}

/// The session a worktree's removal is sent for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct WorktreeRemoveView {
    pub session_id: String,
    pub title: String,
}

/// A session running in a listed worktree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct WorktreeSessionView {
    pub session_id: String,
    pub title: String,
}

/// The usage limits as listed, on their page and a session's info page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct LimitsView {
    /// Asking, offline or failed; `None` otherwise.
    pub status_label: Option<String>,
    /// `Read 12 min ago`; `None` without limits to list.
    pub read_label: Option<String>,
    /// In the agent's order.
    pub rows: Vec<LimitRowView>,
    /// Why there is nothing to list; `None` when there are rows.
    pub empty_label: Option<String>,
}

/// A titled group of a session info page's figures.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct InfoSectionView {
    /// `Session`, `Usage`, `Agent`, `Models`.
    pub title: String,
    pub fields: Vec<InfoFieldView>,
}

/// One labelled figure: `Tokens in` `153k`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct InfoFieldView {
    pub label: String,
    pub value: String,
}

/// A subagent on a session's info page, as its row in the timeline reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct SubagentRowView {
    pub title: String,
    /// The kind of subagent (e.g. `Explore`).
    pub detail: Option<String>,
    /// The subagents it ran under, outermost first: `outer › inner`; empty
    /// for one the main agent launched.
    pub trail: String,
    /// `finished · 14 steps · 4.3s · ctx 12k`.
    pub status_label: String,
    /// Still at work.
    pub live: bool,
}

/// One limit of the usage limits page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct LimitRowView {
    /// `Session`, `Weekly`, `Weekly · Fable`.
    pub label: String,
    /// `75%`; `None` when the agent did not say.
    pub percent_label: Option<String>,
    /// How full its bar is, 0 to 100; `None` without a percent.
    pub fill_percent: Option<u32>,
    /// The agent warns that this limit is close.
    pub warning: bool,
    /// `resets in 2 h 13 min`; `None` when the agent did not say.
    pub resets_label: Option<String>,
}

/// One line of the usage screen: a day, session, directory or model, or
/// the total.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct UsageRowView {
    /// `2026-11-02 Mon`, a session's title, a directory, a model, `Total`.
    pub label: String,
    /// A session row whose session is archived.
    pub archived: bool,
    /// `$1.27`, `$1.27 + 2 unpriced`, `cost unknown`.
    pub cost_label: String,
    /// `3.1k in · 812 out`, `tokens unknown`.
    pub tokens_label: String,
    /// `7 turns`.
    pub turns_label: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, SignalPiece)]
pub enum PairingState {
    Idle,
    InProgress,
    Failed,
}

/// The pairing form's fields as the hub last set them. Dart overwrites
/// its fields only when `rev` changes (a pasted URI filled them, a
/// re-pair prefilled the address); otherwise its fields are the user's.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, SignalPiece)]
pub struct PairForm {
    pub address: String,
    pub code: String,
    pub fingerprint: String,
    pub rev: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, SignalPiece)]
pub enum ListKindView {
    Active,
    Archived,
    /// Active and archived together.
    All,
}

/// One session on a list.
#[derive(Debug, Clone, PartialEq, Serialize, SignalPiece)]
pub struct SessionRowView {
    pub session_id: String,
    pub title: String,
    pub workdir: Option<String>,
    /// The workdir's last component, `skywave`, for a narrow list.
    pub workdir_name: Option<String>,
    /// The repo (else the directory) this row is grouped under. Rows of one
    /// group are adjacent; a head shows a header where it changes.
    pub group: String,
    /// The group's last path component, `skywave`; "no directory" for the
    /// empty group.
    pub group_name: String,
    /// The branch of the git worktree the session runs in.
    pub branch: Option<String>,
    pub state: AgentState,
    /// `idle`, `working`, `approval`, `error`, `stopped`.
    pub badge_label: String,
    /// Agent output this device has not seen.
    pub unread: u32,
    pub approvals: u32,
    /// `1 approval pending`; `None` for none.
    pub approvals_label: Option<String>,
    /// The agent's error, when it errored.
    pub error: Option<String>,
    /// This device keeps an unsent message for it.
    pub has_draft: bool,
    /// What it has cost: `$1.27`.
    pub cost_label: Option<String>,
    /// `2026-10-31 14:02 UTC`.
    pub last_activity_label: String,
    pub last_activity_ms: i64,
    /// This device's rename on its way, until settled.
    pub rename: Option<OutgoingView>,
    /// `queued`, `rename failed: <why>`; `None` once settled.
    pub rename_label: Option<String>,
    /// `importing history…` while an adopted conversation's history is
    /// still arriving; `None` otherwise.
    pub importing_label: Option<String>,
    /// `DeleteSessionLocally` would remove it (archived, nothing waiting).
    pub deletable: bool,
    /// `DeleteSessionEverywhere` would delete it: `deletable`, and no worktree
    /// of it on disk.
    pub deletable_everywhere: bool,
    /// Why it cannot be deleted yet, for an archived row that is not
    /// deletable.
    pub delete_blocked_label: Option<String>,
    /// `RemoveWorktree` is offered: the row runs in a git worktree that is
    /// not removed yet, archived or not.
    pub removable: bool,
    /// The session's worktree was removed; its work is refused.
    pub worktree_removed: bool,
    pub archived: bool,
}

/// A session this device asked for, not in the replica yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct CreatingView {
    pub command_id: String,
    pub workdir: String,
    pub outgoing: OutgoingView,
    /// `waits for the server`, `created, syncing…`, why it was refused;
    /// `Importing conversation history…`, `will import when online` for
    /// one that continues an outside conversation.
    pub note: Option<String>,
    pub discardable: bool,
}

/// The conversations the server's agent keeps, to continue one in a new
/// session: newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct ConversationsView {
    pub rows: Vec<ConversationRowView>,
    /// `listing…`, `only the newest are listed`; `None` otherwise.
    pub status_label: Option<String>,
    /// What an empty picker says: asking, none under the roots, or why
    /// the server cannot list (`offline · …`, `can't list conversations:
    /// …`); `None` when it has rows.
    pub empty_label: Option<String>,
    /// `empty_label` says why the server could not list: a warning.
    pub unavailable: bool,
}

/// One conversation of the picker. `ConversationPick` continues it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct ConversationRowView {
    /// The agent's id for it, for `ConversationPick`.
    pub conversation_id: String,
    /// The agent's title, else its first prompt, else its id.
    pub title: String,
    /// `…/personal/skywave · 3 h ago · 42 messages`.
    pub detail: String,
    /// `already in Skywave: “fix the flaky test”`, `continued in …`;
    /// `None` when Skywave has it nowhere.
    pub link_label: Option<String>,
    /// The session running it now, to open instead (`OpenSession`).
    pub session_id: Option<String>,
}

/// The new-session prompt: a server workdir with completion. The
/// optional outside conversation id is not echoed: it is the user's
/// alone, so Dart owns its field and only sends edits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct CreatePrompt {
    /// The input as the hub last set it; Dart overwrites its field only
    /// when `input_rev` changes (a completion filled it in).
    pub input: String,
    /// Changes when a user action (Tab, Enter, a tap) rewrote the input;
    /// never on a reply.
    pub input_rev: u32,
    /// The candidate list; shown whenever the core has a completion.
    pub popup: Option<CompletionPopup>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct CompletionPopup {
    pub candidates: Vec<CandidateView>,
    /// The highlighted candidate; none until ↓ (Enter then creates).
    pub selected: Option<u32>,
    /// The candidates are for an earlier input while a new directory loads:
    /// shown dimmed, and they cannot be highlighted or chosen.
    pub stale: bool,
    /// A listing is pending: Dart shows a progress bar, not a label.
    pub loading: bool,
    /// `offline · recent only`, `more not shown · keep typing`.
    pub status_label: Option<String>,
    /// `no matching directories`, when there are no candidates.
    pub empty_label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct CandidateView {
    pub path: String,
    /// The text a row shows: for a server listing under the input's
    /// directory, the name past it; otherwise the full path.
    pub label: String,
    /// From recent workdirs, not the server's listing.
    pub recent: bool,
}

/// The rename prompt, over a list row or the open session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct RenamePrompt {
    pub session_id: String,
    /// What the field starts with: the name, or empty for a default title.
    /// Empty restores the default title.
    pub initial: String,
}

/// The open session, but its timeline.
#[derive(Debug, Clone, PartialEq, Serialize, SignalPiece)]
pub struct SessionScreen {
    pub session_id: String,
    pub title: String,
    pub workdir: Option<String>,
    /// The workdir's last component, `skywave`, for a narrow header.
    pub workdir_name: Option<String>,
    pub state: AgentState,
    pub badge_label: String,
    pub rename: Option<OutgoingView>,
    pub rename_label: Option<String>,
    /// `archived · a message brings it back`, while archived.
    pub archived_label: Option<String>,
    /// `agent idle`, `agent working`, `agent error: <message>`.
    pub agent_label: String,
    /// The agent has a turn to interrupt.
    pub can_interrupt: bool,
    /// This device's interrupt is on its way.
    pub interrupt_busy: bool,
    /// Why this device's interrupt failed, while the agent still works.
    pub interrupt_refusal: Option<String>,
    /// `stop queued`, `stop failed: <why>`, while on its way.
    pub stop_label: Option<String>,
    /// `new context queued`, `new context failed: <why>`, while on its way.
    pub clear_label: Option<String>,
    /// `switch queued`, `switch failed: <why>`, while on its way.
    pub switch_label: Option<String>,
    /// Offer "New context"; false in a context with no message yet.
    pub can_clear: bool,
    /// `2 background tasks · 1 loop`; `None` while there is nothing to say.
    pub work_label: Option<String>,
    /// What stopping the agent does to its work; `None` when nothing is
    /// lost, and no confirmation is needed.
    pub stop_warning: Option<String>,
    /// The questions the stop confirmations ask.
    pub stop_headings: StopHeadings,
    /// The agent's background tasks, a child under its parent.
    pub tasks: Vec<TaskRow>,
    /// The agent's loops: information only, Skywave cannot end one.
    pub schedules: Vec<ScheduleRow>,
    /// Beneath the loops; `None` without any.
    pub loops_note: Option<String>,
    /// For the list when it has no rows.
    pub work_empty_label: String,
    /// The contexts the picker lists, in order.
    pub contexts: Vec<ContextRow>,
    /// The context messages go to.
    pub context_id: String,
    /// The context the timeline is filtered to; `None` for the whole session.
    pub viewing_id: Option<String>,
    /// `Viewing start — messages go to velvet walrus`; `None` while the
    /// whole session or the current context shows.
    pub viewing_hint: Option<String>,
    /// The lasting rules granted here, `label (scope)`.
    pub rules: Vec<String>,
    /// `2 rules`; `None` for none.
    pub rules_label: Option<String>,
    /// The agent's mode and model, and what switches them.
    pub agent: AgentSettingsView,
    /// Skywave's safety check: its switch and what a tap on it asks for.
    pub safety_check: SafetyCheckView,
    /// `last turn 12.4s`, `3.1k in / 812 out`, `$0.04`, `ctx 19k/1M · 2%`
    /// (`ctx ?` while unknown), `session $1.27`, `<model>` until the agent
    /// names its model in `agent`: drop from the front when short of room.
    pub usage: Vec<String>,
    /// The approval the agent is blocked on.
    pub approval: Option<ApprovalPanel>,
    /// The question the agent is blocked on; at most one of it and
    /// `approval` is set.
    pub question: Option<QuestionPanel>,
    /// The subagent whose own steps the timeline shows; `None` in the main
    /// view.
    pub subagent: Option<SubagentHeaderView>,
    pub composer: ComposerView,
    /// This device's snippets for the composer's picker, the most recently
    /// used or saved first.
    pub snippets: Vec<SnippetView>,
    /// The rename prompt, while open (`RenameOpen`).
    pub rename_prompt: Option<RenamePrompt>,
    /// Only the conversation shows: the timeline's rows leave out tool
    /// calls, but one an approval waits on (`ToggleConversationOnly`).
    pub conversation_only: bool,
}

/// The questions the stop confirmations ask, one per action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct StopHeadings {
    pub stop_agent: String,
    pub new_context: String,
    pub switch_context: String,
    pub move_context: String,
    /// Stopping the shown subagent's task.
    pub stop_task: String,
}

/// The subagent a session view shows, for its header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct SubagentHeaderView {
    /// Its `Agent` call: which subagent this is.
    pub call: String,
    /// As its call's title.
    pub title: String,
    /// Where it is: the session, then the subagents it runs under,
    /// outermost first: `work › outer`.
    pub trail: String,
    /// `running · 14 steps · Write src/sync.rs`, `finished · 14 steps`.
    pub status_label: String,
    /// Still at work.
    pub live: bool,
    /// The task to stop with `StopTask` while it runs as one; `None` for
    /// a finished subagent and for a foreground one, which has no task
    /// (Interrupt ends it with the turn).
    pub task_id: Option<String>,
}

/// One background task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct TaskRow {
    pub task_id: String,
    /// Levels under the task that started it; 0 for a top task.
    pub depth: u32,
    /// `shell`, `subagent`, `task`.
    pub kind_label: String,
    pub title: String,
    /// `watcher` for an ambient task; `None` otherwise.
    pub ambient_label: Option<String>,
    /// `queued`, `sending`, `failed: <why>`: this device's stop of it, while
    /// on its way.
    pub stop_label: Option<String>,
    /// The `Agent` call of the subagent it runs, to open with
    /// `ViewSubagent`; `None` for any other task.
    pub call: Option<String>,
}

/// One loop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct ScheduleRow {
    pub schedule_id: String,
    /// When it fires, as the agent said.
    pub when: String,
    /// `repeats`, `once`.
    pub repeat_label: String,
    pub prompt: String,
    /// `saved in this folder`, when known to outlive the agent.
    pub saved_label: Option<String>,
}

/// One context in the picker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct ContextRow {
    pub context_id: String,
    /// `velvet walrus · fix the flaky test`, or the name alone.
    pub label: String,
    /// Messages go here.
    pub current: bool,
    /// `queued`, `sending`, ...: this device's clear or switch making it
    /// current, while on its way.
    pub outgoing_label: Option<String>,
    /// The session it was moved to, when it was.
    pub moved_to_session_id: Option<String>,
    /// `moved → “fix the flaky test”`; `None` for one not moved.
    pub moved_label: Option<String>,
    /// Offer "Move to new session".
    pub can_move: bool,
    /// Why it cannot move; `None` while it can, or once moved.
    pub move_refusal: Option<String>,
    /// The command to discard to drop this device's refused or untried move
    /// of it.
    pub discard_move: Option<String>,
}

/// A saved prompt, as the composer's picker lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct SnippetView {
    pub snippet_id: String,
    /// Its name, or else its text's first line.
    pub label: String,
    /// Its text's first line under a name; `None` without one.
    pub detail: Option<String>,
    /// Its name and whole text as saved, for the editor.
    pub name: Option<String>,
    pub text: String,
}

/// The session agent's permission mode, model and reasoning effort: the
/// status line's three chips and their pickers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct AgentSettingsView {
    /// `ask`, `ask → plan` while a switch waits for the agent, `mode
    /// unknown` until it says (`unknown → plan` once one is asked for).
    pub mode_label: String,
    /// `Sonnet`, `Sonnet → Opus`, `model unknown` until the agent names
    /// one (`unknown → Opus` once one is asked for).
    pub model_label: String,
    /// `effort high`, `high → max` while a switch waits for the agent,
    /// `no effort` for a model that takes none, `effort unknown` until the
    /// agent says (`unknown → max` once one is asked for).
    pub effort_label: String,
    /// The model as the agent names it, for the picker's name field.
    pub model_name: Option<String>,
    /// `bypass refused: <why>`, `plan failed: <why>`: the newest refusal
    /// of a mode, until the next switch.
    pub mode_refusal: Option<String>,
    /// Likewise for a model.
    pub model_refusal: Option<String>,
    /// Likewise for an effort.
    pub effort_refusal: Option<String>,
    /// The modes to offer, from the most asking to the least: bypass only
    /// where the server allows it (`skywave_present::format::offered_modes`).
    pub modes: Vec<ModeOption>,
    /// The models the agents offer; empty until one has, when only a name
    /// typed in switches.
    pub models: Vec<ModelOption>,
    /// The efforts the running model takes, least to most; empty when it
    /// takes none.
    pub efforts: Vec<EffortOption>,
}

/// The session's safety check switch: on or off, and this device's switch
/// while it waits for the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct SafetyCheckView {
    /// `safety check off`, `safety check off → on` while this device's
    /// switch waits.
    pub label: String,
    /// What a tap asks for: the opposite of the switch as it will be once
    /// a waiting switch lands (so a tap while `off → on` asks for off).
    pub turn_on: bool,
    /// The switch is on (or this device's waiting switch turns it on).
    pub on: bool,
    /// A switch of this device's is still waiting.
    pub pending: bool,
    /// `safety check on failed: <why>` until the next switch.
    pub refusal: Option<String>,
}

/// The notification switches as Settings → Notifications shows them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, SignalPiece)]
pub struct NotificationsView {
    /// Every notification; off stops all of them.
    pub all: bool,
    /// A session needs an approval.
    pub approvals: bool,
    /// A session's agent went from working to idle.
    pub idle: bool,
    /// A long shell run of this device's finished.
    pub shell: bool,
}

/// A safety rule as Settings lists it: a shell command prefix the safety
/// check trusts, with this device's change to it while on its way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct SafetyRuleRow {
    /// The session a removal is sent in.
    pub session_id: String,
    /// The program, then the literal leading words it must start with.
    pub words: Vec<String>,
    /// The words joined by single spaces.
    pub label: String,
    /// Whether the server's log holds the rule on.
    pub on: bool,
    /// `adding`, `removing` while this device's change waits; `could not
    /// add: <why>` or `could not remove: <why>` when the server refused it.
    pub pending: Option<String>,
    /// The failed change's command id, to dismiss it with
    /// `ActionIntent::Discard`; `None` otherwise.
    pub discard: Option<String>,
}

/// A mode the picker lists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct ModeOption {
    pub mode: PermissionModeView,
    /// `accept edits`.
    pub label: String,
    /// `file edits run without asking`.
    pub detail: String,
    /// It lets every tool run unasked: ask once more before switching.
    pub needs_confirmation: bool,
    /// The agent runs in it.
    pub current: bool,
    /// A switch to it waits for the agent.
    pub pending: bool,
}

/// A model the picker lists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct ModelOption {
    /// What `SetModel` sends back.
    pub id: String,
    pub label: String,
    pub detail: Option<String>,
    /// The agent runs it.
    pub current: bool,
    /// A switch to it waits for the agent.
    pub pending: bool,
}

/// An effort the picker lists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct EffortOption {
    pub effort: EffortView,
    /// `extra high`.
    pub label: String,
    /// The agent runs at it.
    pub current: bool,
    /// A switch to it waits for the agent.
    pub pending: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SignalPiece)]
pub enum EffortView {
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SignalPiece)]
pub enum PermissionModeView {
    Ask,
    AcceptEdits,
    Plan,
    Auto,
    DontAsk,
    Bypass,
}

/// The pending approval's panel.
#[derive(Debug, Clone, PartialEq, Serialize, SignalPiece)]
pub struct ApprovalPanel {
    pub approval_id: String,
    /// `from subagent: Explore`, with `· open it to see the call` where the
    /// view does not show the call; `None` for the main agent's.
    pub from: Option<String>,
    /// The timeline row of the call it gates (to reveal and expand it).
    pub gated_key: Option<String>,
    pub kind: ToolKindView,
    pub tool: String,
    pub title: String,
    pub detail: Option<String>,
    /// What the call will do, capped to a few lines; its note says what
    /// is left out.
    pub preview: Option<SectionView>,
    /// Where the whole input is when nothing else shows: `full input
    /// 4.0 KiB`.
    pub input_label: Option<String>,
    /// The approval asked about an input other than the call's: `approval
    /// input: differs from the call's · 4.0 KiB`. Revealing the call
    /// shows it, labelled so.
    pub asked_label: Option<String>,
    /// The server could not keep some of what the call will do, so what
    /// shows is only a summary or a start: said up front, since the user
    /// decides on it. `None` when everything was kept.
    pub lost: Option<String>,
    /// This device may answer (again).
    pub can_answer: bool,
    /// The lasting rules offered with an approval, by index.
    pub suggestions: Vec<RuleOption>,
    /// The simple commands of a shell approval, each a rule the person can
    /// trust (D61); empty when it is not a shell call.
    pub rule_candidates: Vec<Vec<String>>,
    /// `always allow · <rule> (<scope>)`, `always allow · choose one of 3
    /// rules`; `None` when none is offered or it was answered.
    pub always_label: Option<String>,
    /// Several rules: pick one before approving always.
    pub always_picks: bool,
    /// This device's answer: `you approved · queued 14:02:11`.
    pub answer_label: Option<String>,
    pub answer: Option<OutgoingView>,
}

/// The pending question's panel: the agent asks the user to pick from
/// options, or to write their own answer. The picks stay in the GUI until
/// it sends them all with `ActionIntent::AnswerQuestion`; a denial
/// (`ResolveApproval`) declines.
#[derive(Debug, Clone, PartialEq, Serialize, SignalPiece)]
pub struct QuestionPanel {
    pub approval_id: String,
    /// `from subagent: Explore`; `None` for the main agent's.
    pub from: Option<String>,
    /// The timeline row of the call that asks (to reveal it).
    pub call_key: Option<String>,
    /// At least one.
    pub questions: Vec<QuestionView>,
    /// This device may answer (again).
    pub can_answer: bool,
    /// This device's answer: `you answered: Rye · queued 14:02:11`, `you
    /// declined · sent`.
    pub answer_label: Option<String>,
    pub answer: Option<OutgoingView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct QuestionView {
    /// Short, one line; may be empty.
    pub header: String,
    pub text: String,
    pub options: Vec<QuestionOptionView>,
    /// Any number of options may be picked, and text besides; otherwise
    /// exactly one of an option or text.
    pub multi_select: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct QuestionOptionView {
    pub label: String,
    /// May be empty.
    pub description: String,
    /// Several lines shown as written (monospace), e.g. a mockup.
    pub preview: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct RuleOption {
    pub index: u32,
    /// `<label> (<scope>)`.
    pub label: String,
}

/// The composer. Dart's text field is the user's while typing (reported
/// with `DraftEdited`); it takes `text` and `cursor_utf16` only when
/// `rev` changes: the session opened, the core rewrote the draft, a prompt
/// was recalled, or a sent message cleared it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct ComposerView {
    pub text: String,
    pub cursor_utf16: u32,
    pub rev: u32,
    /// While browsing earlier prompts: `2 of 5`, 1 the newest.
    pub history_label: Option<String>,
    /// The shell history picker, while open: every session's commands the
    /// text matches, the newest first.
    pub history_search: Option<HistorySearchView>,
    /// `to the main agent` in a subagent's view, which takes no message
    /// of its own.
    pub to_label: Option<String>,
    /// What the composer writes: a message to the agent, or a shell
    /// command (`ToggleShellMode`). Its text is that mode's draft.
    pub mode: ComposerModeView,
    /// The server runs shell commands: the shell toggle shows. Switching
    /// back out of shell mode works without it.
    pub shell_enabled: bool,
    /// In shell mode, the prompt before the field.
    pub prompt: Option<ShellPromptView>,
    /// In shell mode, the stop button does something (`StopShell`).
    pub can_stop: bool,
    /// The images of the agent draft, in the order they were attached; none
    /// in shell mode.
    pub images: Vec<ComposerImage>,
    /// The non-image files of the agent draft, in the order attached; none
    /// in shell mode.
    pub files: Vec<ComposerFile>,
    /// Images and files picked or pasted and still being prepared: a message
    /// waits for them.
    pub preparing: u32,
}

/// The shell history picker, as shown above the composer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct HistorySearchView {
    /// Row 0 is the newest; at most a hundred.
    pub rows: Vec<HistoryRowView>,
    /// None when there are no rows.
    pub selected: Option<u32>,
    /// `matching/total`, e.g. `3/120`.
    pub count_label: String,
    /// Shown instead of rows: "No shell commands yet" when the history is
    /// empty, "No match" when nothing matches.
    pub empty_label: Option<String>,
}

/// One command the picker shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct HistoryRowView {
    pub command: String,
    /// The chars the query matched, in UTF-16 code units.
    pub matched: Vec<Utf16Range>,
}

/// An image of the composer's draft.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct ComposerImage {
    /// The image's sha256, 64 hex digits; it names the image to detach.
    pub sha256: String,
    /// The prepared image's file on this device, for a thumbnail.
    pub path: String,
    /// `812 B`, `1.2 MiB`.
    pub size_label: String,
}

/// A non-image file of the composer's draft.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct ComposerFile {
    /// The name it is written as in the workdir root; it names the file to
    /// detach.
    pub name: String,
    /// `812 B`, `1.2 MiB`.
    pub size_label: String,
    /// The file carries a replacement: the user agreed to write over a file
    /// of this name in the workdir.
    pub replaces: bool,
}

/// The shell prompt in its parts, shown in order: `work [if ok ✓] $`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct ShellPromptView {
    /// The last part of the session's cwd: `work`; `None` before any is
    /// known.
    pub cwd: Option<String>,
    /// The condition chip's label: `if ok ✓`, `if failed ✗`; `None` for
    /// always. `CycleCondition` steps it; it resets after each run sent.
    pub when_label: Option<String>,
    /// What closes the prompt: `$`.
    pub sigil: String,
}

/// What a session's composer writes on this device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, SignalPiece)]
pub enum ComposerModeView {
    Agent,
    Shell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, SignalPiece)]
pub enum AgentState {
    Idle,
    Working,
    AwaitingApproval,
    Errored,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, SignalPiece)]
pub enum BlobPolicyView {
    Eager,
    OnDemand,
}
