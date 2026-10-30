package dev.skywave.android.ui.screens

import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.pluralStringResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import dev.skywave.android.R
import dev.skywave.core.ApprovalStatus
import dev.skywave.core.BlobState
import dev.skywave.core.BlobView
import dev.skywave.core.BodyFormat
import dev.skywave.core.Decision
import dev.skywave.core.Delivery
import dev.skywave.core.Entry
import dev.skywave.core.Outgoing
import dev.skywave.core.TimelineItem
import dev.skywave.core.ToolKind
import dev.skywave.core.ToolOutcome
import dev.skywave.core.ToolView
import java.text.DateFormat
import java.util.Date

/** Lines of a tool body or output shown before the item is expanded. */
private const val PREVIEW_LINES = 8

internal object TimelineTags {
    const val LIST = "timeline"
    const val JUMP = "timeline_jump"
    const val DIVIDER = "timeline_divider"
    fun item(key: String) = "item:$key"
}

/** What a timeline item can ask of the screen. */
internal class ItemActions(
    val copy: (String) -> Unit,
    val discard: (String) -> Unit,
    val edit: (String) -> Unit,
)

@Composable
internal fun NewDivider() {
    Row(
        Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 6.dp).testTag(TimelineTags.DIVIDER),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        val color = MaterialTheme.colorScheme.primary
        HorizontalDivider(Modifier.weight(1f), color = color)
        Text(
            stringResource(R.string.sv_new_divider),
            color = color,
            style = MaterialTheme.typography.labelMedium,
            modifier = Modifier.padding(horizontal = 8.dp),
        )
        HorizontalDivider(Modifier.weight(1f), color = color)
    }
}

/**
 * One timeline item. Long-press opens its menu (copy; edit and discard for
 * a message that can still be taken back); tap expands what has details.
 */
@OptIn(ExperimentalFoundationApi::class)
@Composable
internal fun TimelineItemView(row: TimelineRow.Item, actions: ItemActions, gated: GatedApproval? = null) {
    val item = row.item
    var expanded by rememberSaveable(row.key) { mutableStateOf(false) }
    var menu by remember { mutableStateOf(false) }
    val text = remember(item) { copyText(item) }
    val discardable = (item as? TimelineItem.Pending)?.takeIf { it.discardable }
    val expandable = when (item) {
        is TimelineItem.Pending -> true
        is TimelineItem.Logged -> item.entry.let { it is Entry.Tool || (it is Entry.UserMessage && it.delivery != null) }
    }
    Box(
        Modifier
            .fillMaxWidth()
            .testTag(TimelineTags.item(row.key))
            .combinedClickable(
                onClickLabel = when {
                    !expandable -> null
                    expanded -> stringResource(R.string.sv_tool_collapse)
                    item is TimelineItem.Logged && item.entry is Entry.Tool -> stringResource(R.string.sv_tool_expand)
                    else -> stringResource(R.string.sv_message_details)
                },
                onLongClickLabel = stringResource(R.string.sv_item_actions),
                onClick = { if (expandable) expanded = !expanded },
                onLongClick = { if (text != null || discardable != null) menu = true },
            )
            .padding(horizontal = 12.dp, vertical = 4.dp),
    ) {
        when (item) {
            is TimelineItem.Pending -> MessageBubble(
                text = item.text,
                own = true,
                outgoing = item.outgoing,
                delivery = item.delivery,
                expanded = expanded,
                discardable = item.discardable,
                onDiscard = { actions.discard(item.commandId) },
                onEdit = { actions.edit(item.commandId) },
            )
            is TimelineItem.Logged -> LoggedEntry(item, expanded, gated)
        }
        DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
            if (text != null) {
                DropdownMenuItem(
                    text = { Text(stringResource(R.string.sv_copy)) },
                    onClick = {
                        menu = false
                        actions.copy(text)
                    },
                )
            }
            if (discardable != null) {
                DropdownMenuItem(
                    text = { Text(stringResource(R.string.sv_edit)) },
                    onClick = {
                        menu = false
                        actions.edit(discardable.commandId)
                    },
                )
                DropdownMenuItem(
                    text = { Text(stringResource(R.string.sv_discard)) },
                    onClick = {
                        menu = false
                        actions.discard(discardable.commandId)
                    },
                )
            }
        }
    }
}

