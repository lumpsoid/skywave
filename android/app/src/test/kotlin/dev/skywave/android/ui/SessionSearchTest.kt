package dev.skywave.android.ui

import androidx.activity.ComponentActivity
import androidx.activity.compose.BackHandler
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.semantics.SemanticsProperties
import androidx.compose.ui.semantics.getOrNull
import androidx.compose.ui.test.assert
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.assertTextEquals
import androidx.compose.ui.test.hasText
import androidx.compose.ui.test.junit4.v2.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performTextInput
import dev.skywave.android.ui.screens.OpenSearch
import dev.skywave.android.ui.screens.SEARCH_DEBOUNCE_MS
import dev.skywave.android.ui.screens.SearchHits
import dev.skywave.android.ui.screens.SearchTags
import dev.skywave.android.ui.screens.SessionSamples
import dev.skywave.android.ui.screens.SessionScreen
import dev.skywave.android.ui.screens.LocalTimelineSearcher
import dev.skywave.android.ui.screens.TimelineSearcher
import dev.skywave.android.ui.screens.TimelineTags
import dev.skywave.android.ui.screens.copyText
import dev.skywave.android.ui.screens.searchHits
import dev.skywave.android.ui.screens.searchKey
import dev.skywave.android.ui.screens.timelineRows
import dev.skywave.core.Entry
import dev.skywave.core.SearchBlobLine
import dev.skywave.core.SearchHit
import dev.skywave.core.SearchItem
import dev.skywave.core.SearchResult
import dev.skywave.core.TimelineItem
import dev.skywave.core.ToolKind
import dev.skywave.core.ToolOutcome
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import kotlin.coroutines.CoroutineContext
import kotlin.coroutines.EmptyCoroutineContext

/**
 * Searching the session's timeline (Robolectric). The matching is the
 * core's; here a fake searcher stands in for it with a plain smart-case
 * substring match, so the screen's part is what is tested.
 */
@RunWith(RobolectricTestRunner::class)
class SessionSearchTest {
    @get:Rule
    val compose = createAndroidComposeRule<ComponentActivity>()

    private val searcher = FakeSearcher()
    private var backs = 0

    private val timeline: List<TimelineItem> = buildList {
        for (seq in 1..30) {
            add(
                when (seq) {
                    2 -> SessionSamples.message(2, "Find the needle")
                    20 -> SessionSamples.logged(20, Entry.AgentMessage("A needle here"))
                    25 -> SessionSamples.logged(
                        25,
                        SessionSamples.tool(
                            ToolKind.SHELL,
                            "cargo test",
                            outcome = ToolOutcome.Done(false, null, "running", 900u, null, 1200u),
                            name = "Bash",
                        ),
                    )
                    else -> SessionSamples.logged(seq, Entry.AgentMessage("Reply $seq"))
                },
            )
        }
        add(SessionSamples.pending("p1", "needle one"))
        add(SessionSamples.pending("p2", "needle two"))
    }

    private fun show(blobHits: Map<String, ULong> = mapOf("s:25" to 41uL)) {
        searcher.timeline = timeline
        searcher.blobHits = blobHits
        compose.setContent {
            SkywaveTheme(dynamicColor = false) {
                // The shell's back, which the search must come before.
                BackHandler { backs++ }
                CompositionLocalProvider(LocalTimelineSearcher provides searcher) {
                    SessionScreen(SessionSamples.view(timeline = timeline), SessionSamples.noAttention, SessionSamples.noChrome) {}
                }
            }
        }
        compose.waitForIdle()
    }

    private fun tag(t: String) = compose.onNodeWithTag(t)

    private fun search(query: String) {
        tag(SearchTags.OPEN).performClick()
        tag(SearchTags.FIELD).performTextInput(query)
        compose.mainClock.advanceTimeBy(SEARCH_DEBOUNCE_MS + 50)
        compose.waitForIdle()
    }

    private fun back() {
        compose.runOnUiThread { compose.activity.onBackPressedDispatcher.onBackPressed() }
        compose.waitForIdle()
    }

    /** The marked parts of row [key]'s text, with their background. */
    private fun marks(key: String): List<Pair<String, Color>> =
        tag(TimelineTags.item(key)).fetchSemanticsNode().config.getOrNull(SemanticsProperties.Text).orEmpty()
            .flatMap { text ->
                text.spanStyles.filter { it.item.background != Color.Unspecified }
                    .map { text.text.substring(it.start, it.end) to it.item.background }
            }

    @Test
    fun typingSearchesAfterAPause() {
        show()
        tag(SearchTags.OPEN).performClick()
        compose.waitForIdle()
        compose.mainClock.autoAdvance = false
        tag(SearchTags.FIELD).performTextInput("need")
        compose.mainClock.advanceTimeBy(SEARCH_DEBOUNCE_MS / 2)
        tag(SearchTags.FIELD).performTextInput("le")
        compose.mainClock.advanceTimeBy(SEARCH_DEBOUNCE_MS / 2)
        assertEquals(emptyList<String>(), searcher.queries)
        compose.mainClock.advanceTimeBy(SEARCH_DEBOUNCE_MS)
        assertEquals(listOf("needle"), searcher.queries)
    }

    @Test
    fun countsHitsFromWhereTheUserIsAndStepsRoundTheEnds() {
        show()
        search("needle")
        // Opened at the end: the first match on screen is current.
        tag(SearchTags.COUNT).assertTextEquals("3 of 4")
        tag(SearchTags.NEXT).performClick()
        tag(SearchTags.COUNT).assertTextEquals("4 of 4")
        tag(SearchTags.NEXT).performClick()
        tag(SearchTags.COUNT).assertTextEquals("1 of 4")
        tag(SearchTags.PREVIOUS).performClick()
        tag(SearchTags.COUNT).assertTextEquals("4 of 4")
    }

