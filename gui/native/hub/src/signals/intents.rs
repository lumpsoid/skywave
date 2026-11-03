use rinf::{DartSignal, SignalPiece};
use serde::Deserialize;

use super::screen::{EffortView, PermissionModeView, UsageGroupingView, UsageSpanView};
use super::workdir::WorkdirPaneKind;

/// Everything the GUI tells the hub after boot, in one type: rinf orders
/// signals only within a type, and the order matters (an edit before the
/// `Back` that saves it, a typed query before the step through it).
///
/// Ids and keys are strings as the hub sent them; one that does not parse
/// is answered with `PlatformEffect::Error`, never acted on.
#[derive(Debug, Clone, PartialEq, Deserialize, DartSignal)]
pub enum GuiIntent {
    /// The holders are subscribed: resend the banner, the screen, a
    /// timeline snapshot and the search.
    AppReady,
    /// A plain core action.
    Act {
        action: ActionIntent,
    },
    /// The composer's text or cursor changed (every keystroke). Saved as
    /// the draft once typing pauses, or before leaving.
    DraftEdited {
        session_id: String,
        text: String,
        cursor_utf16: u32,
    },
    /// Send the composer's text: a message in agent mode, a shell command
    /// on the condition shown in shell mode (blank runs nothing). It
    /// clears once the core took it, and the condition goes back to
    /// always; if the core refuses, it stays, and nothing is resent.
    ComposerSubmit {
        session_id: String,
    },
    /// Recall the next older or newer earlier prompt into the composer;
    /// in shell mode, the earlier shell commands.
    HistoryOlder,
    HistoryNewer,
    /// Switch the composer between agent and shell mode. The text written
    /// so far stays as its mode's draft, and the other mode's draft shows.
    /// Into shell mode only while the server runs shell commands.
    ToggleShellMode {
        session_id: String,
    },
    /// Step the shell command's condition: always → if ok → if failed →
    /// always. Kept with the shell draft.
    CycleCondition {
        session_id: String,
    },
    /// The shell composer's stop: stops the running run, else takes back
    /// or stops the newest waiting one; nothing when nothing runs or
    /// waits.
    StopShell {
        session_id: String,
    },
    /// One run's own stop (`run_id`, a row's key): this device's run the
    /// server never saw is taken back, any other stopped.
    StopShellRun {
        session_id: String,
        run_id: String,
    },
    /// Show a run's whole output in its row (`whole`), downloading its
    /// output blob if needed; `CollapseShellRun` stops sending it.
    ExpandShellRun {
        session_id: String,
        run_id: String,
    },
    CollapseShellRun {
        session_id: String,
        run_id: String,
    },
    /// Copy a run's whole output, ANSI stripped: its downloaded blob, else
    /// the run's own text (all of it when it has no blob, a running run's
    /// tail so far); a blob not here is downloaded first and the notice
    /// says to copy again.
    CopyShellOutput {
        session_id: String,
        run_id: String,
    },
    /// Run a shell snippet's text as a command at once, on no condition;
    /// the composer keeps what it holds.
    RunSnippet {
        session_id: String,
        snippet_id: String,
    },
    /// Keep a message's text (a `UserMessage` or `PendingMessage` row's)
    /// as a snippet named `name`, blank for none.
    SaveSnippet {
        key: String,
        name: String,
    },
    /// Keep the composer's text as a snippet, as `SaveSnippet`; in shell
    /// mode as a saved shell command.
    SaveComposerSnippet {
        session_id: String,
        name: String,
    },
    /// Put a snippet's text into the composer at its cursor, to edit or
    /// send; the snippet lists first.
    InsertSnippet {
        session_id: String,
        snippet_id: String,
    },
    /// Whether the newest timeline row is on screen; what reaches the
    /// session while it is counts as read.
    FollowingChanged {
        following: bool,
    },
    /// Show a row's content whole (downloading its blob if needed), or
    /// collapse it again.
    ExpandItem {
        key: String,
    },
    CollapseItem {
        key: String,
    },
    /// Download the blob behind a row's content, as `ExpandItem` would.
    DownloadItem {
        key: String,
    },
    /// Open a downloaded blob's whole file (a section's `blob`, or the
    /// workdir diff or file shown), past the display cap.
    OpenFull {
        blob_id: String,
    },
    /// Copy a row's content: `Main` its text, output or file; `Call` the
    /// command or call itself.
    Copy {
        key: String,
        what: CopyWhat,
    },
    /// The search's query, opening a search if none is open.
    SearchTyped {
        query: String,
    },
    /// Jump to the next older (`Some(true)`) or newer match, round the
    /// ends; `None` jumps to the nearest at or above `from_key` (the
    /// newest visible row; the end when `None`).
    SearchStep {
        older: Option<bool>,
        from_key: Option<String>,
    },
    SearchClose,
    /// Open the new-session prompt.
    CreateOpen,
    /// The workdir input changed.
    CompletionTyped {
        input: String,
    },
    /// The outside conversation id field changed.
    CreateConversationTyped {
        conversation: String,
    },
    /// Tab: take the highlighted candidate, or ask for candidates.
    CompletionComplete,
    /// Move the highlight.
    CompletionMove {
        up: bool,
    },
    /// Enter: take the highlighted candidate, or create the session.
    CompletionAccept,
    /// Esc: close the candidate list, or else the prompt.
    CompletionCancel,
    /// A tap on a listed candidate: take the one at `index` (of the
    /// popup's `candidates`). One no longer listed is ignored.
    CompletionPick {
        index: u32,
    },
    /// Open the rename prompt for a session.
    RenameOpen {
        session_id: String,
    },
    /// Rename to `name` (blank restores the default title); the prompt
    /// closes once the core took it.
    RenameSubmit {
        name: String,
    },
    RenameCancel,
    /// The pairing form was submitted. A field holding a pairing URI fills
    /// the form instead.
    PairSubmit {
        address: String,
        code: String,
        fingerprint: String,
    },
    /// Text was pasted into the pairing form: a pairing URI fills it, a
    /// malformed one is reported; anything else is left to the field.
    PairPasted {
        text: String,
    },
    /// A QR code was scanned on the pairing screen: a pairing URI fills the
    /// form; anything else is reported, as no field asked for it.
    PairScanned {
        text: String,
    },
    /// Browse local data behind the revoked notice.
    RevokedDismiss,
    /// What the usage screen totals: the days and the grouping. Kept
    /// while away from it.
    UsageShow {
        span: UsageSpanView,
        grouping: UsageGroupingView,
    },
    /// The app went off screen (hidden or paused). Reported, not decided:
    /// the hub knows what that means on this platform.
    AppBackgrounded,
    /// The app is on screen again (shown or resumed).
    AppForegrounded,
}

