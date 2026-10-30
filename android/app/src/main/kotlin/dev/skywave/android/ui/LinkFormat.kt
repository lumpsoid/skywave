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
 * The countdown [elapsedMs] after a snapshot said the next attempt is
 * [nextInMs] away. Rounds up, so "1s" shows until the attempt is due.
 */
fun countdown(nextInMs: ULong, elapsedMs: Long): Countdown {
    val leftMs = (nextInMs.coerceAtMost(Long.MAX_VALUE.toULong()).toLong() - elapsedMs.coerceAtLeast(0))
        .coerceAtLeast(0)
    val total = (leftMs + 999) / 1000
    return Countdown(minutes = total / 60, seconds = total % 60)
}

/** How long to wait from [elapsedMs] until [countdown] next changes, at most a second. */
fun nextTickInMs(nextInMs: ULong, elapsedMs: Long): Long {
    val leftMs = nextInMs.coerceAtMost(Long.MAX_VALUE.toULong()).toLong() - elapsedMs
    if (leftMs <= 0) return 0
    val partial = leftMs % 1000
    return if (partial == 0L) 1000 else partial
}

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
