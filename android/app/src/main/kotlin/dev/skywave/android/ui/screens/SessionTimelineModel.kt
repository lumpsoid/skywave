package dev.skywave.android.ui.screens

import androidx.compose.runtime.Immutable
import dev.skywave.core.AgentSetting
import dev.skywave.core.Answer
import dev.skywave.core.ApprovalPrompt
import dev.skywave.core.BlobView
import dev.skywave.core.BodyFormat
import dev.skywave.core.Entry
import dev.skywave.core.SearchHit
import dev.skywave.core.SearchItem
import dev.skywave.core.SearchResult
import dev.skywave.core.TimelineItem
import dev.skywave.core.ToolKind
import dev.skywave.core.ToolOutcome
import dev.skywave.core.ToolView
import dev.skywave.core.TurnUsage
import java.util.Locale
import kotlinx.collections.immutable.ImmutableList
import kotlinx.collections.immutable.toImmutableList

/**
 * The session timeline as LazyColumn rows: every item under a stable key,
 * plus the "new" divider. Pure, so the keys and the divider's place are
 * tested without a UI.
 *
 * Value-equal and immutable, so an item whose data is unchanged in a new
 * snapshot is equal to its old row and its composable skips.
 */
@Immutable
internal sealed interface TimelineRow {
    val key: String

    data class Item(val item: TimelineItem, override val key: String) : TimelineRow

    data object Divider : TimelineRow {
        override val key = "divider"
    }
}

/**
 * The LazyColumn key of [item]. A message is keyed by its send command in
 * both forms, so a pending message that becomes logged keeps its key and
 * the list does not jump; every other logged item by its seq.
 */
internal fun timelineKey(item: TimelineItem): String = when (item) {
    is TimelineItem.Pending -> "m:${item.commandId}"
    is TimelineItem.Logged -> when (val entry = item.entry) {
        is Entry.UserMessage -> "m:${entry.commandId}"
        else -> "s:${item.seq}"
    }
}

/**
 * The row key of the item a search hit names: [timelineKey]'s rule, from
 * the ids the core reports instead of the item itself.
 */
internal fun searchKey(item: SearchItem): String = when (item) {
    is SearchItem.Pending -> "m:${item.commandId}"
    is SearchItem.Logged -> item.commandId?.let { "m:$it" } ?: "s:${item.seq}"
}

/**
 * A search's hits among the rows on screen, oldest first. The core searched
 * its own latest snapshot, which may be a little newer than [rows]: a hit
 * on an item not shown yet is left out, and the next search (each new
 * snapshot runs one) brings it in.
 *
 * Immutable: built once by [searchHits], its collections never changed.
 */
@Immutable
internal class SearchHits(val keys: List<String>, private val hits: Map<String, SearchHit>, private val rows: Map<String, Int>) {
    operator fun get(key: String): SearchHit? = hits[key]

    /** The row index of the hit [key]; -1 when it is not one. */
    fun rowOf(key: String): Int = rows[key] ?: -1

    /** The first hit at or below row [from], else the last one; null with no hits. */
    fun from(from: Int): String? = keys.firstOrNull { rowOf(it) >= from } ?: keys.lastOrNull()

    companion object {
        val None = SearchHits(emptyList(), emptyMap(), emptyMap())
    }
}

internal fun searchHits(result: SearchResult?, rows: List<TimelineRow>): SearchHits {
    if (result == null || result.hits.isEmpty()) return SearchHits.None
    val index = HashMap<String, Int>(rows.size * 2)
    rows.forEachIndexed { i, row -> if (row is TimelineRow.Item) index[row.key] = i }
    val hits = LinkedHashMap<String, SearchHit>()
    for (hit in result.hits) {
        val key = searchKey(hit.item)
        if (key in index) hits.putIfAbsent(key, hit)
    }
    return SearchHits(hits.keys.toList(), hits, index.filterKeys { it in hits })
}

/**
 * [timeline] as rows, the divider before the first logged item at or after
 * [unreadFrom]. A key seen twice (which LazyColumn would refuse) falls back
 * to a positional one, so a malformed snapshot still renders.
 */
