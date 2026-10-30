package dev.skywave.android.ui

import android.content.ClipboardManager
import android.content.Intent
import android.net.Uri
import androidx.activity.ComponentActivity
import androidx.compose.foundation.layout.Column
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.test.ExperimentalTestApi
import androidx.compose.ui.test.hasAnyAncestor
import androidx.compose.ui.test.hasTestTag
import androidx.compose.ui.test.hasText
import androidx.compose.ui.test.junit4.StateRestorationTester
import androidx.compose.ui.test.junit4.v2.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.core.content.FileProvider
import dev.skywave.android.ui.screens.BlobActions
import dev.skywave.android.ui.screens.BlobHeadReader
import dev.skywave.android.ui.screens.BlobRow
import dev.skywave.android.ui.screens.BlobSamples
import dev.skywave.android.ui.screens.BlobTags
import dev.skywave.android.ui.screens.LocalBlobHeadReader
import dev.skywave.android.ui.screens.PREVIEW_BYTES
import dev.skywave.android.ui.screens.ApprovalSamples
import dev.skywave.android.ui.screens.SessionSamples
import dev.skywave.android.ui.screens.SessionScreen
import dev.skywave.android.ui.screens.TimelineTags
import dev.skywave.android.ui.screens.ViewerTags
import dev.skywave.core.Action
import dev.skywave.core.BlobHead
import dev.skywave.core.BlobState
import dev.skywave.core.BlobView
import dev.skywave.core.SessionView
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.Shadows.shadowOf
import java.io.File

/** Blobs on the JVM (Robolectric): every state, download, preview, viewer, share, copy. */
@OptIn(ExperimentalTestApi::class)
@RunWith(RobolectricTestRunner::class)
class BlobsTest {
    @get:Rule
    val compose = createAndroidComposeRule<ComponentActivity>()

    private val dispatched = mutableListOf<Action>()
    private val reads = mutableListOf<Pair<String, ULong>>()
    private var head = BlobSamples.head
    private val reader = BlobHeadReader { path, max ->
        synchronized(reads) { reads += path to max }
        head
    }

    /**
     * FileProvider caches each authority's roots for the process, and every
     * Robolectric test has its own files dir: forget the last test's.
     */
    @Before
    fun forgetFileProviderRoots() {
        val cache = FileProvider::class.java.getDeclaredField("sCache").apply { isAccessible = true }
        synchronized(cache.get(null)!!) { (cache.get(null) as MutableMap<*, *>).clear() }
    }

    private fun tag(t: String) = compose.onNodeWithTag(t)
    private fun text(t: String, substring: Boolean = false) = compose.onNodeWithText(t, substring = substring)
    private fun downloads() = dispatched.filterIsInstance<Action.DownloadBlob>()

    private fun waitForText(t: String, substring: Boolean = false) =
        compose.waitUntilAtLeastOneExists(hasText(t, substring = substring), timeoutMillis = 5_000)

    private fun rows(vararg blobs: BlobView) {
        compose.setContent {
            SkywaveTheme(dynamicColor = false) {
                CompositionLocalProvider(LocalBlobHeadReader provides reader) {
                    Column {
                        blobs.forEach { BlobRow("Output", it, BlobActions(download = { id -> dispatched += Action.DownloadBlob(id) }, open = {})) }
                    }
                }
            }
        }
    }

    private fun session(view: SessionView) {
        compose.setContent { SessionContent(view) }
        compose.waitForIdle()
    }

    @androidx.compose.runtime.Composable
    private fun SessionContent(view: SessionView) = SkywaveTheme(dynamicColor = false) {
        CompositionLocalProvider(LocalBlobHeadReader provides reader) {
            SessionScreen(view, SessionSamples.noAttention, SessionSamples.noChrome, dispatch = { dispatched += it })
        }
    }

    /** A blob file of [lines] numbered lines where the core keeps them. */
    private fun blobFile(name: String, lines: Int, width: Int = 40): File {
        val dir = File(compose.activity.filesDir, "skywave/blobs").apply { mkdirs() }
        return File(dir, name).apply {
            bufferedWriter().use { w ->
                for (i in 1..lines) w.write("line ${"%06d".format(i)} ".padEnd(width, '.') + "\n")
            }
        }
    }

