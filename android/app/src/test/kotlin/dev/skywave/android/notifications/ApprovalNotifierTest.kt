package dev.skywave.android.notifications

import dev.skywave.android.ui.sessionView
import dev.skywave.core.Attention
import dev.skywave.core.Link
import dev.skywave.core.ListKind
import dev.skywave.core.PendingApprovalRef
import dev.skywave.core.Screen
import dev.skywave.core.SessionListView
import dev.skywave.core.ViewState
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.test.runCurrent
import kotlinx.coroutines.test.runTest
import org.junit.Assert.assertEquals
import org.junit.Test

private val online = Link.Online(latencyMs = 5u)
private val list = Screen.SessionList(
    SessionListView(kind = ListKind.ACTIVE, sessions = emptyList(), otherCount = 0u, creating = emptyList(), completion = null),
)

private fun approval(id: String, session: String = "s-$id", title: String = "Session $id") =
    PendingApprovalRef(id = id, sessionId = session, title = title)

private fun view(
    alerts: Int,
    vararg approvals: PendingApprovalRef,
    link: Link = online,
    screen: Screen = list,
) = ViewState(
    link = link,
    queued = 0u,
    attention = Attention(approvals = approvals.toList(), elsewhere = approvals.size.toULong(), alerts = alerts.toULong()),
    screen = screen,
    canGoBack = false,
)

private fun onSession(id: String) = Screen.Session(sessionView(archived = false).copy(id = id))

/** The shade as a map from approval id to what was posted. */
private class FakeNotifications(initial: Set<String> = emptySet()) : Notifications {
    val shown = initial.associateWith { ApprovalAlert(it, "?", "?") }.toMutableMap()
    val posts = mutableListOf<ApprovalAlert>()
    val cancels = mutableListOf<String>()

    override fun post(alert: ApprovalAlert) {
        posts += alert
        shown[alert.approvalId] = alert
    }

    override fun cancel(approvalId: String) {
        cancels += approvalId
        shown -= approvalId
    }

    override fun active() = shown.keys.toSet()
}

class ApprovalNotifierTest {
    private val shade = FakeNotifications()
    private var foreground = false
    private val notifier = ApprovalNotifier(shade) { foreground }

    @Test
    fun approvalsPendingAtStartAreNotPosted() {
        notifier.onView(view(3, approval("a"), approval("b")))
        notifier.onView(view(3, approval("a"), approval("b")))
        assertEquals(emptyList<ApprovalAlert>(), shade.posts)
    }

    @Test
    fun aGrowingAlertCountPostsTheNewApprovalOnce() {
        notifier.onView(view(0, approval("a")))
        notifier.onView(view(1, approval("a"), approval("b", session = "s1", title = "Fix the bug")))
        notifier.onView(view(1, approval("a"), approval("b", session = "s1", title = "Fix the bug")))
        notifier.onView(view(1, approval("a"), approval("b", session = "s1", title = "Fix the bug"), link = Link.Connecting))
        assertEquals(listOf(ApprovalAlert("b", "s1", "Fix the bug")), shade.posts)
    }

    @Test
    fun newApprovalsWithoutAnAlertAreNotPosted() {
        // Answered here and refused, or pending while online but judged no
        // news by the core: alerts stays put, so nothing is posted, then or later.
        notifier.onView(view(0))
        notifier.onView(view(0, approval("a")))
        notifier.onView(view(1, approval("a"), approval("b")))
        assertEquals(listOf("b"), shade.posts.map { it.approvalId })
    }

    @Test
    fun anApprovalSeenWhileCatchingUpIsPostedWhenItAlertsOnline() {
        notifier.onView(view(0, link = Link.CatchingUp(have = 1u, server = 9u)))
        notifier.onView(view(0, approval("a"), link = Link.CatchingUp(have = 5u, server = 9u)))
        notifier.onView(view(1, approval("a")))
        assertEquals(listOf("a"), shade.posts.map { it.approvalId })
    }

    @Test
    fun aRestartDoesNotRepostAndStillAlertsAfterwards() {
        notifier.onView(view(0))
        notifier.onView(view(1, approval("a")))
        // A new process: the core counts from zero again and "a" is not news.
        val restarted = ApprovalNotifier(shade) { false }
        restarted.onView(view(0, approval("a"), link = Link.Connecting))
        restarted.onView(view(0, approval("a")))
        assertEquals(listOf("a"), shade.posts.map { it.approvalId })
        // Its first alert is posted, though the count is below the old one's.
        restarted.onView(view(1, approval("a"), approval("b")))
        assertEquals(listOf("a", "b"), shade.posts.map { it.approvalId })
    }

