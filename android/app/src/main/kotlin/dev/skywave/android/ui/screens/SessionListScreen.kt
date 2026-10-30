package dev.skywave.android.ui.screens

import android.text.format.DateUtils
import androidx.compose.foundation.background
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.Edit
import androidx.compose.material.icons.filled.MoreVert
import androidx.compose.material.icons.filled.Settings
import androidx.compose.material.icons.filled.Warning
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Badge
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExtendedFloatingActionButton
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.ListItem
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
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
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.pluralStringResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import dev.skywave.android.R
import dev.skywave.android.ui.ScreenScaffold
import dev.skywave.android.ui.ShellChrome
import dev.skywave.core.Action
import dev.skywave.core.FailReason
import dev.skywave.core.ListKind
import dev.skywave.core.Outgoing
import dev.skywave.core.PendingSession
import dev.skywave.core.Route
import dev.skywave.core.SessionListView
import dev.skywave.core.SessionRow
import dev.skywave.core.SessionState

/** Test tags of the session list and its dialogs. */
internal object SessionListTags {
    const val NEW = "sessions_new"
    const val RENAME_FIELD = "sessions_rename_field"
    const val RENAME_CONFIRM = "sessions_rename_confirm"
    const val DELETE_CONFIRM = "sessions_delete_confirm"

    fun menu(sessionId: String) = "sessions_menu_$sessionId"
    fun discard(commandId: String) = "sessions_discard_$commandId"
}

/**
 * Active or archived sessions, as the core lists them. Rows open on tap;
 * their menu (long press or ⋮) renames, archives or unarchives, and on the
 * archived list deletes from this device when the core says the row is
 * [SessionRow.deletable]. The active list also starts sessions and shows
 * this device's creations the replica does not hold yet. Every rule is the
 * core's (D2): this screen only renders and dispatches, so it all works
 * offline and the link banner says what is queued.
 */
@Composable
fun SessionListScreen(view: SessionListView, chrome: ShellChrome, dispatch: (Action) -> Unit) {
    val archived = view.kind == ListKind.ARCHIVED
    var creating by rememberSaveable { mutableStateOf(false) }
    // Dialogs name the session by id; the row is looked up in the current
    // view, so a session that left the list closes its dialog.
    var renamingId by rememberSaveable { mutableStateOf<String?>(null) }
    var deletingId by rememberSaveable { mutableStateOf<String?>(null) }

    ScreenScaffold(
        title = stringResource(if (archived) R.string.sessions_archived_title else R.string.sessions_title),
        chrome = chrome,
        actions = {
            if (!archived) {
                IconButton(onClick = { dispatch(Action.Navigate(Route.ARCHIVED_SESSIONS)) }) {
                    Icon(painterResource(R.drawable.ic_archive), stringResource(R.string.sessions_open_archived))
                }
                IconButton(onClick = { dispatch(Action.Navigate(Route.SETTINGS)) }) {
                    Icon(Icons.Filled.Settings, stringResource(R.string.sessions_open_settings))
                }
            }
        },
        floatingActionButton = {
            // A new session is active: it is started from the active list.
            if (!archived) {
                val label = stringResource(R.string.sessions_new)
                // The extended FAB's text does not reach its merged semantics: say it here.
                ExtendedFloatingActionButton(
                    onClick = { creating = true },
                    icon = { Icon(Icons.Filled.Add, contentDescription = null) },
                    text = { Text(label) },
                    modifier = Modifier
                        .semantics { contentDescription = label }
                        .testTag(SessionListTags.NEW),
                )
            }
        },
    ) { padding ->
        if (view.sessions.isEmpty() && view.creating.isEmpty()) {
            EmptyList(view, Modifier.fillMaxSize().padding(padding), dispatch)
        } else {
            LazyColumn(Modifier.fillMaxSize(), contentPadding = padding) {
                if (view.creating.isNotEmpty()) {
                    item(key = "creating_header", contentType = "header") {
                        Text(
                            stringResource(R.string.sessions_creating_header),
                            style = MaterialTheme.typography.titleSmall,
                            color = MaterialTheme.colorScheme.primary,
                            modifier = Modifier.padding(start = 16.dp, end = 16.dp, top = 12.dp, bottom = 4.dp),
                        )
                    }
                    items(view.creating, key = { "creating:${it.commandId}" }, contentType = { "creating" }) {
                        PendingRow(it, onDiscard = { dispatch(Action.Discard(it.commandId)) })
                    }
                    item(key = "creating_divider", contentType = "divider") { HorizontalDivider() }
                }
                items(view.sessions, key = { it.id }, contentType = { "session" }) { row ->
                    SessionRowItem(
                        row = row,
                        archived = archived,
                        onOpen = { dispatch(Action.OpenSession(row.id)) },
                        onRename = { renamingId = row.id },
                        onArchive = { dispatch(Action.SetSessionArchived(row.id, !archived)) },
                        onDelete = { deletingId = row.id },
                    )
                }
                // Room for the last row's menu above the FAB.
                if (!archived) item(key = "fab_space", contentType = "space") { Box(Modifier.size(80.dp)) }
            }
        }
    }

    if (creating) {
        NewSessionDialog(
            completion = view.completion,
            dispatch = dispatch,
            onClose = { creating = false },
        )
    }
    renamingId?.let { id -> view.sessions.find { it.id == id } }?.let { row ->
        RenameDialog(
            row = row,
            onRename = { name ->
                renamingId = null
                dispatch(Action.RenameSession(row.id, name))
            },
            onDismiss = { renamingId = null },
        )
    }
    deletingId?.let { id -> view.sessions.find { it.id == id && it.deletable } }?.let { row ->
        DeleteDialog(
            row = row,
            onDelete = {
                deletingId = null
                dispatch(Action.DeleteSessionLocally(row.id))
            },
            onDismiss = { deletingId = null },
        )
    }
}

