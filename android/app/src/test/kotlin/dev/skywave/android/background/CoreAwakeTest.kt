package dev.skywave.android.background

import org.junit.Assert.assertEquals
import org.junit.Test

/** Records the calls a core gets. */
class RecordingCore : Wakeable {
    val calls = mutableListOf<String>()

    override fun suspend() {
        calls += "suspend"
    }

    override fun resume() {
        calls += "resume"
    }
}

class CoreAwakeTest {
    private val core = RecordingCore()
    private val awake = CoreAwake()

    @Test
    fun aCoreNothingHoldsIsSuspendedWhenAttached() {
        awake.attach(core)
        assertEquals(listOf("suspend"), core.calls)
    }

    @Test
    fun aCoreAlreadyHeldStaysRunningWhenAttached() {
        // The activity's ON_START, or the worker, before the core opened.
        awake.hold(Holder.FOREGROUND)
        awake.attach(core)
        assertEquals(emptyList<String>(), core.calls)
    }

    @Test
    fun holdersThatCameAndWentBeforeAttachLeaveItSuspended() {
        awake.hold(Holder.WORKER)
        awake.release(Holder.WORKER)
        awake.attach(core)
        assertEquals(listOf("suspend"), core.calls)
    }

    @Test
    fun foregroundAloneResumesAndSuspends() {
        awake.attach(core)
        awake.hold(Holder.FOREGROUND)
        awake.release(Holder.FOREGROUND)
        assertEquals(listOf("suspend", "resume", "suspend"), core.calls)
    }

    @Test
    fun workerFinishingWhileTheAppIsVisibleDoesNotSuspend() {
        awake.hold(Holder.WORKER)
        awake.attach(core)
        awake.hold(Holder.FOREGROUND) // the user opened the app meanwhile
        awake.release(Holder.WORKER)
        assertEquals(emptyList<String>(), core.calls)
        awake.release(Holder.FOREGROUND)
        assertEquals(listOf("suspend"), core.calls)
    }

    @Test
    fun goingToTheBackgroundDuringACatchUpKeepsTheLink() {
        awake.hold(Holder.FOREGROUND)
        awake.attach(core)
        awake.hold(Holder.WORKER)
        awake.release(Holder.FOREGROUND)
        assertEquals(emptyList<String>(), core.calls)
        awake.release(Holder.WORKER)
        assertEquals(listOf("suspend"), core.calls)
    }

    @Test
    fun theServiceKeepsItAwakeAcrossForegroundAndWorker() {
        awake.attach(core)
        awake.hold(Holder.SERVICE)
        awake.hold(Holder.FOREGROUND)
        awake.release(Holder.FOREGROUND)
        awake.hold(Holder.WORKER)
        awake.release(Holder.WORKER)
        assertEquals(listOf("suspend", "resume"), core.calls)
        awake.release(Holder.SERVICE)
        assertEquals(listOf("suspend", "resume", "suspend"), core.calls)
    }

    @Test
    fun repeatedHoldsAndStrayReleasesChangeNothing() {
        awake.attach(core)
        awake.hold(Holder.FOREGROUND)
        awake.hold(Holder.FOREGROUND)
        awake.release(Holder.SERVICE) // never held
        assertEquals(listOf("suspend", "resume"), core.calls)
        awake.release(Holder.FOREGROUND)
        awake.release(Holder.FOREGROUND)
        assertEquals(listOf("suspend", "resume", "suspend"), core.calls)
    }
}
