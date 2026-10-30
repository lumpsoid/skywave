package dev.skywave.android.ui

import java.time.Instant
import java.time.ZoneId
import java.time.format.DateTimeFormatter
import java.time.format.FormatStyle
import java.util.Locale

/** Time left before the next reconnect attempt, split for display. */
data class Countdown(val minutes: Long, val seconds: Long) {
    val isOver: Boolean get() = minutes == 0L && seconds == 0L
}

/**
 * The countdown at wall time [nowMs] to the attempt due at [retryAtMs]
 * (both Unix-epoch millis). Rounds up, so "1s" shows until the attempt is due.
 */
fun countdown(retryAtMs: Long, nowMs: Long): Countdown {
    val leftMs = leftMs(retryAtMs, nowMs)
    val total = leftMs / 1000 + if (leftMs % 1000 == 0L) 0 else 1
    return Countdown(minutes = total / 60, seconds = total % 60)
}

/** How long to wait from [nowMs] until [countdown] next changes, at most a second; 0 once it is over. */
fun nextTickInMs(retryAtMs: Long, nowMs: Long): Long {
    val leftMs = leftMs(retryAtMs, nowMs)
    if (leftMs == 0L) return 0
    val partial = leftMs % 1000
    return if (partial == 0L) 1000 else partial
}

/** Millis from [nowMs] to [retryAtMs], never negative, safe at the extremes. */
private fun leftMs(retryAtMs: Long, nowMs: Long): Long =
    if (retryAtMs <= nowMs) 0 else (retryAtMs - nowMs).let { if (it < 0) Long.MAX_VALUE else it }

/**
 * "Offline since" for [sinceMs] (Unix-epoch millis): the time alone when it
 * is the same day as [nowMs] in [zone], else date and time.
 */
fun formatOfflineSince(sinceMs: Long, nowMs: Long, zone: ZoneId, locale: Locale): String {
    val since = Instant.ofEpochMilli(sinceMs).atZone(zone)
    val now = Instant.ofEpochMilli(nowMs).atZone(zone)
    val style = if (since.toLocalDate() == now.toLocalDate()) {
        DateTimeFormatter.ofLocalizedTime(FormatStyle.SHORT)
    } else {
        DateTimeFormatter.ofLocalizedDateTime(FormatStyle.SHORT)
    }
    return style.withLocale(locale).format(since)
}