@Composable
private fun EmptyList(view: SessionListView, modifier: Modifier, dispatch: (Action) -> Unit) {
    val archived = view.kind == ListKind.ARCHIVED
    Column(
        modifier.padding(32.dp),
        verticalArrangement = Arrangement.spacedBy(8.dp, Alignment.CenterVertically),
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Text(
            stringResource(if (archived) R.string.sessions_archived_empty else R.string.sessions_empty),
            style = MaterialTheme.typography.bodyLarge,
        )
        Text(
            stringResource(if (archived) R.string.sessions_archived_empty_hint else R.string.sessions_empty_hint),
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            textAlign = TextAlign.Center,
        )
        // Everything may be archived: say so rather than look empty.
        if (!archived && view.otherCount > 0u) {
            val count = view.otherCount.toInt()
            TextButton(onClick = { dispatch(Action.Navigate(Route.ARCHIVED_SESSIONS)) }) {
                Text(pluralStringResource(R.plurals.sessions_show_archived, count, count))
            }
        }
    }
}

@Composable
private fun SessionRowItem(
    row: SessionRow,
    archived: Boolean,
    onOpen: () -> Unit,
    onRename: () -> Unit,
    onArchive: () -> Unit,
    onDelete: () -> Unit,
) {
    var menu by remember { mutableStateOf(false) }
    val longClickLabel = stringResource(R.string.session_actions_long_click)
    ListItem(
        leadingContent = { StateIndicator(row.state) },
        headlineContent = { Text(row.title, maxLines = 1, overflow = TextOverflow.Ellipsis) },
        supportingContent = { RowDetails(row) },
        trailingContent = {
            Row(verticalAlignment = Alignment.CenterVertically) {
                if (row.unread > 0u) {
                    val unread = row.unread.toInt()
                    val label = pluralStringResource(R.plurals.session_unread, unread, unread)
                    Badge(Modifier.semantics { contentDescription = label }) { Text(unread.toString()) }
                }
                Box {
                    IconButton(onClick = { menu = true }, modifier = Modifier.testTag(SessionListTags.menu(row.id))) {
                        Icon(Icons.Filled.MoreVert, stringResource(R.string.session_actions, row.title))
                    }
                    RowMenu(
                        expanded = menu,
                        row = row,
                        archived = archived,
                        onDismiss = { menu = false },
                        onRename = onRename,
                        onArchive = onArchive,
                        onDelete = onDelete,
                    )
                }
            }
        },
        modifier = Modifier.combinedClickable(
            onClick = onOpen,
            onLongClickLabel = longClickLabel,
            onLongClick = { menu = true },
        ),
    )
}

