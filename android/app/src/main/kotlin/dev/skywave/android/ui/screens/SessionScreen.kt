package dev.skywave.android.ui.screens

import android.content.ClipData
import android.os.Build
import android.widget.Toast
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyListState
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.KeyboardArrowDown
import androidx.compose.material.icons.filled.MoreVert
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExtendedFloatingActionButton
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.derivedStateOf
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.runtime.snapshotFlow
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.ClipEntry
import androidx.compose.ui.platform.LocalClipboard
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.pluralStringResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import dev.skywave.android.R
import dev.skywave.android.ui.ScreenScaffold
import dev.skywave.android.ui.ShellChrome
import dev.skywave.core.Action
import dev.skywave.core.Attention
import dev.skywave.core.Outgoing
import dev.skywave.core.SessionState
import dev.skywave.core.SessionView
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch

/**
 * Least time between two read marks while the timeline follows new items:
 * an agent streaming many small batches must not cost a durable write each.
 * Leaving the screen marks at once.
 */
internal const val READ_MARK_EVERY_MS = 2_000L

internal object SessionTags {
    const val INTERRUPT = "session_interrupt"
    const val MORE = "session_more"
    const val STATUS = "session_status"
    const val RENAME_FIELD = "session_rename_field"
    const val RENAME_CONFIRM = "session_rename_confirm"
}

/**
 * One session (D2: heads only render): its timeline, the approval its agent
 * waits on, a composer whose draft the core keeps, and the session's
 * controls. [attention] is the app's pending approvals, for the jump to the
 * next one elsewhere. Everything is keyed by the session, so opening
 * another one starts afresh.
 */
@Composable
fun SessionScreen(view: SessionView, attention: Attention, chrome: ShellChrome, dispatch: (Action) -> Unit) {
    // A Box, so the blob viewer stacks over the session.
    Box(Modifier.fillMaxSize()) {
        key(view.id) { SessionContent(view, attention, chrome, dispatch) }
    }
}

@Composable
private fun SessionContent(view: SessionView, attention: Attention, chrome: ShellChrome, dispatch: (Action) -> Unit) {
    val composer = rememberComposer(view, dispatch)
    val rows = remember(view.timeline, view.unreadFrom) { timelineRows(view.timeline, view.unreadFrom) }
    var renaming by rememberSaveable { mutableStateOf(false) }
    val prompt = view.pendingApproval
    // Each new approval opens the panel; a collapsed one stays so until reopened.
    var approvalOpen by rememberSaveable(prompt?.id) { mutableStateOf(true) }
    val gated = remember(prompt) { prompt?.let { GatedApproval(it) { approvalOpen = true } } }
    val gatedKey = remember(rows, prompt) { gatedRowKey(rows, prompt) }
    // The blob open in the viewer: presentation only (not a core route),
    // saved so rotation keeps it open.
    var viewing by rememberSaveable(stateSaver = OpenBlob.Saver) { mutableStateOf<OpenBlob?>(null) }
    val blobs = remember(dispatch) {
        BlobActions(download = { dispatch(Action.DownloadBlob(it)) }, open = { viewing = it })
    }
    val search = rememberSearch(view.id, view.timeline)
    val searching = search?.active == true
    val hits = remember(search?.result, rows) { searchHits(search?.result, rows) }

    ScreenScaffold(
        title = view.title,
        chrome = chrome,
        titleContent = if (searching) {
            { SearchField(search!!, onSearch = { search.step(hits, forward = true) }) }
        } else {
            null
        },
        actions = {
            if (searching) {
                SearchActions(search!!, hits)
            } else {
                search?.let { SearchButton(onClick = it::open) }
                NextApprovalButton(attention.elsewhere) { dispatch(Action.OpenNextApproval) }
                SessionActions(view, dispatch, onRename = { renaming = true })
            }
        },
        bottomBar = {
            Column {
                if (prompt != null) {
                    val tool = remember(view.timeline, prompt) { gatedTool(view.timeline, prompt) }
                    ApprovalPanel(
                        prompt = prompt,
                        tool = tool,
                        open = approvalOpen,
                        onOpen = { approvalOpen = it },
                        onAnswer = { decision, rule -> dispatch(Action.ResolveApproval(prompt.id, decision, rule)) },
                        blobs = blobs,
                    )
                }
                ComposerBar(composer, remember(view.prompts) { view.prompts.take(50) })
            }
        },
    ) { padding ->
        Column(Modifier.fillMaxSize().padding(padding)) {
            StatusLine(view)
            if (searching) SearchHint(search!!.result)
            Box(Modifier.weight(1f).fillMaxWidth()) {
                if (rows.isEmpty()) {
                    Text(
                        stringResource(R.string.sv_empty),
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        modifier = Modifier.align(Alignment.Center).padding(24.dp),
                    )
                } else {
                    Timeline(view, rows, dispatch, composer, gated, gatedKey, blobs, search.takeIf { searching }, hits)
                }
            }
        }
    }

    // Back closes the search first; the viewer, composed later, before it.
    BackHandler(enabled = searching) { search?.close() }

    // Over the session, which stays composed below: closing it comes back
    // to the timeline where it was.
    viewing?.let { BlobViewer(it, onClose = { viewing = null }) }

    if (renaming) {
        RenameDialog(
            view,
            onRename = {
                renaming = false
                dispatch(Action.RenameSession(view.id, it))
            },
            onDismiss = { renaming = false },
        )
    }
}

