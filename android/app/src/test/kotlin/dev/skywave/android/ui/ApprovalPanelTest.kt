package dev.skywave.android.ui

import androidx.activity.ComponentActivity
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.test.SemanticsNodeInteraction
import androidx.compose.ui.test.assert
import androidx.compose.ui.test.assertContentDescriptionEquals
import androidx.compose.ui.test.assertIsEnabled
import androidx.compose.ui.test.assertIsNotEnabled
import androidx.compose.ui.test.hasAnyAncestor
import androidx.compose.ui.test.hasTestTag
import androidx.compose.ui.test.hasText
import androidx.compose.ui.test.junit4.v2.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performScrollToKey
import dev.skywave.android.ui.screens.ApprovalSamples
import dev.skywave.android.ui.screens.ApprovalTags
import dev.skywave.android.ui.screens.SessionSamples
import dev.skywave.android.ui.screens.SessionScreen
import dev.skywave.android.ui.screens.TimelineTags
import dev.skywave.core.Action
import dev.skywave.core.ApprovalPrompt
import dev.skywave.core.ApprovalStatus
import dev.skywave.core.Attention
import dev.skywave.core.Decision
import dev.skywave.core.Entry
import dev.skywave.core.FailReason
import dev.skywave.core.Outgoing
import dev.skywave.core.SessionState
import dev.skywave.core.SessionView
import dev.skywave.core.ToolApproval
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner

/** Answering approvals on the session screen (Robolectric): the view in, actions out. */
@RunWith(RobolectricTestRunner::class)
class ApprovalPanelTest {
    @get:Rule
    val compose = createAndroidComposeRule<ComponentActivity>()

    private val dispatched = mutableListOf<Action>()
    private var view by mutableStateOf(SessionSamples.view())
    private var attention by mutableStateOf(SessionSamples.noAttention)

    private fun show(v: SessionView, a: Attention = SessionSamples.noAttention) {
        view = v
        attention = a
        compose.setContent {
            SkywaveTheme(dynamicColor = false) {
                SessionScreen(view, attention, SessionSamples.noChrome, dispatch = { dispatched += it })
            }
        }
        compose.waitForIdle()
    }

    private fun tag(t: String) = compose.onNodeWithTag(t)
    private fun inPanel(t: String): SemanticsNodeInteraction =
        compose.onNode(hasText(t, substring = true) and hasAnyAncestor(hasTestTag(ApprovalTags.PANEL)))
    private fun answers() = dispatched.filterIsInstance<Action.ResolveApproval>()

    /** The gated item, third in every blocked sample. */
    private val gatedItem get() = tag(TimelineTags.item("s:3"))

    private fun seeGated(vararg expected: String) {
        tag(TimelineTags.LIST).performScrollToKey("s:3")
        expected.forEach { gatedItem.assert(hasText(it, substring = true)) }
    }

    @Test
    fun panelShowsTheDiffOfAnEdit() {
        show(ApprovalSamples.blocked(ApprovalSamples.editTool))
        inPanel("Approval needed").assertExists()
        inPanel("Edit").assertExists()
        inPanel("src/sync.rs").assertExists()
        inPanel("+    apply(&batch)?;").assertExists()
        inPanel("-    apply(batch);").assertExists()
        // Six lines fit: nothing to expand.
        tag(ApprovalTags.MORE).assertDoesNotExist()
    }

    @Test
    fun panelShowsTheWholeCommandOnce() {
        show(ApprovalSamples.blocked(ApprovalSamples.commandTool))
        inPanel("Shell").assertExists()
        inPanel("cargo test -p sync -- --nocapture").assertExists()
        inPanel("Run the sync tests").assertExists()
        // The command is the body; the "$ " title does not repeat it.
        inPanel("$ cargo").assertDoesNotExist()
    }

    @Test
    fun panelShowsFileContentBoundedWithAWayToSeeMore() {
        show(ApprovalSamples.blocked(ApprovalSamples.writeTool))
        inPanel("Write").assertExists()
        inPanel("notes/plan.md").assertExists()
        inPanel("Line 1 of the plan").assertExists()
        inPanel("Line 14 of the plan").assertExists()
        inPanel("Full content: 2.0 kB, not downloaded").assertExists()
        tag(ApprovalTags.MORE).assert(hasText("Show more")).performClick()
        tag(ApprovalTags.MORE).assert(hasText("Show less"))
    }