    @Test
    fun aJumpScrollsToTheMatch() {
        show()
        search("needle")
        tag(SearchTags.NEXT).performClick()
        tag(SearchTags.NEXT).performClick()
        compose.waitForIdle()
        tag(TimelineTags.item("m:c2")).assertIsDisplayed()
        tag(SearchTags.NEXT).performClick()
        compose.waitForIdle()
        tag(TimelineTags.item("s:20")).assertIsDisplayed()
    }

    @Test
    fun aMatchInAToolCallOpensItAndSaysWhereInItsContent() {
        show()
        search("cargo")
        tag(SearchTags.COUNT).assertTextEquals("1 of 1")
        tag(TimelineTags.item("s:25")).assertIsDisplayed()
        // Expanded: the output's line count shows only then.
        tag(TimelineTags.item("s:25")).assert(hasText("900 lines in all", substring = true))
        compose.onNodeWithTag(SearchTags.IN_BLOB, useUnmergedTree = true).assertTextEquals("Match in the downloaded content, line 42")
    }

    @Test
    fun matchesAreMarkedTheCurrentOneApart() {
        show()
        search("needle")
        val current = marks("m:p1")
        val other = marks("m:p2")
        assertEquals(listOf("needle"), current.map { it.first })
        assertEquals(listOf("needle"), other.map { it.first })
        assertNotEquals(current.single().second, other.single().second)
        // A row the search did not match is not marked.
        assertTrue(marks("s:30").isEmpty())
    }

    @Test
    fun backClosesTheSearchBeforeLeaving() {
        show()
        search("needle")
        back()
        assertEquals(0, backs)
        assertEquals(0, compose.onAllNodes(hasTestTag(SearchTags.FIELD)).fetchSemanticsNodes().size)
        assertTrue(marks("m:p1").isEmpty())
        back()
        assertEquals(1, backs)
    }

    @Test
    fun closeEndsTheSearch() {
        show()
        search("needle")
        tag(SearchTags.CLOSE).performClick()
        compose.waitForIdle()
        tag(SearchTags.OPEN).assertIsDisplayed()
        assertEquals(1, searcher.closed)
    }

    @Test
    fun saysWhatContentWentUnsearched() {
        searcher.notDownloaded = 2u
        searcher.searched = 1u
        show()
        search("needle")
        tag(SearchTags.HINT).assertTextEquals("1 downloaded searched, 2 not downloaded")
    }

    @Test
    fun noMatchesIsSaid() {
        show()
        search("nothing like it")
        tag(SearchTags.COUNT).assertTextEquals("No matches")
        compose.onNodeWithText("No matches").assertIsDisplayed()
    }

    @Test
    fun hitsAreKeyedLikeRowsAndStaleOnesSkipped() {
        assertEquals("m:c1", searchKey(SearchItem.Pending("c1")))
        assertEquals("m:c1", searchKey(SearchItem.Logged(4u, "c1")))
        assertEquals("s:4", searchKey(SearchItem.Logged(4u, null)))
        val rows = timelineRows(timeline, unreadFrom = 10u)
        // The divider sits before seq 10: row indices count it.
        val hits = searchHits(
            result(
                SearchItem.Logged(20u, null),
                SearchItem.Logged(99u, null), // newer than the rows: not shown yet
                SearchItem.Pending("p1"),
            ),
            rows,
        )
        assertEquals(listOf("s:20", "m:p1"), hits.keys)
        assertEquals(20, hits.rowOf("s:20"))
        assertEquals(-1, hits.rowOf("s:99"))
        assertEquals("s:20", hits.from(0))
        assertEquals("m:p1", hits.from(21))
        assertEquals("m:p1", hits.from(1000))
        assertEquals(SearchHits.None.keys, searchHits(null, rows).keys)
    }

    private fun result(vararg items: SearchItem) =
        SearchResult("q", items.map { SearchHit(it, null) }, 0u, 0u, 0u, 1_048_576u)

    private fun hasTestTag(t: String) = androidx.compose.ui.test.hasTestTag(t)
}

/** Smart-case substring search over each item's copy text: the core's rules, simplified. */
private class FakeSearcher : TimelineSearcher {
    var timeline: List<TimelineItem> = emptyList()
    var blobHits: Map<String, ULong> = emptyMap()
    var notDownloaded: ULong = 0u
    var searched: ULong = 0u
    val queries = mutableListOf<String>()
    var closed = 0

    override val context: CoroutineContext = EmptyCoroutineContext

    override fun open(sessionId: String) = object : OpenSearch {
        override fun find(query: String): SearchResult {
            queries += query
            val hits = timeline.filter { ranges(query, copyText(it).orEmpty()).isNotEmpty() }.map { item ->
                val searchItem = when (item) {
                    is TimelineItem.Pending -> SearchItem.Pending(item.commandId)
                    is TimelineItem.Logged -> SearchItem.Logged(item.seq, (item.entry as? Entry.UserMessage)?.commandId)
                }
                val line = blobHits[searchKey(searchItem)]
                SearchHit(searchItem, line?.let { SearchBlobLine("blob", it) })
            }
            return SearchResult(query, hits, searched, notDownloaded, 0u, 1_048_576u)
        }

        override fun close() {
            closed++
        }
    }

    override fun ranges(query: String, text: String): List<IntRange> {
        if (query.isEmpty()) return emptyList()
        val ignoreCase = query.none { it.isUpperCase() }
        val out = mutableListOf<IntRange>()
        var at = text.indexOf(query, 0, ignoreCase)
        while (at >= 0) {
            out += at until at + query.length
            at = text.indexOf(query, at + query.length, ignoreCase)
        }
        return out
    }
}
