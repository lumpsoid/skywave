use rinf::{DartSignal, SignalPiece};
use serde::Deserialize;

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
    /// Send the composer's text. It clears once the core took it; if the
    /// core refuses, it stays, and nothing is resent.
    ComposerSubmit {
        session_id: String,
    },
    /// Recall the next older or newer earlier prompt into the composer.
    HistoryOlder,
    HistoryNewer,
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
    /// Open a downloaded blob's whole file (a section's `blob`), past the
    /// display cap.
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
    /// Browse local data behind the revoked notice.
    RevokedDismiss,
    /// The app went off screen (hidden or paused). Reported, not decided:
    /// the hub knows what that means on this platform.
    AppBackgrounded,
    /// The app is on screen again (shown or resumed).
    AppForegrounded,
}

/// A core action the GUI asks for directly.
#[derive(Debug, Clone, PartialEq, Deserialize, SignalPiece)]
pub enum ActionIntent {
    /// Take back a discardable message or session creation, or dismiss a
    /// refused one.
    Discard {
        command_id: String,
    },
    /// Take a discardable message back into the draft.
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
    SetSessionArchived {
        session_id: String,
        archived: bool,
    },
    DeleteSessionLocally {
        session_id: String,
    },
    OpenSession {
        session_id: String,
    },
    OpenNextApproval,
    DownloadBlob {
        blob_id: String,
    },
    SetBlobPolicy {
        policy: BlobPolicyChoice,
    },
    RetryNow,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, SignalPiece)]
pub enum CopyWhat {
    /// A message's text; a call's output, file or diff.
    Main,
    /// A shell call's command without the `$ ` prompt; another call's
    /// title and detail.
    Call,
}
