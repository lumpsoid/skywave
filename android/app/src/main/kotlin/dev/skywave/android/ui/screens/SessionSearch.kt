package dev.skywave.android.ui.screens

import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.KeyboardArrowDown
import androidx.compose.material.icons.filled.KeyboardArrowUp
import androidx.compose.material.icons.filled.Search
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextField
import androidx.compose.material3.TextFieldDefaults
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.Immutable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.Stable
import androidx.compose.runtime.compositionLocalOf
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.listSaver
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.runtime.staticCompositionLocalOf
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.pluralStringResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.unit.dp
import dev.skywave.android.R
import dev.skywave.core.SearchHit
import dev.skywave.core.SearchResult
import dev.skywave.core.TimelineItem
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.withContext
import kotlin.coroutines.CoroutineContext

/** A pause in typing this long runs the search; so does a new snapshot. */
internal const val SEARCH_DEBOUNCE_MS = 250L

internal object SearchTags {
    const val OPEN = "search_open"
    const val FIELD = "search_field"
    const val COUNT = "search_count"
    const val PREVIOUS = "search_previous"
    const val NEXT = "search_next"
    const val CLOSE = "search_close"
    const val HINT = "search_hint"
    const val IN_BLOB = "search_in_blob"
}

/**
 * Searches a session's timeline. In the app this is the core
 * (`core/CoreSearch.kt`: what matches is the core's, D2); tests use a fake,
 * as the native core does not load on the JVM.
 */
interface TimelineSearcher {
    /** Starts a search through session [sessionId]; close it when the search ends. */
    fun open(sessionId: String): OpenSearch

    /** Where [query] matches in [text] as rendered, by the same rules: for highlighting. */
    fun ranges(query: String, text: String): List<IntRange>

    /** Where [OpenSearch.find] runs: off the main thread, as it may read files. */
    val context: CoroutineContext get() = Dispatchers.IO
}

/** One search through one session; holds what it read until closed. */
interface OpenSearch : AutoCloseable {
    /** The items [query] matches in the core's latest snapshot. Blocking. */
    fun find(query: String): SearchResult
}

/** The searcher the session screen uses. Provided by the Activity; without one there is no search. */
val LocalTimelineSearcher = staticCompositionLocalOf<TimelineSearcher?> { null }

/**
 * The session screen's search: presentation state only, the matching is
 * the core's. The current match is kept by its row key, so it stays put as
 * items arrive.
 */
@Stable
internal class SearchState(private val searcher: TimelineSearcher, active: Boolean, query: String, current: String?) {
    var active by mutableStateOf(active)
        private set

    /** The query as typed. */
    var query by mutableStateOf(query)

    /** The last search's result; its query may lag behind [query] while typing. */
    var result by mutableStateOf<SearchResult?>(null)
        private set

    /** The current match's row key. */
    var current by mutableStateOf(current)
        private set

    /** Bumped for each jump to [current]: the timeline scrolls to it. */
    var jumps by mutableIntStateOf(0)
        private set

    /** The next result is a new query's: its first match is picked and jumped to. */
    private var picking = current == null
    private var search: OpenSearch? = null
    private var searchedFor: String? = query.takeIf { current != null }

    fun open() {
        active = true
    }

    fun close() {
        active = false
        query = ""
        result = null
        current = null
        searchedFor = null
        closeSearch()
    }

    internal fun closeSearch() {
        search?.close()
        search = null
    }

    /** Runs the search for [query] in session [sessionId] off the main thread. */
    suspend fun run(sessionId: String) {
        val q = query
        if (!active || q.isEmpty()) {
            result = null
            current = null
            searchedFor = null
            return
        }
        val found = try {
            val open = search ?: searcher.open(sessionId).also { search = it }
            withContext(searcher.context) { open.find(q) }
        } catch (e: CancellationException) {
            throw e
        } catch (e: Exception) {
            // The core shut down, say: nothing found, nothing to show.
            null
        }
        if (q != searchedFor) {
            picking = true
            current = null
            searchedFor = q
        }
        result = found
    }

    /**
     * Settles the current match among [hits] (the rows on screen): a new
     * query's first match at or below row [from] (where the user is), with
     * a jump; a match no longer there gives way to the next one from there.
     */
    fun settle(hits: SearchHits, from: Int) {
        val cur = current
        when {
            hits.keys.isEmpty() -> current = null
            picking -> {
                current = hits.from(from)
                picking = false
                jumps++
            }
            cur == null || hits.rowOf(cur) < 0 -> current = hits.from(from)
        }
    }

