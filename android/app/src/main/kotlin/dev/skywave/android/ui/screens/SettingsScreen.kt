package dev.skywave.android.ui.screens

import androidx.compose.foundation.clickable
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.material3.ListItem
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.stringResource
import dev.skywave.android.R
import dev.skywave.android.ui.ScreenScaffold
import dev.skywave.android.ui.ShellChrome
import dev.skywave.core.Action
import dev.skywave.core.BlobPolicy
import dev.skywave.core.Route
import dev.skywave.core.SettingsView

/** Settings. Stub: read-only values and the way back to pairing. */
@Composable
fun SettingsScreen(view: SettingsView, chrome: ShellChrome, dispatch: (Action) -> Unit) {
    ScreenScaffold(title = stringResource(R.string.settings_title), chrome = chrome) { padding ->
        val unknown = stringResource(R.string.settings_unknown)
        LazyColumn(Modifier.fillMaxSize(), contentPadding = padding) {
            item {
                ListItem(
                    headlineContent = { Text(stringResource(R.string.settings_server)) },
                    supportingContent = { Text(view.serverAddress ?: unknown) },
                )
            }
            item {
                ListItem(
                    headlineContent = { Text(stringResource(R.string.settings_device)) },
                    supportingContent = { Text(view.deviceId ?: unknown) },
                )
            }
            item {
                val policy = when (view.blobPolicy) {
                    BlobPolicy.EAGER -> R.string.settings_downloads_eager
                    BlobPolicy.ON_DEMAND -> R.string.settings_downloads_on_demand
                }
                ListItem(
                    headlineContent = { Text(stringResource(R.string.settings_downloads)) },
                    supportingContent = { Text(stringResource(policy)) },
                )
            }
            item {
                ListItem(
                    headlineContent = { Text(stringResource(R.string.settings_pair)) },
                    modifier = Modifier.clickable { dispatch(Action.Navigate(Route.PAIRING)) },
                )
            }
        }
    }
}
