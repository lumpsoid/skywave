package dev.skywave.android.core

import dev.skywave.core.Action
import dev.skywave.core.Attention
import dev.skywave.core.Link
import dev.skywave.core.PairingStatus
import dev.skywave.core.PairingView
import dev.skywave.core.Screen
import dev.skywave.core.SkywaveCoreInterface
import dev.skywave.core.SkywaveException
import dev.skywave.core.ViewListener
import dev.skywave.core.ViewState
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.test.StandardTestDispatcher
import kotlinx.coroutines.test.TestScope
import kotlinx.coroutines.test.runCurrent
import kotlinx.coroutines.test.runTest
import org.junit.Assert.assertEquals
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Test
import java.util.Collections

private fun viewState(link: Link = Link.Connecting, queued: ULong = 0u) = ViewState(
    link = link,
    queued = queued,
    attention = Attention(approvals = emptyList(), elsewhere = 0u, alerts = 0u),
    screen = Screen.Pairing(PairingView(paired = null, status = PairingStatus.Idle)),
)

private val reconnecting = Link.Reconnecting(attempt = 3u, nextInMs = 4_000u, offlineSinceMs = 0)

private fun draft(i: Int) = Action.SaveDraft(sessionId = "s", text = "t$i", cursor = 0u)

/** Stands in for `SkywaveCore` without loading the native library. */
private class FakeCore(initial: ViewState) : SkywaveCoreInterface {
    @Volatile var current: ViewState = initial
    val listeners = mutableListOf<ViewListener>()
    val dispatched: MutableList<Action> = Collections.synchronizedList(mutableListOf())
    var failWith: ((Action) -> Exception?) = { null }
    var dispatchDelayMs = 0L
    var suspended = false

    /** What the core does after a change: tell every listener. */
    fun publish(view: ViewState) {
        current = view
        listeners.forEach { it.onView(view) }
    }

    override fun dispatch(action: Action) {
        if (dispatchDelayMs > 0) Thread.sleep(dispatchDelayMs)
        failWith(action)?.let { throw it }
        dispatched += action
    }

    override fun subscribe(listener: ViewListener): ULong {
        listeners += listener
        listener.onView(current)
        return listeners.size.toULong()
    }

    override fun view(): ViewState = current
    override fun resume() { suspended = false }
    override fun suspend() { suspended = true }
    override fun shutdown() = Unit
    override fun unsubscribe(id: ULong) = Unit
}

@OptIn(ExperimentalCoroutinesApi::class) // runCurrent
class CoreRepositoryTest {
    private fun TestScope.repository(core: FakeCore) =
        CoreRepository(core, backgroundScope, StandardTestDispatcher(testScheduler))

    @Test
    fun initialValueIsTheHydratedView() = runTest {
        val hydrated = viewState(queued = 7u)
        val repo = repository(FakeCore(hydrated))
        assertSame(hydrated, repo.state.value)
    }

    @Test
    fun listenerUpdatesTheFlow() = runTest {
        val core = FakeCore(viewState())
        val repo = repository(core)
        val online = viewState(link = Link.Online(42u))
        core.publish(online)
        assertSame(online, repo.state.value)
    }

    @Test
    fun dispatchRunsOffTheCallerAndKeepsOrder() = runTest {
        val core = FakeCore(viewState())
        val repo = repository(core)
        val actions = (0 until 500).map(::draft)
        actions.forEach(repo::dispatch)
        assertTrue("dispatch must not run the action inline", core.dispatched.isEmpty())
        runCurrent()
        assertEquals(actions, core.dispatched)
    }

    @Test
    fun orderHoldsOnTheRealIoPool() = runTest {
        val core = FakeCore(viewState()).apply { dispatchDelayMs = 1 }
        val repo = CoreRepository(core, backgroundScope, Dispatchers.IO)
        val actions = (0 until 50).map(::draft)
        actions.forEach(repo::dispatch)
        val deadline = System.nanoTime() + 10_000_000_000
        while (core.dispatched.size < actions.size && System.nanoTime() < deadline) Thread.sleep(5)
        assertEquals(actions, core.dispatched.toList())
    }

    @Test
    fun dispatchErrorBecomesAMessageAndLaterActionsStillRun() = runTest {
        val core = FakeCore(viewState())
        core.failWith = { a -> if (a == draft(1)) SkywaveException.Store("disk full") else null }
        val repo = repository(core)
        repo.dispatch(draft(1)) // must not throw
        repo.dispatch(draft(2))
        runCurrent()
        assertEquals("Storage error: disk full", repo.messages.first())
        assertEquals(listOf(draft(2)), core.dispatched)
    }

    @Test
    fun nonSkywaveErrorDoesNotStopTheConsumer() = runTest {
        val core = FakeCore(viewState())
        core.failWith = { a -> if (a == draft(1)) IllegalStateException("boom") else null }
        val repo = repository(core)
        repo.dispatch(draft(1))
        repo.dispatch(draft(2))
        runCurrent()
        assertEquals("boom", repo.messages.first())
        assertEquals(listOf(draft(2)), core.dispatched)
    }

    @Test
    fun networkBackSendsRetryNowOnlyWhileReconnecting() = runTest {
        val core = FakeCore(viewState())
        val repo = repository(core)
        for (link in listOf(Link.Connecting, Link.Online(5u), Link.CatchingUp(1u, 9u))) {
            core.publish(viewState(link = link))
            repo.onNetworkAvailable()
        }
        runCurrent()
        assertTrue(core.dispatched.isEmpty())

        core.publish(viewState(link = reconnecting))
        repo.onNetworkAvailable()
        runCurrent()
        assertEquals(listOf<Action>(Action.RetryNow), core.dispatched)
    }

    @Test
    fun suspendAndResumeReachTheCore() = runTest {
        val core = FakeCore(viewState())
        val repo = repository(core)
        repo.suspend()
        assertTrue(core.suspended)
        repo.resume()
        assertTrue(!core.suspended)
    }

    @Test
    fun openFailureBecomesFailedStartup() = runTest {
        val startup = startCore(
            open = { throw SkywaveException.Store("database is locked") },
            scope = backgroundScope,
            dispatchContext = StandardTestDispatcher(testScheduler),
        )
        assertEquals(CoreStartup.Failed("Storage error: database is locked"), startup)
    }

    @Test
    fun openSuccessIsReadyWithTheHydratedView() = runTest {
        val hydrated = viewState(queued = 2u)
        val startup = startCore({ FakeCore(hydrated) }, backgroundScope, StandardTestDispatcher(testScheduler))
        assertSame(hydrated, (startup as CoreStartup.Ready).repository.state.value)
    }
}

class LabelsTest {
    @Test
    fun linkLabels() {
        assertEquals("Connecting", Link.Connecting.label())
        assertEquals("Online 42 ms", Link.Online(42u).label())
        assertEquals("Reconnecting #3", reconnecting.label())
        assertEquals("Pairing", viewState().screen.label())
    }
}
