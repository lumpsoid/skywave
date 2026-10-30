package dev.skywave.android.background

import dev.skywave.android.core.LinkStatus
import dev.skywave.core.Link
import dev.skywave.core.LinkRejection
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.async
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.test.TestScope
import kotlinx.coroutines.test.advanceTimeBy
import kotlinx.coroutines.test.currentTime
import kotlinx.coroutines.test.runCurrent
import kotlinx.coroutines.test.runTest
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Test
import kotlin.time.Duration.Companion.seconds

private val reconnecting = Link.Reconnecting(attempt = 2u, retryAtMs = 10_000, offlineSinceMs = 0)

/** The catch-up's waiting, on virtual time against a fake link. */
@OptIn(ExperimentalCoroutinesApi::class)
class CatchUpTest {
    private val core = RecordingCore()
    private val awake = CoreAwake().apply { attach(core) } // suspended, as in the background
    private val links = MutableStateFlow(LinkStatus(Link.Online(5u), stale = false))
    private val catchUp = CatchUp(awake, timeout = 60.seconds, grace = 2.seconds)

    private fun TestScope.start() = async { catchUp.run { links } }

    @Test
    fun resumesWaitsForAFreshOnlineThenLetsTheNotifierPostAndSuspends() = runTest {
        // What resume() leaves: the link from before the suspend, stale.
        links.value = LinkStatus(Link.Online(5u), stale = true)
        val run = start()
        runCurrent()
        assertEquals(listOf("suspend", "resume"), core.calls)
        assertFalse("a stale Online is not a catch-up", run.isCompleted)

        advanceTimeBy(1_000)
        links.value = LinkStatus(Link.Connecting, stale = false)
        runCurrent()
        advanceTimeBy(1_000)
        links.value = LinkStatus(Link.CatchingUp(have = 1u, server = 9u), stale = false)
        runCurrent()
        links.value = LinkStatus(Link.Online(7u), stale = false)
        runCurrent()
        assertEquals("still awake while the notifier posts", listOf("suspend", "resume"), core.calls)

        assertEquals(CatchUpResult.CAUGHT_UP, run.await())
        assertEquals(4_000, currentTime) // 2 s to get online, then the 2 s grace
        assertEquals(listOf("suspend", "resume", "suspend"), core.calls)
    }

    @Test
    fun rejectedEndsAtOnceWithoutTheGrace() = runTest {
        links.value = LinkStatus(Link.Rejected(LinkRejection.Revoked), stale = true)
        assertEquals(CatchUpResult.STOPPED, start().await())
        assertEquals(0, currentTime)
        assertEquals(listOf("suspend", "resume", "suspend"), core.calls)
    }

    @Test
    fun unpairedEndsAtOnce() = runTest {
        links.value = LinkStatus(Link.Unpaired, stale = false)
        assertEquals(CatchUpResult.STOPPED, start().await())
        assertEquals(0, currentTime)
    }

    @Test
    fun aLinkStillDownGivesUpAtTheTimeout() = runTest {
        links.value = LinkStatus(reconnecting, stale = true)
        val run = start()
        runCurrent()
        links.value = LinkStatus(Link.Connecting, stale = false)
        runCurrent()
        links.value = LinkStatus(reconnecting.copy(attempt = 3u), stale = false)
        assertEquals(CatchUpResult.TIMED_OUT, run.await())
        assertEquals(60_000, currentTime)
        assertEquals(listOf("suspend", "resume", "suspend"), core.calls)
    }

    @Test
    fun aCoreTheAppKeepsAwakeIsNeitherResumedNorSuspended() = runTest {
        awake.hold(Holder.FOREGROUND)
        core.calls.clear()
        val run = start()
        assertEquals(CatchUpResult.CAUGHT_UP, run.await()) // already online: just the grace
        assertEquals(2_000, currentTime)
        assertEquals(emptyList<String>(), core.calls)
    }

    @Test
    fun theAppComingToTheForegroundMeanwhileKeepsTheLink() = runTest {
        links.value = LinkStatus(Link.Connecting, stale = false)
        val run = start()
        runCurrent()
        awake.hold(Holder.FOREGROUND)
        links.value = LinkStatus(Link.Online(3u), stale = false)
        run.await()
        assertEquals(listOf("suspend", "resume"), core.calls)
    }

    @Test
    fun aCoreThatWillNotOpenStillLetsGo() = runTest {
        assertEquals(CatchUpResult.NO_CORE, catchUp.run { null })
        assertEquals(listOf("suspend", "resume", "suspend"), core.calls)
    }

    @Test
    fun cancelledByWorkManagerStillLetsGo() = runTest {
        links.value = LinkStatus(Link.Connecting, stale = false)
        val run = start()
        runCurrent()
        run.cancel()
        runCurrent()
        assertEquals(listOf("suspend", "resume", "suspend"), core.calls)
    }
}