/**
 * The scrolling timeline. It follows the newest item while the user is at
 * the bottom; once they scroll up it stays still (LazyColumn keeps its
 * first visible item by key) and offers a jump back with the count of
 * items since. Marks the session read when the bottom is on screen.
 */
@Composable
private fun Timeline(
    view: SessionView,
    rows: List<TimelineRow>,
    dispatch: (Action) -> Unit,
    composer: ComposerState,
    gated: GatedApproval?,
    gatedKey: String?,
    blobs: BlobActions,
    search: SearchState?,
    hits: SearchHits,
) {
    val divider = remember { rows.indexOf(TimelineRow.Divider) }
    // Opens on the divider when there is news, else at the end; LazyColumn
    // pulls back to fill the screen when what follows is short.
    val listState = rememberLazyListState(initialFirstVisibleItemIndex = if (divider >= 0) divider else rows.lastIndex)
    var following by rememberSaveable { mutableStateOf(divider < 0) }
    var leftAtKey by rememberSaveable { mutableStateOf<String?>(null) }
    val currentRows by rememberUpdatedState(rows)
    val currentFollowing by rememberUpdatedState(following)

    val scroller = remember(listState) { EndScroller(listState) }
    FollowBottom(listState, scroller, currentRows = { currentRows }, following = { currentFollowing }) { now ->
        following = now
        leftAtKey = if (now) null else currentRows.lastOrNull()?.key
    }
    LaunchedEffect(rows, following) {
        if (following) scroller.scrollToEnd(rows)
    }
    if (divider >= 0) {
        // Opened on the divider: if everything after it fits, this is the bottom.
        LaunchedEffect(Unit) {
            snapshotFlow { listState.layoutInfo.totalItemsCount }.first { it > 0 }
            if (!listState.canScrollForward) following = true
        }
    }
    ReadMarker(view, listState, dispatch)
    if (search != null) {
        // A new result settles the current match from where the user is.
        LaunchedEffect(search, hits) { search.settle(hits, listState.firstVisibleItemIndex) }
        // Each jump brings the current match to the top and stops following.
        LaunchedEffect(search, search.jumps) {
            val key = search.current ?: return@LaunchedEffect
            val index = currentRows.indexOfFirst { it.key == key }
            if (index < 0) return@LaunchedEffect
            following = false
            leftAtKey = currentRows.lastOrNull()?.key
            listState.scrollToItem(index)
        }
    }

    val clipboard = LocalClipboard.current
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    val clipLabel = stringResource(R.string.sv_clip_label)
    val copied = stringResource(R.string.sv_copied)
    val actions = remember(composer, dispatch, blobs) {
        ItemActions(
            copy = { text ->
                scope.launch {
                    clipboard.setClipEntry(ClipEntry(ClipData.newPlainText(clipLabel, text)))
                    // Android 13 and later confirm a copy themselves.
                    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) {
                        Toast.makeText(context, copied, Toast.LENGTH_SHORT).show()
                    }
                }
            },
            discard = { dispatch(Action.Discard(it)) },
            edit = composer::editQueued,
            blobs = blobs,
        )
    }

    Box(Modifier.fillMaxSize()) {
        LazyColumn(
            state = listState,
            contentPadding = PaddingValues(vertical = 8.dp),
            modifier = Modifier.fillMaxSize().testTag(TimelineTags.LIST),
        ) {
            items(rows, key = { it.key }, contentType = { contentType(it) }) { row ->
                when (row) {
                    TimelineRow.Divider -> NewDivider()
                    is TimelineRow.Item -> TimelineItemView(
                        row,
                        actions,
                        gated?.takeIf { row.key == gatedKey },
                        search?.let { s -> hits[row.key]?.let { rowSearch(s, row.key, it) } },
                    )
                }
            }
        }
        val showJump by remember { derivedStateOf { !currentFollowing && listState.canScrollForward } }
        if (showJump) {
            val newer = leftAtKey?.let { key -> rows.indexOfFirst { it.key == key } }
                ?.takeIf { it >= 0 }
                ?.let { rows.lastIndex - it } ?: 0
            val jumpLabel = stringResource(R.string.sv_jump_latest)
            val shown = if (newer > 0) pluralStringResource(R.plurals.sv_jump_new, newer, newer) else jumpLabel
            // The extended FAB's text does not reach its merged semantics: say it all here.
            val described = if (newer > 0) "$jumpLabel, $shown" else jumpLabel
            ExtendedFloatingActionButton(
                onClick = { following = true; leftAtKey = null },
                icon = { Icon(Icons.Filled.KeyboardArrowDown, null) },
                text = { Text(shown) },
                modifier = Modifier
                    .align(Alignment.BottomEnd)
                    .padding(16.dp)
                    .semantics { contentDescription = described }
                    .testTag(TimelineTags.JUMP),
            )
        }
    }
}

