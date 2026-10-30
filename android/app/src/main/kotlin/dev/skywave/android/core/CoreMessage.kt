package dev.skywave.android.core

import dev.skywave.core.SkywaveException

/**
 * Something the user should read about a failed core call. Carries data
 * only; the UI turns it into text from resources (`ui/Messages.kt`).
 */
sealed interface CoreMessage {
    data class Storage(val reason: String) : CoreMessage

    data class Io(val reason: String) : CoreMessage

    data class InvalidPairUri(val reason: String) : CoreMessage

    data object ShutDown : CoreMessage

    /** The core's native library could not be loaded or linked. */
    data class NativeLibrary(val reason: String) : CoreMessage

    /** Any other failure; [detail] is shown as is. */
    data class Other(val detail: String) : CoreMessage
}

/** What the user is told about [e], a failed core call. */
internal fun describe(e: Throwable): CoreMessage = when (e) {
    is SkywaveException.Store -> CoreMessage.Storage(e.reason)
    is SkywaveException.Io -> CoreMessage.Io(e.reason)
    is SkywaveException.InvalidPairUri -> CoreMessage.InvalidPairUri(e.reason)
    is SkywaveException.ShutDown -> CoreMessage.ShutDown
    is LinkageError -> CoreMessage.NativeLibrary(e.message ?: e::class.simpleName ?: e.toString())
    is SkywaveException -> CoreMessage.Other("${e::class.simpleName}: ${e.message}")
    else -> CoreMessage.Other(e.message ?: e::class.simpleName ?: e.toString())
}
