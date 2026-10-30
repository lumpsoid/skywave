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

    /**
     * Queues [action]; actions reach the core in the order they were queued.
     *
     * Not durable on return: the action sits in process memory until it is
     * handed to the core, which writes it to its store before accepting it.
     * Only then does it survive the process dying. Queued actions are lost if
     * the process is killed first; a refused one is reported on [messages].
     */
    fun dispatch(action: Action)
}
