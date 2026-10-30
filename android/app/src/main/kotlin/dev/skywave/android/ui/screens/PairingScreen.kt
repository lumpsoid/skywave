package dev.skywave.android.ui.screens

import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.consumeWindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.LocalTextStyle
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardCapitalization
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import com.journeyapps.barcodescanner.ScanContract
import com.journeyapps.barcodescanner.ScanOptions
import dev.skywave.android.R
import dev.skywave.android.ui.PairInbox
import dev.skywave.android.ui.PairInput
import dev.skywave.android.ui.ScreenScaffold
import dev.skywave.android.ui.ShellChrome
import dev.skywave.core.Action
import dev.skywave.core.PairedInfo
import dev.skywave.core.PairingStatus
import dev.skywave.core.PairingView

/** Test tags of the form's fields. */
internal object PairingTags {
    const val ADDRESS = "pairing_address"
    const val CODE = "pairing_code"
    const val FINGERPRINT = "pairing_fingerprint"
    const val PAIR = "pairing_pair"
    const val CONFIRM = "pairing_confirm"
}

/**
 * Pairing with a server: the address, code and fingerprint `skywaved pair`
 * printed, typed, scanned from its QR code or taken from a `skywave://pair`
 * link ([inbox]). What the user typed goes to the core as is, trimmed only;
 * the core normalises and checks it (D2) and reports in [PairingView.status].
 * A scan or link only fills the form: the user checks the fingerprint and
 * taps Pair. On success the core moves to the session list.
 */
