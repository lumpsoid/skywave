package dev.skywave.android.ui

import androidx.activity.ComponentActivity
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.test.hasClickAction
import androidx.compose.ui.test.hasText
import androidx.compose.ui.test.junit4.v2.createAndroidComposeRule
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import dev.skywave.android.core.CoreClient
import dev.skywave.android.core.CoreMessage
import dev.skywave.android.ui.screens.SettingsSamples
import dev.skywave.core.Action
import dev.skywave.core.BlobPolicy
import dev.skywave.core.Entry
import dev.skywave.core.Link
import dev.skywave.core.LinkRejection
import dev.skywave.core.Route
import dev.skywave.core.Screen
import dev.skywave.core.SessionState
import dev.skywave.core.SessionView
import dev.skywave.core.SettingsView
import dev.skywave.core.TimelineItem
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

    private val inbox = PairInbox { PairInput.Unreadable("no links in these tests") }

    private fun show(state: ViewState) {
        compose.setContent { SkywaveTheme(dynamicColor = false) { AppShell(state, dispatch = { dispatched += it }, inbox, SettingsSamples.about) } }
    }

    private fun text(s: String, substring: Boolean = false) = compose.onNodeWithText(s, substring = substring)

    @Test
    fun connectingShowsAQuietLine() {
        show(Samples.state(Link.Connecting))
        text("Connecting…").assertExists()
        text("Fix the sync bug").assertExists()
    }

    @Test
    fun unpairedShowsNoLinkBanner() {
        show(Samples.state(Link.Unpaired, Samples.unpaired))
        text("Pair with a server").assertExists()
        text("Connecting…").assertDoesNotExist()
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
        show(Samples.state(Samples.reconnecting(), queued = 3u))
        text("Offline · 3 queued · retry in", substring = true).assertExists()
        text("attempt 3", substring = true).assertExists()
        text("since ", substring = true).assertExists()
        text("Retry now").performClick()
        assertEquals(listOf<Action>(Action.RetryNow), dispatched)
    }

    /**
     * The countdown runs on the wall clock to the snapshot's `retryAtMs`: a
     * later snapshot carrying the same link (built anew, equal by value) and
     * a recreated banner, as after rotation, both pick up where it stood.
     */
    @Test
    fun countdownRunsToTheDeadlineAcrossSnapshotsAndRecreation() {
        var nowMs = 1_700_000_000_000
        val clock = { nowMs }
        val first = Link.Reconnecting(attempt = 3u, retryAtMs = nowMs + 10_000, offlineSinceMs = nowMs - 60_000)
        var state by mutableStateOf(Samples.state(first))
        var generation by mutableIntStateOf(0)
        compose.mainClock.autoAdvance = false
        compose.setContent {
            CompositionLocalProvider(LocalWallClock provides clock) {
                SkywaveTheme(dynamicColor = false) { key(generation) { AppShell(state, dispatch = {}, inbox, SettingsSamples.about) } }
            }
        }
        compose.mainClock.advanceTimeByFrame()
        text("Offline · retry in 10s").assertExists()

        nowMs += 3_000
        compose.mainClock.advanceTimeBy(3_000)
        text("Offline · retry in 7s").assertExists()

        val again = Link.Reconnecting(attempt = 3u, retryAtMs = first.retryAtMs, offlineSinceMs = first.offlineSinceMs)
        assertEquals(first, again)
        state = Samples.state(again, queued = 1u)
        compose.waitForIdle()
        compose.mainClock.advanceTimeByFrame()
        text("Offline · 1 queued · retry in 7s").assertExists()

        generation++
        compose.waitForIdle()
        compose.mainClock.advanceTimeByFrame()
        text("Offline · 1 queued · retry in 7s").assertExists()

        nowMs += 7_000
        compose.mainClock.advanceTimeBy(7_000)
        text("Offline · 1 queued · retrying…").assertExists()
    }

    @Test
    fun reconnectingWithNothingQueuedLeavesTheCountOut() {
        show(Samples.state(Samples.reconnecting()))
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
    fun systemBackOnRevokedKeepsReadingInsteadOfNavigating() {
        show(
            Samples.state(
                Link.Rejected(LinkRejection.Revoked),
                Screen.Session(sessionView(archived = false, items = 3)),
                canGoBack = true,
            ),
        )
        text("Message the agent").assertDoesNotExist()
        compose.runOnUiThread { compose.activity.onBackPressedDispatcher.onBackPressed() }
        compose.waitForIdle()
        compose.mainClock.advanceTimeByFrame()
        assertEquals(emptyList<Action>(), dispatched)
        text("Message the agent").assertExists() // the session, not the list behind it
        text("This device was revoked").assertExists() // now as a banner
        // With the full screen gone, back navigates again.
        compose.runOnUiThread { compose.activity.onBackPressedDispatcher.onBackPressed() }
        compose.waitForIdle()
        compose.mainClock.advanceTimeByFrame()
        assertEquals(listOf<Action>(Action.Back), dispatched)
    }

    @Test
    fun revokedOnPairingIsOnlyABanner() {
        show(Samples.state(Link.Rejected(LinkRejection.Revoked), Samples.repair))
        text("Paired as dev-7f3a", substring = true).assertExists()
        text("This device was revoked").assertExists()
        compose.onNode(hasText("Pair again") and hasClickAction()).assertDoesNotExist() // the title only
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
        show(Samples.state(Link.Online(5u), Screen.Settings(SettingsView(BlobPolicy.EAGER, null, null, null)), canGoBack = true))
        compose.onNodeWithContentDescription("Back").performClick()
        compose.runOnUiThread { compose.activity.onBackPressedDispatcher.onBackPressed() }
        compose.waitForIdle()
        compose.mainClock.advanceTimeByFrame()
        assertEquals(List(2) { Action.Back }, dispatched)
    }

    @Test
    fun systemBackAtTheRootIsLeftToTheSystem() {
        show(Samples.state(Link.Online(5u)))
        compose.runOnUiThread { compose.activity.onBackPressedDispatcher.onBackPressed() }
        compose.waitForIdle()
        assertEquals(emptyList<Action>(), dispatched)
    }

    @Test
    fun sessionShowsTitleAndComposer() {
        var state by mutableStateOf(Samples.state(Link.Online(5u), Screen.Session(sessionView(archived = true, items = 3)), canGoBack = true))
        compose.setContent { SkywaveTheme(dynamicColor = false) { AppShell(state, dispatch = { dispatched += it }, inbox, SettingsSamples.about) } }
        text("Fix the sync bug").assertExists()
        text("Message the agent").assertExists()
        compose.onNodeWithContentDescription("Back").performClick()
        assertEquals(listOf<Action>(Action.Back), dispatched)
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
        compose.setContent { SkywaveTheme(dynamicColor = false) { SkywaveRoot(client, inbox, SettingsSamples.about) } }
        messages.trySend(CoreMessage.Storage("disk full"))
        compose.waitUntil(5_000) {
            compose.onAllNodesWithText("Storage error: disk full").fetchSemanticsNodes().isNotEmpty()
        }
        text("Retry now").assertDoesNotExist()
    }
}

internal fun sessionView(archived: Boolean, items: Int = 0) = SessionView(
    id = "s1",
    title = "Fix the sync bug",
    named = true,
    rename = null,
    archived = archived,
    workdir = null,
    state = SessionState.Idle,
    timeline = List(items) {
        TimelineItem.Logged(seq = it.toULong(), atMs = 0, entry = Entry.Interrupted)
    },
    pendingApproval = null,
    rules = emptyList(),
    usage = null,
    interrupt = null,
    stop = null,
    draft = null,
    draftRev = 0u,
    lastSeq = 0u,
    unreadFrom = null,
    prompts = emptyList(),
)
