package dev.skywave.android.ui.screens

import android.content.ClipData
import android.os.Build
import android.widget.Toast
import androidx.annotation.StringRes
import androidx.compose.foundation.clickable
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.selection.selectable
import androidx.compose.foundation.selection.selectableGroup
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.ListItem
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.RadioButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.Immutable
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.ClipEntry
import androidx.compose.ui.platform.LocalClipboard
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.heading
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import dev.skywave.android.R
import dev.skywave.android.ui.ScreenScaffold
import dev.skywave.android.ui.ShellChrome
import dev.skywave.core.Action
import dev.skywave.core.BlobPolicy
import dev.skywave.core.Route
import dev.skywave.core.SettingsView
import kotlinx.coroutines.launch

/**
 * What Settings shows about this build and device that is not in the
 * core's view. Read once outside composition: [coreVersion] is a native
 * call, and the JVM tests cannot load the library.
 */
@Immutable
data class AboutInfo(
    /** The app's versionName; null if the package manager would not say. */
    val appVersion: String?,
    /** The core's crate and protocol version, e.g. "0.1.0 (protocol 4)". */
    val coreVersion: String,
    /** The name this device gives the server when it pairs (D17). */
    val deviceName: String,
)

/**
 * Settings (D2: the core owns every rule). Picking a download policy only
 * tells the core; what switching starts or drops is the core's business,
 * and the choice shown is always the one in [view].
 */
@Composable
fun SettingsScreen(view: SettingsView, chrome: ShellChrome, dispatch: (Action) -> Unit, about: AboutInfo) {
    ScreenScaffold(title = stringResource(R.string.settings_title), chrome = chrome) { padding ->
        Column(
            Modifier
                .fillMaxSize()
                .verticalScroll(rememberScrollState())
                .padding(padding),
        ) {
            SectionHeader(R.string.settings_section_downloads)
            Column(Modifier.selectableGroup()) {
                PolicyOption(
                    BlobPolicy.ON_DEMAND, view.blobPolicy, dispatch,
                    R.string.settings_downloads_on_demand, R.string.settings_downloads_on_demand_body,
                )
                PolicyOption(
                    BlobPolicy.EAGER, view.blobPolicy, dispatch,
                    R.string.settings_downloads_eager, R.string.settings_downloads_eager_body,
                )
            }
            // A15: the background-sync toggle belongs here, under Downloads.

            HorizontalDivider()
            SectionHeader(R.string.settings_section_server)
            ServerSection(view, about.deviceName, onPair = { dispatch(Action.Navigate(Route.PAIRING)) })

            HorizontalDivider()
            SectionHeader(R.string.settings_section_about)
            Setting(
                R.string.settings_app_version,
                about.appVersion ?: stringResource(R.string.settings_version_unknown),
            )
            Setting(R.string.settings_core_version, about.coreVersion)
        }
    }
}

@Composable
private fun SectionHeader(@StringRes title: Int) {
    Text(
        stringResource(title),
        style = MaterialTheme.typography.titleSmall,
        color = MaterialTheme.colorScheme.primary,
        modifier = Modifier
            .padding(start = 16.dp, end = 16.dp, top = 24.dp, bottom = 8.dp)
            .semantics { heading() },
    )
}

/** One radio row; choosing the policy already in force sends nothing. */
@Composable
private fun PolicyOption(
    policy: BlobPolicy,
    current: BlobPolicy,
    dispatch: (Action) -> Unit,
    @StringRes title: Int,
    @StringRes body: Int,
) {
    val selected = policy == current
    ListItem(
        headlineContent = { Text(stringResource(title)) },
        supportingContent = { Text(stringResource(body)) },
        // The row is the control; the button only shows the state.
        leadingContent = { RadioButton(selected = selected, onClick = null) },
        modifier = Modifier.selectable(
            selected = selected,
            role = Role.RadioButton,
            onClick = { if (!selected) dispatch(Action.SetBlobPolicy(policy)) },
        ),
    )
}

@Composable
private fun ServerSection(view: SettingsView, deviceName: String, onPair: () -> Unit) {
    val address = view.serverAddress
    val deviceId = view.deviceId
    val paired = address != null || deviceId != null
    if (paired) {
        address?.let { Setting(R.string.settings_server_address, it) }
        deviceId?.let { DeviceId(it) }
    } else {
        ListItem(
            headlineContent = { Text(stringResource(R.string.settings_not_paired)) },
            supportingContent = { Text(stringResource(R.string.settings_not_paired_body)) },
        )
    }
    Setting(R.string.settings_device_name, deviceName)
    ListItem(
        headlineContent = {
            Text(
                stringResource(if (paired) R.string.settings_pair_again else R.string.settings_pair),
                color = MaterialTheme.colorScheme.primary,
            )
        },
        supportingContent = if (paired) {
            { Text(stringResource(R.string.settings_pair_again_body)) }
        } else {
            null
        },
        modifier = Modifier.clickable(role = Role.Button, onClick = onPair),
    )
}

/** The device id in monospace; tap or long-press copies it. */
@Composable
private fun DeviceId(id: String) {
    val clipboard = LocalClipboard.current
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    val copiedText = stringResource(R.string.settings_device_id_copied)
    val label = stringResource(R.string.settings_device_id)
    val copy: () -> Unit = {
        scope.launch {
            clipboard.setClipEntry(ClipEntry(ClipData.newPlainText(label, id)))
            // Android 13 and later confirm a copy themselves.
            if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) {
                Toast.makeText(context, copiedText, Toast.LENGTH_SHORT).show()
            }
        }
    }
    ListItem(
        headlineContent = { Text(label) },
        supportingContent = { Text(id, fontFamily = FontFamily.Monospace) },
        modifier = Modifier.combinedClickable(
            onClickLabel = stringResource(R.string.settings_device_id_copy),
            onLongClick = copy,
            onClick = copy,
        ),
    )
}

@Composable
private fun Setting(@StringRes title: Int, value: String) {
    ListItem(
        headlineContent = { Text(stringResource(title)) },
        supportingContent = { Text(value) },
    )
}
