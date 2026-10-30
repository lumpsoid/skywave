package dev.skywave.android.ui.screens

import android.content.res.Configuration
import androidx.compose.runtime.Composable
import androidx.compose.ui.tooling.preview.Preview
import dev.skywave.android.ui.ShellChrome
import dev.skywave.android.ui.SkywaveTheme
import dev.skywave.core.BlobPolicy
import dev.skywave.core.SettingsView
import kotlinx.coroutines.flow.MutableStateFlow

/** Hand-made settings for previews and UI tests; nothing here talks to a core. */
internal object SettingsSamples {
    val about = AboutInfo(appVersion = "0.1.0", coreVersion = "0.1.0 (protocol 4)", deviceName = "Pixel 8")

    val paired = SettingsView(
        blobPolicy = BlobPolicy.ON_DEMAND,
        serverAddress = "skywave.home.arpa:7443",
        deviceId = "0192f3a4-7b1c-7d2e-9f30-4a5b6c7d8e9f",
        serverFingerprint = "3F:A1:09:C4:7E:22:B8:5D:91:0C:E6:4A:F3:18:77:2B:" +
            "D0:5E:89:A2:4C:13:FB:66:0E:9D:C7:31:58:AF:24:E0",
    )

    val unpaired = SettingsView(
        blobPolicy = BlobPolicy.ON_DEMAND,
        serverAddress = null,
        deviceId = null,
        serverFingerprint = null,
    )

    val noChrome = ShellChrome(banners = {}, snackbarHost = {}, onUp = {})

    /** Off, and stays off: previews and tests that do not look at it. */
    val stayConnected: StayConnectedSetting = InMemoryStayConnected(false)
}

/** A [StayConnectedSetting] that only remembers; [changes] records each [set]. */
internal class InMemoryStayConnected(initial: Boolean) : StayConnectedSetting {
    override val on = MutableStateFlow(initial)
    val changes = mutableListOf<Boolean>()

    override fun set(on: Boolean) {
        changes += on
        this.on.value = on
    }
}

@Composable
private fun SettingsPreview(view: SettingsView, stayConnected: Boolean = false) = SkywaveTheme(dynamicColor = false) {
    SettingsScreen(
        view,
        SettingsSamples.noChrome,
        dispatch = {},
        about = SettingsSamples.about,
        stayConnected = InMemoryStayConnected(stayConnected),
    )
}

@Preview(name = "Settings: paired", heightDp = 900)
@Composable
private fun PairedPreview() = SettingsPreview(SettingsSamples.paired)

@Preview(name = "Settings: paired, eager, staying connected, dark", heightDp = 900, uiMode = Configuration.UI_MODE_NIGHT_YES)
@Composable
private fun PairedDarkPreview() =
    SettingsPreview(SettingsSamples.paired.copy(blobPolicy = BlobPolicy.EAGER), stayConnected = true)

@Preview(name = "Settings: not paired", heightDp = 900)
@Composable
private fun UnpairedPreview() = SettingsPreview(SettingsSamples.unpaired)

@Preview(name = "Settings: not paired, dark", heightDp = 900, uiMode = Configuration.UI_MODE_NIGHT_YES)
@Composable
private fun UnpairedDarkPreview() = SettingsPreview(SettingsSamples.unpaired)
