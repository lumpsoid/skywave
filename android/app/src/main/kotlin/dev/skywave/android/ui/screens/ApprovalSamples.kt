package dev.skywave.android.ui.screens

import android.content.res.Configuration
import androidx.compose.runtime.Composable
import androidx.compose.ui.tooling.preview.Preview
import dev.skywave.android.ui.screens.SessionSamples.logged
import dev.skywave.android.ui.screens.SessionSamples.message
import dev.skywave.android.ui.screens.SessionSamples.tool
import dev.skywave.core.Answer
import dev.skywave.core.ApprovalPrompt
import dev.skywave.core.ApprovalStatus
import dev.skywave.core.Attention
import dev.skywave.core.BlobState
import dev.skywave.core.BodyFormat
import dev.skywave.core.BodyView
import dev.skywave.core.Decision
import dev.skywave.core.Delivery
import dev.skywave.core.Entry
import dev.skywave.core.Outgoing
import dev.skywave.core.PendingApprovalRef
import dev.skywave.core.RuleScope
import dev.skywave.core.RuleSuggestion
import dev.skywave.core.SessionState
import dev.skywave.core.SessionView
import dev.skywave.core.TimelineItem
import dev.skywave.core.ToolApproval
import dev.skywave.core.ToolKind
import dev.skywave.core.ToolOutcome

/** Sessions blocked on an approval, for previews and UI tests. */
internal object ApprovalSamples {
    const val ID = "ap-1"
    private const val T0 = 1_761_800_000_000L
    private val running = ToolOutcome.Running(T0)
    private val pending = ToolApproval(ID, ApprovalStatus.Pending)

    val diff = "@@ -10,4 +10,5 @@ fn sync()\n     let batch = pull()?;\n-    apply(batch);\n+    apply(&batch)?;\n+    mark(batch.last());\n     Ok(())"

    val editTool = tool(
        ToolKind.FILE_EDIT, "src/sync.rs", name = "Edit",
        body = BodyView(BodyFormat.DIFF, diff, 6u, null), approval = pending, outcome = running,
    )
    val commandTool = tool(
        ToolKind.SHELL, "$ cargo test -p sync -- --nocapture", name = "Bash",
        detail = "Run the sync tests", approval = pending, outcome = running,
    )
    val content = (1..14).joinToString("\n") { "Line $it of the plan" }
    val writeTool = tool(
        ToolKind.FILE_WRITE, "notes/plan.md", name = "Write",
        body = BodyView(BodyFormat.TEXT, content, 40u, SessionSamples.blob(BlobState.NotDownloaded(2048u))),
        approval = pending, outcome = running,
    )

    val rules = listOf(
        RuleSuggestion("Edit files in src/", RuleScope.SESSION),
        RuleSuggestion("Edit any file", RuleScope.PROJECT),
    )

    fun prompt(gated: Entry.Tool, suggestions: List<RuleSuggestion> = emptyList(), answer: Answer? = null) =
        ApprovalPrompt(ID, gated.tool.call, gated.tool.name, gated.tool.title, null, suggestions, answer)

    fun answer(
        decision: Decision = Decision.APPROVE,
        outgoing: Outgoing = Outgoing.Queued,
        rule: RuleSuggestion? = null,
        retryable: Boolean = false,
    ) = Answer(decision, rule, outgoing, Delivery(T0, null, null, 0u), retryable)

    /** [gated] cancelled with this device's [answer] refused as too late. */
    fun cancelledUnderAnswer(gated: Entry.Tool, answer: Answer): SessionView {
        val approval = ToolApproval(ID, ApprovalStatus.Cancelled("daemon restarted"), answer)
        val settled = Entry.Tool(gated.tool.copy(approval = approval, outcome = ToolOutcome.Cancelled("daemon restarted")))
        return SessionSamples.view(timeline = before() + logged(3, settled), state = SessionState.Idle)
    }

    private fun before(): List<TimelineItem> = listOf(
        message(1, "Make sync apply batches by reference"),
        logged(2, Entry.AgentMessage("I'll change `apply` and run the tests.")),
    )

    /** A session blocked on [gated], the gated call last in its timeline. */
    fun blocked(gated: Entry.Tool, prompt: ApprovalPrompt = prompt(gated)): SessionView = SessionSamples.view(
        timeline = before() + logged(3, gated),
        state = SessionState.AwaitingApproval(ID),
        pendingApproval = prompt,
    )

    /** [gated] answered by another device: settled, no prompt any more. */
    fun answeredElsewhere(gated: Entry.Tool, decision: Decision = Decision.APPROVE): SessionView {
        val status = ApprovalStatus.Resolved(decision, SessionSamples.OTHER, false, null)
        val outcome = if (decision == Decision.DENY) ToolOutcome.Denied(SessionSamples.OTHER, false) else gated.tool.outcome
        val settled = Entry.Tool(gated.tool.copy(approval = ToolApproval(ID, status), outcome = outcome))
        return SessionSamples.view(timeline = before() + logged(3, settled), state = SessionState.AgentWorking)
    }

    /** [n] approvals waiting in other sessions. */
    fun elsewhere(n: Int) = Attention(
        List(n) { PendingApprovalRef("ap-x$it", "s-x$it", "Other session $it", 0u) },
        n.toULong(),
        n.toULong(),
    )
}

@Preview(name = "Approval: diff", heightDp = 900)
@Composable
private fun DiffApprovalPreview() = SessionPreview(ApprovalSamples.blocked(ApprovalSamples.editTool))

@Preview(name = "Approval: command, others waiting", heightDp = 900)
@Composable
private fun CommandApprovalPreview() =
    SessionPreview(ApprovalSamples.blocked(ApprovalSamples.commandTool), ApprovalSamples.elsewhere(2))

@Preview(name = "Approval: file content, dark", heightDp = 900, uiMode = Configuration.UI_MODE_NIGHT_YES)
@Composable
private fun ContentApprovalPreview() = SessionPreview(ApprovalSamples.blocked(ApprovalSamples.writeTool))

@Preview(name = "Approval: with rules", heightDp = 900)
@Composable
private fun RulesApprovalPreview() = SessionPreview(
    ApprovalSamples.blocked(ApprovalSamples.editTool, ApprovalSamples.prompt(ApprovalSamples.editTool, ApprovalSamples.rules)),
)

@Preview(name = "Approval: answered offline, queued", heightDp = 900)
@Composable
private fun QueuedApprovalPreview() = SessionPreview(
    ApprovalSamples.blocked(
        ApprovalSamples.commandTool,
        ApprovalSamples.prompt(ApprovalSamples.commandTool, answer = ApprovalSamples.answer()),
    ),
)

@Preview(name = "Approval: answered on another device")
@Composable
private fun ElsewhereApprovalPreview() = SessionPreview(ApprovalSamples.answeredElsewhere(ApprovalSamples.editTool))
