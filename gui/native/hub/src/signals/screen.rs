use rinf::{RustSignal, SignalPiece};
use serde::{Deserialize, Serialize};

use super::timeline::{OutgoingView, SectionView, ToolKindView};

/// The screen to show, with everything on it but the timeline (its own
/// signal) and the banner. Sent whole whenever it changes; Dart replaces
/// it, and the navigator follows `screen` (D18).
#[derive(Debug, Clone, PartialEq, Serialize, RustSignal)]
pub struct ScreenUpdate {
    pub screen: ScreenView,
    /// `Back` goes somewhere; when false, system back leaves the app.
    pub can_go_back: bool,
    /// The device was revoked: a full-screen notice covers `screen` until
    /// `RevokedDismiss` (browse local data) or `Navigate(Pairing)`.
    pub revoked_full_screen: bool,
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
        /// `Sessions`: the Active/Archived toggle carries the counts.
        title: String,
        /// What an empty list says; `None` when it has rows.
        empty_label: Option<String>,
        rows: Vec<SessionRowView>,
        /// How many sessions the other list holds.
        other_count: u32,
        /// Sessions this device asked for that are not in the replica yet,
        /// or that the server refused; oldest first.
        creating: Vec<CreatingView>,
        /// The refused creation `Discard` takes off the list, if any.
        dismissable: Option<String>,
        /// The new-session prompt, while open (`CreateOpen`); it closes
        /// once the core took the session.
        create: Option<CreatePrompt>,
        /// The rename prompt, while open (`RenameOpen`).
        rename: Option<RenamePrompt>,
    },
    Session {
        session: SessionScreen,
    },
    Settings {
        blob_policy: BlobPolicyView,
        /// `on demand (download when opened)`.
        blob_policy_label: String,
        /// The server address, or `not paired`.
        server: String,
        /// This device's id, or `not paired`.
        device: String,
        /// The pinned server CA fingerprint, to compare with what
        /// `skywaved` prints.
        fingerprint: Option<String>,
    },
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
}

/// One session on a list.
#[derive(Debug, Clone, PartialEq, Serialize, SignalPiece)]
pub struct SessionRowView {
    pub session_id: String,
    pub title: String,
    pub workdir: Option<String>,
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
    /// `DeleteSessionLocally` would remove it (archived, nothing waiting).
    pub deletable: bool,
    /// Why it cannot be deleted yet, for an archived row that is not
    /// deletable.
    pub delete_blocked_label: Option<String>,
    pub archived: bool,
}

/// A session this device asked for, not in the replica yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct CreatingView {
    pub command_id: String,
    pub workdir: String,
    pub outgoing: OutgoingView,
    /// `waits for the server`, `created, syncing…`, why it was refused.
    pub note: Option<String>,
    pub discardable: bool,
}

/// The new-session prompt: a server workdir with completion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct CreatePrompt {
    /// The input as the hub last set it; Dart overwrites its field only
    /// when `input_rev` changes (a completion filled it in).
    pub input: String,
    pub input_rev: u32,
    /// The candidate list, while open.
    pub popup: Option<CompletionPopup>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct CompletionPopup {
    pub candidates: Vec<CandidateView>,
    /// The highlighted candidate.
    pub selected: u32,
    /// `listing…`, `offline · recent only`, `more not shown · keep typing`.
    pub status_label: Option<String>,
    /// `no matching directories`, when there are no candidates.
    pub empty_label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, SignalPiece)]
pub struct CandidateView {
    pub path: String,
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
    /// `last turn 12.4s`, `3.1k in / 812 out`, `$0.04`, `session $1.27`,
    /// `<model>` until the agent names its model in `agent`: drop from
    /// the front when short of room.
    pub usage: Vec<String>,
    /// The approval the agent is blocked on.
    pub approval: Option<ApprovalPanel>,
    pub composer: ComposerView,
    /// This device's snippets for the composer's picker, the most recently
    /// used or saved first.
    pub snippets: Vec<SnippetView>,
    /// The rename prompt, while open (`RenameOpen`).
    pub rename_prompt: Option<RenamePrompt>,
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
    /// Every mode, from the most asking to the least.
    pub modes: Vec<ModeOption>,
    /// The models the agents offer; empty until one has, when only a name
    /// typed in switches.
    pub models: Vec<ModelOption>,
    /// The efforts the running model takes, least to most; empty when it
    /// takes none.
    pub efforts: Vec<EffortOption>,
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
    /// This device may answer (again).
    pub can_answer: bool,
    /// The lasting rules offered with an approval, by index.
    pub suggestions: Vec<RuleOption>,
    /// `always allow · <rule> (<scope>)`, `always allow · choose one of 3
    /// rules`; `None` when none is offered or it was answered.
    pub always_label: Option<String>,
    /// Several rules: pick one before approving always.
    pub always_picks: bool,
    /// This device's answer: `you approved · queued 14:02:11`.
    pub answer_label: Option<String>,
    pub answer: Option<OutgoingView>,
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
    /// `offline, sends when back online`, while reconnecting.
    pub offline_label: Option<String>,
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
