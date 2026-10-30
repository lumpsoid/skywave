package dev.skywave.android.ui

import android.content.ClipboardManager
import androidx.activity.ComponentActivity
import androidx.compose.ui.test.assertIsNotSelected
import androidx.compose.ui.test.assertIsSelected
import androidx.compose.ui.test.junit4.v2.createAndroidComposeRule
import androidx.compose.ui.test.longClick
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performScrollTo
import androidx.compose.ui.test.performTouchInput
import dev.skywave.android.ui.screens.AboutInfo
import dev.skywave.android.ui.screens.SettingsSamples
import dev.skywave.android.ui.screens.SettingsScreen
import dev.skywave.core.Action
import dev.skywave.core.BlobPolicy
import dev.skywave.core.Route
import dev.skywave.core.SettingsView
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner

/** Settings on the JVM (Robolectric): the view in, actions out, versions injected. */
@RunWith(RobolectricTestRunner::class)
class SettingsScreenTest {
    @get:Rule
    val compose = createAndroidComposeRule<ComponentActivity>()

    private val dispatched = mutableListOf<Action>()

    private fun show(view: SettingsView, about: AboutInfo = SettingsSamples.about) {
        compose.setContent {
            SkywaveTheme(dynamicColor = false) {
                SettingsScreen(view, SettingsSamples.noChrome, dispatch = { dispatched += it }, about = about)
            }
        }
    }

    private fun text(t: String) = compose.onNodeWithText(t)

    @Test
    fun showsTheViewsPolicy() {
        show(SettingsSamples.paired.copy(blobPolicy = BlobPolicy.EAGER))
        text("Download right away").assertIsSelected()
        text("Download when opened").assertIsNotSelected()
    }

    @Test
    fun choosingAPolicyTellsTheCoreAndNothingElse() {
        show(SettingsSamples.paired.copy(blobPolicy = BlobPolicy.ON_DEMAND))
        text("Download right away").performClick()
        assertEquals(listOf<Action>(Action.SetBlobPolicy(BlobPolicy.EAGER)), dispatched)
        // The core has not answered: the view's policy is still the one shown.
        text("Download when opened").assertIsSelected()
    }

    @Test
    fun choosingOnDemandFromEager() {
        show(SettingsSamples.paired.copy(blobPolicy = BlobPolicy.EAGER))
        text("Download when opened").performClick()
        text("Download right away").performClick() // already in force: nothing sent
        assertEquals(listOf<Action>(Action.SetBlobPolicy(BlobPolicy.ON_DEMAND)), dispatched)
    }

    @Test
    fun pairedShowsServerAndDeviceAndPairsAgain() {
        show(SettingsSamples.paired)
        text("skywave.home.arpa:7443").assertExists()
        text("0192f3a4-7b1c-7d2e-9f30-4a5b6c7d8e9f").assertExists()
        text("Not paired").assertDoesNotExist()
        text("Pair again").performScrollTo().performClick()
        assertEquals(listOf<Action>(Action.Navigate(Route.PAIRING)), dispatched)
    }

    @Test
    fun unpairedSaysSoAndOffersPairing() {
        show(SettingsSamples.unpaired)
        text("Not paired").assertExists()
        text("Device ID").assertDoesNotExist()
        text("Pair with a server").performScrollTo().performClick()
        assertEquals(listOf<Action>(Action.Navigate(Route.PAIRING)), dispatched)
    }

    @Test
    fun versionsAndDeviceNameComeFromWhatWasInjected() {
        show(SettingsSamples.unpaired, AboutInfo(appVersion = "9.8.7", coreVersion = "1.2.3 (protocol 42)", deviceName = "Lab phone"))
        text("Name sent when pairing").assertExists()
        text("Lab phone").performScrollTo().assertExists()
        text("9.8.7").performScrollTo().assertExists()
        text("1.2.3 (protocol 42)").performScrollTo().assertExists()
    }

    @Test
    fun unknownAppVersionSaysSo() {
        show(SettingsSamples.paired, SettingsSamples.about.copy(appVersion = null))
        text("Unknown").performScrollTo().assertExists()
    }

    @Test
    fun tappingOrLongPressingTheDeviceIdCopiesIt() {
        val clipboard = compose.activity.getSystemService(ClipboardManager::class.java)
        show(SettingsSamples.paired)
        val id = "0192f3a4-7b1c-7d2e-9f30-4a5b6c7d8e9f"
        text(id).performScrollTo().performClick()
        compose.waitForIdle()
        assertEquals(id, clipboard.primaryClip?.getItemAt(0)?.text)

        clipboard.clearPrimaryClip()
        text(id).performTouchInput { longClick() }
        compose.waitForIdle()
        assertEquals(id, clipboard.primaryClip?.getItemAt(0)?.text)
        assertEquals(emptyList<Action>(), dispatched)
    }
}
