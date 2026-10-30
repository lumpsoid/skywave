package dev.skywave.android.ui.screens

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.Immutable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.saveable.listSaver
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.runtime.staticCompositionLocalOf
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import dev.skywave.android.R
import dev.skywave.core.BlobHead
import dev.skywave.core.BlobState
import dev.skywave.core.BlobView
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import java.io.File

/** Bytes of a downloaded blob read for its inline preview. */
internal const val PREVIEW_BYTES = 6 * 1024

/** Lines of the inline preview; the viewer shows the rest. */
private const val PREVIEW_LINES = 30

internal object BlobTags {
    fun row(id: String) = "blob:$id"
    fun download(id: String) = "blob_download:$id"
    fun retry(id: String) = "blob_retry:$id"
    fun preview(id: String) = "blob_preview:$id"
    fun open(id: String) = "blob_open:$id"
    fun head(id: String) = "blob_head:$id"
}

/**
 * Reads the start of a downloaded blob's file. In the app this is the core's
 * `readHead` (`core/BlobHeads.kt`); tests use a fake, as the native core does
 * not load on the JVM. Blocking: called off the main thread. Throws when the
 * file cannot be read.
 */
fun interface BlobHeadReader {
    fun read(path: String, max: ULong): BlobHead
}

/**
 * The [BlobHeadReader] the inline previews use. Provided by the Activity;
 * without one a preview says it cannot read the file.
 */
val LocalBlobHeadReader = staticCompositionLocalOf<BlobHeadReader> {
    BlobHeadReader { _, _ -> throw IllegalStateException("no BlobHeadReader provided") }
}

/** A downloaded blob the full viewer shows: its file and what to call it. */
@Immutable
internal data class OpenBlob(val id: String, val path: String, val title: String) {
    companion object {
        /** Saves the viewer's blob, or none, with the screen's state. */
        val Saver = listSaver<OpenBlob?, String>(
            save = { it?.let { b -> listOf(b.id, b.path, b.title) }.orEmpty() },
            restore = { if (it.size == 3) OpenBlob(it[0], it[1], it[2]) else null },
        )
    }
}

/** What a blob row can ask of the screen: download one (D2: the core fetches), or open one. */
@Immutable
internal class BlobActions(val download: (String) -> Unit, val open: (OpenBlob) -> Unit) {
    companion object {
        val None = BlobActions(download = {}, open = {})
    }
}

/**
 * One blob: what it is ([label]), its size and download state, and what can
 * be done with it now: download, retry, preview inline or open in the viewer
 * (titled [title]). Every state shows the size (8a0464).
 */
@Composable
internal fun BlobRow(label: String, blob: BlobView, actions: BlobActions, title: String = label) {
    val colors = MaterialTheme.colorScheme
    val id = blob.id
    Column(Modifier.fillMaxWidth().testTag(BlobTags.row(id)), verticalArrangement = Arrangement.spacedBy(2.dp)) {
        when (val s = blob.state) {
            is BlobState.NotDownloaded -> Row(verticalAlignment = Alignment.CenterVertically) {
                Heading(label, s.size, Modifier.weight(1f))
                TextButton(onClick = { actions.download(id) }, modifier = Modifier.testTag(BlobTags.download(id))) {
                    Text(stringResource(R.string.blob_download, formatBytes(s.size)))
                }
            }
            is BlobState.Requested -> {
                Heading(label, s.size)
                Minor(stringResource(R.string.blob_waiting))
            }
            is BlobState.Downloading -> {
                Heading(label, s.size)
                val fraction = if (s.size == 0uL) 0f else (s.got.toDouble() / s.size.toDouble()).toFloat().coerceIn(0f, 1f)
                LinearProgressIndicator(progress = { fraction }, modifier = Modifier.fillMaxWidth())
                Minor(
                    stringResource(
                        if (s.paused) R.string.blob_paused else R.string.blob_progress,
                        formatBytes(s.got),
                        formatBytes(s.size),
                    ),
                )
            }
            is BlobState.Failed -> Row(verticalAlignment = Alignment.CenterVertically) {
                Column(Modifier.weight(1f)) {
                    Heading(label, s.size)
                    Minor(stringResource(R.string.blob_failed, s.reason), color = colors.error)
                }
                TextButton(onClick = { actions.download(id) }, modifier = Modifier.testTag(BlobTags.retry(id))) {
                    Text(stringResource(R.string.blob_retry))
                }
            }
            is BlobState.Available -> AvailableBlob(label, id, s.path, actions, title)
        }
    }
}

