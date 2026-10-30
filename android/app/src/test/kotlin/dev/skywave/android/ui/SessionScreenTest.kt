package dev.skywave.android.ui

import android.content.ClipboardManager
import androidx.activity.ComponentActivity
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.semantics.SemanticsProperties
import androidx.compose.ui.semantics.getOrNull
import androidx.compose.ui.test.SemanticsNodeInteraction
import androidx.compose.ui.test.assert
import androidx.compose.ui.test.assertContentDescriptionEquals
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.assertIsNotEnabled
import androidx.compose.ui.test.hasText
import androidx.compose.ui.test.junit4.v2.createAndroidComposeRule
import androidx.compose.ui.test.longClick
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performScrollToKey
import androidx.compose.ui.test.performTextClearance
import androidx.compose.ui.test.performTextInput
import androidx.compose.ui.test.performTouchInput
import androidx.compose.ui.test.swipeDown
import androidx.compose.ui.text.TextRange
import dev.skywave.android.ui.screens.ComposerTags
import dev.skywave.android.ui.screens.DRAFT_MAX_UNSAVED_MS
import dev.skywave.android.ui.screens.DRAFT_PAUSE_MS
import dev.skywave.android.ui.screens.READ_MARK_EVERY_MS
import dev.skywave.android.ui.screens.SessionSamples
import dev.skywave.android.ui.screens.SessionScreen
import dev.skywave.android.ui.screens.SessionTags
import dev.skywave.android.ui.screens.TimelineTags
import dev.skywave.core.Action
import dev.skywave.core.Draft
import dev.skywave.core.Entry
import dev.skywave.core.FailReason
import dev.skywave.core.Outgoing
import dev.skywave.core.SessionState
import dev.skywave.core.SessionView
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner

/** The session screen on the JVM (Robolectric): the view in, actions out. */
@RunWith(RobolectricTestRunner::class)
class SessionScreenTest {
    @get:Rule
    val compose = createAndroidComposeRule<ComponentActivity>()

    private val dispatched = mutableListOf<Action>()
    private var view by mutableStateOf(SessionSamples.view())
    private var shown by mutableStateOf(true)

    private fun show(v: SessionView) {
        view = v
        compose.setContent {
            SkywaveTheme(dynamicColor = false) {
                if (shown) SessionScreen(view, SessionSamples.noAttention, SessionSamples.noChrome, dispatch = { dispatched += it })
            }
        }
        compose.waitForIdle()
    }

    private inline fun <reified T : Action> sent(): List<T> = dispatched.filterIsInstance<T>()

    private fun text(t: String) = compose.onNodeWithText(t)
    private fun tag(t: String) = compose.onNodeWithTag(t)
    private val list get() = tag(TimelineTags.LIST)
    private val field get() = tag(ComposerTags.FIELD)

    private fun SemanticsNodeInteraction.editable(): String =
        fetchSemanticsNode().config.getOrNull(SemanticsProperties.EditableText)?.text.orEmpty()

    private fun SemanticsNodeInteraction.selection(): TextRange? =
        fetchSemanticsNode().config.getOrNull(SemanticsProperties.TextSelectionRange)

    /** Scrolls the item [key] into view and checks [expected] is shown. */
    private fun seeAt(key: String, vararg expected: String) {
        list.performScrollToKey(key)
        expected.forEach { tag(TimelineTags.item(key)).assert(hasText(it, substring = true)) }
    }

    private fun text(t: String, substring: Boolean) = compose.onNodeWithText(t, substring = substring)

    @Test
    fun rendersEveryEntryKind() {
        show(SessionSamples.view())
        seeAt("m:c1", "Fix the failing sync test")
        seeAt("s:2", "Looking at", "fn main() {}", "Then the test.")
        seeAt("s:12", "Call OldTool: legacy call")
        seeAt("s:13", "Result: legacy result")
        seeAt("s:14", "Approval for Bash: rm -rf build", "Approval cancelled: turn ended")
        seeAt("s:15", "Agent error", "rate limited")
        seeAt("s:16", "Turn interrupted")
        seeAt("s:17", "Agent stopped")
        seeAt("s:18", "Renamed to “Sync fix” by you")
        seeAt("s:19", "Archived from another device")
        seeAt("s:20", "12.3k in · 840 out · $0.0123 · 4.1s · sonnet")
        seeAt("m:c21", "And from my laptop", "Another device")
    }

