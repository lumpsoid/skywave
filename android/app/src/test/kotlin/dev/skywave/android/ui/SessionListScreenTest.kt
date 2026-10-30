package dev.skywave.android.ui

import androidx.activity.ComponentActivity
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.test.assert
import androidx.compose.ui.test.assertIsNotEnabled
import androidx.compose.ui.test.hasText
import androidx.compose.ui.test.junit4.v2.createAndroidComposeRule
import androidx.compose.ui.test.longClick
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performImeAction
import androidx.compose.ui.test.performTextClearance
import androidx.compose.ui.test.performTextInput
import androidx.compose.ui.test.performTouchInput
import dev.skywave.android.ui.screens.NewSessionTags
import dev.skywave.android.ui.screens.SessionListSamples
import dev.skywave.android.ui.screens.SessionListTags
import dev.skywave.android.ui.screens.SettingsSamples
import dev.skywave.core.Action
import dev.skywave.core.Completion
import dev.skywave.core.CompletionStatus
import dev.skywave.core.Link
import dev.skywave.core.Route
import dev.skywave.core.Screen
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner

/** The session list on the JVM (Robolectric), from hand-made snapshots. */
@RunWith(RobolectricTestRunner::class)
class SessionListScreenTest {
    @get:Rule
    val compose = createAndroidComposeRule<ComponentActivity>()

    private val dispatched = mutableListOf<Action>()
    private var screen by mutableStateOf<Screen>(SessionListSamples.active)
    private var link by mutableStateOf<Link>(Link.Online(5u))
    private var core: (Action) -> Unit = {}
    private var shown = false

    /**
     * Shows [next]; later calls swap the snapshot as the core would. [answer]
     * stands in for the core's answer to each action.
     */
    private fun show(next: Screen, online: Link = Link.Online(5u), answer: (Action) -> Unit = {}) {
        screen = next
        link = online
        core = answer
        if (shown) {
            compose.waitForIdle()
            return
        }
        shown = true
        val inbox = PairInbox { PairInput.Unreadable("tests read no links") }
        compose.setContent {
            SkywaveTheme(dynamicColor = false) {
                AppShell(
                    Samples.state(link, screen),
                    dispatch = {
                        dispatched += it
                        core(it)
                    },
                    inbox,
                    SettingsSamples.about,
                )
            }
        }
    }

    private fun text(t: String, substring: Boolean = false) = compose.onNodeWithText(t, substring = substring)
    private fun tag(t: String) = compose.onNodeWithTag(t)

    /** The active list with [completion] in place of the current one. */
    private fun withCompletion(completion: Completion?) {
        val list = (screen as Screen.SessionList).view
        screen = Screen.SessionList(list.copy(completion = completion))
    }

    @Test
    fun rowsShowWhatTheCoreSaysAndOpenOnTap() {
        show(SessionListSamples.active)
        text("Fix the sync bug").assertExists()
        compose.onNodeWithContentDescription("Working").assertExists()
        compose.onNodeWithContentDescription("3 unread").assertExists()
        text("$0.42", substring = true).assertExists()
        compose.onNodeWithContentDescription("Waiting for approval").assertExists()
        text("1 approval waiting").assertExists()
        compose.onNodeWithContentDescription("Draft").assertExists()
        text("Error: agent exited with status 1").assertExists()
        text("Rename refused", substring = true).assertExists()

        text("Write the Android shell").performClick()
        assertEquals(listOf<Action>(Action.OpenSession("s2")), dispatched)
    }

    @Test
    fun emptyActiveListInvitesANewSessionAndPointsAtTheArchive() {
        show(SessionListSamples.emptyActive)
        text("No sessions yet").assertExists()
        text("Show", substring = true).assertDoesNotExist()
        tag(SessionListTags.NEW).assertExists()

        show(SessionListSamples.allArchived)
        text("Show 2 archived sessions").performClick()
        assertEquals(listOf<Action>(Action.Navigate(Route.ARCHIVED_SESSIONS)), dispatched)
    }

    @Test
    fun archivedListHasItsTitleBackAndNoNewSession() {
        show(SessionListSamples.emptyArchived)
        text("Archived sessions").assertExists()
        text("No archived sessions").assertExists()
        tag(SessionListTags.NEW).assertDoesNotExist()
        compose.onNodeWithContentDescription("Settings").assertDoesNotExist()
        compose.onNodeWithContentDescription("Back").performClick()
        assertEquals(listOf<Action>(Action.Navigate(Route.SESSION_LIST)), dispatched)
    }

