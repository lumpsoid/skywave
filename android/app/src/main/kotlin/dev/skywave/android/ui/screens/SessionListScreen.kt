package dev.skywave.android.ui.screens

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Settings
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.ListItem
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import dev.skywave.android.R
import dev.skywave.android.ui.ScreenScaffold
import dev.skywave.android.ui.ShellChrome
import dev.skywave.core.Action
import dev.skywave.core.ListKind
import dev.skywave.core.Route
import dev.skywave.core.SessionListView

/** Active or archived sessions. Stub: plain titles; rows and creation come later. */
@Composable
fun SessionListScreen(view: SessionListView, chrome: ShellChrome, dispatch: (Action) -> Unit) {
    val archived = view.kind == ListKind.ARCHIVED
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
    ) { padding ->
        if (view.sessions.isEmpty()) {
            Box(Modifier.fillMaxSize().padding(padding), contentAlignment = Alignment.Center) {
                Text(
                    stringResource(if (archived) R.string.sessions_archived_empty else R.string.sessions_empty),
                    style = MaterialTheme.typography.bodyLarge,
                )
            }
        } else {
            LazyColumn(Modifier.fillMaxSize(), contentPadding = padding) {
                items(view.sessions, key = { it.id }) { row ->
                    ListItem(
                        headlineContent = { Text(row.title) },
                        modifier = Modifier.clickable { dispatch(Action.OpenSession(row.id)) },
                    )
                }
            }
        }
    }
}