    @Test
    fun toolItemsBranchOnTheirKind() {
        show(SessionSamples.view())
        seeAt("s:3", "Read", "src/sync.rs", "Done in 1.2s")
        seeAt("s:4", "Edit", "+new line", "Approved by you")
        seeAt("s:5", "Shell", "cargo test -p sync", "test sync ... FAILED\nfailures: 1", "Failed (exit 101) in 8.3s")
        seeAt("s:6", "Write", "# Notes")
        seeAt("s:7", "Search", "in src/")
        seeAt("s:8", "Fetch", "Denied on another device (…000002)")
        seeAt("s:9", "Subagent", "Cancelled: turn ended")
        seeAt("s:10", "Tool Frobnicate", "No result")
        seeAt("s:11", "Running since")
        // Expanding shows the line count and the output blob's state (A13 fetches it).
        list.performScrollToKey("s:5")
        tag(TimelineTags.item("s:5")).performClick()
        text("Output: 4.1 kB, not downloaded").assertExists()
        text("40 lines in all").assertExists()
    }

    @Test
    fun ownMessagesShowDeliveryAndCanBeTakenBack() {
        show(SessionSamples.offline)
        text("Queued").assertExists()
        text("Refused: the session is unknown to the server").assertExists()
        tag(TimelineTags.item("m:p1")).performClick()
        text("queued", substring = true).assertExists()
        text("not sent yet", substring = true).assertExists()
        text("Discard").performClick()
        text("Edit").performClick()
        assertEquals(listOf(Action.Discard("p1")), sent<Action.Discard>())
        assertEquals(listOf(Action.EditQueued("p1")), sent<Action.EditQueued>())
    }

    @Test
    fun editSavesUnsavedTypingFirstSoTheCoreJoinsIt() {
        show(SessionSamples.offline)
        field.performTextInput("half")
        tag(TimelineTags.item("m:p1")).performTouchInput { longClick() }
        text("Edit").performClick()
        val tail = dispatched.filter { it is Action.SaveDraft || it is Action.EditQueued }
        assertEquals(listOf(Action.SaveDraft("s1", "half", 4u), Action.EditQueued("p1")), tail)
    }

    @Test
    fun sendDispatchesAndClears() {
        show(SessionSamples.view())
        field.performTextInput("hello")
        tag(ComposerTags.SEND).performClick()
        assertEquals(listOf(Action.SendMessage("s1", "hello")), sent<Action.SendMessage>())
        assertEquals("", field.editable())
        compose.mainClock.advanceTimeBy(DRAFT_MAX_UNSAVED_MS + 100)
        assertEquals(emptyList<Action>(), sent<Action.SaveDraft>())
    }

    @Test
    fun blankTextIsNotSent() {
        show(SessionSamples.view())
        field.performTextInput("   ")
        tag(ComposerTags.SEND).performClick()
        assertEquals(emptyList<Action>(), sent<Action.SendMessage>())
    }

    @Test
    fun draftIsRestoredAndRefilledWhenTheCoreRewritesIt() {
        show(SessionSamples.view(draft = Draft("half done", 4u)))
        assertEquals("half done", field.editable())
        assertEquals(TextRange(4), field.selection())
        // A snapshot with the same rev leaves what is being typed alone.
        field.performTextInput("!")
        view = view.copy(draft = Draft("other", 0u))
        compose.waitForIdle()
        assertTrue(field.editable().contains("!"))
        // A new rev (EditQueued) replaces it.
        view = view.copy(draft = Draft("queued text\n\nhalf done", 11u), draftRev = 1u)
        compose.waitForIdle()
        assertEquals("queued text\n\nhalf done", field.editable())
        assertEquals(TextRange(11), field.selection())
    }

    @Test
    fun draftIsSavedAfterAPauseNotPerKeystroke() {
        show(SessionSamples.view())
        compose.mainClock.autoAdvance = false
        field.performTextInput("a")
        compose.mainClock.advanceTimeBy(DRAFT_PAUSE_MS / 2)
        field.performTextInput("b")
        compose.mainClock.advanceTimeBy(DRAFT_PAUSE_MS / 2)
        field.performTextInput("c")
        compose.mainClock.advanceTimeBy(DRAFT_PAUSE_MS - 200)
        assertEquals(emptyList<Action>(), sent<Action.SaveDraft>())
        compose.mainClock.advanceTimeBy(400)
        assertEquals(listOf(Action.SaveDraft("s1", "abc", 3u)), sent<Action.SaveDraft>())
        // Nothing new typed: nothing more saved.
        compose.mainClock.advanceTimeBy(DRAFT_MAX_UNSAVED_MS * 2)
        assertEquals(1, sent<Action.SaveDraft>().size)
    }

