package dev.skywave.android.core

import dev.skywave.core.Action
import dev.skywave.core.ViewState
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.StateFlow

/**
 * What the UI sees of the core: the latest snapshot and a way to act on it.
 * Nothing here throws or blocks; failures arrive on [messages].
 */
interface CoreClient {
    /** The latest snapshot; starts at the local replica, before any network. */
    val state: StateFlow<ViewState>

    /** One-shot failures (a refused action), each delivered to one collector. */
    val messages: Flow<CoreMessage>

    /** Queues [action]; actions reach the core in the order they were queued. */
    fun dispatch(action: Action)
}
