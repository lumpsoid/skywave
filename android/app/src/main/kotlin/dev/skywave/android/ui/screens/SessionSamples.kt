package dev.skywave.android.ui.screens

import android.content.res.Configuration
import androidx.compose.runtime.Composable
import androidx.compose.ui.tooling.preview.Preview
import dev.skywave.android.ui.ShellChrome
import dev.skywave.android.ui.SkywaveTheme
import dev.skywave.core.AgentSetting
import dev.skywave.core.AgentView
import dev.skywave.core.ApprovalPrompt
import dev.skywave.core.Attention
import dev.skywave.core.ApprovalStatus
import dev.skywave.core.BlobState
import dev.skywave.core.BlobView
import dev.skywave.core.BodyFormat
import dev.skywave.core.BodyView
import dev.skywave.core.Decision
import dev.skywave.core.Delivery
import dev.skywave.core.DeviceRef
import dev.skywave.core.Draft
import dev.skywave.core.Entry
import dev.skywave.core.FailReason
import dev.skywave.core.ModelChoice
import dev.skywave.core.Outgoing
import dev.skywave.core.PermissionMode
import dev.skywave.core.SessionState
import dev.skywave.core.SessionUsage
import dev.skywave.core.SessionView
import dev.skywave.core.TimelineItem
import dev.skywave.core.ToolApproval
import dev.skywave.core.ToolKind
import dev.skywave.core.ToolOutcome
import dev.skywave.core.ToolView
import dev.skywave.core.TurnUsage

/** Hand-made sessions for previews and UI tests; nothing here talks to a core. */
internal object SessionSamples {
    private const val ME = "0192f3a4-0000-7000-8000-000000000001"
    private const val OTHER = "0192f3a4-0000-7000-8000-000000000002"
    val meDevice = DeviceRef(ME, "Pixel 8", "Pixel 8", true)
    val otherDevice = DeviceRef(OTHER, "laptop", "laptop", false)
    private const val T0 = 1_761_800_000_000L

    val noChrome = ShellChrome(banners = {}, snackbarHost = {}, onUp = {})
    val noAttention = Attention(emptyList(), 0u, 0u)

    fun logged(seq: Int, entry: Entry) = TimelineItem.Logged(seq.toULong(), T0 + seq * 1000L, entry)

    fun message(seq: Int, text: String, own: Boolean = true, commandId: String = "c$seq") = logged(
        seq,
        Entry.UserMessage(
            commandId = commandId,
            text = text,
            from = if (own) meDevice else otherDevice,
            outgoing = if (own) Outgoing.Accepted(seq.toULong()) else null,
            delivery = if (own) Delivery(T0, T0 + 200, T0 + 900, 1u) else null,
        ),
    )

    fun pending(
        commandId: String,
        text: String,
        outgoing: Outgoing = Outgoing.Queued,
        discardable: Boolean = true,
        delivery: Delivery? = Delivery(T0, null, null, 0u),
    ) = TimelineItem.Pending(commandId, text, outgoing, discardable, delivery)

    fun tool(
        kind: ToolKind,
        title: String,
        outcome: ToolOutcome = ToolOutcome.Done(false, null, "ok", 1u, null, 1200u),
        body: BodyView? = null,
        detail: String? = null,
        approval: ToolApproval? = null,
        name: String = "Tool",
        input: BlobView? = null,
    ) = Entry.Tool(ToolView("call-$title", name, kind, title, detail, body, input, approval, outcome))

    fun blob(state: BlobState) = BlobView("blob1", state)

    val usage = TurnUsage(durationMs = 4100u, tokensIn = 12_300u, tokensOut = 840u, costUsd = 0.0123, model = "sonnet")

    val agent = AgentView(
        mode = PermissionMode.PLAN,
        model = "claude-sonnet-4-5",
        modelLabel = "Sonnet",
        modeChange = null,
        modelChange = null,
        modeRefused = null,
        modelRefused = null,
        models = listOf(ModelChoice("sonnet", "Sonnet", "Fast and capable", "claude-sonnet-4-5")),
    )