/// A core action the GUI asks for directly.
#[derive(Debug, Clone, PartialEq, Deserialize, SignalPiece)]
pub enum ActionIntent {
    /// Take back a discardable message, shell run or session creation, or
    /// dismiss a refused one.
    Discard {
        command_id: String,
    },
    /// Take a discardable message back into the draft; a shell run into
    /// the shell draft, with its condition.
    EditQueued {
        command_id: String,
    },
    /// Answer an approval; `rule` grants that suggestion (by index) with an
    /// approval.
    ResolveApproval {
        approval_id: String,
        decision: DecisionView,
        rule: Option<u32>,
    },
    Interrupt {
        session_id: String,
    },
    StopSession {
        session_id: String,
    },
    /// End one of the session's background tasks, the agent otherwise
    /// running on.
    StopTask {
        session_id: String,
        task_id: String,
    },
    /// Start a session in the open session's workdir and open it once
    /// the server logged it.
    CreateSessionLike {
        session_id: String,
    },
    /// Clear the session's agent context: a fresh conversation in the same
    /// workdir, in a new context.
    ClearContext {
        session_id: String,
    },
    /// Make `context_id` (one the session lists) the context messages go
    /// to; the agent resumes its conversation.
    SwitchContext {
        session_id: String,
        context_id: String,
    },
    /// Continue `context_id` (one the session lists as movable) as a new
    /// session of its own; here it stays as it was.
    MoveContext {
        session_id: String,
        context_id: String,
    },
    /// Show only `context_id`'s part of the timeline; `None` shows the
    /// whole session. Messages still go to the current context.
    ViewContext {
        session_id: String,
        context_id: Option<String>,
    },
    /// Show the steps of the subagent the call `call` (a row's `opens`, a
    /// task's `call`) starts; `None` shows the main view. Messages still
    /// go to the main agent.
    ViewSubagent {
        session_id: String,
        call: Option<String>,
    },
    /// Switch the session's agent to `mode`, starting it if need be.
    SetPermissionMode {
        session_id: String,
        mode: PermissionModeView,
    },
    /// Switch the session's agent to `model`: an offered model's `id`, or
    /// a name typed in. Blank does nothing.
    SetModel {
        session_id: String,
        model: String,
    },
    /// Switch the session's agent to reasoning effort `effort`, starting
    /// it if need be.
    SetEffort {
        session_id: String,
        effort: EffortView,
    },
    SetSessionArchived {
        session_id: String,
        archived: bool,
    },
    DeleteSessionLocally {
        session_id: String,
    },
    /// Give a snippet `name` (blank for none) and `text`.
    EditSnippet {
        snippet_id: String,
        name: String,
        text: String,
    },
    DeleteSnippet {
        snippet_id: String,
    },
    OpenSession {
        session_id: String,
    },
    /// The session's workdir screen, at its changes; it reads them.
    OpenWorkdir {
        session_id: String,
    },
    /// A pane of the workdir screen: `path` names the diff's file, the
    /// directory or the file (ignored for `Changes`); `untracked` tells a
    /// diff's two entries for one path apart.
    ShowWorkdir {
        pane: WorkdirPaneKind,
        path: String,
        untracked: bool,
    },
    /// Read the workdir pane shown again.
    RefreshWorkdir,
    OpenNextApproval,
    DownloadBlob {
        blob_id: String,
    },
    SetBlobPolicy {
        policy: BlobPolicyChoice,
    },
    RetryNow,
    /// Reach the paired server at `address` (`host:port`) from now on,
    /// without re-pairing. One that does not parse is answered with
    /// `PlatformEffect::Error`.
    SetServerAddress {
        address: String,
    },
    Navigate {
        route: RouteView,
    },
    Back,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, SignalPiece)]
pub enum DecisionView {
    Approve,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, SignalPiece)]
pub enum BlobPolicyChoice {
    Eager,
    OnDemand,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, SignalPiece)]
pub enum RouteView {
    SessionList,
    ArchivedSessions,
    Settings,
    Pairing,
    Usage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, SignalPiece)]
pub enum CopyWhat {
    /// A message's text; a call's output, file or diff.
    Main,
    /// A shell call's command without the `$ ` prompt; another call's
    /// title and detail.
    Call,
}
