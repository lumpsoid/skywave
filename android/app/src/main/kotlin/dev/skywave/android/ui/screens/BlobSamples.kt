package dev.skywave.android.ui.screens

import android.content.res.Configuration
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Surface
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.ui.Modifier
import androidx.compose.ui.tooling.preview.Preview
import androidx.compose.ui.unit.dp
import dev.skywave.android.ui.SkywaveTheme
import dev.skywave.core.BlobHead
import dev.skywave.core.BlobState
import dev.skywave.core.BlobView
import dev.skywave.core.BodyFormat
import dev.skywave.core.BodyView
import dev.skywave.core.ToolKind
import dev.skywave.core.ToolOutcome

/** Blobs in every state, for previews and tests. */
internal object BlobSamples {
    val notDownloaded = BlobView("b-new", BlobState.NotDownloaded(12_288u))
    val requested = BlobView("b-req", BlobState.Requested(12_288u))
    val downloading = BlobView("b-dl", BlobState.Downloading(3_100_000u, 10_485_760u, false))
    val paused = BlobView("b-paused", BlobState.Downloading(3_100_000u, 10_485_760u, true))
    val failed = BlobView("b-failed", BlobState.Failed(12_288u, "the size disagrees"))
    fun available(path: String = "/data/skywave/blobs/b-ok") = BlobView("b-ok", BlobState.Available(path))

    val all = listOf(notDownloaded, requested, downloading, paused, failed, available())

    /** The head of a long build log, as the core would read it. */
    val head = BlobHead((1..40).joinToString("\n") { "   Compiling crate-$it v0.1.$it" }, 48_000u)

    /** Reads [head] whatever the path: previews have no core. */
    val reader = BlobHeadReader { _, _ -> head }

    /** A session whose shell call's output is a blob in [state]. */
    fun session(state: BlobState) = SessionSamples.view(
        timeline = listOf(
            SessionSamples.logged(
                1,
                SessionSamples.tool(
                    ToolKind.SHELL,
                    "cargo build",
                    name = "Bash",
                    outcome = ToolOutcome.Done(false, null, "Compiling…", 900u, BlobView("b-out", state), 41_000u),
                    body = BodyView(BodyFormat.TEXT, "cargo build --workspace", 1u, null),
                ),
            ),
        ),
    )
}

@Composable
private fun BlobRowsPreview() = SkywaveTheme(dynamicColor = false) {
    CompositionLocalProvider(LocalBlobHeadReader provides BlobSamples.reader) {
        Surface {
            Column(Modifier.padding(12.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
                BlobSamples.all.forEach { BlobRow("Output", it, BlobActions.None) }
            }
        }
    }
}

@Composable
private fun OneBlobPreview(blob: BlobView) = SkywaveTheme(dynamicColor = false) {
    CompositionLocalProvider(LocalBlobHeadReader provides BlobSamples.reader) {
        Surface { Column(Modifier.padding(12.dp)) { BlobRow("Output", blob, BlobActions.None) } }
    }
}

@Preview(name = "Blob: every state")
@Composable
private fun AllStatesPreview() = BlobRowsPreview()

@Preview(name = "Blob: every state, dark", uiMode = Configuration.UI_MODE_NIGHT_YES)
@Composable
private fun AllStatesDarkPreview() = BlobRowsPreview()

@Preview(name = "Blob: not downloaded")
@Composable
private fun NotDownloadedPreview() = OneBlobPreview(BlobSamples.notDownloaded)

@Preview(name = "Blob: requested")
@Composable
private fun RequestedPreview() = OneBlobPreview(BlobSamples.requested)

@Preview(name = "Blob: downloading")
@Composable
private fun DownloadingPreview() = OneBlobPreview(BlobSamples.downloading)

@Preview(name = "Blob: paused offline")
@Composable
private fun PausedPreview() = OneBlobPreview(BlobSamples.paused)

@Preview(name = "Blob: failed")
@Composable
private fun FailedPreview() = OneBlobPreview(BlobSamples.failed)

@Preview(name = "Blob: available")
@Composable
private fun AvailablePreview() = OneBlobPreview(BlobSamples.available())