internal fun timelineRows(timeline: List<TimelineItem>, unreadFrom: ULong?): ImmutableList<TimelineRow> {
    val rows = ArrayList<TimelineRow>(timeline.size + 1)
    val seen = HashSet<String>(timeline.size * 2)
    var divided = unreadFrom == null
    timeline.forEachIndexed { index, item ->
        if (!divided && item is TimelineItem.Logged && item.seq >= unreadFrom!!) {
            rows += TimelineRow.Divider
            divided = true
        }
        val key = timelineKey(item).takeIf { seen.add(it) } ?: "i:$index"
        rows += TimelineRow.Item(item, key)
    }
    return rows.toImmutableList()
}

/**
 * Whether the timeline [item] is the one [prompt] gates: its tool call
 * (spec §7: `pending_approval.call` names it), else, for an approval with
 * no logged call, the standalone approval entry.
 */
internal fun gates(prompt: ApprovalPrompt, item: TimelineItem): Boolean {
    val entry = (item as? TimelineItem.Logged)?.entry ?: return false
    return when (entry) {
        is Entry.Tool -> prompt.call != null && entry.tool.call == prompt.call
        is Entry.Approval -> prompt.call == null && entry.id == prompt.id
        else -> false
    }
}

/** The key of the row [prompt] gates, or null when it is not in [rows]. */
internal fun gatedRowKey(rows: List<TimelineRow>, prompt: ApprovalPrompt?): String? {
    prompt ?: return null
    return rows.firstOrNull { it is TimelineRow.Item && gates(prompt, it.item) }?.key
}

/** The tool call [prompt] gates, if it is a logged one. */
internal fun gatedTool(timeline: List<TimelineItem>, prompt: ApprovalPrompt): ToolView? =
    timeline.firstOrNull { gates(prompt, it) }
        ?.let { ((it as TimelineItem.Logged).entry as? Entry.Tool)?.tool }

/** What an approval asks to do, as the panel shows it. */
@Immutable
internal sealed interface ApprovalBody {
    /** A unified diff, e.g. an edit. */
    data class Diff(val text: String, val lines: UInt, val full: BlobView?) : ApprovalBody

    /** A file's content, or any other text the call carries. */
    data class Content(val text: String, val lines: UInt, val full: BlobView?) : ApprovalBody

    /** A shell command, whole. */
    data class Command(val text: String) : ApprovalBody

    /** Nothing more than the title says. */
    data object None : ApprovalBody
}

/**
 * The body of the panel for [tool] (null for an approval with no logged
 * call, whose [prompt] summary is all there is). The server puts a command
 * too long for the title in the body (D4), so a shell's body wins over its
 * title; a shell's title carries a "$ " the command does not.
 */
internal fun approvalBody(prompt: ApprovalPrompt, tool: ToolView?): ApprovalBody {
    val body = tool?.body
    return when {
        tool == null -> prompt.summary.takeIf { it.isNotBlank() }?.let { ApprovalBody.Content(it, it.lineCount(), null) }
            ?: ApprovalBody.None
        body != null && body.format == BodyFormat.DIFF -> ApprovalBody.Diff(body.preview, body.lines, body.full)
        body != null && tool.kind == ToolKind.SHELL && body.full == null -> ApprovalBody.Command(body.preview)
        body != null -> ApprovalBody.Content(body.preview, body.lines, body.full)
        tool.kind == ToolKind.SHELL -> ApprovalBody.Command(tool.title.removePrefix("$ "))
        else -> ApprovalBody.None
    }
}

private fun String.lineCount(): UInt = (count { it == '\n' } + 1).toUInt()

/**
 * Whether this device has had its say: an [answer] given, and not refused
 * in a way that leaves the approval to answer again (the core's
 * `retryable`). One refused because the approval closed meanwhile is final.
 */
internal fun answered(answer: Answer?): Boolean = answer != null && !answer.retryable

/** A piece of agent text: prose, or a fenced code block. */
internal sealed interface TextBlock {
    data class Prose(val text: String) : TextBlock

    data class Code(val text: String) : TextBlock
}

/**
 * Splits agent text at ``` fences (markdown-lite): code blocks become
 * [TextBlock.Code] without the fence lines and their language tag; an
 * unclosed fence runs to the end, as it does while the agent is writing.
 */