@Composable
private fun LoggedEntry(item: TimelineItem.Logged, expanded: Boolean, gated: GatedApproval?) {
    when (val e = item.entry) {
        is Entry.UserMessage -> MessageBubble(
            text = e.text,
            own = e.own,
            outgoing = e.outgoing,
            delivery = e.delivery,
            expanded = expanded,
            discardable = false,
            onDiscard = {},
            onEdit = {},
        )
        is Entry.AgentMessage -> AgentText(e.text)
        is Entry.Tool -> ToolItem(e.tool, expanded, gated)
        is Entry.ToolCall -> Column {
            Label(stringResource(R.string.sv_tool_call, e.name, e.summary))
            e.input?.let { Minor(stringResource(R.string.sv_tool_input, blobText(it))) }
        }
        is Entry.ToolResult -> Column {
            Label(stringResource(R.string.sv_tool_result, e.summary))
            e.output?.let { Minor(stringResource(R.string.sv_tool_output, blobText(it))) }
        }
        is Entry.Approval -> Column {
            Label(stringResource(R.string.sv_approval, e.tool, e.summary))
            ApprovalLine(e.status, gated)
        }
        Entry.Interrupted -> SystemLine(stringResource(R.string.sv_interrupted))
        Entry.Stopped -> SystemLine(stringResource(R.string.sv_stopped))
        is Entry.Renamed -> SystemLine(
            listOf(
                e.name?.let { stringResource(R.string.sv_renamed, it) } ?: stringResource(R.string.sv_renamed_default),
                byText(e.own),
            ).joinToString(" "),
        )
        is Entry.Archived -> SystemLine(
            listOf(
                stringResource(if (e.archived) R.string.sv_archived else R.string.sv_unarchived),
                byText(e.own),
            ).joinToString(" "),
        )
        is Entry.AgentErrored -> Surface(
            color = MaterialTheme.colorScheme.errorContainer,
            shape = RoundedCornerShape(8.dp),
            modifier = Modifier.fillMaxWidth(),
        ) {
            Column(Modifier.padding(10.dp)) {
                Text(
                    stringResource(R.string.sv_agent_error),
                    style = MaterialTheme.typography.labelMedium,
                    color = MaterialTheme.colorScheme.onErrorContainer,
                )
                Text(e.message, color = MaterialTheme.colorScheme.onErrorContainer)
            }
        }
        is Entry.TurnUsage -> {
            val line = usageLine(e.usage, stringResource(R.string.sv_tokens_in), stringResource(R.string.sv_tokens_out))
            if (line.isNotEmpty()) SystemLine(line)
        }
    }
}

@Composable
private fun byText(own: Boolean) = stringResource(if (own) R.string.sv_by_you else R.string.sv_by_other)

/** A message sent into the session; this device's on the right with its delivery state. */
@Composable
private fun MessageBubble(
    text: String,
    own: Boolean,
    outgoing: Outgoing?,
    delivery: Delivery?,
    expanded: Boolean,
    discardable: Boolean,
    onDiscard: () -> Unit,
    onEdit: () -> Unit,
) {
    val colors = MaterialTheme.colorScheme
    Column(
        Modifier.fillMaxWidth(),
        horizontalAlignment = if (own) Alignment.End else Alignment.Start,
    ) {
        Text(
            stringResource(if (own) R.string.sv_you else R.string.sv_other_device),
            style = MaterialTheme.typography.labelSmall,
            color = colors.onSurfaceVariant,
        )
        Surface(
            color = if (own) colors.primaryContainer else colors.secondaryContainer,
            shape = RoundedCornerShape(12.dp),
            modifier = Modifier.widthIn(max = 560.dp),
        ) {
            Text(text, modifier = Modifier.padding(horizontal = 12.dp, vertical = 8.dp))
        }
        // Accepted is the normal end: said only when asked for details.
        if (outgoing != null && (outgoing !is Outgoing.Accepted || expanded)) {
            Minor(outgoingText(outgoing), color = if (outgoing is Outgoing.Failed) colors.error else null)
        }
        if (expanded && delivery != null) Minor(deliveryText(delivery))
        if (expanded && discardable) {
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                OutlinedButton(onClick = onEdit) { Text(stringResource(R.string.sv_edit)) }
                OutlinedButton(onClick = onDiscard) { Text(stringResource(R.string.sv_discard)) }
            }
        }
    }
}