    /** Moves to the next ([forward]) or previous match, round the ends. */
    fun step(hits: SearchHits, forward: Boolean) {
        val keys = hits.keys
        if (keys.isEmpty()) return
        val at = keys.indexOf(current)
        current = when {
            at < 0 -> if (forward) keys.first() else keys.last()
            forward -> keys[(at + 1) % keys.size]
            else -> keys[(at - 1 + keys.size) % keys.size]
        }
        jumps++
    }

    /** How a row shows matches of the last search, current or not. */
    fun highlight(current: Boolean, style: SpanStyle): Highlight? {
        val q = result?.query?.takeIf { it.isNotEmpty() } ?: return null
        return Highlight(q, current, style) { searcher.ranges(q, it) }
    }

    companion object {
        fun saver(searcher: TimelineSearcher) = listSaver<SearchState, Any?>(
            save = { listOf(it.active, it.query, it.current) },
            restore = { SearchState(searcher, it[0] as Boolean, it[1] as String, it[2] as String?) },
        )
    }
}

/**
 * The search of session [sessionId], or null when no searcher is provided.
 * Searches again after a pause in typing and on each new [timeline].
 */
@Composable
internal fun rememberSearch(sessionId: String, timeline: List<TimelineItem>): SearchState? {
    val searcher = LocalTimelineSearcher.current ?: return null
    val state = rememberSaveable(searcher, saver = SearchState.saver(searcher)) {
        SearchState(searcher, active = false, query = "", current = null)
    }
    DisposableEffect(state) { onDispose { state.closeSearch() } }
    LaunchedEffect(state, state.active, state.query, timeline) {
        if (state.active && state.query.isNotEmpty()) delay(SEARCH_DEBOUNCE_MS)
        state.run(sessionId)
    }
    return state
}

/** How matches show in one timeline row: [ranges] by the core's rules, in [style]. */
@Immutable
internal class Highlight(
    private val query: String,
    private val current: Boolean,
    private val style: SpanStyle,
    private val ranges: (String) -> List<IntRange>,
) {
    /** [text] with its matches marked. */
    fun over(text: AnnotatedString): AnnotatedString {
        val found = ranges(text.text)
        if (found.isEmpty()) return text
        return AnnotatedString.Builder(text).apply {
            for (r in found) mark(r, 0, text.length)
        }.toAnnotatedString()
    }

    /**
     * [text] with the matches inside [parts] (substrings of it, in order)
     * marked: a label's own words ("Call", "Approval for") are not what a
     * search looks through.
     */
    fun overParts(text: String, parts: List<String>): AnnotatedString {
        val builder = AnnotatedString.Builder(text)
        var from = 0
        for (part in parts) {
            if (part.isEmpty()) continue
            val at = text.indexOf(part, from)
            if (at < 0) continue
            for (r in ranges(part)) builder.mark(r, at, text.length)
            from = at + part.length
        }
        return builder.toAnnotatedString()
    }

    private fun AnnotatedString.Builder.mark(range: IntRange, offset: Int, length: Int) {
        val start = (range.first + offset).coerceIn(0, length)
        val end = (range.last + 1 + offset).coerceIn(start, length)
        if (end > start) addStyle(style, start, end)
    }

    override fun equals(other: Any?) =
        other is Highlight && other.query == query && other.current == current && other.style == style

    override fun hashCode() = (query.hashCode() * 31 + current.hashCode()) * 31 + style.hashCode()
}

/** The highlight of the row being composed; null outside a matching row. */
internal val LocalHighlight = compositionLocalOf<Highlight?> { null }

/** [text] with the row's matches marked. */
@Composable
internal fun marked(text: String): AnnotatedString = marked(AnnotatedString(text))

@Composable
internal fun marked(text: AnnotatedString): AnnotatedString {
    val h = LocalHighlight.current
    return remember(text, h) { h?.over(text) ?: text }
}

/** [text] with the row's matches inside [parts] marked. */
@Composable
internal fun markedParts(text: String, vararg parts: String?): AnnotatedString {
    val h = LocalHighlight.current
    return remember(text, h, parts.toList()) { h?.overParts(text, parts.filterNotNull()) ?: AnnotatedString(text) }
}

