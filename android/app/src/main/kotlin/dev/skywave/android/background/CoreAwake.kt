package dev.skywave.android.background

import dev.skywave.android.core.LinkStatus
import kotlinx.coroutines.flow.StateFlow
import java.util.EnumSet

/**
 * What the background pieces need from the process: `SkywaveApp`, a fake in
 * tests. One per process, like the core.
 */
interface BackgroundHost {
    val awake: CoreAwake

    val stayConnected: StayConnectedPreference

    /**
     * The core's link reports, opening the core if this process is cold
     * (which also starts the approval notifier); null if it failed to open.
     */
    fun openLinks(): StateFlow<LinkStatus>?
}

/** What can keep the core's link up (D21, A15). */
enum class Holder {
    /** The app is visible (`ProcessLifecycleOwner` started). */
    FOREGROUND,

    /** A periodic catch-up is running ([CatchUpWorker]). */
    WORKER,

    /** The user chose to stay connected ([StayConnectedService]). */
    SERVICE,
}

/** The two calls [CoreAwake] makes; `CoreRepository` in the app. */
interface Wakeable {
    fun suspend()

    fun resume()
}

/**
 * The one owner of whether the core's link runs (D21): it runs while any
 * [Holder] holds it and is suspended once none does, so the worker
 * finishing never cuts the link the visible app or the service still needs,
 * and the app going to the background never cuts the worker's catch-up.
 *
 * The core opens running, possibly after holders already came and went
 * (the activity's `ON_START`, the worker): [attach] reconciles once, and
 * until then [hold] and [release] only record. Thread-safe.
 */
class CoreAwake {
    private val holders = EnumSet.noneOf(Holder::class.java)
    private var core: Wakeable? = null

    /** Hands over the opened core; suspends it if nothing holds it. Once per process. */
    @Synchronized
    fun attach(core: Wakeable) {
        check(this.core == null) { "the core is attached once" }
        this.core = core
        if (holders.isEmpty()) core.suspend()
    }

    /** [holder] needs the link; resumes the core if nothing held it. Repeats are no-ops. */
    @Synchronized
    fun hold(holder: Holder) {
        val wasAsleep = holders.isEmpty()
        holders += holder
        if (wasAsleep) core?.resume()
    }

    /** [holder] is done; suspends the core if it was the last. Releasing what is not held is a no-op. */
    @Synchronized
    fun release(holder: Holder) {
        if (holders.remove(holder) && holders.isEmpty()) core?.suspend()
    }
}