@Composable
private fun outgoingText(outgoing: Outgoing): String = when (outgoing) {
    Outgoing.Queued -> stringResource(R.string.sv_out_queued)
    Outgoing.Sending -> stringResource(R.string.sv_out_sending)
    is Outgoing.Accepted -> stringResource(R.string.sv_out_accepted)
    is Outgoing.Failed -> stringResource(R.string.sv_out_failed, failText(outgoing.reason))
}

/** "queued 14:02:11 · sent +3.2s · 2 tries · answered +8.1s", by this device's clock. */
@Composable
private fun deliveryText(d: Delivery): String {
    fun after(ms: Long) = formatDuration((ms - d.queuedAtMs).coerceAtLeast(0).toULong())
    return buildList {
        add(stringResource(R.string.sv_delivery_queued_at, clockTime(d.queuedAtMs)))
        when {
            d.firstSentAtMs != null -> add(stringResource(R.string.sv_delivery_sent_after, after(d.firstSentAtMs!!)))
            d.tries > 0u -> add(stringResource(R.string.sv_delivery_sent))
            else -> add(stringResource(R.string.sv_delivery_not_sent))
        }
        if (d.tries > 1u) add(pluralStringResource(R.plurals.sv_delivery_tries, d.tries.toInt(), d.tries.toInt()))
        d.answeredAtMs?.let { add(stringResource(R.string.sv_delivery_answered_after, after(it))) }
    }.joinToString(" · ")
}

private fun clockTime(ms: Long): String = DateFormat.getTimeInstance(DateFormat.MEDIUM).format(Date(ms))

/** Agent prose with code blocks in monospace and `inline code` marked. */
@Composable
internal fun AgentText(text: String) {
    val blocks = remember(text) { textBlocks(text) }
    val codeBg = MaterialTheme.colorScheme.surfaceVariant
    Column(Modifier.fillMaxWidth(), verticalArrangement = Arrangement.spacedBy(6.dp)) {
        for (block in blocks) {
            when (block) {
                is TextBlock.Prose -> Text(remember(block, codeBg) { proseWithInlineCode(block.text, codeBg) })
                is TextBlock.Code -> CodeBox(block.text)
            }
        }
    }
}

private fun proseWithInlineCode(text: String, codeBg: Color): AnnotatedString = buildAnnotatedString {
    var at = 0
    for (span in inlineCodeSpans(text)) {
        append(text, at, span.first - 1)
        pushStyle(SpanStyle(fontFamily = FontFamily.Monospace, background = codeBg))
        append(text, span.first, span.last + 1)
        pop()
        at = span.last + 2
    }
    append(text, at, text.length)
}

@Composable
internal fun CodeBox(text: String, color: Color? = null, maxLines: Int = Int.MAX_VALUE, wrap: Boolean = false) {
    Surface(
        color = MaterialTheme.colorScheme.surfaceVariant,
        shape = RoundedCornerShape(6.dp),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Text(
            remember(text, maxLines) { firstLines(text, maxLines) },
            fontFamily = FontFamily.Monospace,
            style = MaterialTheme.typography.bodySmall,
            color = color ?: MaterialTheme.colorScheme.onSurfaceVariant,
            softWrap = wrap,
            modifier = (if (wrap) Modifier else Modifier.horizontalScroll(rememberScrollState())).padding(8.dp),
        )
    }
}

/**
 * The first [max] lines of [text], a "…" line in place of the rest. (Text's
 * own maxLines with softWrap off ellipsizes the first line only.)
 */
internal fun firstLines(text: String, max: Int): String {
    if (max == Int.MAX_VALUE) return text
    var at = -1
    repeat(max) {
        at = text.indexOf('\n', at + 1)
        if (at < 0) return text
    }
    return text.substring(0, at) + "\n…"
}

