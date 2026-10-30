package dev.skywave.android.ui

import androidx.activity.ComponentActivity
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.test.junit4.v2.createAndroidComposeRule
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import dev.skywave.android.core.CoreClient
import dev.skywave.android.core.CoreMessage
import dev.skywave.core.Action
import dev.skywave.core.BlobPolicy
import dev.skywave.core.Link
import dev.skywave.core.LinkRejection
import dev.skywave.core.Route
import dev.skywave.core.Screen
import dev.skywave.core.SettingsView
import dev.skywave.core.ViewState
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.receiveAsFlow
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner

/** Renders [AppShell] on the JVM (Robolectric) with fixed snapshots. */
@RunWith(RobolectricTestRunner::class)
class AppShellTest {
    @get:Rule
    val compose = createAndroidComposeRule<ComponentActivity>()

    private val dispatched = mutableListOf<Action>()

    private fun show(state: ViewState) {
        compose.setContent { SkywaveTheme(dynamicColor = false) { AppShell(state, dispatch = { dispatched += it }) } }
    }

    private fun text(s: String, substring: Boolean = false) = compose.onNodeWithText(s, substring = substring)

    @Test
    fun connectingShowsAQuietLine() {
        show(Samples.state(Link.Connecting, Samples.unpaired))
        text("Connecting…").assertExists()
        text("Not paired yet").assertExists()
    }

    @Test
    fun onlineShowsNoBanner() {
        show(Samples.state(Link.Online(latencyMs = 42u)))
        text("Sessions").assertExists()
        text("Fix the sync bug").assertExists()
        text("Connecting…").assertDoesNotExist()
        text("Offline", substring = true).assertDoesNotExist()
        text("42", substring = true).assertDoesNotExist()
    }

    @Test
    fun catchingUpShowsProgress() {
        show(Samples.state(Link.CatchingUp(have = 120u, server = 480u)))
        text("Syncing 120 of 480").assertExists()
        text("Fix the sync bug").assertExists() // the replica stays on screen
    }

    @Test
    fun reconnectingShowsQueuedCountdownAndRetries() {
        show(Samples.state(Samples.reconnecting, queued = 3u))
        text("Offline · 3 queued · retry in", substring = true).assertExists()
        text("attempt 3", substring = true).assertExists()
        text("since ", substring = true).assertExists()
        text("Retry now").performClick()
        assertEquals(listOf<Action>(Action.RetryNow), dispatched)
    }

    @Test
    fun reconnectingWithNothingQueuedLeavesTheCountOut() {
        show(Samples.state(Samples.reconnecting))
        text("queued", substring = true).assertDoesNotExist()
        text("Offline · retry in", substring = true).assertExists()
    }

    @Test
    fun attentionBannerOpensTheNextApproval() {
        show(Samples.state(Link.Online(5u), attention = Samples.twoElsewhere))
        text("2 approvals elsewhere").performClick()
        assertEquals(listOf<Action>(Action.OpenNextApproval), dispatched)
    }

    @Test
    fun noAttentionBannerWhenNothingIsElsewhere() {
        show(Samples.state(Link.Online(5u)))
        text("elsewhere", substring = true).assertDoesNotExist()
    }

    @Test
    fun revokedTakesTheScreenAndOffersPairing() {
        show(Samples.state(Link.Rejected(LinkRejection.Revoked)))
        text("This device was revoked").assertExists()
        text("Fix the sync bug").assertDoesNotExist()
        text("Pair again").performClick()
        assertEquals(listOf<Action>(Action.Navigate(Route.PAIRING)), dispatched)
    }

    @Test
    fun revokedCanBeSetAsideToKeepReading() {
        show(Samples.state(Link.Rejected(LinkRejection.Revoked)))
        text("Keep reading").performClick()
        text("Fix the sync bug").assertExists()
        text("This device was revoked").assertExists() // now as a banner
        text("Pair again").assertExists()
    }

    @Test
    fun revokedOnPairingIsOnlyABanner() {
        show(Samples.state(Link.Rejected(LinkRejection.Revoked), Samples.unpaired))
        text("Not paired yet").assertExists()
        text("This device was revoked").assertExists()
        text("Pair again").assertDoesNotExist()
    }

    @Test
    fun protocolMismatchExplainsTheVersions() {
        show(Samples.state(Link.Rejected(LinkRejection.ProtocolMismatch(client = 7u, server = 8u))))
        text("Update needed").assertExists()
        text("(app: 7, server: 8)", substring = true).assertExists()
        text("Pair again").assertDoesNotExist()
    }

    @Test
    fun sessionListNavigates() {
        show(Samples.state(Link.Online(5u)))
        text("Write the Android shell").performClick()
        compose.onNodeWithContentDescription("Settings").performClick()
        compose.onNodeWithContentDescription("Archived sessions").performClick()
        assertEquals(
            listOf(
                Action.OpenSession("s2"),
                Action.Navigate(Route.SETTINGS),
                Action.Navigate(Route.ARCHIVED_SESSIONS),
            ),
            dispatched,
        )
        compose.onNodeWithContentDescription("Back").assertDoesNotExist()
    }

    @Test
    fun systemBackAndUpNavigateThroughTheCore() {
        show(Samples.state(Link.Online(5u), Screen.Settings(SettingsView(BlobPolicy.EAGER, null, null))))
        compose.onNodeWithContentDescription("Back").performClick()
        compose.runOnUiThread { compose.activity.onBackPressedDispatcher.onBackPressed() }
        compose.waitForIdle()
        assertEquals(List(2) { Action.Navigate(Route.SESSION_LIST) }, dispatched)
    }

    @Test
    fun sessionStubShowsTitleAndItemCount() {
        var state by mutableStateOf(Samples.state(Link.Online(5u), Screen.Session(sessionView(archived = true, items = 3))))
        compose.setContent { SkywaveTheme(dynamicColor = false) { AppShell(state, dispatch = { dispatched += it }) } }
        text("Fix the sync bug").assertExists()
        text("3 items").assertExists()
        compose.onNodeWithContentDescription("Back").performClick()
        assertEquals(listOf<Action>(Action.Navigate(Route.ARCHIVED_SESSIONS)), dispatched)
        // The core answers with a new screen; the shell just follows.
        state = Samples.state(Link.Online(5u))
        text("Sessions").assertExists()
    }

    @Test
    fun failedActionsBecomeSnackbars() {
        val messages = Channel<CoreMessage>(Channel.UNLIMITED)
        val client = object : CoreClient {
            override val state = MutableStateFlow(Samples.state(Link.Online(5u)))
            override val messages = messages.receiveAsFlow()
            override fun dispatch(action: Action) {
                dispatched += action
            }
        }
        compose.setContent { SkywaveTheme(dynamicColor = false) { SkywaveRoot(client) } }
        messages.trySend(CoreMessage.Storage("disk full"))
        compose.waitUntil(5_000) {
            compose.onAllNodesWithText("Storage error: disk full").fetchSemanticsNodes().isNotEmpty()
        }
        text("Retry now").assertDoesNotExist()
    }
}