    @Test
    fun anApprovalLeavingTheListIsCancelled() {
        notifier.onView(view(0))
        notifier.onView(view(1, approval("a")))
        notifier.onView(view(2, approval("a"), approval("b")))
        notifier.onView(view(2, approval("b")))
        assertEquals(listOf("a"), shade.cancels)
        notifier.onView(view(2))
        assertEquals(listOf("a", "b"), shade.cancels)
        assertEquals(emptySet<String>(), shade.active())
        notifier.onView(view(2))
        assertEquals(listOf("a", "b"), shade.cancels)
    }

    @Test
    fun aPreviousProcessesNotificationIsCancelledOnceResolved() {
        val leftover = FakeNotifications(initial = setOf("old", "kept"))
        val fresh = ApprovalNotifier(leftover) { false }
        fresh.onView(view(0, approval("kept"), link = Link.Connecting))
        assertEquals(listOf("old"), leftover.cancels)
        fresh.onView(view(0))
        assertEquals(listOf("old", "kept"), leftover.cancels)
    }

    @Test
    fun notPostedWhileThatSessionIsOnScreenInTheForeground() {
        foreground = true
        notifier.onView(view(0, screen = onSession("s1")))
        notifier.onView(view(1, approval("a", session = "s1"), screen = onSession("s1")))
        assertEquals(emptyList<ApprovalAlert>(), shade.posts)
        // Leaving the session does not post it late: the user saw it.
        notifier.onView(view(1, approval("a", session = "s1")))
        assertEquals(emptyList<ApprovalAlert>(), shade.posts)
    }

    @Test
    fun postedForOtherSessionsOrWhenInTheBackground() {
        foreground = true
        notifier.onView(view(0, screen = onSession("s1")))
        notifier.onView(view(1, approval("a", session = "s2"), screen = onSession("s1")))
        foreground = false
        notifier.onView(view(2, approval("a", session = "s2"), approval("b", session = "s1"), screen = onSession("s1")))
        assertEquals(listOf("a", "b"), shade.posts.map { it.approvalId })
    }

    @Test
    fun openingTheSessionInTheForegroundCancelsItsNotifications() {
        notifier.onView(view(0))
        notifier.onView(view(1, approval("a", session = "s1"), approval("b", session = "s2")))
        foreground = true
        notifier.onView(view(1, approval("a", session = "s1"), approval("b", session = "s2"), screen = onSession("s1")))
        assertEquals(listOf("a"), shade.cancels)
        assertEquals(setOf("b"), shade.active())
    }

    @Test
    fun titlesAreSanitised() {
        notifier.onView(view(0))
        notifier.onView(view(1, approval("a", title = "\u001bred\u0007\n  title\u202e")))
        assertEquals("red title", shade.posts.single().sessionTitle)
    }

    @OptIn(ExperimentalCoroutinesApi::class)
    @Test
    fun followsAStateFlow() = runTest {
        val state = MutableStateFlow(view(2, approval("a")))
        val job = launch { notifier.follow(state) }
        runCurrent()
        state.value = view(3, approval("a"), approval("b"))
        runCurrent()
        state.value = view(3, approval("a"))
        runCurrent()
        job.cancel()
        assertEquals(listOf("b"), shade.posts.map { it.approvalId })
        assertEquals(listOf("b"), shade.cancels)
    }
}

class SanitiseTitleTest {
    @Test
    fun controlAndFormatCharactersAreDropped() {
        assertEquals("ab c", sanitiseTitle("a\u0000b\tc\u200f"))
        assertEquals("", sanitiseTitle(" \u0001\n "))
    }

    @Test
    fun longTitlesAreCappedWithAnEllipsis() {
        val title = sanitiseTitle("x".repeat(200))
        assertEquals(MAX_TITLE, title.length)
        assertEquals('…', title.last())
        assertEquals("short", sanitiseTitle("short"))
    }

    @Test
    fun theCapNeverSplitsASurrogatePair() {
        val title = sanitiseTitle("x".repeat(MAX_TITLE - 2) + "😀".repeat(10))
        assertEquals("x".repeat(MAX_TITLE - 2) + "…", title)
    }
}
