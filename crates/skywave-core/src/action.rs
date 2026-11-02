//! User intents a head hands to [`Core::dispatch`](crate::Core::dispatch).

use crate::store::BlobPolicy;
use crate::view::{
    ApprovalId, BlobId, CommandId, ContextId, Decision, Effort, PermissionMode, Seq, SessionId,
    SnippetId, TaskId,
};

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Pair with a server using what `skywaved pair` printed. Also re-pairs
    /// a paired device, e.g. after `Rejected(Revoked)`.
    Pair {
        address: String,
        code: String,
        fingerprint: String,
    },
    /// Start a session in a server directory under a configured root (D14).
    ///
    /// `conversation` is the agent's own id of a conversation it had
    /// outside Skywave, which the session continues; blank is `None`.
    /// Whether it is usable is the server's and the agent's call.
    CreateSession {
        workdir: String,
        conversation: Option<String>,
    },
    /// Start a session in the open session's workdir, and open it once its
    /// `SessionCreated` reaches the replica, unless the user went elsewhere
    /// meanwhile.
    CreateSessionLike(SessionId),
    /// Complete `input` as a workdir: the session list's
    /// [`completion`](crate::SessionListView::completion) shows the
    /// candidates. Heads send it on Tab and on every edit while the
    /// candidates are shown. The server is asked again when the directory
    /// part changes, when the same input is completed twice, or after a
    /// failed listing; otherwise the last listing is filtered.
    CompleteWorkdir {
        input: String,
    },
    /// The new-session prompt closed: forget the completion and its
    /// listing.
    EndCompletion,
    /// Queue a message, written for the session view's
    /// [`context`](crate::SessionView::context).
    SendMessage {
        session_id: SessionId,
        text: String,
    },
    /// Keep the session's unsent message on this device; blank text drops
    /// it. `cursor` counts `char`s. Heads save when the user pauses or
    /// leaves, not per keystroke: every save is a durable write.
    SaveDraft {
        session_id: SessionId,
        text: String,
        cursor: usize,
    },
    /// Keep `text` on this device as a snippet, to use again in any
    /// session, named `name` (blank for none; normalised as a session
    /// name). It lists first. Saving a text saved already moves that
    /// snippet first instead, renamed unless `name` is blank. Blank text
    /// is a no-op.
    SaveSnippet {
        name: String,
        text: String,
    },
    /// The snippet was put into a composer: it lists first.
    UseSnippet(SnippetId),
    /// Change a snippet's name (blank for none; normalised as a session
    /// name) and text; it keeps its place. Another snippet with that text
    /// already goes. Blank text, or an unknown snippet, is a no-op.
    EditSnippet {
        id: SnippetId,
        name: String,
        text: String,
    },
    DeleteSnippet(SnippetId),
    /// Take back one of this device's messages or session creations that
    /// the server never saw, or dismiss one it refused. Only items shown as
    /// `discardable` can be; anything else is a no-op.
    Discard(CommandId),
    /// Take back a `discardable` message into its session's draft, ahead of
    /// what the draft held, to edit and send again. The session view's
    /// `draft_rev` changes so heads refill their composer.
    EditQueued(CommandId),
    /// Answer an approval. `rule` grants the prompt's suggestion with that
    /// index along with an approval; with a denial, or an index the prompt
    /// does not offer, the action is a no-op.
    ResolveApproval {
        id: ApprovalId,
        decision: Decision,
        rule: Option<u8>,
    },
    /// Stop the agent's current turn.
    Interrupt {
        session_id: SessionId,
    },
    /// Shut down the session's agent; the next message starts it again.
    StopSession {
        session_id: SessionId,
    },
    /// Start the session afresh in a new, empty context, which messages go
    /// to from now on; the earlier ones stay to switch back to. The core
    /// picks its id and a name no context of the session has, as far as
    /// this device knows. It is current at once, marked by the view's
    /// `clear` until its effect is in the replica.
    ClearContext {
        session_id: SessionId,
    },
    /// Make one of the session's contexts current again: messages go to it
    /// and its agent resumes it. Shown at once, marked by the view's
    /// `switch` until its effect is in the replica.
    SwitchContext {
        session_id: SessionId,
        context: ContextId,
    },
    /// Move one of the open session's contexts into a new session of its
    /// own, where its agent goes on from its conversation; here it is never
    /// current again. A no-op unless the session view lists it as
    /// [`can_move`](crate::ContextView::can_move). The context messages go
    /// to now is cleared first (as [`Action::ClearContext`]), so the
    /// session goes on in a fresh one. The new session's first context
    /// keeps the moved one's name, and it opens once it is in the replica
    /// if this session is still on screen.
    MoveContext {
        session_id: SessionId,
        context: ContextId,
    },
    /// Narrow the open session's timeline to one context (`None`: the whole
    /// session). This device only, kept until the session closes; where
    /// messages go does not change.
    ViewContext {
        session_id: SessionId,
        context: Option<ContextId>,
    },
    /// Name the session, or with a blank `name` restore its default title.
    /// The core normalises the name as the server will (control characters
    /// stripped, trimmed, cut to 80 chars). It shows at once, marked by the
    /// view's `rename` until its effect is in the replica.
    RenameSession {
        session_id: SessionId,
        name: String,
    },
    /// Move the session to the archived list (`true`) or back (`false`).
    /// Absolute, never a toggle, so a resend cannot flip it twice. It moves
    /// at once; a message sent to it later brings it back on every device.
    /// Archiving does not stop the agent.
    SetSessionArchived {
        session_id: SessionId,
        archived: bool,
    },
    /// Ask the session's agent to run in `mode`. The agent is started for
    /// it if it is not running; the session view's
    /// [`agent`](crate::SessionView::agent) shows the switch until the
    /// agent answers, then the mode it runs in or its refusal.
    SetPermissionMode {
        session_id: SessionId,
        mode: PermissionMode,
    },
    /// Ask the session's agent to stop its background `task` (one the
    /// view's [`work`](crate::SessionView::work) lists). The session view's
    /// [`task_stops`](crate::SessionView::task_stops) shows it until the
    /// task is gone or the agent refuses. Schedules cannot be stopped this
    /// way.
    StopTask {
        session_id: SessionId,
        task: TaskId,
    },
    /// Ask the session's agent to run `model`: the
    /// [`id`](crate::ModelChoice::id) of one the view's
    /// [`models`](crate::AgentView::models) offers, or a name. Normalised as
    /// the server will (control characters stripped, trimmed, cut to 80
    /// chars); a blank one is a no-op. Shown as for a mode.
    SetModel {
        session_id: SessionId,
        model: String,
    },
    /// Ask the session's agent to run at reasoning effort `effort`, one of
    /// the view's [`efforts`](crate::AgentView::efforts). A no-op for an
    /// unknown session. Shown as for a mode.
    SetEffort {
        session_id: SessionId,
        effort: Effort,
    },
    /// Delete an archived session from this device only, to free space and
    /// drop its content here; it stays on the server and other devices.
    /// Its events, summary, blobs (files too), draft and read mark go, and
    /// it never comes back: later events for it are skipped, approvals in
    /// it do not alert here. A no-op unless the session's row is
    /// [`deletable`](crate::SessionRow::deletable). If it is open, its list
    /// shows instead.
    DeleteSessionLocally {
        session_id: SessionId,
    },
    OpenSession(SessionId),
    /// Open the session of the next approval in
    /// [`Attention::approvals`](crate::Attention::approvals) outside the
    /// open session: the oldest, or, when the open session has one, the
    /// first after that, wrapping round, so repeating it cycles. A no-op
    /// when there is none.
    OpenNextApproval,
    /// The user has seen the open session through `through`: its newest
    /// item was on screen. Only ever moves forward, never past the session's
    /// newest event, and only for the open session. Heads mark sparingly
    /// (every mark is a durable write) and never move the session view's
    /// `unread_from` with it.
    MarkRead {
        session_id: SessionId,
        through: Seq,
    },
    /// Fetch a blob's content (e.g. to expand a tool output).
    DownloadBlob(BlobId),
    SetBlobPolicy(BlobPolicy),
    /// While the link is `Reconnecting`, skip the rest of the wait and
    /// reconnect now. The attempt count and `offline_since` carry on.
    /// After `Rejected(ProtocolMismatch)`, connect again: the server may
    /// have been updated since.
    RetryNow,
    Navigate(Route),
    /// Go back from the screen shown: a session to the list it is on, the
    /// archived list, settings and re-pairing to the active list. A no-op
    /// where [`ViewState::can_go_back`](crate::ViewState::can_go_back) is
    /// false (the active list, pairing an unpaired device): there back
    /// leaves the app, which is the head's to do.
    Back,
}

/// Screens reachable without a parameter; a session opens with
/// [`Action::OpenSession`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// The active sessions.
    SessionList,
    /// The archived sessions, shown as a
    /// [`ListKind::Archived`](crate::ListKind::Archived) session list.
    ArchivedSessions,
    Settings,
    Pairing,
}
