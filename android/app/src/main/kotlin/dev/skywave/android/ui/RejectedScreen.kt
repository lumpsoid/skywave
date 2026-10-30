package dev.skywave.android.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import dev.skywave.android.R
import dev.skywave.core.LinkRejection

/**
 * The server refused this device for good. Takes the whole screen until
 * the user pairs again or chooses to keep reading the local replica.
 */
@Composable
fun RejectedScreen(
    rejection: LinkRejection,
    onPairAgain: () -> Unit,
    onKeepReading: () -> Unit,
    contentPadding: PaddingValues,
    modifier: Modifier = Modifier,
) {
    val (title, body) = when (rejection) {
        LinkRejection.Revoked ->
            stringResource(R.string.rejected_revoked_title) to stringResource(R.string.rejected_revoked_body)
        is LinkRejection.ProtocolMismatch ->
            stringResource(R.string.rejected_mismatch_title) to
                stringResource(R.string.rejected_mismatch_body, rejection.client.toLong(), rejection.server.toLong())
    }
    Column(
        modifier = modifier
            .fillMaxSize()
            .padding(contentPadding)
            .verticalScroll(rememberScrollState())
            .padding(32.dp),
        verticalArrangement = Arrangement.spacedBy(16.dp, Alignment.CenterVertically),
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Text(title, style = MaterialTheme.typography.headlineSmall, textAlign = TextAlign.Center)
        Text(body, style = MaterialTheme.typography.bodyLarge, textAlign = TextAlign.Center)
        // Pairing again cannot fix a version gap; only updating can.
        if (rejection == LinkRejection.Revoked) {
            Button(onClick = onPairAgain) { Text(stringResource(R.string.rejected_pair_again)) }
        }
        TextButton(onClick = onKeepReading) { Text(stringResource(R.string.rejected_keep_reading)) }
    }
}