@Composable
private fun AvailableBlob(label: String, id: String, path: String, actions: BlobActions, title: String) {
    var previewing by rememberSaveable(id) { mutableStateOf(false) }
    // The state carries no size once the file is whole: ask the file.
    val size by produceState<Long?>(null, path) { value = withContext(Dispatchers.IO) { File(path).length() } }
    val open = { actions.open(OpenBlob(id, path, title)) }
    Row(verticalAlignment = Alignment.CenterVertically) {
        val heading = size?.let { stringResource(R.string.blob_label_size, label, formatBytes(it.toULong())) } ?: label
        Text(heading, style = MaterialTheme.typography.bodySmall, modifier = Modifier.weight(1f))
        TextButton(onClick = { previewing = !previewing }, modifier = Modifier.testTag(BlobTags.preview(id))) {
            Text(stringResource(if (previewing) R.string.blob_hide_preview else R.string.blob_preview))
        }
        TextButton(onClick = open, modifier = Modifier.testTag(BlobTags.open(id))) {
            Text(stringResource(R.string.blob_open))
        }
    }
    if (previewing) BlobPreview(id, path, open)
}

/** What reading a preview came to. */
private sealed interface Head {
    data object Reading : Head
    data class Read(val head: BlobHead) : Head
    data class Unreadable(val reason: String) : Head
}

/**
 * The first [PREVIEW_BYTES] of the file, read with the provided
 * [BlobHeadReader] off the main thread, never the whole file; how much more
 * there is, and a way to the viewer.
 */
@Composable
private fun BlobPreview(id: String, path: String, onOpen: () -> Unit) {
    val reader = LocalBlobHeadReader.current
    val head by produceState<Head>(Head.Reading, path, reader) {
        value = withContext(Dispatchers.IO) {
            try {
                Head.Read(reader.read(path, PREVIEW_BYTES.toULong()))
            } catch (e: Exception) {
                // SkywaveException.Io for a file that will not read; anything
                // else must not end the screen either.
                Head.Unreadable(e.message ?: e.toString())
            }
        }
    }
    Column(Modifier.fillMaxWidth().testTag(BlobTags.head(id)), verticalArrangement = Arrangement.spacedBy(2.dp)) {
        when (val h = head) {
            Head.Reading -> Minor(stringResource(R.string.blob_reading))
            is Head.Unreadable -> Minor(stringResource(R.string.blob_unreadable, h.reason), color = MaterialTheme.colorScheme.error)
            is Head.Read -> {
                val shown = leadingLines(h.head.text, PREVIEW_LINES)
                CodeBox(shown)
                val rest = h.head.totalLen.toLong() - shown.encodeToByteArray().size
                if (rest > 0) {
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Minor(stringResource(R.string.blob_more, formatBytes(rest.toULong())))
                        TextButton(onClick = onOpen) { Text(stringResource(R.string.blob_open)) }
                    }
                }
            }
        }
    }
}

/** At most the first [max] lines of [text], without a trailing newline. */
internal fun leadingLines(text: String, max: Int): String {
    var at = -1
    repeat(max) {
        at = text.indexOf('\n', at + 1)
        if (at < 0) return text
    }
    return text.substring(0, at)
}

@Composable
private fun Heading(label: String, size: ULong, modifier: Modifier = Modifier) = Text(
    stringResource(R.string.blob_label_size, label, formatBytes(size)),
    style = MaterialTheme.typography.bodySmall,
    modifier = modifier,
)
