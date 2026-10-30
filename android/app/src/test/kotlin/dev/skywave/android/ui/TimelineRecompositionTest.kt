package dev.skywave.android.ui

import androidx.activity.ComponentActivity
import androidx.compose.runtime.Composer
import androidx.compose.runtime.CompositionTracer
import androidx.compose.runtime.InternalComposeTracingApi
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.test.junit4.v2.createAndroidComposeRule
import dev.skywave.android.ui.screens.SessionSamples
import dev.skywave.android.ui.screens.SessionScreen
import dev.skywave.core.Entry
import dev.skywave.core.SessionState
import dev.skywave.core.TimelineItem
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner

/**
 * A new snapshot recomposes only the timeline items whose data changed: the
 * rest are equal to their old rows and skip. Counted through the Compose
 * runtime's tracer, which hears each composable that runs rather than skips.
 */
@OptIn(InternalComposeTracingApi::class)
@RunWith(RobolectricTestRunner::class)
class TimelineRecompositionTest {
    @get:Rule
    val compose = createAndroidComposeRule<ComponentActivity>()

    private var itemRuns = 0

    private val tracer = object : CompositionTracer {
        override fun isTraceInProgress() = true
        override fun traceEventStart(key: Int, dirty1: Int, dirty2: Int, info: String) {
            if (info.startsWith("dev.skywave.android.ui.screens.TimelineItemView ")) itemRuns++
        }
        override fun traceEventEnd() {}
    }

    @After
    fun dropTracer() = Composer.setTracer(null)

    /** A fresh copy each call: equal items, never the same instances. */
    private fun timeline(reply: String): List<TimelineItem> = listOf(
        SessionSamples.message(1, "first"),
        SessionSamples.logged(2, Entry.AgentMessage(reply)),
        SessionSamples.message(3, "second"),
    )

    @Test
    fun onlyTheChangedItemRecomposesInANewSnapshot() {
        var view by mutableStateOf(SessionSamples.view(timeline("thinking")))
        compose.setContent {
            SkywaveTheme(dynamicColor = false) {
                SessionScreen(view, SessionSamples.noAttention, SessionSamples.noChrome, dispatch = {})
            }
        }
        compose.waitForIdle()
        Composer.setTracer(tracer)

        // The screen recomposes for another part of the view: no item does.
        view = SessionSamples.view(timeline("thinking"), state = SessionState.AgentWorking)
        compose.waitForIdle()
        assertEquals(0, itemRuns)

        // One item's text changes, so the rows are rebuilt from new
        // instances: that item, and only it, recomposes.
        view = SessionSamples.view(timeline("done"), state = SessionState.AgentWorking)
        compose.waitForIdle()
        assertEquals(1, itemRuns)
    }
}
