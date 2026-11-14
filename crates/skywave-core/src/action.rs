//! User intents a head hands to [`Core::dispatch`](crate::Core::dispatch).

use skywave_proto::ContentHash;

use crate::attachment::AttachedImage;
use crate::login::LoginCode;
use crate::store::BlobPolicy;
use crate::view::{
    ApprovalId, BlobId, BlobUse, CallId, CommandId, ComposerMode, ContextId, Decision, Effort,
    LineSpan, PermissionMode, QuestionAnswer, RunWhen, Seq, SessionId, SnippetId, TaskId,
    WorkdirPane,
};

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Pair with a server using what `skywaved pair` printed. Also re-pairs
    /// a paired device, e.g. after a rejection that
    /// [`needs_repair`](crate::LinkRejection::needs_repair).
    Pair {
        address: String,
        code: String,
        fingerprint: String,
    },
    /// Reach the paired server at another address, e.g. after its IP
    /// changed, and reconnect there. No re-pairing: the pinned CA and this
    /// device's certificate still decide whom the link trusts, so a wrong
    /// address can only fail to connect. The address it replaces is kept
    /// among the others ([`ServerAddresses::switch_to`](crate::ServerAddresses::switch_to)),
    /// to switch back to. Trimmed; a no-op when unpaired, unchanged or not
    /// a [`ServerAddress`](crate::ServerAddress).
    SetServerAddress {
        address: String,
    },
    /// Drop one of the server's other addresses; a no-op for one not kept,
    /// the current address included.
    ForgetServerAddress {
        address: String,
    },
    /// Start a session in a server directory under a configured root (D14).
    ///
    /// `conversation` is the agent's own id of a conversation it had
    /// outside Skywave, which the session continues; blank is `None`.
    /// Whether it is usable is the server's and the agent's call. Ends the
    /// conversation picker and the workdir completion.
    ///
    /// `worktree` is a new branch to create a git worktree on; the session
    /// runs in that worktree. `None` or blank: run in `workdir`.
    CreateSession {
        workdir: String,
        conversation: Option<String>,
        worktree: Option<String>,
    },
    /// Start a session in the session's workdir, from its page or from a
    /// session list, and open it once its `SessionCreated` reaches the
    /// replica, unless the user went elsewhere meanwhile.
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
    /// Open the conversation picker on the session list (paired only): ask
    /// the server for its agent's conversations, which the list's
    /// [`conversations`](crate::SessionListView::conversations) shows.
    /// Sending it again asks again; an older answer is then ignored.
    /// Offline it is `Unavailable` at once and nothing is asked later.
    ListConversations,
    /// The conversation picker closed: forget its listing.
    EndConversations,
    /// Ask the server for its storage report again, on the storage screen
    /// (paired only). The report shown stays until the answer; offline the
    /// screen is `Unavailable` at once and nothing is asked later.
    RefreshStorage,
    /// Ask the server for its worktree list again, on the worktrees screen
    /// (paired only). The list shown stays until the answer; offline the
    /// screen is `Unavailable` at once and nothing is asked later.
    RefreshWorktrees,
    /// Ask the server for its agent's usage limits again, on the usage
    /// limits page or a session's info page (paired only). The stored answer stays shown until the
    /// new one; offline the page is `Unavailable` at once and nothing is
    /// asked later.
    RefreshUsageLimits,
    /// Start logging the server's agent in, on the settings screen (paired
    /// only): the settings view's [`login`](crate::SettingsView::login)
    /// shows the URL to log in at once the server opens it. Sending it
    /// again starts over (the server joins the login it has open); an
    /// older answer is then ignored. Offline the login ends `Offline` at
    /// once and nothing is asked later.
    StartAgentLogin,
    /// Send the code the login page showed, on the settings screen, for
    /// the login open there, trimmed. Nothing is sent while none is open;
    /// a blank code or one over the server's cap is not sent either, and
    /// the login stays open saying why
    /// ([`LoginCodeRefused`](crate::LoginCodeRefused)). Offline nothing is
    /// sent: the code waits for [`Action::RetryAgentLogin`]. A code the
    /// server refuses ends the login; start again.
    FinishAgentLogin {
        code: LoginCode,
    },
    /// Send the code that got no answer again, with its flow, unchanged
    /// ([`LoginFlowView::NoAnswer`](crate::LoginFlowView::NoAnswer)); the
    /// server answers a repeated code with the first one's outcome.
    RetryAgentLogin,
    /// Forget this device's login, the code included, sending nothing; an
    /// answer still on its way is dropped.
    DismissAgentLogin,
    /// Queue a message, written for the session view's
    /// [`context`](crate::SessionView::context), with the session's draft
    /// images ([`SessionView::draft_images`](crate::SessionView::draft_images))
    /// as its attachments, in order. Sent when the text is not blank or the
    /// draft holds images, so a blank message with images goes as it is.
    /// Past either cap the draft's images allow (see [`Action::Attach`]) it
    /// queues nothing. Queuing drops the session's agent draft, text and
    /// images.
    SendMessage {
        session_id: SessionId,
        text: String,
    },
    /// Queue a shell command to run in the session's workdir on the server,
    /// in send order with its messages. `when` is the condition it runs on,
    /// as the user picked it; the core adds nothing. Blank text (after
    /// trimming), or an unknown session, is a no-op. A command over the
    /// server's size limit is queued anyway and shows as refused, so its
    /// text can be taken back. Queuing drops the session's shell draft.
    RunShell {
        session_id: SessionId,
        command: String,
        when: RunWhen,
    },
    /// Ask the server to stop the shell run `run` (the id of this or any
    /// device's `RunShell`). A no-op for an unknown session.
    StopShell {
        session_id: SessionId,
        run: CommandId,
    },
    /// Switch what the session's composer writes on this device. Kept per
    /// session across restarts; a no-op for an unknown session.
    SetComposerMode {
        session_id: SessionId,
        mode: ComposerMode,
    },
    /// Keep the session's unsent message (`mode` agent) or shell command
    /// (`mode` shell) on this device. `cursor` counts `char`s. Blank text
    /// drops the draft, except a shell draft whose `when` is not
    /// [`RunWhen::Always`]: the condition was picked before typing. `when`
    /// is ignored for an agent draft. Heads save when the user pauses or
    /// leaves, not per keystroke: every save is a durable write.
    SaveDraft {
        session_id: SessionId,
        mode: ComposerMode,
        text: String,
        cursor: usize,
        when: RunWhen,
    },
    /// Add an image to the session's agent draft, after the images it holds.
    /// The head prepares the picked or pasted file and stores it first
    /// (`attachment::prepare`, `attachment::store`); the action carries
    /// metadata only. A no-op for an unknown session, for an image the draft
    /// already holds (a draft holds each image once), and for one that would
    /// take the draft past `skywave_proto::limits::ATTACHMENTS_MAX` images or
    /// `skywave_proto::limits::ATTACHMENTS_MAX_TOTAL_BYTES` bytes together.
    /// There is no notice from the core: the head counts the images its view
    /// shows and words the refusal.
    Attach {
        session_id: SessionId,
        image: AttachedImage,
    },
    /// Take the image with `sha256` off the session's agent draft. A no-op
    /// when the draft does not hold it.
    Detach {
        session_id: SessionId,
        sha256: ContentHash,
    },
    /// Keep `text` on this device as a snippet of `kind`, to use again in
    /// any session, named `name` (blank for none; normalised as a session
    /// name). It lists first. Saving a text saved already as that kind
    /// moves that snippet first instead, renamed unless `name` is blank.
    /// Blank text is a no-op.
    SaveSnippet {
        name: String,
        text: String,
        kind: ComposerMode,
    },
    /// The snippet was put into a composer: it lists first.
    UseSnippet(SnippetId),
    /// Change a snippet's name (blank for none; normalised as a session
    /// name) and text; it keeps its place and kind. Another snippet of its
    /// kind with that text already goes. Blank text, or an unknown snippet,
    /// is a no-op.
    EditSnippet {
        id: SnippetId,
        name: String,
        text: String,
    },
    DeleteSnippet(SnippetId),
    /// Take back one of this device's messages, shell runs or session
    /// creations that the server never saw, or dismiss one it refused. Only
    /// items shown as `discardable` can be; anything else is a no-op.
    Discard(CommandId),
    /// Take back a `discardable` message into its session's draft, ahead of
    /// what the draft held, its images ahead of the draft's, to edit and send
    /// again; a shell run likewise
    /// into the shell draft, with its `when`. The session view's
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
    /// Answer a question ([`QuestionPrompt`](crate::QuestionPrompt)), one
    /// answer per question in order. A no-op unless the open session asks
    /// it and the answers fit ([`skywave_proto::answers_fit`]); decline it
    /// with a denial ([`Action::ResolveApproval`]).
    AnswerQuestion {
        id: ApprovalId,
        answers: Vec<QuestionAnswer>,
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
    /// Show the timeline of the subagent an `Agent` call of the open
    /// session runs (`None`: back to the main agent's). This device only,
    /// kept until another screen shows; messages still go to the main
    /// agent. Ignored for a call that is not a subagent's in that session.
    /// [`Action::Back`] steps out to the subagent it runs under, then to
    /// the main timeline.
    ViewSubagent {
        session_id: SessionId,
        call: Option<CallId>,
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
    /// Remove the git worktree the session runs in and delete its branch on the
    /// server. Refused there while a turn runs, when the worktree has changes or
    /// when its branch is not merged. Nothing changes here until
    /// `WorktreeRemoved` is in the replica.
    RemoveWorktree {
        session_id: SessionId,
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
    /// Show the session's workdir, on its changes
    /// ([`Screen::Workdir`](crate::Screen::Workdir)). A no-op for an
    /// unknown session.
    OpenWorkdir {
        session_id: SessionId,
    },
    /// Show the session's particulars
    /// ([`Screen::SessionInfo`](crate::Screen::SessionInfo)): its usage,
    /// the agent's settings, every subagent it ran and the agent account's
    /// usage limits, asked for on opening only when none are stored. Back
    /// returns to the session. A no-op for an unknown session.
    OpenSessionInfo {
        session_id: SessionId,
    },
    /// Show another pane of the workdir on screen. A pane whose read has no
    /// stored answer asks the server once (offline it fails at once); a
    /// stored one shows until [`Action::RefreshWorkdir`]. A diff or file
    /// pane whose content is not downloaded asks for it: opening it is the
    /// request, whatever the blob policy.
    ShowWorkdir(WorkdirPane),
    /// Read again what the workdir pane on screen shows (a diff: the
    /// changes). Offline it fails at once, and is not queued.
    RefreshWorkdir,
    /// Download a file of the session's workdir, whatever pane is on screen:
    /// the server is asked for the file as it is now, unless that download is
    /// already in flight (then nothing is asked). Once the answer carries the
    /// content as a blob, the blob is downloaded whatever is on screen. A no-op
    /// for an unknown session. Its state is [`WorkdirView::downloads`](crate::WorkdirView::downloads).
    DownloadWorkdirFile {
        session_id: SessionId,
        path: String,
    },
    /// Fetch a blob's content (e.g. to expand a tool output).
    DownloadBlob(BlobId),
    /// Delete a blob from the server and from every device. Queued in the
    /// outbox like any command, offline too, and discardable until the
    /// server takes it. `BlobId` is the blob a view shows
    /// ([`BlobView::id`](crate::BlobView::id)): after a crop that is the
    /// crop's, not the event's original.
    DeleteBlob(BlobId),
    /// Crop a blob to the lines of its content `keep` names, on the server
    /// and on every device. Only a tool's or a shell run's output. Queued and discardable as
    /// [`Action::DeleteBlob`], and for the same `blob`. Sent while the
    /// crop page of `blob` shows, the page is left once it is queued, as
    /// [`Action::Back`] leaves it.
    CropBlob {
        blob: BlobId,
        keep: Vec<LineSpan>,
    },
    /// Show the page choosing which lines of `blob` to keep
    /// ([`Screen::Crop`](crate::Screen::Crop)), from the item or storage row
    /// naming it: `blob` as [`Action::CropBlob`] names it, the `session`
    /// and what it `holds`, and the `title` the page shows. Downloads
    /// nothing. Back returns to the session on screen, or to the storage
    /// screen if it was opened there.
    OpenCrop {
        blob: BlobId,
        session: SessionId,
        holds: BlobUse,
        title: String,
    },
    /// The crop page's selection, stored as given. A no-op elsewhere.
    SetCropKeep(Vec<LineSpan>),
    SetBlobPolicy(BlobPolicy),
    /// While the link is `Reconnecting`, skip the rest of the wait and
    /// reconnect now. The attempt count and `offline_since` carry on.
    /// After a rejection a server-side fix may clear
    /// ([`LinkRejection::can_retry`](crate::LinkRejection::can_retry):
    /// `ProtocolMismatch`, `ServerChanged`, `NameMismatch`), connect again.
    RetryNow,
    /// After `Rejected(HistoryLost)`, start the replica over from the
    /// server's log: the replicated log, cursor, sessions, outbox (queued
    /// commands too) and blobs go; settings, drafts, composer modes,
    /// snippets and read marks stay. Then connect again. Ignored in any
    /// other link state.
    ResetReplica,
    Navigate(Route),
    /// Go back from the screen shown: a subagent's timeline to the one of
    /// the agent that launched it, a workdir file to its directory, a diff
    /// to the changes, a directory to its parent, the changes or the
    /// workdir itself to the session, a session to the list it is on, the
    /// archived list, settings and re-pairing to the active list, the crop
    /// page to where it was opened. A no-op
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
    /// The paired server's addresses, to switch between, add and forget;
    /// back returns to the settings.
    ServerAddresses,
    Pairing,
    /// Cost and tokens totalled across sessions, from the replica alone.
    Usage,
    /// What the server's data takes on its disk: opening it asks the
    /// server for its report.
    Storage,
    /// Every git worktree of the repositories sessions' worktrees came
    /// from, with the sessions in each: opening it asks the server for the
    /// list.
    Worktrees,
    /// The usage limits of the account the server's agent runs as: the
    /// stored answer, asked for on opening only when none is stored. Back
    /// returns to the session it was opened from, if any.
    UsageLimits,
}
