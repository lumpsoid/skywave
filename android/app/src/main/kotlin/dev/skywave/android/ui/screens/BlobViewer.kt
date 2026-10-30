package dev.skywave.android.ui.screens

import android.content.ActivityNotFoundException
import android.content.ClipData
import android.content.Context
import android.content.Intent
import android.net.Uri
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Close
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.ClipEntry
import androidx.compose.ui.platform.LocalClipboard
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalResources
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.pluralStringResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.core.content.FileProvider
import dev.skywave.android.R
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import java.io.File

/**
 * Most bytes Copy puts on the clipboard. A clip crosses Binder, whose
 * transactions share a 1 MB buffer, as UTF-16: past this the copy fails or
 * crashes, so it is refused and Share offered instead.
 */
internal const val COPY_MAX_BYTES = 256 * 1024

/** The FileProvider over the core's blobs directory (`res/xml/blob_paths.xml`). */
internal fun blobAuthority(context: Context) = "${context.packageName}.blobs"

internal object ViewerTags {
    const val VIEWER = "blob_viewer"
    const val LINES = "blob_viewer_lines"
    const val CLOSE = "blob_viewer_close"
    const val SHARE = "blob_viewer_share"
    const val OPEN_WITH = "blob_viewer_open_with"
    const val COPY = "blob_viewer_copy"
    const val NOTICE = "blob_viewer_notice"
}

/**
 * A downloaded blob in full, over the session screen: monospace lines read a
 * page at a time ([PagedText]), so a file of any size scrolls without being
 * loaded whole; share it, open it in another app or copy it. The file is the
 * core's: it is only ever read, and handed out read-only.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
internal fun BlobViewer(blob: OpenBlob, onClose: () -> Unit) {
    BackHandler(onBack = onClose)
    val context = LocalContext.current
    val clipboard = LocalClipboard.current
    val resources = LocalResources.current
    val scope = rememberCoroutineScope()
    val file = remember(blob.path) { File(blob.path) }
    val text = remember(blob.path) { PagedText(BlobPages(file), Dispatchers.IO) }
    val size by produceState<Long?>(null, file) { value = withContext(Dispatchers.IO) { file.length() } }
    var notice by remember(blob.id) { mutableStateOf<String?>(null) }

    val noApp = stringResource(R.string.blob_no_app)
    val notShareable = stringResource(R.string.blob_not_shareable)
    val copied = stringResource(R.string.blob_copied)
    val shareTitle = stringResource(R.string.blob_share)
    val openTitle = stringResource(R.string.blob_open_with)
    /** Starts [intent], or says why it cannot. */
    fun launch(intent: () -> Intent) {
        notice = try {
            context.startActivity(intent())
            null
        } catch (_: ActivityNotFoundException) {
            noApp
        } catch (_: IllegalArgumentException) {
            // FileProvider refuses a path outside the blobs directory.
            notShareable
        }
    }

    Surface(Modifier.fillMaxSize().testTag(ViewerTags.VIEWER)) {
        Scaffold(
            topBar = {
                TopAppBar(
                    title = {
                        Column {
                            Text(blob.title, maxLines = 1, overflow = TextOverflow.Ellipsis)
                            size?.let {
                                Text(formatBytes(it.toULong()), style = MaterialTheme.typography.labelMedium)
                            }
                        }
                    },
                    navigationIcon = {
                        IconButton(onClick = onClose, modifier = Modifier.testTag(ViewerTags.CLOSE)) {
                            Icon(Icons.Filled.Close, stringResource(R.string.blob_close))
                        }
                    },
                )
            },
            bottomBar = {
                Surface(tonalElevation = 2.dp) {
                    Column(Modifier.fillMaxWidth().navigationBarsPadding().padding(horizontal = 8.dp)) {
                        notice?.let {
                            Text(
                                it,
                                style = MaterialTheme.typography.bodySmall,
                                modifier = Modifier.padding(8.dp).testTag(ViewerTags.NOTICE),
                            )
                        }
                        Row(horizontalArrangement = Arrangement.spacedBy(4.dp)) {
                            TextButton(
                                onClick = { launch { shareBlobIntent(context, blob, shareTitle) } },
                                modifier = Modifier.testTag(ViewerTags.SHARE),
                            ) { Text(shareTitle) }
                            TextButton(
                                onClick = { launch { openBlobIntent(context, blob, openTitle) } },
                                modifier = Modifier.testTag(ViewerTags.OPEN_WITH),
                            ) { Text(openTitle) }
                            TextButton(
                                onClick = {
                                    scope.launch {
                                        notice = when (val c = withContext(Dispatchers.IO) { readForCopy(file) }) {
                                            is ForCopy.TooBig -> resources.getString(
                                                R.string.blob_too_big_to_copy,
                                                formatBytes(c.size.toULong()),
                                                formatBytes(COPY_MAX_BYTES.toULong()),
                                            )
                                            is ForCopy.Text -> {
                                                clipboard.setClipEntry(ClipEntry(ClipData.newPlainText(blob.title, c.text)))
                                                copied
                                            }
                                        }
                                    }
                                },
                                modifier = Modifier.testTag(ViewerTags.COPY),
                            ) { Text(stringResource(R.string.blob_copy)) }
                        }
                    }
                }
            },
        ) { padding ->
            Lines(text, Modifier.padding(padding))
        }
    }
}