/** The span a match shows in: the current one stronger than the rest. */
@Composable
internal fun matchStyle(current: Boolean): SpanStyle {
    val colors = MaterialTheme.colorScheme
    return if (current) {
        SpanStyle(background = colors.tertiary, color = colors.onTertiary)
    } else {
        SpanStyle(background = colors.tertiaryContainer, color = colors.onTertiaryContainer)
    }
}

/** The top bar's search action. */
@Composable
internal fun SearchButton(onClick: () -> Unit) {
    IconButton(onClick = onClick, modifier = Modifier.testTag(SearchTags.OPEN)) {
        Icon(Icons.Filled.Search, stringResource(R.string.search_open))
    }
}

/** The query field, in the title's place while searching. */
@Composable
internal fun SearchField(search: SearchState, onSearch: () -> Unit) {
    val focus = remember { FocusRequester() }
    LaunchedEffect(Unit) { focus.requestFocus() }
    TextField(
        value = search.query,
        onValueChange = { search.query = it },
        placeholder = { Text(stringResource(R.string.search_hint)) },
        singleLine = true,
        keyboardOptions = KeyboardOptions(imeAction = ImeAction.Search),
        keyboardActions = KeyboardActions(onSearch = { onSearch() }),
        colors = TextFieldDefaults.colors(
            focusedContainerColor = Color.Transparent,
            unfocusedContainerColor = Color.Transparent,
        ),
        modifier = Modifier.fillMaxWidth().focusRequester(focus).testTag(SearchTags.FIELD),
    )
}

/** Where the search stands, previous and next match, and close. */
@Composable
internal fun SearchActions(search: SearchState, hits: SearchHits) {
    val result = search.result
    val count = when {
        result == null || search.query.isEmpty() -> ""
        hits.keys.isEmpty() -> stringResource(R.string.search_no_matches)
        else -> stringResource(R.string.search_position, hits.keys.indexOf(search.current) + 1, hits.keys.size)
    }
    Row(verticalAlignment = Alignment.CenterVertically) {
        Text(
            count,
            style = MaterialTheme.typography.labelMedium,
            maxLines = 1,
            modifier = Modifier.testTag(SearchTags.COUNT),
        )
        val any = hits.keys.isNotEmpty()
        IconButton(onClick = { search.step(hits, forward = false) }, enabled = any, modifier = Modifier.testTag(SearchTags.PREVIOUS)) {
            Icon(Icons.Filled.KeyboardArrowUp, stringResource(R.string.search_previous))
        }
        IconButton(onClick = { search.step(hits, forward = true) }, enabled = any, modifier = Modifier.testTag(SearchTags.NEXT)) {
            Icon(Icons.Filled.KeyboardArrowDown, stringResource(R.string.search_next))
        }
        IconButton(onClick = search::close, modifier = Modifier.testTag(SearchTags.CLOSE)) {
            Icon(Icons.Filled.Close, stringResource(R.string.search_close))
        }
    }
}

/** What the search could not look through: blobs not downloaded, heads only. */
@Composable
internal fun SearchHint(result: SearchResult?) {
    result ?: return
    val notDownloaded = result.notDownloaded.toInt()
    val cut = result.cut.toInt()
    if (notDownloaded == 0 && cut == 0) return
    val parts = buildList {
        if (notDownloaded > 0) {
            add(pluralStringResource(R.plurals.search_not_downloaded, notDownloaded, notDownloaded, result.searchedBlobs.toInt()))
        }
        if (cut > 0) add(pluralStringResource(R.plurals.search_cut, cut, cut, formatBytes(result.headBytes)))
    }
    Surface(tonalElevation = 2.dp, modifier = Modifier.fillMaxWidth()) {
        Text(
            parts.joinToString(stringResource(R.string.separator)),
            style = MaterialTheme.typography.labelMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            modifier = Modifier.padding(horizontal = 16.dp, vertical = 4.dp).testTag(SearchTags.HINT),
        )
    }
}

/** How a matching row shows the search. */
@Immutable
internal data class RowSearch(
    val highlight: Highlight?,
    /** The row holds the current match. */
    val current: Boolean,
    /** [SearchState.jumps] when composed: a new value is a new jump. */
    val jump: Int,
    /** The current match's line in the row's downloaded content, 0 the first. */
    val blobLine: ULong?,
)

@Composable
internal fun rowSearch(search: SearchState, key: String, hit: SearchHit): RowSearch {
    val current = key == search.current
    val style = matchStyle(current)
    val highlight = remember(search.result, current, style) { search.highlight(current, style) }
    return RowSearch(highlight, current, search.jumps, hit.blob?.line)
}
