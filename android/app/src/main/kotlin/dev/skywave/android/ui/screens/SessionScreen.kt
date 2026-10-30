package dev.skywave.android.ui.screens

import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.pluralStringResource
import androidx.compose.ui.unit.dp
import dev.skywave.android.R
import dev.skywave.android.ui.ScreenScaffold
import dev.skywave.android.ui.ShellChrome
import dev.skywave.core.Action
import dev.skywave.core.SessionView

/** One session's timeline. Stub: the item count; the timeline comes later. */
@Composable
fun SessionScreen(view: SessionView, chrome: ShellChrome, dispatch: (Action) -> Unit) {
    ScreenScaffold(title = view.title, chrome = chrome) { padding ->
        Column(Modifier.fillMaxSize().padding(padding).padding(16.dp)) {
            Text(
                pluralStringResource(R.plurals.session_items, view.timeline.size, view.timeline.size),
                style = MaterialTheme.typography.bodyLarge,
            )
        }
    }
}