    @Test
    fun createCompletesFillsAndCreates() {
        show(SessionListSamples.emptyActive) { action ->
            when (action) {
                is Action.CompleteWorkdir -> withCompletion(
                    when (action.input) {
                        "/home/me/s" -> SessionListSamples.completion(
                            action.input,
                            "/home/me/src",
                            "/home/me/scratch",
                            fill = null,
                        )
                        "/home/me/sr" -> SessionListSamples.completion(action.input, "/home/me/src", fill = "/home/me/src/")
                        else -> SessionListSamples.completion(action.input, "/home/me/src/skywave")
                    },
                )
                is Action.CreateSession, Action.EndCompletion -> withCompletion(null)
                else -> Unit
            }
        }
        tag(SessionListTags.NEW).performClick()
        tag(NewSessionTags.CREATE).assertIsNotEnabled()

        // Typing alone asks nothing: the candidates are not showing yet.
        tag(NewSessionTags.WORKDIR).performTextInput("/home/me/s")
        assertEquals(emptyList<Action>(), dispatched)

        // Complete (the TUI's Tab) asks; candidates show.
        tag(NewSessionTags.COMPLETE).performClick()
        tag(NewSessionTags.candidate("/home/me/scratch")).assertExists()

        // While they show, every edit asks again; a settled fill is applied.
        tag(NewSessionTags.WORKDIR).performTextInput("r")
        tag(NewSessionTags.WORKDIR).performImeAction() // the keyboard's action key completes too
        tag(NewSessionTags.WORKDIR).assert(hasText("/home/me/src/"))

        // Tapping a candidate fills it in, which lists what is inside.
        tag(NewSessionTags.candidate("/home/me/src/skywave")).performClick()
        tag(NewSessionTags.WORKDIR).assert(hasText("/home/me/src/skywave/"))

        tag(NewSessionTags.CREATE).performClick()
        tag(NewSessionTags.WORKDIR).assertDoesNotExist()
        assertEquals(
            listOf(
                Action.CompleteWorkdir("/home/me/s"),
                Action.CompleteWorkdir("/home/me/sr"),
                Action.CompleteWorkdir("/home/me/sr"),
                Action.CompleteWorkdir("/home/me/src/"),
                Action.CompleteWorkdir("/home/me/src/skywave/"),
                Action.CreateSession("/home/me/src/skywave/"),
            ),
            dispatched,
        )
    }

    @Test
    fun completionStatusIsShown() {
        show(SessionListSamples.emptyActive) { action ->
            if (action is Action.CompleteWorkdir) {
                withCompletion(SessionListSamples.completion(action.input, status = CompletionStatus.Unavailable))
            }
        }
        tag(SessionListTags.NEW).performClick()
        tag(NewSessionTags.COMPLETE).performClick()
        text("The server cannot list folders now", substring = true).assertExists()
        text("No matching folders").assertExists()
    }

    @Test
    fun closingTheDialogEndsTheCompletion() {
        show(SessionListSamples.emptyActive)
        tag(SessionListTags.NEW).performClick()
        tag(NewSessionTags.WORKDIR).performTextInput("/tmp")
        tag(NewSessionTags.CANCEL).performClick()
        tag(NewSessionTags.WORKDIR).assertDoesNotExist()
        assertEquals(listOf<Action>(Action.EndCompletion), dispatched)
    }

    @Test
    fun renameSendsTheNameAndBlankRestoresTheDefault() {
        show(SessionListSamples.active)
        tag(SessionListTags.menu("s1")).performClick()
        text("Rename").performClick()
        tag(SessionListTags.RENAME_FIELD).performTextClearance()
        tag(SessionListTags.RENAME_FIELD).performTextInput("Sync fix")
        tag(SessionListTags.RENAME_CONFIRM).performClick()

        // A long press opens the same menu; a default title starts blank.
        text("skywave").performTouchInput { longClick() }
        text("Rename").performClick()
        tag(SessionListTags.RENAME_CONFIRM).performClick()

        assertEquals(
            listOf<Action>(Action.RenameSession("s1", "Sync fix"), Action.RenameSession("s3", "")),
            dispatched,
        )
    }

    @Test
    fun archiveAndUnarchiveAreAbsolute() {
        show(SessionListSamples.active)
        tag(SessionListTags.menu("s2")).performClick()
        text("Archive").performClick()

        show(SessionListSamples.archived)
        tag(SessionListTags.menu("s4")).performClick()
        text("Unarchive").performClick()

        assertEquals(
            listOf<Action>(Action.SetSessionArchived("s2", true), Action.SetSessionArchived("s4", false)),
            dispatched,
        )
    }

    @Test
    fun deleteIsOfferedOnlyWhenDeletableAndOnlyAfterConfirming() {
        show(SessionListSamples.active)
        tag(SessionListTags.menu("s1")).performClick()
        text("Delete", substring = true).assertDoesNotExist()

        show(SessionListSamples.archived)
        tag(SessionListTags.menu("s5")).performClick()
        text("Delete from this device: waits for an answer first").assertIsNotEnabled()
        text("Delete from this device: waits for an answer first").performClick()
        assertEquals(emptyList<Action>(), dispatched)

        show(SessionListSamples.archived)
        tag(SessionListTags.menu("s4")).performClick()
        text("Delete from this device").performClick()
        text("stays on the server", substring = true).assertExists()
        assertEquals(emptyList<Action>(), dispatched) // nothing before confirming
        text("Cancel").performClick()
        assertEquals(emptyList<Action>(), dispatched)

        tag(SessionListTags.menu("s4")).performClick()
        text("Delete from this device").performClick()
        tag(SessionListTags.DELETE_CONFIRM).performClick()
        assertEquals(listOf<Action>(Action.DeleteSessionLocally("s4")), dispatched)
    }

    @Test
    fun offlinePendingCreationsShowTheirStateAndDiscard() {
        show(SessionListSamples.offlineWithPending, Samples.reconnecting())
        text("Queued: starts when the server is reachable").assertExists()
        text("Starting…").assertExists()
        text("Refused: the folder is outside the server's roots").assertExists()
        tag(SessionListTags.discard("c3")).assertDoesNotExist() // accepted: not the device's to take back
        tag(SessionListTags.discard("c2")).assert(hasText("Dismiss")).performClick()
        tag(SessionListTags.discard("c1")).assert(hasText("Discard")).performClick()
        assertEquals(listOf<Action>(Action.Discard("c2"), Action.Discard("c1")), dispatched)
    }
}