    @Test
    fun steadyTypingIsSavedOnceTheFirstUnsavedEditIsOldEnough() {
        show(SessionSamples.view())
        compose.mainClock.autoAdvance = false
        var typed = ""
        repeat(((DRAFT_MAX_UNSAVED_MS / 1000) + 1).toInt()) {
            field.performTextInput("x")
            typed += "x"
            compose.mainClock.advanceTimeBy(1000)
        }
        val saves = sent<Action.SaveDraft>()
        assertEquals(1, saves.size)
        assertTrue(saves[0].text.length in 5..6)
    }

    @Test
    fun draftIsSavedWhenTheScreenGoes() {
        show(SessionSamples.view())
        field.performTextInput("unsent")
        assertEquals(emptyList<Action>(), sent<Action.SaveDraft>())
        shown = false
        compose.waitForIdle()
        assertEquals(listOf(Action.SaveDraft("s1", "unsent", 6u)), sent<Action.SaveDraft>())
    }

    @Test
    fun clearingTheComposerSavesTheBlankDraft() {
        show(SessionSamples.view(draft = Draft("old", 3u)))
        field.performTextClearance()
        shown = false
        compose.waitForIdle()
        assertEquals(listOf(Action.SaveDraft("s1", "", 0u)), sent<Action.SaveDraft>())
    }

    @Test
    fun marksReadOnceWhenTheBottomIsVisibleAndOnlyForward() {
        show(SessionSamples.view())
        assertEquals(listOf(Action.MarkRead("s1", 21u)), sent<Action.MarkRead>())
        compose.mainClock.advanceTimeBy(READ_MARK_EVERY_MS * 2)
        assertEquals(1, sent<Action.MarkRead>().size)

        // A new event at the bottom marks again, after the cool-down.
        val more = view.timeline + SessionSamples.logged(22, Entry.AgentMessage("more"))
        view = view.copy(timeline = more, lastSeq = 22u)
        compose.waitForIdle()
        compose.mainClock.advanceTimeBy(READ_MARK_EVERY_MS + 100)
        assertEquals(listOf(21uL, 22uL), sent<Action.MarkRead>().map { it.through })

        // Never backwards.
        view = view.copy(lastSeq = 20u)
        compose.mainClock.advanceTimeBy(READ_MARK_EVERY_MS * 2)
        assertEquals(listOf(21uL, 22uL), sent<Action.MarkRead>().map { it.through })
    }

    @Test
    fun opensOnTheDividerAndDoesNotMarkWhatIsNotSeen() {
        val timeline = SessionSamples.long(200)
        show(SessionSamples.view(timeline = timeline, unreadFrom = 20u))
        tag(TimelineTags.DIVIDER).assertIsDisplayed()
        compose.mainClock.advanceTimeBy(READ_MARK_EVERY_MS * 2)
        assertEquals(emptyList<Action>(), sent<Action.MarkRead>())
        // Going to the latest marks it.
        tag(TimelineTags.JUMP).performClick()
        compose.waitForIdle()
        tag(TimelineTags.item("s:200")).assertIsDisplayed()
        assertEquals(listOf(Action.MarkRead("s1", 200u)), sent<Action.MarkRead>())
    }

    @Test
    fun followsTheBottomUntilTheUserScrollsUp() {
        show(SessionSamples.view(timeline = SessionSamples.long(100)))
        tag(TimelineTags.item("s:100")).assertIsDisplayed()
        view = view.copy(timeline = view.timeline + SessionSamples.logged(101, Entry.AgentMessage("one more")), lastSeq = 101u)
        compose.waitForIdle()
        tag(TimelineTags.item("s:101")).assertIsDisplayed()

        list.performTouchInput { swipeDown() }
        compose.waitForIdle()
        tag(TimelineTags.item("s:101")).assertDoesNotExist()
        val added = (102..104).map { SessionSamples.logged(it, Entry.AgentMessage("later $it")) }
        view = view.copy(timeline = view.timeline + added, lastSeq = 104u)
        compose.waitForIdle()
        // Still where the user left it, with a way back that counts what came.
        tag(TimelineTags.item("s:104")).assertDoesNotExist()
        tag(TimelineTags.JUMP).assertContentDescriptionEquals("Jump to latest, 3 new")
        tag(TimelineTags.JUMP).performClick()
        compose.waitForIdle()
        tag(TimelineTags.item("s:104")).assertIsDisplayed()
    }

