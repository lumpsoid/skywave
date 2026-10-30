package dev.skywave.android.core

import dev.skywave.core.SkywaveCoreInterface
import kotlinx.coroutines.CoroutineScope
import kotlin.coroutines.CoroutineContext

/** The outcome of opening the core: the UI renders one or the other. */
sealed interface CoreStartup {
    data class Ready(val repository: CoreRepository) : CoreStartup

    /** The core could not open (e.g. an unreadable store); nothing else works. */
    data class Failed(val reason: CoreMessage) : CoreStartup
}

/**
 * Opens the core with [open] and wraps it in a [CoreRepository]. Any failure,
 * including reading the first snapshot, becomes [CoreStartup.Failed]; so does
 * a native library that will not load ([LinkageError]), since [open] is where
 * the first native call happens. A core that opened but could not be wrapped
 * is shut down, so its engine does not run on with no one to drive it.
 */
internal fun startCore(
    open: () -> SkywaveCoreInterface,
    scope: CoroutineScope,
    dispatchContext: CoroutineContext,
): CoreStartup {
    val core = try {
        open()
    } catch (e: Exception) {
        return CoreStartup.Failed(describe(e))
    } catch (e: LinkageError) {
        return CoreStartup.Failed(describe(e))
    }
    return try {
        CoreStartup.Ready(CoreRepository(core, scope, dispatchContext))
    } catch (e: Exception) {
        // Best effort: the reason it failed to wrap is what the user sees.
        runCatching { core.shutdown() }
        CoreStartup.Failed(describe(e))
    }
}