private fun contentType(row: TimelineRow): String = when (row) {
    TimelineRow.Divider -> "divider"
    is TimelineRow.Item -> when (val item = row.item) {
        is dev.skywave.core.TimelineItem.Pending -> "message"
        is dev.skywave.core.TimelineItem.Logged -> item.entry::class.simpleName ?: "entry"
    }
}

/**
 * The timeline's own scrolls to the end, told apart from everyone else's
 * (a drag, a fling, an accessibility or test scroll) by [running].
 */
private class EndScroller(private val listState: LazyListState) {
    var running by mutableStateOf(false)
        private set

    /** Scrolls so the end of the last row is at the bottom. */
    suspend fun scrollToEnd(rows: List<TimelineRow>) {
        if (rows.isEmpty()) return
        running = true
        try {
            // The offset is clamped: the last row ends at the viewport's bottom.
            listState.scrollToItem(rows.lastIndex, Int.MAX_VALUE)
        } finally {
            running = false
        }
    }
}

/**
 * Tracks whether the timeline follows the bottom. Any scroll but the
 * timeline's own (a drag, a fling, an accessibility or test scroll) that
 * leaves the end out of view stops following at once, so nothing arriving
 * pulls the list from under the finger; one that reaches the end follows
 * again. While following, the end is kept in view whatever moved it: new
 * items, the keyboard opening, the last item growing or expanding.
 */
@Composable
private fun FollowBottom(
    listState: LazyListState,
    scroller: EndScroller,
    currentRows: () -> List<TimelineRow>,
    following: () -> Boolean,
    setFollowing: (Boolean) -> Unit,
) {
    LaunchedEffect(listState) {
        // Where the list stands, by the key of its first visible item: new
        // items, a pending message logged, the keyboard or a growing item do
        // not move it; only a scroll does.
        var last: Pair<Any?, Int>? = null
        snapshotFlow {
            val at = listState.layoutInfo.visibleItemsInfo.firstOrNull()?.key to listState.firstVisibleItemScrollOffset
            Triple(at, scroller.running, listState.canScrollForward to following())
        }.collect { (at, ours, flags) ->
            val (canScrollForward, follow) = flags
            when {
                ours -> last = null
                last != null && at != last -> {
                    // Someone else scrolled: where they left it decides.
                    last = at
                    if (!canScrollForward != follow) setFollowing(!canScrollForward)
                }
                else -> {
                    last = at
                    // The end moved out of view on its own: bring it back.
                    if (follow && canScrollForward) scroller.scrollToEnd(currentRows())
                }
            }
        }
    }
}

/**
 * Marks the session read through its newest event while the bottom of the
 * timeline is on screen and the list is at rest: at once the first time,
 * then at most once per [READ_MARK_EVERY_MS], and on leaving. Only ever
 * forward; the divider stays where the core put it.
 */
@Composable
private fun ReadMarker(view: SessionView, listState: LazyListState, dispatch: (Action) -> Unit) {
    var marked by rememberSaveable { mutableLongStateOf(0L) }
    var cooling by remember { mutableStateOf(false) }
    val atEnd by remember {
        derivedStateOf { listState.layoutInfo.totalItemsCount > 0 && !listState.canScrollForward }
    }
    val lastSeq = view.lastSeq.toLong()
    val scrolling = listState.isScrollInProgress
    val mark = { dispatch(Action.MarkRead(view.id, lastSeq.toULong())); marked = lastSeq }
    LaunchedEffect(atEnd, scrolling, lastSeq, cooling) {
        if (atEnd && !scrolling && !cooling && lastSeq > marked) {
            mark()
            cooling = true
        }
    }
    LaunchedEffect(cooling) {
        if (cooling) {
            delay(READ_MARK_EVERY_MS)
            cooling = false
        }
    }
    val leave by rememberUpdatedState { if (atEnd && lastSeq > marked) mark() }
    DisposableEffect(Unit) { onDispose { leave() } }
}