    /** The session sample's shell output, available at [file], with its viewer opened. */
    private fun openViewer(file: File) {
        session(BlobSamples.session(BlobState.Available(file.path)))
        tag(TimelineTags.item("s:1")).performClick()
        tag(BlobTags.open("b-out")).performClick()
        tag(ViewerTags.VIEWER).assertExists()
    }

    @Test
    fun everyStateShowsItsSizeAndWhatCanBeDone() {
        rows(*BlobSamples.all.toTypedArray())
        text("Download (12.3 kB)").assertExists()
        compose.onAllNodes(hasText("Output · 12.3 kB")).fetchSemanticsNodes().let { assertEquals(3, it.size) }
        text("Waiting…").assertExists()
        text("3.1 MB of 10.5 MB").assertExists()
        text("3.1 MB of 10.5 MB · paused — offline").assertExists()
        text("Download failed: the size disagrees").assertExists()
        tag(BlobTags.retry("b-failed")).assertExists()
        tag(BlobTags.preview("b-ok")).assertExists()
        tag(BlobTags.open("b-ok")).assertExists()
        assertTrue(downloads().isEmpty())
    }

    @Test
    fun downloadAndRetryAskTheCore() {
        var view by mutableStateOf(BlobSamples.session(BlobState.NotDownloaded(5_000u)))
        compose.setContent { SessionContent(view) }
        tag(TimelineTags.item("s:1")).performClick()
        text("Output · 5.0 kB").assertExists()
        tag(BlobTags.download("b-out")).performClick()
        assertEquals(listOf(Action.DownloadBlob("b-out")), downloads())

        dispatched.clear()
        view = BlobSamples.session(BlobState.Failed(5_000u, "the sizes disagree"))
        text("Download failed: the sizes disagree").assertExists()
        tag(BlobTags.retry("b-out")).performClick()
        assertEquals(listOf(Action.DownloadBlob("b-out")), downloads())
    }

    @Test
    fun theApprovalPanelOffersItsBodyBlob() {
        session(ApprovalSamples.blocked(ApprovalSamples.writeTool))
        compose.onNode(hasTestTag(BlobTags.download("blob1")) and hasAnyAncestor(hasTestTag("approval_panel"))).performClick()
        assertEquals(listOf(Action.DownloadBlob("blob1")), downloads())
    }

    @Test
    fun previewReadsABoundedHeadAndSaysHowMuchMore() {
        rows(BlobSamples.available("/blobs/b-ok"))
        tag(BlobTags.preview("b-ok")).performClick()
        waitForText("Compiling crate-1 v0.1.1", substring = true)
        assertEquals(listOf("/blobs/b-ok" to PREVIEW_BYTES.toULong()), synchronized(reads) { reads.toList() })
        // 30 lines shown of the 40 read; the rest of the 48 kB is left to the viewer.
        compose.onAllNodes(hasText("crate-30 ", substring = true)).fetchSemanticsNodes().let { assertEquals(1, it.size) }
        compose.onAllNodes(hasText("crate-31 ", substring = true)).fetchSemanticsNodes().let { assertTrue(it.isEmpty()) }
        waitForText("… ", substring = true)
        text("more", substring = true).assertExists()
    }

    @Test
    fun aWholeHeadSaysNothingMore() {
        head = BlobHead("tiny", 4u)
        rows(BlobSamples.available())
        tag(BlobTags.preview("b-ok")).performClick()
        waitForText("tiny")
        compose.onAllNodes(hasText("more", substring = true)).fetchSemanticsNodes().let { assertTrue(it.isEmpty()) }
    }

    @Test
    fun anUnreadableFileSaysSo() {
        val failing = BlobHeadReader { _, _ -> throw IllegalStateException("gone") }
        compose.setContent {
            SkywaveTheme(dynamicColor = false) {
                CompositionLocalProvider(LocalBlobHeadReader provides failing) {
                    BlobRow("Output", BlobSamples.available(), BlobActions.None)
                }
            }
        }
        tag(BlobTags.preview("b-ok")).performClick()
        waitForText("Cannot read it: gone")
    }