/** A diff with added and removed lines coloured. */
@Composable
internal fun DiffBox(text: String, maxLines: Int = Int.MAX_VALUE) {
    val added = MaterialTheme.colorScheme.primary
    val removed = MaterialTheme.colorScheme.error
    val styled = remember(text, maxLines, added, removed) {
        buildAnnotatedString {
            firstLines(text, maxLines).split('\n').forEachIndexed { i, line ->
                if (i > 0) append('\n')
                val color = when {
                    line.startsWith("+++") || line.startsWith("---") -> null
                    line.startsWith("+") -> added
                    line.startsWith("-") -> removed
                    else -> null
                }
                if (color != null) {
                    pushStyle(SpanStyle(color = color))
                    append(line)
                    pop()
                } else {
                    append(line)
                }
            }
        }
    }
    Surface(
        color = MaterialTheme.colorScheme.surfaceVariant,
        shape = RoundedCornerShape(6.dp),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Text(
            styled,
            fontFamily = FontFamily.Monospace,
            style = MaterialTheme.typography.bodySmall,
            softWrap = false,
            modifier = Modifier.horizontalScroll(rememberScrollState()).padding(8.dp),
        )
    }
}

/** The label of a tool kind (D3: the kind decides, the name is only shown). */
@Composable
internal fun kindLabel(tool: ToolView): String = when (tool.kind) {
    ToolKind.FILE_WRITE -> stringResource(R.string.sv_kind_write)
    ToolKind.FILE_EDIT -> stringResource(R.string.sv_kind_edit)
    ToolKind.FILE_READ -> stringResource(R.string.sv_kind_read)
    ToolKind.SHELL -> stringResource(R.string.sv_kind_shell)
    ToolKind.SEARCH -> stringResource(R.string.sv_kind_search)
    ToolKind.FETCH -> stringResource(R.string.sv_kind_fetch)
    ToolKind.SUBAGENT -> stringResource(R.string.sv_kind_subagent)
    ToolKind.OTHER -> stringResource(R.string.sv_kind_other, tool.name)
}

/**
 * Whether a finished call's output is worth showing collapsed: errors
 * always; a shell's output is the point of it; for other kinds with a
 * body (a diff, a file written) the body says more than the output.
 */
private fun outputShownCollapsed(tool: ToolView, done: ToolOutcome.Done): Boolean =
    done.isError || tool.kind == ToolKind.SHELL || tool.body == null

/** One tool call: kind and title, body preview, outcome; tap shows everything. */
@Composable
private fun ToolItem(tool: ToolView, expanded: Boolean, gated: GatedApproval?) {
    val colors = MaterialTheme.colorScheme
    val outcome = tool.outcome
    val failed = (outcome is ToolOutcome.Done && outcome.isError) || outcome is ToolOutcome.Denied
    Surface(
        shape = RoundedCornerShape(8.dp),
        color = colors.surfaceContainer,
        modifier = Modifier.fillMaxWidth(),
    ) {
        Column(Modifier.padding(10.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(
                    kindLabel(tool),
                    style = MaterialTheme.typography.labelMedium,
                    color = colors.primary,
                    fontWeight = FontWeight.SemiBold,
                )
                Spacer(Modifier.padding(start = 8.dp))
                Text(
                    tool.title,
                    fontFamily = if (tool.kind == ToolKind.SHELL) FontFamily.Monospace else FontFamily.Default,
                    maxLines = if (expanded) Int.MAX_VALUE else 2,
                    overflow = TextOverflow.Ellipsis,
                )
            }
            tool.detail?.let { Minor(it) }
            tool.body?.let { body ->
                val lines = if (expanded) Int.MAX_VALUE else PREVIEW_LINES
                when (body.format) {
                    BodyFormat.DIFF -> DiffBox(body.preview, lines)
                    BodyFormat.TEXT -> CodeBox(body.preview, maxLines = lines)
                }
                if (expanded) {
                    Minor(pluralStringResource(R.plurals.sv_more_lines, body.lines.toInt(), body.lines.toInt()))
                    body.full?.let { Minor(stringResource(R.string.sv_tool_full, blobText(it))) }
                }
            }
            if (outcome is ToolOutcome.Done && outcome.preview.isNotEmpty() &&
                (expanded || outputShownCollapsed(tool, outcome))
            ) {
                CodeBox(
                    outcome.preview,
                    color = if (outcome.isError) colors.error else null,
                    maxLines = if (expanded) Int.MAX_VALUE else PREVIEW_LINES,
                )
            }
            // A denial is the outcome line's to say, unless it granted more.
            tool.approval?.takeUnless { outcome is ToolOutcome.Denied && it.status.isPlainDenial() }
                ?.let { ApprovalLine(it.status, gated) }
            Text(
                outcomeText(outcome),
                style = MaterialTheme.typography.labelMedium,
                color = if (failed) colors.error else colors.onSurfaceVariant,
            )
            if (expanded) {
                (outcome as? ToolOutcome.Done)?.let { done ->
                    if (done.preview.isNotEmpty()) {
                        Minor(pluralStringResource(R.plurals.sv_more_lines, done.lines.toInt(), done.lines.toInt()))
                    }
                    done.output?.let { Minor(stringResource(R.string.sv_tool_output, blobText(it))) }
                }
                tool.input?.let { Minor(stringResource(R.string.sv_tool_input, blobText(it))) }
            }
        }
    }
}

