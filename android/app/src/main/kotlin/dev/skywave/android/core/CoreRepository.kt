package dev.skywave.android.core

import dev.skywave.android.ui.screens.TimelineSearcher
import dev.skywave.core.Action
import dev.skywave.core.SkywaveCoreInterface
import dev.skywave.core.ViewListener
import dev.skywave.core.ViewState
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.channels.BufferOverflow
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.receiveAsFlow
import kotlinx.coroutines.launch
import kotlin.coroutines.CoroutineContext

/**
 * The process's bridge to its one core (D17).
 *
 * Snapshots: the first value is `core.view()`, read synchronously here, so
 * the UI's first frame is the local replica. The listener only sets the
 * conflating [MutableStateFlow]; it runs on a core thread and must not block.
 *
 * Actions: `core.dispatch` writes to SQLite before it returns, so it never
 * runs on the caller's thread. [dispatch] puts the action on an unbounded
 * channel and a single consumer coroutine on [dispatchContext] hands them to
 * the core one at a time: the channel is the one owner of ordering, whatever
 * thread pool [dispatchContext] uses.
 */
class CoreRepository internal constructor(
    private val core: SkywaveCoreInterface,
    scope: CoroutineScope,
    dispatchContext: CoroutineContext,
) : CoreClient {
    private val views = MutableStateFlow(core.view())
    override val state: StateFlow<ViewState> = views.asStateFlow()

    private val errors = Channel<CoreMessage>(capacity = MESSAGE_BUFFER, onBufferOverflow = BufferOverflow.DROP_OLDEST)
    override val messages: Flow<CoreMessage> = errors.receiveAsFlow()

    private val actions = Channel<Action>(Channel.UNLIMITED)

    init {
        // The first snapshot arrives later, on the core's delivery thread;
        // until then the value is view() above, so nothing is lost.
        core.subscribe(
            object : ViewListener {
                override fun onView(view: ViewState) {
                    views.value = view
                }
            },
        )
        scope.launch(dispatchContext) {
            for (action in actions) apply(action)
        }
    }

    override fun dispatch(action: Action) {
        // Unbounded: only fails once the consumer scope is gone, i.e. the
        // process is going away.
        actions.trySend(action)
    }

    /** Searches the open session's timeline through this core. */
    val searcher: TimelineSearcher = CoreTimelineSearcher(core)

    /** App went to the background (D21): pause the network, keep the replica. */
    fun suspend() = core.suspend()

    /** App came back to the foreground (D21). */
    fun resume() = core.resume()

    /**
     * A network became the default: if the link is waiting out a backoff, it
     * tries now instead of when the timer fires. The core ignores
     * [Action.RetryNow] unless it is reconnecting, so no check here (one
     * read from the callback thread could be stale anyway).
     */
    fun onNetworkAvailable() = dispatch(Action.RetryNow)

    private fun apply(action: Action) {
        try {
            core.dispatch(action)
        } catch (e: Exception) {
            // SkywaveException for anything the core refuses; anything else
            // (an FFI-internal error) must not end the consumer either.
            errors.trySend(describe(e))
        }
    }

    private companion object {
        const val MESSAGE_BUFFER = 16
    }
}