/** The lines read so far, the next page read as the end comes into view. */
@Composable
private fun Lines(text: PagedText, modifier: Modifier) {
    val listState = rememberLazyListState()
    val more = if (text.complete) 0 else 1
    LazyColumn(state = listState, modifier = modifier.fillMaxSize().testTag(ViewerTags.LINES)) {
        items(text.lineCount + more) { i ->
            if (i >= text.lineCount) {
                // Reaching the end asks for the next page.
                val next = text.pageCount
                LaunchedEffect(next) { text.load(next) }
                Text(stringResource(R.string.blob_reading), Modifier.padding(8.dp))
                return@items
            }
            val (page, line) = text.locate(i)
            val held = text.held(page)
            if (held == null) {
                // Dropped while far away: read it again.
                LaunchedEffect(page) { text.load(page) }
                Text("", fontFamily = FontFamily.Monospace)
                return@items
            }
            Text(
                held.lines[line],
                fontFamily = FontFamily.Monospace,
                style = MaterialTheme.typography.bodySmall,
                modifier = Modifier.fillMaxWidth().padding(horizontal = 8.dp),
            )
            held.cut[line].takeIf { it > 0 }?.let {
                Text(
                    pluralStringResource(R.plurals.blob_line_cut, it, it),
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    modifier = Modifier.padding(horizontal = 8.dp),
                )
            }
        }
    }
}

/** What Copy can take of a file. */
internal sealed interface ForCopy {
    class Text(val text: String) : ForCopy
    class TooBig(val size: Long) : ForCopy
}

/** The whole file as text if it is at most [COPY_MAX_BYTES], else only its size. Blocking. */
internal fun readForCopy(file: File): ForCopy {
    val size = file.length()
    return if (size > COPY_MAX_BYTES) ForCopy.TooBig(size) else ForCopy.Text(file.readText())
}

/** A read-only content URI for [blob]'s file. Throws if it is not under the blobs directory. */
internal fun blobUri(context: Context, blob: OpenBlob): Uri =
    FileProvider.getUriForFile(context, blobAuthority(context), File(blob.path))

/** The share sheet for [blob]'s file as plain text, readable by the app chosen. */
internal fun shareBlobIntent(context: Context, blob: OpenBlob, title: String): Intent {
    val uri = blobUri(context, blob)
    val send = Intent(Intent.ACTION_SEND)
        .setType(TEXT_PLAIN)
        .putExtra(Intent.EXTRA_STREAM, uri)
        .putExtra(Intent.EXTRA_SUBJECT, blob.title)
        .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
    // The chooser passes the grant on only for URIs in the clip.
    send.clipData = ClipData.newRawUri(blob.title, uri)
    return Intent.createChooser(send, title)
}

/** Another app's viewer for [blob]'s file as plain text, readable by the app chosen. */
internal fun openBlobIntent(context: Context, blob: OpenBlob, title: String): Intent {
    val view = Intent(Intent.ACTION_VIEW)
        .setDataAndType(blobUri(context, blob), TEXT_PLAIN)
        .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
    return Intent.createChooser(view, title)
}

private const val TEXT_PLAIN = "text/plain"