    @Test
    fun anApprovalWithNoLoggedCallShowsItsSummary() {
        val prompt = ApprovalPrompt("ap-v1", null, "Bash", "rm -rf build\nand more", null, emptyList(), null)
        val entry = Entry.Approval("ap-v1", "Bash", "rm -rf build", null, ApprovalStatus.Pending)
        show(
            SessionSamples.view(
                timeline = listOf(SessionSamples.logged(1, entry)),
                state = SessionState.AwaitingApproval("ap-v1"),
                pendingApproval = prompt,
            ),
        )
        inPanel("Bash").assertExists()
        inPanel("and more").assertExists()
        tag(TimelineTags.item("s:1")).assert(hasText("Waiting for your approval", substring = true))
        tag(ApprovalTags.ANSWER).assertExists()
    }

    @Test
    fun approveAndDenyDispatchTheirAnswer() {
        show(ApprovalSamples.blocked(ApprovalSamples.editTool))
        tag(ApprovalTags.APPROVE).performClick()
        tag(ApprovalTags.DENY).performClick()
        assertEquals(
            listOf(
                Action.ResolveApproval(ApprovalSamples.ID, Decision.APPROVE, null),
                Action.ResolveApproval(ApprovalSamples.ID, Decision.DENY, null),
            ),
            answers(),
        )
    }

    @Test
    fun aRuleChoiceApprovesWithItsIndex() {
        val gated = ApprovalSamples.editTool
        show(ApprovalSamples.blocked(gated, ApprovalSamples.prompt(gated, ApprovalSamples.rules)))
        tag(ApprovalTags.rule(0)).assert(hasText("Always allow: Edit files in src/ (this session)"))
        tag(ApprovalTags.rule(1)).assert(hasText("Always allow: Edit any file (this project)")).performClick()
        assertEquals(listOf(Action.ResolveApproval(ApprovalSamples.ID, Decision.APPROVE, 1u)), answers())
    }

    @Test
    fun rulesAreOfferedOnlyWhenTheAgentSuggestsThem() {
        show(ApprovalSamples.blocked(ApprovalSamples.editTool))
        tag(ApprovalTags.APPROVE).assertExists()
        tag(ApprovalTags.rule(0)).assertDoesNotExist()
        inPanel("Always allow").assertDoesNotExist()
    }

    @Test
    fun anAnswerGivenOfflineShowsQueuedAndTurnsTheControlsOff() {
        val gated = ApprovalSamples.commandTool
        val prompt = ApprovalSamples.prompt(gated, ApprovalSamples.rules, ApprovalSamples.answer(Decision.DENY))
        show(ApprovalSamples.blocked(gated, prompt))
        tag(ApprovalTags.STATE).assert(hasText("You denied · queued, goes out when back online"))
        tag(ApprovalTags.APPROVE).assertIsNotEnabled()
        tag(ApprovalTags.DENY).assertIsNotEnabled()
        tag(ApprovalTags.rule(0)).assertDoesNotExist()
        // The gated item says so too, with no way to answer again.
        seeGated("You denied · queued")
        tag(ApprovalTags.ANSWER).assertDoesNotExist()

        // Sent, then accepted: still off until the agent's answer settles it.
        view = ApprovalSamples.blocked(gated, prompt.copy(answer = ApprovalSamples.answer(Decision.DENY, Outgoing.Accepted(9u))))
        compose.waitForIdle()
        tag(ApprovalTags.STATE).assert(hasText("You denied · sent, waiting for the agent"))
        tag(ApprovalTags.DENY).assertIsNotEnabled()
    }

    @Test
    fun aRuleGrantedOfflineSaysWhichRule() {
        val gated = ApprovalSamples.editTool
        val answer = ApprovalSamples.answer(rule = ApprovalSamples.rules[0])
        show(ApprovalSamples.blocked(gated, ApprovalSamples.prompt(gated, ApprovalSamples.rules, answer)))
        tag(ApprovalTags.STATE).assert(
            hasText("You approved, always allowing Edit files in src/ (this session) · queued", substring = true),
        )
    }

