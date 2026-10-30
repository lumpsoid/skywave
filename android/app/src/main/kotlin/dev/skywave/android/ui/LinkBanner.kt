package dev.skywave.android.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.RowScope
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.WindowInsetsSides
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.only
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawing
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.runtime.staticCompositionLocalOf
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.res.pluralStringResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import dev.skywave.android.R
import dev.skywave.core.Link
import dev.skywave.core.LinkRejection
import kotlinx.coroutines.delay
import java.time.ZoneId

/**
 * The connection's state, under the top bar of every screen. Quiet while
 * online; [onPairAgain] is null where pairing is already on screen.
 */
@Composable
fun LinkBanner(
    link: Link,
    queued: ULong,
    onRetryNow: () -> Unit,
    onPairAgain: (() -> Unit)?,
    modifier: Modifier = Modifier,
) {
    when (link) {
        is Link.Online -> Unit
        Link.Connecting -> QuietLine(stringResource(R.string.link_connecting), modifier) {
            CircularProgressIndicator(modifier = Modifier.size(12.dp), strokeWidth = 1.5.dp)
        }
        is Link.CatchingUp -> CatchingUp(link, modifier)
        is Link.Reconnecting -> Reconnecting(link, queued, onRetryNow, modifier)
        is Link.Rejected -> Rejected(link.rejection, onPairAgain, modifier)
    }
}

/** A single small line, for states that need no attention. */
@Composable
private fun QuietLine(text: String, modifier: Modifier, leading: @Composable () -> Unit = {}) {
    Row(
        modifier = modifier.fillMaxWidth().windowInsetsPadding(bannerInsets).padding(horizontal = 16.dp, vertical = 4.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        leading()
        Text(text, style = MaterialTheme.typography.labelMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
    }
}

@Composable
private fun CatchingUp(link: Link.CatchingUp, modifier: Modifier) {
    Column(modifier.fillMaxWidth()) {
        if (link.server > 0uL) {
            LinearProgressIndicator(
                progress = { (link.have.toDouble() / link.server.toDouble()).toFloat().coerceIn(0f, 1f) },
                modifier = Modifier.fillMaxWidth(),
            )
        } else {
            LinearProgressIndicator(modifier = Modifier.fillMaxWidth())
        }
        QuietLine(stringResource(R.string.link_catching_up, link.have.toLong(), link.server.toLong()), Modifier)
    }
}

@Composable
private fun Reconnecting(link: Link.Reconnecting, queued: ULong, onRetryNow: () -> Unit, modifier: Modifier) {
    val clock = LocalWallClock.current
    val left = rememberCountdown(link.retryAtMs, clock)
    val separator = stringResource(R.string.separator)
    val headline = buildList {
        add(stringResource(R.string.link_offline))
        if (queued > 0uL) add(pluralStringResource(R.plurals.link_queued, queued.toCount(), queued.toLong()))
        add(
            when {
                left.isOver -> stringResource(R.string.link_retrying)
                left.minutes > 0 -> stringResource(R.string.link_retry_in_minutes, left.minutes, left.seconds)
                else -> stringResource(R.string.link_retry_in_seconds, left.seconds)
            },
        )
    }.joinToString(separator)
    val locale = LocalConfiguration.current.locales[0]
    val since = remember(link.offlineSinceMs, locale) {
        formatOfflineSince(link.offlineSinceMs, clock(), ZoneId.systemDefault(), locale)
    }
    val detail = listOf(
        stringResource(R.string.link_offline_since, since),
        stringResource(R.string.link_attempt, link.attempt.toLong()),
    ).joinToString(separator)

    BannerSurface(
        container = MaterialTheme.colorScheme.secondaryContainer,
        content = MaterialTheme.colorScheme.onSecondaryContainer,
        modifier = modifier,
    ) {
        Column(Modifier.weight(1f)) {
            Text(headline, style = MaterialTheme.typography.bodyMedium)
            Text(detail, style = MaterialTheme.typography.bodySmall)
        }
        TextButton(onClick = onRetryNow) { Text(stringResource(R.string.link_retry_now)) }
    }
}

@Composable
private fun Rejected(rejection: LinkRejection, onPairAgain: (() -> Unit)?, modifier: Modifier) {
    BannerSurface(
        container = MaterialTheme.colorScheme.errorContainer,
        content = MaterialTheme.colorScheme.onErrorContainer,
        modifier = modifier,
    ) {
        val text = when (rejection) {
            LinkRejection.Revoked -> stringResource(R.string.link_revoked)
            is LinkRejection.ProtocolMismatch ->
                stringResource(R.string.link_protocol_mismatch, rejection.client.toLong(), rejection.server.toLong())
        }
        Text(text, style = MaterialTheme.typography.bodyMedium, modifier = Modifier.weight(1f))
        if (rejection == LinkRejection.Revoked && onPairAgain != null) {
            TextButton(onClick = onPairAgain) { Text(stringResource(R.string.rejected_pair_again)) }
        } else {
            Spacer(Modifier.width(8.dp))
        }
    }
}

/** A full-width coloured strip holding one row. */
@Composable
internal fun BannerSurface(
    container: Color,
    content: Color,
    modifier: Modifier = Modifier,
    onClick: (() -> Unit)? = null,
    row: @Composable RowScope.() -> Unit,
) {
    val inner: @Composable () -> Unit = {
        Row(
            modifier = Modifier.fillMaxWidth().windowInsetsPadding(bannerInsets).padding(start = 16.dp, end = 8.dp, top = 4.dp, bottom = 4.dp),
            verticalAlignment = Alignment.CenterVertically,
            content = row,
        )
    }
    if (onClick != null) {
        Surface(onClick = onClick, modifier = modifier.fillMaxWidth(), color = container, contentColor = content, content = inner)
    } else {
        Surface(modifier = modifier.fillMaxWidth(), color = container, contentColor = content, content = inner)
    }
}

/** Banners sit below the top bar, so only the side insets (landscape cutouts) apply. */
private val bannerInsets: WindowInsets
    @Composable get() = WindowInsets.safeDrawing.only(WindowInsetsSides.Horizontal)

/**
 * The wall clock the banner counts against, in Unix-epoch millis. The core's
 * times are wall-clock instants, so this keeps counting through deep sleep
 * and reads the same after the Activity is recreated. Tests provide a fake.
 */
internal val LocalWallClock = staticCompositionLocalOf<() -> Long> { System::currentTimeMillis }

/**
 * Counts down to [retryAtMs] on [clock], recomputed at each second boundary.
 * Presentation only: the core decides when to retry and sends a new snapshot
 * when it does. Nothing is anchored to when a snapshot arrived, so a new
 * snapshot or a recreated screen picks up where the countdown stood.
 */
@Composable
private fun rememberCountdown(retryAtMs: Long, clock: () -> Long): Countdown = key(retryAtMs) {
    val left by produceState(countdown(retryAtMs, clock())) {
        while (true) {
            val now = clock()
            value = countdown(retryAtMs, now)
            val wait = nextTickInMs(retryAtMs, now)
            if (wait == 0L) break
            delay(wait)
        }
    }
    left
}

/** Plural quantity for a count from the core; clamps the (unreachable) huge ones. */
internal fun ULong.toCount(): Int = coerceAtMost(Int.MAX_VALUE.toULong()).toInt()