    @Test
    fun theViewerPagesAHugeFileAndClosesBackToTheSession() {
        val big = blobFile("big", 120_000, width = 42) // 5.2 MB
        assertTrue(big.length() > 5_000_000)
        openViewer(big)
        waitForText("line 000001", substring = true)
        // Only the first page or two are read: nothing from deep in the file.
        compose.onAllNodes(hasText("line 060000", substring = true)).fetchSemanticsNodes().let { assertTrue(it.isEmpty()) }
        compose.onAllNodes(hasText("line 119999", substring = true)).fetchSemanticsNodes().let { assertTrue(it.isEmpty()) }
        waitForText("5.2 MB")
        tag(ViewerTags.CLOSE).performClick()
        tag(ViewerTags.VIEWER).assertDoesNotExist()
        tag(TimelineTags.LIST).assertExists()
    }

    @Test
    fun theViewerSurvivesRecreation() {
        val file = blobFile("small", 20)
        val restore = StateRestorationTester(compose)
        restore.setContent { SessionContent(BlobSamples.session(BlobState.Available(file.path))) }
        tag(TimelineTags.item("s:1")).performClick()
        tag(BlobTags.open("b-out")).performClick()
        waitForText("line 000001", substring = true)
        restore.emulateSavedInstanceStateRestore()
        tag(ViewerTags.VIEWER).assertExists()
        waitForText("line 000001", substring = true)
    }

    private fun startedTarget(): Pair<Intent, Intent> {
        val started = shadowOf(compose.activity).nextStartedActivity
        assertEquals(Intent.ACTION_CHOOSER, started.action)
        @Suppress("DEPRECATION")
        val target = started.getParcelableExtra<Intent>(Intent.EXTRA_INTENT)!!
        return started to target
    }

    private fun assertBlobUri(uri: Uri?) {
        assertEquals("content", uri?.scheme)
        assertEquals("${compose.activity.packageName}.blobs", uri?.authority)
    }

    @Test
    fun shareSendsAReadOnlyContentUri() {
        openViewer(blobFile("share", 10))
        tag(ViewerTags.SHARE).performClick()
        val (_, target) = startedTarget()
        assertEquals(Intent.ACTION_SEND, target.action)
        assertEquals("text/plain", target.type)
        @Suppress("DEPRECATION")
        assertBlobUri(target.getParcelableExtra(Intent.EXTRA_STREAM))
        assertBlobUri(target.clipData?.getItemAt(0)?.uri)
        assertTrue(target.flags and Intent.FLAG_GRANT_READ_URI_PERMISSION != 0)
        assertTrue(target.flags and Intent.FLAG_GRANT_WRITE_URI_PERMISSION == 0)
    }

    @Test
    fun openWithViewsAReadOnlyContentUri() {
        openViewer(blobFile("open", 10))
        tag(ViewerTags.OPEN_WITH).performClick()
        tag(ViewerTags.NOTICE).assertDoesNotExist()
        val (_, target) = startedTarget()
        assertEquals(Intent.ACTION_VIEW, target.action)
        assertEquals("text/plain", target.type)
        assertBlobUri(target.data)
        assertTrue(target.flags and Intent.FLAG_GRANT_READ_URI_PERMISSION != 0)
    }

    @Test
    fun copyTakesASmallFile() {
        openViewer(blobFile("copy", 3))
        tag(ViewerTags.COPY).performClick()
        waitForText("Copied")
        val clip = compose.activity.getSystemService(ClipboardManager::class.java).primaryClip
        assertTrue(clip!!.getItemAt(0).text.startsWith("line 000001"))
    }

    @Test
    fun copyRefusesAFileOverTheCap() {
        openViewer(blobFile("huge", 10_000)) // 400 kB, over 256 KiB
        tag(ViewerTags.COPY).performClick()
        waitForText("too much to copy", substring = true)
        assertNull(compose.activity.getSystemService(ClipboardManager::class.java).primaryClip)
    }
}