@Composable
private fun RowMenu(
    expanded: Boolean,
    row: SessionRow,
    archived: Boolean,
    onDismiss: () -> Unit,
    onRename: () -> Unit,
    onArchive: () -> Unit,
    onDelete: () -> Unit,
) {
    DropdownMenu(expanded = expanded, onDismissRequest = onDismiss) {
        DropdownMenuItem(
            text = { Text(stringResource(R.string.session_rename)) },
            onClick = {
                onDismiss()
                onRename()
            },
        )
        DropdownMenuItem(
            text = { Text(stringResource(if (archived) R.string.session_unarchive else R.string.session_archive)) },
            onClick = {
                onDismiss()
                onArchive()
            },
        )
        // Only archived sessions are ever deletable; the core says when
        // something about one still waits on an answer.
        if (archived) {
            DropdownMenuItem(
                text = {
                    Text(
                        stringResource(
                            if (row.deletable) R.string.session_delete_locally else R.string.session_delete_not_yet,
                        ),
                    )
                },
                enabled = row.deletable,
                onClick = {
                    onDismiss()
                    onDelete()
                },
            )
        }
    }
}

/** What the agent is doing, as a mark in front of the row. */
@Composable
private fun StateIndicator(state: SessionState) {
    val label = stringResource(
        when (state) {
            SessionState.Idle -> R.string.session_state_idle
            SessionState.AgentWorking -> R.string.session_state_working
            is SessionState.AwaitingApproval -> R.string.session_state_awaiting
            is SessionState.Errored -> R.string.session_state_errored
            SessionState.Stopped -> R.string.session_state_stopped
        },
    )
    Box(Modifier.size(24.dp).semantics { contentDescription = label }, contentAlignment = Alignment.Center) {
        val colors = MaterialTheme.colorScheme
        when (state) {
            SessionState.Idle -> Dot(colors.primary)
            SessionState.Stopped -> Dot(colors.outlineVariant)
            SessionState.AgentWorking -> CircularProgressIndicator(Modifier.size(18.dp), strokeWidth = 2.dp)
            is SessionState.AwaitingApproval -> Icon(Icons.Filled.Warning, null, tint = colors.tertiary)
            is SessionState.Errored -> Icon(Icons.Filled.Warning, null, tint = colors.error)
        }
    }
}

@Composable
private fun Dot(color: Color) = Box(Modifier.size(10.dp).background(color, CircleShape))

/** The row's second lines: where, when, what it cost, and what waits. */
@Composable
private fun RowDetails(row: SessionRow) {
    val colors = MaterialTheme.colorScheme
    Column {
        row.workdir?.let {
            Text(
                it,
                maxLines = 1,
                overflow = TextOverflow.StartEllipsis,
                fontFamily = FontFamily.Monospace,
                style = MaterialTheme.typography.bodySmall,
            )
        }
        val facts = buildList {
            // The server's clock may run ahead of this device's: never "in 1 min".
            val now = maxOf(System.currentTimeMillis(), row.lastActivityMs)
            add(
                DateUtils.getRelativeTimeSpanString(
                    row.lastActivityMs,
                    now,
                    DateUtils.MINUTE_IN_MILLIS,
                    DateUtils.FORMAT_ABBREV_RELATIVE,
                ).toString(),
            )
            row.costUsd?.let { add(stringResource(R.string.session_cost, it)) }
            when (row.rename) {
                null, is Outgoing.Accepted -> Unit
                Outgoing.Queued, Outgoing.Sending -> add(stringResource(R.string.session_rename_pending))
                is Outgoing.Failed -> add(stringResource(R.string.session_rename_refused))
            }
        }
        Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(6.dp)) {
            Text(facts.joinToString(" · "), maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.weight(1f, fill = false))
            if (row.hasDraft) {
                Icon(Icons.Filled.Edit, stringResource(R.string.session_draft), Modifier.size(14.dp))
            }
        }
        if (row.pendingApprovals > 0u) {
            val n = row.pendingApprovals.toInt()
            Text(pluralStringResource(R.plurals.session_pending_approvals, n, n), color = colors.tertiary)
        }
        (row.state as? SessionState.Errored)?.let {
            Text(
                stringResource(R.string.session_error, it.error.message),
                color = colors.error,
                maxLines = 2,
                overflow = TextOverflow.Ellipsis,
            )
        }
    }
}

