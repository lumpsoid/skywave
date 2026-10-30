package dev.skywave.android.ui

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.compose.ui.test.assertIsEnabled
import androidx.compose.ui.test.assertIsNotEnabled
import androidx.compose.ui.test.assertTextContains
import androidx.compose.ui.test.junit4.v2.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performScrollTo
import androidx.compose.ui.test.performTextInput
import dev.skywave.android.ui.screens.PairingTags
import dev.skywave.android.ui.screens.SettingsSamples
import dev.skywave.core.Action
import dev.skywave.core.Link
import dev.skywave.core.Screen
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner

/** The pairing form on the JVM (Robolectric), with a fake link reader. */
@RunWith(RobolectricTestRunner::class)
class PairingScreenTest {
    @get:Rule
    val compose = createAndroidComposeRule<ComponentActivity>()

    private val dispatched = mutableListOf<Action>()

    private val inbox = PairInbox { uri ->
        if (uri == GOOD_URI) PairInput.Details("10.0.2.2:7443", CODE, FP) else PairInput.Unreadable("not a skywave:// link")
    }

    private fun show(screen: Screen.Pairing) {
        // The core reports Unpaired until the first pairing succeeds.
        val link = if (screen.view.paired == null) Link.Unpaired else Link.Connecting
        compose.setContent {
            SkywaveTheme(dynamicColor = false) {
                AppShell(Samples.state(link, screen), dispatch = { dispatched += it }, inbox, SettingsSamples.about)
            }
        }
    }

    private fun tag(t: String) = compose.onNodeWithTag(t)

    private fun type(address: String, code: String, fingerprint: String) {
        tag(PairingTags.ADDRESS).performTextInput(address)
        tag(PairingTags.CODE).performTextInput(code)
        tag(PairingTags.FINGERPRINT).performTextInput(fingerprint)
    }

    @Test
    fun pairSendsWhatWasTypedTrimmed() {
        show(Samples.unpaired)
        tag(PairingTags.PAIR).assertIsNotEnabled() // nothing typed yet
        type(" 10.0.2.2:7443 ", "abcde-12345 ", " ab:cd")
        tag(PairingTags.PAIR).performScrollTo().assertIsEnabled().performClick()
        assertEquals(listOf<Action>(Action.Pair("10.0.2.2:7443", "abcde-12345", "ab:cd")), dispatched)
    }

    @Test
    fun inProgressDisablesPairAndShowsProgress() {
        show(Samples.pairingInProgress)
        compose.onNodeWithText("Pairing with 10.0.2.2:7443…").assertExists()
        tag(PairingTags.PAIR).assertIsNotEnabled()
        compose.onNodeWithText("Scan QR code").assertIsNotEnabled()
    }

    @Test
    fun failedShowsTheCoresReason() {
        show(Samples.pairingFailed)
        compose.onNodeWithText("the server's fingerprint does not match", substring = true).assertExists()
        compose.onNodeWithText("Pairing with 10.0.2.2:7443 failed", substring = true).assertExists()
    }

    @Test
    fun pairingAgainAsksFirstAndPairsOnlyOnConfirm() {
        show(Samples.repair)
        compose.onNodeWithText("Paired as dev-7f3a with skywave.home.arpa:7443").assertExists()
        compose.onNodeWithText("deletes the sessions stored on this device", substring = true).assertExists()
        type("other.example:7443", CODE, FP)

        tag(PairingTags.PAIR).performScrollTo().performClick()
        compose.onNodeWithText("Pair with another server?").assertExists()
        compose.onNodeWithText("Cancel").performClick()
        compose.onNodeWithText("Pair with another server?").assertDoesNotExist()
        assertEquals(emptyList<Action>(), dispatched)

        tag(PairingTags.PAIR).performScrollTo().performClick()
        tag(PairingTags.CONFIRM).performClick()
        assertEquals(listOf<Action>(Action.Pair("other.example:7443", CODE, FP)), dispatched)
    }

    @Test
    fun aLinkFillsTheFormWithoutPairing() {
        show(Samples.unpaired)
        compose.runOnUiThread { inbox.offerUri(GOOD_URI) }
        compose.waitForIdle()
        tag(PairingTags.ADDRESS).assertTextContains("10.0.2.2:7443")
        tag(PairingTags.CODE).assertTextContains(CODE)
        tag(PairingTags.FINGERPRINT).assertTextContains(FP)
        compose.onNodeWithText("Check that the fingerprint matches", substring = true).assertExists()
        assertNull(inbox.pending) // taken, once
        assertEquals(emptyList<Action>(), dispatched)
        tag(PairingTags.PAIR).assertIsEnabled()
    }

    @Test
    fun anUnreadableLinkShowsWhy() {
        show(Samples.unpaired)
        compose.runOnUiThread { inbox.offerUri("https://example.org/") }
        compose.waitForIdle()
        compose.onNodeWithText("Not a Skywave pairing code: not a skywave:// link").assertExists()
        assertNull(inbox.pending)
        assertEquals(emptyList<Action>(), dispatched)
    }

    @Test
    fun aLinkNotYetShownSurvivesSavedState() {
        for (uri in listOf(GOOD_URI, "nope")) {
            inbox.offerUri(uri)
            val saved = Bundle().also(inbox::saveTo)
            val again = PairInbox(inbox.parser).apply { restoreFrom(saved) }
            assertEquals(inbox.pending, again.pending)
        }
        assertNull(PairInbox(inbox.parser).apply { restoreFrom(Bundle()) }.pending)
    }

    private companion object {
        const val CODE = "ABCDE12345"
        const val FP = "ab12cd34"
        const val GOOD_URI = "skywave://pair?addr=10.0.2.2%3A7443&code=ABCDE12345&fp=ab12cd34"
    }
}