/** What the agent is doing, and what this device's last controls did. */
@Composable
private fun StatusLine(view: SessionView) {
    val parts = buildList {
        add(
            when (val state = view.state) {
                SessionState.Idle -> stringResource(R.string.session_state_idle)
                SessionState.AgentWorking -> stringResource(R.string.session_state_working)
                is SessionState.AwaitingApproval -> stringResource(R.string.session_state_awaiting)
                is SessionState.Errored -> stringResource(R.string.session_error, state.error.message)
                SessionState.Stopped -> stringResource(R.string.session_state_stopped)
            },
        )
        view.usage?.total?.costUsd?.let { add(stringResource(R.string.sv_total_cost, it)) }
        when (val r = view.rename) {
            is Outgoing.Failed -> add(stringResource(R.string.session_rename_refused))
            Outgoing.Queued, Outgoing.Sending -> add(stringResource(R.string.session_rename_pending))
            else -> {}
        }
        outgoingNote(view.interrupt, R.string.sv_interrupting)?.let(::add)
        outgoingNote(view.stop, R.string.sv_stopping)?.let(::add)
    }
    Surface(tonalElevation = 1.dp, modifier = Modifier.fillMaxWidth()) {
        Text(
            parts.joinToString(stringResource(R.string.separator)),
            style = MaterialTheme.typography.labelMedium,
            color = if (view.state is SessionState.Errored) {
                MaterialTheme.colorScheme.error
            } else {
                MaterialTheme.colorScheme.onSurfaceVariant
            },
            maxLines = 2,
            overflow = TextOverflow.Ellipsis,
            modifier = Modifier.padding(horizontal = 16.dp, vertical = 4.dp).testTag(SessionTags.STATUS),
        )
    }
}

@Composable
private fun outgoingNote(outgoing: Outgoing?, pending: Int): String? = when (outgoing) {
    Outgoing.Queued, Outgoing.Sending -> stringResource(pending)
    is Outgoing.Failed -> stringResource(R.string.sv_refused, failText(outgoing.reason))
    else -> null
}

/** Interrupt while the agent works; the rest in the overflow menu. */
@Composable
private fun SessionActions(view: SessionView, dispatch: (Action) -> Unit, onRename: () -> Unit) {
    val busy = view.state is SessionState.AgentWorking || view.state is SessionState.AwaitingApproval
    if (busy) {
        val sent = view.interrupt == Outgoing.Queued || view.interrupt == Outgoing.Sending
        TextButton(
            onClick = { dispatch(Action.Interrupt(view.id)) },
            enabled = !sent,
            modifier = Modifier.testTag(SessionTags.INTERRUPT),
        ) {
            Text(stringResource(if (sent) R.string.sv_interrupting else R.string.sv_interrupt))
        }
    }
    var open by remember { mutableStateOf(false) }
    Box {
        IconButton(onClick = { open = true }, modifier = Modifier.testTag(SessionTags.MORE)) {
            Icon(Icons.Filled.MoreVert, stringResource(R.string.sv_more))
        }
        DropdownMenu(expanded = open, onDismissRequest = { open = false }) {
            DropdownMenuItem(
                text = { Text(stringResource(R.string.session_rename)) },
                onClick = { open = false; onRename() },
            )
            if (view.state != SessionState.Stopped) {
                DropdownMenuItem(
                    text = { Text(stringResource(R.string.sv_stop)) },
                    onClick = { open = false; dispatch(Action.StopSession(view.id)) },
                )
            }
            DropdownMenuItem(
                text = { Text(stringResource(if (view.archived) R.string.session_unarchive else R.string.session_archive)) },
                onClick = { open = false; dispatch(Action.SetSessionArchived(view.id, !view.archived)) },
            )
        }
    }
}

/** A new name for the session; blank goes back to the default title. */
@Composable
private fun RenameDialog(view: SessionView, onRename: (String) -> Unit, onDismiss: () -> Unit) {
    var name by rememberSaveable { mutableStateOf(if (view.named) view.title else "") }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(stringResource(R.string.session_rename_title)) },
        text = {
            OutlinedTextField(
                value = name,
                onValueChange = { name = it },
                label = { Text(stringResource(R.string.session_rename_name)) },
                placeholder = { Text(view.title, maxLines = 1, overflow = TextOverflow.Ellipsis) },
                supportingText = { Text(stringResource(R.string.session_rename_hint)) },
                singleLine = true,
                modifier = Modifier.testTag(SessionTags.RENAME_FIELD),
            )
        },
        confirmButton = {
            TextButton(onClick = { onRename(name) }, modifier = Modifier.testTag(SessionTags.RENAME_CONFIRM)) {
                Text(stringResource(R.string.session_rename_confirm))
            }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text(stringResource(R.string.sessions_cancel)) } },
    )
}