internal fun textBlocks(text: String): List<TextBlock> {
    val blocks = mutableListOf<TextBlock>()
    val prose = StringBuilder()
    val code = StringBuilder()
    var inCode = false
    fun flushProse() {
        val t = prose.toString().trim('\n')
        if (t.isNotEmpty()) blocks += TextBlock.Prose(t)
        prose.clear()
    }
    for (line in text.split('\n')) {
        if (line.trimStart().startsWith("```")) {
            if (inCode) {
                blocks += TextBlock.Code(code.toString().removeSuffix("\n"))
                code.clear()
            } else {
                flushProse()
            }
            inCode = !inCode
            continue
        }
        (if (inCode) code else prose).append(line).append('\n')
    }
    if (inCode) blocks += TextBlock.Code(code.toString().removeSuffix("\n"))
    flushProse()
    return blocks
}

/**
 * The spans of [text] between single backticks, as (start, end) of the
 * code without its ticks. An unpaired tick is left as text.
 */
internal fun inlineCodeSpans(text: String): List<IntRange> {
    val spans = mutableListOf<IntRange>()
    var open = -1
    text.forEachIndexed { i, c ->
        if (c == '`') {
            if (open < 0) {
                open = i
            } else {
                if (i > open + 1) spans += (open + 1) until i
                open = -1
            }
        } else if (c == '\n') {
            open = -1
        }
    }
    return spans
}

/** A byte count for people: "512 B", "1.2 kB", "3.4 MB". */
internal fun formatBytes(bytes: ULong): String {
    val b = bytes.toDouble()
    return when {
        b < 1000 -> "$bytes B"
        b < 1_000_000 -> String.format(Locale.ROOT, "%.1f kB", b / 1e3)
        b < 1_000_000_000 -> String.format(Locale.ROOT, "%.1f MB", b / 1e6)
        else -> String.format(Locale.ROOT, "%.1f GB", b / 1e9)
    }
}

/** A duration: tenths below ten seconds, else "12s", "2m05s", "1h02m". */
internal fun formatDuration(ms: ULong): String {
    val s = ms.toLong() / 1000
    return when {
        ms < 10_000u -> String.format(Locale.ROOT, "%.1fs", ms.toDouble() / 1000)
        s < 60 -> "${s}s"
        s < 3600 -> String.format(Locale.ROOT, "%dm%02ds", s / 60, s % 60)
        else -> String.format(Locale.ROOT, "%dh%02dm", s / 3600, (s % 3600) / 60)
    }
}

/** A token count: "950", "12.3k", "1.2M". */
internal fun formatTokens(n: ULong): String {
    val d = n.toDouble()
    return when {
        d < 1000 -> n.toString()
        d < 1_000_000 -> String.format(Locale.ROOT, "%.1fk", d / 1e3)
        else -> String.format(Locale.ROOT, "%.1fM", d / 1e6)
    }
}

/** One turn's usage on one line, figures the agent did not report left out. */
internal fun usageLine(usage: TurnUsage, inLabel: String, outLabel: String): String = buildList {
    usage.tokensIn?.let { add("${formatTokens(it)} $inLabel") }
    usage.tokensOut?.let { add("${formatTokens(it)} $outLabel") }
    usage.costUsd?.let { add(String.format(Locale.ROOT, "$%.4f", it)) }
    usage.durationMs?.let { add(formatDuration(it)) }
    usage.model?.let { add(it) }
}.joinToString(" · ")

/** What copying [item] puts on the clipboard: its text, as shown. */
internal fun copyText(item: TimelineItem): String? = when (item) {
    is TimelineItem.Pending -> item.text
    is TimelineItem.Logged -> when (val e = item.entry) {
        is Entry.UserMessage -> e.text
        is Entry.AgentMessage -> e.text
        is Entry.Tool -> buildList {
            add(e.tool.title)
            e.tool.detail?.let(::add)
            e.tool.body?.preview?.takeIf { it.isNotEmpty() }?.let(::add)
            (e.tool.outcome as? ToolOutcome.Done)?.preview?.takeIf { it.isNotEmpty() }?.let(::add)
        }.joinToString("\n")
        is Entry.ToolCall -> "${e.name}: ${e.summary}"
        is Entry.ToolResult -> e.summary
        is Entry.Approval -> "${e.tool}: ${e.summary}"
        is Entry.AgentErrored -> e.message
        is Entry.TurnUsage -> usageLine(e.usage, "in", "out")
        is Entry.Renamed -> e.name
        // The text in it that came from the agent, as search reads it.
        is Entry.AgentSetting -> listOfNotNull((e.setting as? AgentSetting.Model)?.model, e.refused)
            .joinToString(": ")
            .ifEmpty { null }
        Entry.Interrupted, Entry.Stopped, is Entry.Archived -> null
    }
}