/** A session this device asked for, not in the replica yet, or refused. */
@Composable
private fun PendingRow(pending: PendingSession, onDiscard: () -> Unit) {
    val outgoing = pending.outgoing
    ListItem(
        leadingContent = {
            Box(Modifier.size(24.dp), contentAlignment = Alignment.Center) {
                when (outgoing) {
                    is Outgoing.Failed -> Icon(Icons.Filled.Warning, null, tint = MaterialTheme.colorScheme.error)
                    Outgoing.Queued -> Dot(MaterialTheme.colorScheme.outlineVariant)
                    Outgoing.Sending, is Outgoing.Accepted ->
                        CircularProgressIndicator(Modifier.size(18.dp), strokeWidth = 2.dp)
                }
            }
        },
        headlineContent = {
            Text(pending.workdir, fontFamily = FontFamily.Monospace, maxLines = 1, overflow = TextOverflow.StartEllipsis)
        },
        supportingContent = {
            when (outgoing) {
                Outgoing.Queued -> Text(stringResource(R.string.creating_queued))
                Outgoing.Sending -> Text(stringResource(R.string.creating_sending))
                is Outgoing.Accepted -> Text(stringResource(R.string.creating_accepted))
                is Outgoing.Failed -> Text(
                    stringResource(R.string.creating_failed, failText(outgoing.reason)),
                    color = MaterialTheme.colorScheme.error,
                )
            }
        },
        trailingContent = if (pending.discardable) {
            {
                TextButton(onClick = onDiscard, modifier = Modifier.testTag(SessionListTags.discard(pending.commandId))) {
                    Text(
                        stringResource(
                            if (outgoing is Outgoing.Failed) R.string.creating_dismiss else R.string.creating_discard,
                        ),
                    )
                }
            }
        } else {
            null
        },
    )
}

@Composable
internal fun failText(reason: FailReason): String = stringResource(
    when (reason) {
        FailReason.WORKDIR_NOT_ALLOWED -> R.string.fail_workdir_not_allowed
        FailReason.WORKDIR_NOT_FOUND -> R.string.fail_workdir_not_found
        FailReason.UNKNOWN_SESSION -> R.string.fail_unknown_session
        FailReason.UNKNOWN_APPROVAL -> R.string.fail_unknown_approval
        FailReason.APPROVAL_ALREADY_RESOLVED -> R.string.fail_approval_resolved
        FailReason.APPROVAL_CANCELLED -> R.string.fail_approval_cancelled
        FailReason.NO_ACTIVE_TURN -> R.string.fail_no_active_turn
        FailReason.UNKNOWN_RULE -> R.string.fail_unknown_rule
    },
)

/**
 * A new name for [row]; blank goes back to the default title. The core
 * normalises the name as the server will, so it goes out as typed.
 */
@Composable
private fun RenameDialog(row: SessionRow, onRename: (String) -> Unit, onDismiss: () -> Unit) {
    var name by rememberSaveable(row.id) { mutableStateOf(if (row.named) row.title else "") }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(stringResource(R.string.session_rename_title)) },
        text = {
            OutlinedTextField(
                value = name,
                onValueChange = { name = it },
                label = { Text(stringResource(R.string.session_rename_name)) },
                placeholder = { Text(row.title, maxLines = 1, overflow = TextOverflow.Ellipsis) },
                supportingText = { Text(stringResource(R.string.session_rename_hint)) },
                singleLine = true,
                modifier = Modifier.testTag(SessionListTags.RENAME_FIELD),
            )
        },
        confirmButton = {
            TextButton(onClick = { onRename(name) }, modifier = Modifier.testTag(SessionListTags.RENAME_CONFIRM)) {
                Text(stringResource(R.string.session_rename_confirm))
            }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text(stringResource(R.string.sessions_cancel)) } },
    )
}

/** Asks before deleting [row] from this device; it stays on the server. */
@Composable
private fun DeleteDialog(row: SessionRow, onDelete: () -> Unit, onDismiss: () -> Unit) {
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(stringResource(R.string.session_delete_title, row.title)) },
        text = { Text(stringResource(R.string.session_delete_body)) },
        confirmButton = {
            TextButton(onClick = onDelete, modifier = Modifier.testTag(SessionListTags.DELETE_CONFIRM)) {
                Text(stringResource(R.string.session_delete_confirm), color = MaterialTheme.colorScheme.error)
            }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text(stringResource(R.string.sessions_cancel)) } },
    )
}