@Composable
fun PairingScreen(
    view: PairingView,
    chrome: ShellChrome,
    dispatch: (Action) -> Unit,
    inbox: PairInbox,
) {
    var address by rememberSaveable { mutableStateOf("") }
    var code by rememberSaveable { mutableStateOf("") }
    var fingerprint by rememberSaveable { mutableStateOf("") }
    // What the last link or scan left for the user to read.
    var filledFromLink by rememberSaveable { mutableStateOf(false) }
    var unreadable by rememberSaveable { mutableStateOf<String?>(null) }
    var confirming by rememberSaveable { mutableStateOf(false) }

    val pending = inbox.pending
    LaunchedEffect(pending) {
        when (pending) {
            null -> return@LaunchedEffect
            is PairInput.Details -> {
                address = pending.address
                code = pending.code
                fingerprint = pending.fingerprint
                filledFromLink = true
                unreadable = null
            }
            is PairInput.Unreadable -> {
                unreadable = pending.reason
                filledFromLink = false
            }
        }
        inbox.consume(pending)
    }

    val scanPrompt = stringResource(R.string.pairing_scan_prompt)
    val scanner = rememberLauncherForActivityResult(ScanContract()) { result ->
        result.contents?.let(inbox::offerUri) // null: cancelled
    }

    val inProgress = view.status is PairingStatus.InProgress
    val canPair = !inProgress && address.isNotBlank() && code.isNotBlank() && fingerprint.isNotBlank()
    val pair = {
        unreadable = null
        filledFromLink = false
        dispatch(Action.Pair(address.trim(), code.trim(), fingerprint.trim()))
    }
    // Pairing with another server resets the local replica: ask first.
    val submit = {
        if (canPair) {
            if (view.paired != null) confirming = true else pair()
        }
    }

    val title = stringResource(if (view.paired == null) R.string.pairing_title else R.string.pairing_title_repair)
    ScreenScaffold(title = title, chrome = chrome) { padding ->
        Column(
            Modifier
                .fillMaxSize()
                .padding(padding)
                .consumeWindowInsets(padding)
                .imePadding()
                .verticalScroll(rememberScrollState())
                .padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            view.paired?.let { CurrentPairing(it) }
            Text(stringResource(R.string.pairing_instructions), style = MaterialTheme.typography.bodyMedium)
            OutlinedButton(
                onClick = {
                    scanner.launch(
                        ScanOptions()
                            .setDesiredBarcodeFormats(ScanOptions.QR_CODE)
                            .setPrompt(scanPrompt)
                            .setBeepEnabled(false)
                            .setOrientationLocked(false),
                    )
                },
                enabled = !inProgress,
                modifier = Modifier.fillMaxWidth(),
            ) { Text(stringResource(R.string.pairing_scan)) }
            unreadable?.let {
                Text(
                    stringResource(R.string.pairing_unreadable, it),
                    color = MaterialTheme.colorScheme.error,
                    style = MaterialTheme.typography.bodyMedium,
                )
            }

            val mono = LocalTextStyle.current.copy(fontFamily = FontFamily.Monospace)
            OutlinedTextField(
                value = address,
                onValueChange = { address = it },
                label = { Text(stringResource(R.string.pairing_address)) },
                placeholder = { Text(stringResource(R.string.pairing_address_hint)) },
                singleLine = true,
                enabled = !inProgress,
                keyboardOptions = KeyboardOptions(
                    keyboardType = KeyboardType.Uri,
                    autoCorrectEnabled = false,
                    imeAction = ImeAction.Next,
                ),
                modifier = Modifier.fillMaxWidth().testTag(PairingTags.ADDRESS),
            )
            OutlinedTextField(
                value = code,
                onValueChange = { code = it },
                label = { Text(stringResource(R.string.pairing_code)) },
                placeholder = { Text(stringResource(R.string.pairing_code_hint), style = mono) },
                singleLine = true,
                enabled = !inProgress,
                textStyle = mono,
                keyboardOptions = KeyboardOptions(
                    capitalization = KeyboardCapitalization.Characters,
                    keyboardType = KeyboardType.Ascii,
                    autoCorrectEnabled = false,
                    imeAction = ImeAction.Next,
                ),
                modifier = Modifier.fillMaxWidth().testTag(PairingTags.CODE),
            )
            OutlinedTextField(
                value = fingerprint,
                onValueChange = { fingerprint = it },
                label = { Text(stringResource(R.string.pairing_fingerprint)) },
                placeholder = { Text(stringResource(R.string.pairing_fingerprint_hint), style = mono) },
                maxLines = 3,
                enabled = !inProgress,
                textStyle = mono,
                keyboardOptions = KeyboardOptions(
                    capitalization = KeyboardCapitalization.Characters,
                    keyboardType = KeyboardType.Ascii,
                    autoCorrectEnabled = false,
                    imeAction = ImeAction.Done,
                ),
                keyboardActions = KeyboardActions(onDone = { submit() }),
                modifier = Modifier.fillMaxWidth().testTag(PairingTags.FINGERPRINT),
            )
            if (filledFromLink) {
                Text(stringResource(R.string.pairing_check_fingerprint), style = MaterialTheme.typography.bodyMedium)
            }

            Button(onClick = submit, enabled = canPair, modifier = Modifier.fillMaxWidth().testTag(PairingTags.PAIR)) {
                Text(stringResource(R.string.pairing_pair))
            }
            Status(view.status)
        }
    }

    if (confirming) {
        AlertDialog(
            onDismissRequest = { confirming = false },
            title = { Text(stringResource(R.string.pairing_confirm_title)) },
            text = { Text(stringResource(R.string.pairing_confirm_body, address.trim())) },
            confirmButton = {
                TextButton(
                    onClick = {
                        confirming = false
                        pair()
                    },
                    modifier = Modifier.testTag(PairingTags.CONFIRM),
                ) { Text(stringResource(R.string.pairing_confirm_pair)) }
            },
            dismissButton = {
                TextButton(onClick = { confirming = false }) { Text(stringResource(R.string.pairing_confirm_cancel)) }
            },
        )
    }
}

/** Who this device is paired with now, and what pairing elsewhere costs. */
@Composable
private fun CurrentPairing(paired: PairedInfo) {
    Card(
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.secondaryContainer),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
            val server = paired.serverAddress
            Text(
                if (server == null) {
                    stringResource(R.string.pairing_paired, paired.deviceId)
                } else {
                    stringResource(R.string.pairing_paired_with, paired.deviceId, server)
                },
                style = MaterialTheme.typography.bodyLarge,
            )
            Text(stringResource(R.string.pairing_replaces_data), style = MaterialTheme.typography.bodyMedium)
        }
    }
}

@Composable
private fun Status(status: PairingStatus) {
    when (status) {
        PairingStatus.Idle -> Unit
        is PairingStatus.InProgress -> Row(
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            CircularProgressIndicator(Modifier.size(20.dp), strokeWidth = 2.dp)
            Text(stringResource(R.string.pairing_in_progress, status.address), style = MaterialTheme.typography.bodyLarge)
        }
        is PairingStatus.Failed -> Text(
            stringResource(R.string.pairing_failed, status.address, status.reason),
            color = MaterialTheme.colorScheme.error,
            style = MaterialTheme.typography.bodyLarge,
        )
    }
}
