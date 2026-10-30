package dev.skywave.android.core

import dev.skywave.android.ui.screens.OpenSearch
import dev.skywave.android.ui.screens.TimelineSearcher
import dev.skywave.core.SearchResult
import dev.skywave.core.SkywaveCoreInterface
import dev.skywave.core.searchRanges

/**
 * The core searches timelines (D2): smart case, every field the TUI looks
 * through, and the heads of downloaded blobs. Only [ranges] runs on the
 * caller's thread; it reads no file.
 */
internal class CoreTimelineSearcher(private val core: SkywaveCoreInterface) : TimelineSearcher {
    override fun open(sessionId: String): OpenSearch {
        val search = core.startSearch(sessionId)
        return object : OpenSearch {
            override fun find(query: String): SearchResult = search.find(query)

            override fun close() = search.close()
        }
    }

    override fun ranges(query: String, text: String): List<IntRange> =
        searchRanges(query, text).map { it.start.toInt() until it.end.toInt() }
}