    @Test
    fun aRefusedAnswerCanBeGivenAgain() {
        val gated = ApprovalSamples.editTool
        val refused = ApprovalSamples.answer(outgoing = Outgoing.Failed(FailReason.UNKNOWN_APPROVAL))
        show(ApprovalSamples.blocked(gated, ApprovalSamples.prompt(gated, answer = refused)))
        tag(ApprovalTags.STATE).assert(hasText("refused", substring = true))
        tag(ApprovalTags.APPROVE).assertIsEnabled().performClick()
        assertEquals(listOf(Action.ResolveApproval(ApprovalSamples.ID, Decision.APPROVE, null)), answers())
        tag(ApprovalTags.ANSWER).assertExists()
    }

    @Test
    fun anAnswerFromAnotherDeviceRemovesTheControls() {
        show(ApprovalSamples.blocked(ApprovalSamples.editTool))
        tag(ApprovalTags.APPROVE).assertExists()
        view = ApprovalSamples.answeredElsewhere(ApprovalSamples.editTool)
        compose.waitForIdle()
        tag(ApprovalTags.PANEL).assertDoesNotExist()
        tag(ApprovalTags.APPROVE).assertDoesNotExist()
        tag(ApprovalTags.ANSWER).assertDoesNotExist()
        // No device names in the view yet (e4d763): the device's id stands in.
        seeGated("Approved on another device (…000002)")

        view = ApprovalSamples.answeredElsewhere(ApprovalSamples.editTool, Decision.DENY)
        compose.waitForIdle()
        seeGated("Denied on another device (…000002)")
    }

    @Test
    fun aSnapshotStillNamingASettledApprovalOffersNothing() {
        // The prompt has not caught up with the gated call yet.
        val gated = ApprovalSamples.editTool
        val settled = Entry.Tool(
            gated.tool.copy(
                approval = ToolApproval(
                    ApprovalSamples.ID,
                    ApprovalStatus.Resolved(Decision.APPROVE, SessionSamples.OTHER, false, null),
                ),
            ),
        )
        show(ApprovalSamples.blocked(settled, ApprovalSamples.prompt(gated, ApprovalSamples.rules)))
        tag(ApprovalTags.SETTLED).assert(hasText("Approved on another device (…000002)"))
        tag(ApprovalTags.APPROVE).assertDoesNotExist()
        tag(ApprovalTags.rule(0)).assertDoesNotExist()
    }

    @Test
    fun theGatedItemShowsPendingAndOpensThePanel() {
        show(ApprovalSamples.blocked(ApprovalSamples.editTool))
        seeGated("Waiting for your approval")
        tag(ApprovalTags.COLLAPSE).performClick()
        tag(ApprovalTags.PANEL).assertDoesNotExist()
        compose.onNode(hasText("Approval needed: Edit src/sync.rs") and hasAnyAncestor(hasTestTag(ApprovalTags.STRIP))).assertExists()
        tag(ApprovalTags.ANSWER).performClick()
        tag(ApprovalTags.PANEL).assertExists()
        tag(ApprovalTags.APPROVE).assertExists()
        // The strip's own button reopens it too.
        tag(ApprovalTags.COLLAPSE).performClick()
        tag(ApprovalTags.OPEN).performClick()
        tag(ApprovalTags.PANEL).assertExists()
        assertEquals(emptyList<Action>(), answers())
    }

    @Test
    fun nextApprovalJumpsElsewhereAndCountsThem() {
        show(ApprovalSamples.blocked(ApprovalSamples.editTool), ApprovalSamples.elsewhere(2))
        tag(ApprovalTags.NEXT)
            .assertContentDescriptionEquals("Next approval, 2 waiting in other sessions")
            .assert(hasText("2"))
            .performClick()
        assertEquals(listOf(Action.OpenNextApproval), dispatched.filterIsInstance<Action.OpenNextApproval>())

        attention = SessionSamples.noAttention
        compose.waitForIdle()
        tag(ApprovalTags.NEXT).assertDoesNotExist()
    }
}