    @Test
    fun expandingTheLastItemWhileFollowingKeepsItsEndInView() {
        show(SessionSamples.view(timeline = SessionSamples.long(60) + SessionSamples.pending("p1", "last one")))
        tag(TimelineTags.item("m:p1")).performClick()
        compose.waitForIdle()
        text("Discard").assertIsDisplayed()
    }

    @Test
    fun aLongSessionComposes() {
        show(SessionSamples.view(timeline = SessionSamples.long(2000)))
        tag(TimelineTags.item("s:2000")).assertIsDisplayed()
        list.performScrollToKey("m:c1")
        text("Message 1").assertIsDisplayed()
    }

    @Test
    fun controlsDispatch() {
        show(SessionSamples.view(state = SessionState.AgentWorking))
        tag(SessionTags.INTERRUPT).performClick()
        tag(SessionTags.MORE).performClick()
        text("Stop agent").performClick()
        tag(SessionTags.MORE).performClick()
        text("Archive").performClick()
        tag(SessionTags.MORE).performClick()
        text("Rename").performClick()
        tag(SessionTags.RENAME_FIELD).performTextClearance()
        tag(SessionTags.RENAME_FIELD).performTextInput("Sync fix")
        tag(SessionTags.RENAME_CONFIRM).performClick()
        val controls = dispatched.filter { it !is Action.MarkRead }
        assertEquals(
            listOf(
                Action.Interrupt("s1"),
                Action.StopSession("s1"),
                Action.SetSessionArchived("s1", true),
                Action.RenameSession("s1", "Sync fix"),
            ),
            controls,
        )
    }

    @Test
    fun stateAndControlsFollowTheView() {
        show(SessionSamples.view(archived = true, interrupt = Outgoing.Queued, state = SessionState.AgentWorking))
        tag(SessionTags.STATUS).assert(hasText("Interrupting…", substring = true))
        tag(SessionTags.INTERRUPT).assertIsNotEnabled()
        tag(SessionTags.MORE).performClick()
        text("Unarchive").performClick()
        assertEquals(listOf(Action.SetSessionArchived("s1", false)), sent<Action.SetSessionArchived>())

        view = SessionSamples.view(state = SessionState.Idle)
        compose.waitForIdle()
        tag(SessionTags.INTERRUPT).assertDoesNotExist()
        tag(SessionTags.STATUS).assert(hasText("Idle", substring = true))
    }

    @Test
    fun recallFillsTheComposer() {
        show(SessionSamples.view(prompts = listOf("newest", "older")))
        tag(ComposerTags.RECALL).performClick()
        text("older").performClick()
        assertEquals("older", field.editable())
        assertEquals(TextRange(5), field.selection())
    }

    @Test
    fun longPressCopiesAnItem() {
        val clipboard = compose.activity.getSystemService(ClipboardManager::class.java)
        show(SessionSamples.view(timeline = listOf(SessionSamples.logged(1, Entry.AgentMessage("copy me")))))
        tag(TimelineTags.item("s:1")).performTouchInput { longClick() }
        text("Copy text").performClick()
        compose.waitForIdle()
        assertEquals("copy me", clipboard.primaryClip?.getItemAt(0)?.text)
    }

    @Test
    fun emptySessionInvitesAMessage() {
        show(SessionSamples.view(timeline = emptyList()))
        text("No messages yet", substring = true).assertExists()
        assertEquals(emptyList<Action>(), sent<Action.MarkRead>())
    }

    @Test
    fun failedOwnMessageSaysWhy() {
        val failed = SessionSamples.pending("p9", "nope", Outgoing.Failed(FailReason.NO_ACTIVE_TURN))
        show(SessionSamples.view(timeline = listOf(failed)))
        text("Refused: nothing is running").assertExists()
    }
}
