package dev.skywave.android.ui.screens

import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import dev.skywave.android.R
import dev.skywave.android.ui.ScreenScaffold
import dev.skywave.android.ui.ShellChrome
import dev.skywave.core.Action
import dev.skywave.core.PairingStatus
import dev.skywave.core.PairingView

/** Pairing with a server. Stub: shows the status; the form comes later (A09). */
@Composable
fun PairingScreen(
    view: PairingView,
    chrome: ShellChrome,
    dispatch: (Action) -> Unit,
) {
    val title = stringResource(if (view.paired == null) R.string.pairing_title else R.string.pairing_title_repair)
    ScreenScaffold(title = title, chrome = chrome) { padding ->
        Column(Modifier.fillMaxSize().padding(padding).padding(16.dp)) {
            Text(view.statusText(), style = MaterialTheme.typography.bodyLarge)
        }
    }
}

@Composable
private fun PairingView.statusText(): String = when (val s = status) {
    PairingStatus.Idle -> {
        val p = paired
        when {
            p == null -> stringResource(R.string.pairing_unpaired)
            p.serverAddress == null -> stringResource(R.string.pairing_paired, p.deviceId)
            else -> stringResource(R.string.pairing_paired_with, p.deviceId, p.serverAddress.orEmpty())
        }
    }
    is PairingStatus.InProgress -> stringResource(R.string.pairing_in_progress, s.address)
    is PairingStatus.Failed -> stringResource(R.string.pairing_failed, s.address, s.reason)
}
