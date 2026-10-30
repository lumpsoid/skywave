package dev.skywave.android.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalResources
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.tooling.preview.Preview
import androidx.compose.ui.unit.dp
import dev.skywave.android.R
import dev.skywave.android.core.CoreMessage

/**
 * The core could not open; nothing else can work, so say why and stop.
 * Calls nothing native: the reason may be that the library did not load.
 */
@Composable
fun StartupFailedScreen(reason: CoreMessage, appVersion: String?) {
    Scaffold { padding ->
        Column(
            modifier = Modifier.fillMaxSize().padding(padding).padding(32.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp, Alignment.CenterVertically),
            horizontalAlignment = Alignment.CenterHorizontally,
        ) {
            Text(stringResource(R.string.startup_failed_title), style = MaterialTheme.typography.headlineSmall)
            Text(reason.text(LocalResources.current), style = MaterialTheme.typography.bodyLarge, textAlign = TextAlign.Center)
            if (appVersion != null) {
                Text(stringResource(R.string.startup_app_version, appVersion), style = MaterialTheme.typography.bodySmall)
            }
        }
    }
}

@Preview
@Composable
private fun StartupFailedPreview() {
    SkywaveTheme(dynamicColor = false) {
        StartupFailedScreen(CoreMessage.Storage("database is locked"), "0.1.0")
    }
}
