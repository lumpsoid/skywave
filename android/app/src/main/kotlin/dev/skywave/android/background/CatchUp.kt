package dev.skywave.android.background

import android.content.Context
import androidx.work.Constraints
import androidx.work.CoroutineWorker
import androidx.work.ExistingPeriodicWorkPolicy
import androidx.work.NetworkType
import androidx.work.PeriodicWorkRequestBuilder
import androidx.work.WorkManager
import androidx.work.WorkerParameters
import dev.skywave.android.core.LinkStatus
import dev.skywave.core.Link
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.mapNotNull
import kotlinx.coroutines.withTimeoutOrNull
import java.util.concurrent.TimeUnit
import kotlin.time.Duration
import kotlin.time.Duration.Companion.seconds

/** How one catch-up ended. */
enum class CatchUpResult {
    /** Online after a resume: caught up, and the core alerted for what is new. */
    CAUGHT_UP,

    /** Unpaired or refused: there is nothing to wait for. */
    STOPPED,

    /** Neither within the time allowed (server down, slow network). */
    TIMED_OUT,

    /** The core did not open; nothing ran. */
    NO_CORE,
}

/**
 * Waits for [links] to say the catch-up is over: `Online` and not stale
 * ([LinkStatus]), or a state that will not change by waiting (`Unpaired`,
 * `Rejected`). Gives up after [timeout] rather than sit through backoffs.
 */
internal suspend fun awaitCaughtUp(links: Flow<LinkStatus>, timeout: Duration): CatchUpResult =
    withTimeoutOrNull(timeout) { links.mapNotNull(::settled).first() } ?: CatchUpResult.TIMED_OUT

private fun settled(status: LinkStatus): CatchUpResult? = when (status.link) {
    is Link.Online -> if (status.stale) null else CatchUpResult.CAUGHT_UP
    Link.Unpaired, is Link.Rejected -> CatchUpResult.STOPPED
    Link.Connecting, is Link.CatchingUp, is Link.Reconnecting -> null
}

/**
 * One background catch-up (D21, A15): holds the core awake as [Holder.WORKER],
 * waits for the link to settle, leaves the approval notifier [grace] to post
 * what the catch-up alerted, and lets go. Whether the core then suspends is
 * [awake]'s call: not if the app came to the foreground or the service holds it.
 *
 * @param open the core's link reports, opening the core if this process is
 *   cold (which also starts the notifier); null if it failed to open.
 */
class CatchUp(
    private val awake: CoreAwake,
    private val timeout: Duration = TIMEOUT,
    private val grace: Duration = GRACE,
) {
    suspend fun run(open: () -> Flow<LinkStatus>?): CatchUpResult {
        // Held before opening, so a cold core is never suspended in between.
        awake.hold(Holder.WORKER)
        try {
            val links = open() ?: return CatchUpResult.NO_CORE
            val result = awaitCaughtUp(links, timeout)
            // The alert is in the very snapshot that says Online; the
            // notifier collects it on the main thread.
            if (result == CatchUpResult.CAUGHT_UP) delay(grace)
            return result
        } finally {
            awake.release(Holder.WORKER)
        }
    }

    companion object {
        /** A catch-up is a hello and a few pulls; a link still down after this waits for the next run. */
        val TIMEOUT = 60.seconds

        val GRACE = 2.seconds
    }
}

/**
 * The periodic catch-up WorkManager runs. A timeout is a success too: the
 * next period is the retry, so an unreachable server costs one short run
 * every 15 minutes rather than a backoff chain.
 */
class CatchUpWorker(context: Context, params: WorkerParameters) : CoroutineWorker(context, params) {
    override suspend fun doWork(): Result {
        val host = applicationContext as BackgroundHost
        val result = CatchUp(host.awake).run(host::openLinks)
        return if (result == CatchUpResult.NO_CORE) Result.failure() else Result.success()
    }

    companion object {
        private const val NAME = "catch-up"

        /**
         * Schedules the catch-up every 15 minutes (WorkManager's minimum)
         * while a network is up; keeps an existing schedule, so calling this
         * on every start does not push the next run back.
         */
        fun schedule(context: Context) {
            val request = PeriodicWorkRequestBuilder<CatchUpWorker>(15, TimeUnit.MINUTES)
                .setConstraints(Constraints.Builder().setRequiredNetworkType(NetworkType.CONNECTED).build())
                .build()
            WorkManager.getInstance(context).enqueueUniquePeriodicWork(NAME, ExistingPeriodicWorkPolicy.KEEP, request)
        }
    }
}
