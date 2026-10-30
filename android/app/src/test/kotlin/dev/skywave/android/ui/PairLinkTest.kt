package dev.skywave.android.ui

import android.content.Intent
import android.net.Uri
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner

/** Which intents are pairing links, read with a fake parser. */
@RunWith(RobolectricTestRunner::class)
class PairLinkTest {
    private val seen = mutableListOf<String>()
    private val parser = PairUriParser { uri ->
        seen += uri
        PairInput.Details("host:7443", "CODE", "FP")
    }

    private fun view(uri: String) = Intent(Intent.ACTION_VIEW, Uri.parse(uri))

    @Test
    fun aPairLinkIsHandedToTheParserWhole() {
        val uri = "skywave://pair?addr=10.0.2.2%3A7443&code=ABCDE12345&fp=ab12"
        assertEquals(PairInput.Details("host:7443", "CODE", "FP"), pairLinkInput(view(uri), parser))
        assertEquals(listOf(uri), seen)
    }

    @Test
    fun schemeAndHostAreCaseInsensitive() {
        assertEquals(PairInput.Details("host:7443", "CODE", "FP"), pairLinkInput(view("SKYWAVE://Pair?addr=x"), parser))
    }

    @Test
    fun otherIntentsAreNotPairLinks() {
        assertNull(pairLinkInput(null, parser))
        assertNull(pairLinkInput(Intent(Intent.ACTION_MAIN).addCategory(Intent.CATEGORY_LAUNCHER), parser))
        assertNull(pairLinkInput(view("skywave://session/1"), parser))
        assertNull(pairLinkInput(view("https://pair/?addr=x"), parser))
        assertNull(pairLinkInput(Intent(Intent.ACTION_SEND, Uri.parse("skywave://pair?addr=x")), parser))
        assertEquals(emptyList<String>(), seen)
    }
}
