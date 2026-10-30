package dev.skywave.android.ui.screens

import android.content.res.Configuration
import androidx.compose.runtime.Composable
import androidx.compose.ui.tooling.preview.Preview
import dev.skywave.android.ui.ShellChrome
import dev.skywave.android.ui.SkywaveTheme
import dev.skywave.core.BlobPolicy
import dev.skywave.core.SettingsView

/** Hand-made settings for previews and UI tests; nothing here talks to a core. */
internal object SettingsSamples {
    val about = AboutInfo(appVersion = "0.1.0", coreVersion = "0.1.0 (protocol 4)", deviceName = "Pixel 8")

    val paired = SettingsView(
        blobPolicy = BlobPolicy.ON_DEMAND,
        serverAddress = "skywave.home.arpa:7443",
        deviceId = "0192f3a4-7b1c-7d2e-9f30-4a5b6c7d8e9f",
    )

    val unpaired = SettingsView(blobPolicy = BlobPolicy.ON_DEMAND, serverAddress = null, deviceId = null)

    val noChrome = ShellChrome(banners = {}, snackbarHost = {}, onUp = {})
}

@Composable
private fun SettingsPreview(view: SettingsView) = SkywaveTheme(dynamicColor = false) {
    SettingsScreen(view, SettingsSamples.noChrome, dispatch = {}, about = SettingsSamples.about)
}

@Preview(name = "Settings: paired", heightDp = 900)
@Composable
private fun PairedPreview() = SettingsPreview(SettingsSamples.paired)

@Preview(name = "Settings: paired, eager, dark", heightDp = 900, uiMode = Configuration.UI_MODE_NIGHT_YES)
@Composable
private fun PairedDarkPreview() = SettingsPreview(SettingsSamples.paired.copy(blobPolicy = BlobPolicy.EAGER))

@Preview(name = "Settings: not paired", heightDp = 900)
@Composable
private fun UnpairedPreview() = SettingsPreview(SettingsSamples.unpaired)

@Preview(name = "Settings: not paired, dark", heightDp = 900, uiMode = Configuration.UI_MODE_NIGHT_YES)
@Composable
private fun UnpairedDarkPreview() = SettingsPreview(SettingsSamples.unpaired)