@Composable
private fun outcomeText(outcome: ToolOutcome): String = when (outcome) {
    is ToolOutcome.Running -> stringResource(R.string.sv_tool_running, clockTime(outcome.sinceMs))
    is ToolOutcome.Done -> {
        val took = formatDuration(outcome.tookMs)
        val status = outcome.status
        when {
            outcome.isError && status != null -> stringResource(R.string.sv_tool_failed_status, status, took)
            outcome.isError -> stringResource(R.string.sv_tool_failed, took)
            status != null -> stringResource(R.string.sv_tool_done_status, status, took)
            else -> stringResource(R.string.sv_tool_done, took)
        }
    }
    is ToolOutcome.Denied -> if (outcome.byMe) {
        stringResource(R.string.sv_tool_denied_me)
    } else {
        stringResource(R.string.sv_tool_denied_other, shortDevice(outcome.by))
    }
    is ToolOutcome.Cancelled -> stringResource(R.string.sv_tool_cancelled, outcome.reason)
    ToolOutcome.NoResult -> stringResource(R.string.sv_tool_no_result)
}

private fun ApprovalStatus.isPlainDenial() =
    this is ApprovalStatus.Resolved && decision == Decision.DENY && rule == null

/**
 * An approval's status as text. Another device is named by its id: the
 * view carries no device names yet (e4d763).
 */
@Composable
internal fun approvalText(status: ApprovalStatus): String = when (status) {
    ApprovalStatus.Pending -> stringResource(R.string.sv_approval_pending)
    is ApprovalStatus.Resolved -> {
        val base = when {
            status.byMe -> stringResource(
                if (status.decision == Decision.APPROVE) R.string.sv_approval_approved_me else R.string.sv_approval_denied_me,
            )
            else -> stringResource(
                if (status.decision == Decision.APPROVE) R.string.sv_approval_approved_other else R.string.sv_approval_denied_other,
                shortDevice(status.by),
            )
        }
        base + (status.rule?.let { stringResource(R.string.sv_approval_rule, ruleText(it)) } ?: "")
    }
    is ApprovalStatus.Cancelled -> stringResource(R.string.sv_approval_cancelled, status.reason)
}

/** A blob's size and download state as text; fetching it is A13's. */
@Composable
internal fun blobText(blob: BlobView): String = when (val s = blob.state) {
    is BlobState.NotDownloaded -> stringResource(R.string.sv_blob_not_downloaded, formatBytes(s.size))
    is BlobState.Requested -> stringResource(R.string.sv_blob_requested, formatBytes(s.size))
    is BlobState.Downloading -> stringResource(
        if (s.paused) R.string.sv_blob_paused else R.string.sv_blob_downloading,
        formatBytes(s.got),
        formatBytes(s.size),
    )
    is BlobState.Failed -> stringResource(R.string.sv_blob_failed, formatBytes(s.size), s.reason)
    is BlobState.Available -> stringResource(R.string.sv_blob_available)
}

@Composable
private fun Label(text: String) = Text(text, style = MaterialTheme.typography.bodyMedium)

@Composable
internal fun Minor(text: String, color: Color? = null) = Text(
    text,
    style = MaterialTheme.typography.bodySmall,
    color = color ?: MaterialTheme.colorScheme.onSurfaceVariant,
)

@Composable
private fun SystemLine(text: String) = Text(
    text,
    style = MaterialTheme.typography.bodySmall,
    fontStyle = FontStyle.Italic,
    color = MaterialTheme.colorScheme.onSurfaceVariant,
    textAlign = TextAlign.Center,
    modifier = Modifier.fillMaxWidth(),
)
