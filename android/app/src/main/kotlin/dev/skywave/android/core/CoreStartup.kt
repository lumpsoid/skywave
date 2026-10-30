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
 * including reading the first snapshot, becomes [CoreStartup.Failed].
 */
internal fun startCore(
    open: () -> SkywaveCoreInterface,
    scope: CoroutineScope,
    dispatchContext: CoroutineContext,
): CoreStartup = try {
    CoreStartup.Ready(CoreRepository(open(), scope, dispatchContext))
} catch (e: Exception) {
    CoreStartup.Failed(describe(e))
}