    /** Every kind of entry once, in a plausible order. */
    val everything: List<TimelineItem> = listOf(
        message(1, "Fix the failing sync test"),
        logged(2, Entry.AgentMessage("Looking at `sync.rs` first.\n\n```rust\nfn main() {}\n```\nThen the test.")),
        logged(3, tool(ToolKind.FILE_READ, "src/sync.rs", name = "Read")),
        logged(
            4,
            tool(
                ToolKind.FILE_EDIT, "src/sync.rs", name = "Edit",
                body = BodyView(BodyFormat.DIFF, "@@ -1 +1 @@\n-old line\n+new line", 3u, null),
                approval = ToolApproval("ap1", ApprovalStatus.Resolved(Decision.APPROVE, meDevice, null)),
            ),
        ),
        logged(
            5,
            tool(
                ToolKind.SHELL, "cargo test -p sync", name = "Bash",
                outcome = ToolOutcome.Done(true, "exit 101", "test sync ... FAILED\nfailures: 1", 40u, blob(BlobState.NotDownloaded(4096u)), 8300u),
            ),
        ),
        logged(6, tool(ToolKind.FILE_WRITE, "notes.md", name = "Write", body = BodyView(BodyFormat.TEXT, "# Notes", 1u, null))),
        logged(7, tool(ToolKind.SEARCH, "fn sync", name = "Grep", detail = "in src/")),
        logged(8, tool(ToolKind.FETCH, "https://example.com", name = "WebFetch", outcome = ToolOutcome.Denied(otherDevice))),
        logged(9, tool(ToolKind.SUBAGENT, "Explore the tests", name = "Task", outcome = ToolOutcome.Cancelled("turn ended"))),
        logged(10, tool(ToolKind.OTHER, "do a thing", name = "Frobnicate", outcome = ToolOutcome.NoResult)),
        logged(11, tool(ToolKind.SHELL, "sleep 60", name = "Bash", outcome = ToolOutcome.Running(T0))),
        logged(12, Entry.ToolCall("OldTool", "legacy call", null)),
        logged(13, Entry.ToolResult("legacy result", null)),
        logged(14, Entry.Approval("ap2", "Bash", "rm -rf build", null, ApprovalStatus.Cancelled("turn ended"))),
        logged(15, Entry.AgentErrored("rate limited")),
        logged(16, Entry.Interrupted),
        logged(17, Entry.Stopped),
        logged(18, Entry.Renamed("Sync fix", meDevice)),
        logged(19, Entry.Archived(true, otherDevice)),
        logged(20, Entry.TurnUsage(usage)),
        message(21, "And from my laptop", own = false),
        logged(22, Entry.AgentSetting(AgentSetting.Mode(PermissionMode.PLAN), meDevice, null)),
        logged(23, Entry.AgentSetting(AgentSetting.Model("claude-sonnet-4-5"), null, null)),
        logged(24, Entry.AgentSetting(AgentSetting.Mode(PermissionMode.BYPASS), otherDevice, "not allowed here")),
    )

    fun view(
        timeline: List<TimelineItem> = everything,
        state: SessionState = SessionState.Idle,
        lastSeq: ULong = timeline.filterIsInstance<TimelineItem.Logged>().maxOfOrNull { it.seq } ?: 0u,
        unreadFrom: ULong? = null,
        draft: Draft? = null,
        draftRev: ULong = 0u,
        prompts: List<String> = emptyList(),
        archived: Boolean = false,
        pendingApproval: ApprovalPrompt? = null,
        interrupt: Outgoing? = null,
    ) = SessionView(
        id = "s1",
        title = "Fix the sync bug",
        named = true,
        rename = null,
        archived = archived,
        workdir = "/home/me/src/skywave",
        state = state,
        timeline = timeline,
        pendingApproval = pendingApproval,
        rules = emptyList(),
        usage = SessionUsage(usage, usage),
        agent = agent,
        interrupt = interrupt,
        stop = null,
        draft = draft,
        draftRev = draftRev,
        lastSeq = lastSeq,
        unreadFrom = unreadFrom,
        prompts = prompts,
    )

    /** A long session: messages, agent replies, tools and usage in turn. */
    fun long(items: Int): List<TimelineItem> = List(items) { i ->
        val seq = i + 1
        when (i % 4) {
            0 -> message(seq, "Message $seq")
            1 -> logged(seq, Entry.AgentMessage("Reply $seq with `code`"))
            2 -> logged(seq, tool(ToolKind.SHELL, "echo $seq", name = "Bash"))
            else -> logged(seq, Entry.TurnUsage(usage))
        }
    }

    val offline = view(
        timeline = everything.take(3) + listOf(
            pending("p1", "Queued while offline"),
            pending("p2", "Refused", Outgoing.Failed(FailReason.UNKNOWN_SESSION)),
        ),
    )
}

@Composable
internal fun SessionPreview(view: SessionView, attention: Attention = SessionSamples.noAttention) =
    SkywaveTheme(dynamicColor = false) {
        SessionScreen(view, attention, SessionSamples.noChrome, dispatch = {})
    }

@Preview(name = "Session: every entry", heightDp = 1600)
@Composable
private fun EverythingPreview() = SessionPreview(SessionSamples.view(state = SessionState.AgentWorking))

@Preview(name = "Session: every entry, dark", heightDp = 1600, uiMode = Configuration.UI_MODE_NIGHT_YES)
@Composable
private fun EverythingDarkPreview() = SessionPreview(SessionSamples.view(unreadFrom = 15u))

@Preview(name = "Session: offline, queued")
@Composable
private fun OfflinePreview() = SessionPreview(SessionSamples.offline)

@Preview(name = "Session: empty")
@Composable
private fun EmptyPreview() = SessionPreview(SessionSamples.view(timeline = emptyList()))
