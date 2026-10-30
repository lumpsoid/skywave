package dev.skywave.android.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import java.time.ZoneOffset
import java.util.Locale

class LinkFormatTest {
    private val now = 1_700_000_000_000

    @Test
    fun countdownRoundsUpToWholeSeconds() {
        assertEquals(Countdown(0, 4), countdown(now + 4_000, now))
        assertEquals(Countdown(0, 4), countdown(now + 4_000, now + 1))
        assertEquals(Countdown(0, 3), countdown(now + 4_000, now + 1_000))
        assertEquals(Countdown(0, 1), countdown(now + 4_000, now + 3_999))
    }

    @Test
    fun countdownStopsAtZero() {
        assertTrue(countdown(now + 4_000, now + 4_000).isOver)
        assertTrue(countdown(now + 4_000, now + 60_000).isOver)
        assertTrue(countdown(now, now).isOver)
        assertTrue(countdown(Long.MIN_VALUE, Long.MAX_VALUE).isOver)
    }

    @Test
    fun countdownSplitsMinutes() {
        assertEquals(Countdown(1, 5), countdown(now + 65_000, now))
        assertEquals(Countdown(2, 0), countdown(now + 119_001, now))
    }

    @Test
    fun countdownDoesNotOverflowAtTheExtremes() {
        assertTrue(countdown(Long.MAX_VALUE, Long.MIN_VALUE).minutes > 0)
    }

    @Test
    fun nextTickLandsOnTheNextSecondBoundary() {
        assertEquals(1_000, nextTickInMs(now + 4_000, now))
        assertEquals(500, nextTickInMs(now + 4_000, now + 500))
        assertEquals(1, nextTickInMs(now + 4_000, now + 3_999))
        assertEquals(0, nextTickInMs(now + 4_000, now + 4_000))
        assertEquals(0, nextTickInMs(now + 4_000, now + 9_000))
    }

    @Test
    fun offlineSinceShowsTimeTodayAndDateOtherwise() {
        val since = 1_700_000_000_000 // 2023-11-14T22:13:20Z
        val sameDay = formatOfflineSince(since, since + 60_000, ZoneOffset.UTC, Locale.US)
        assertTrue(sameDay, sameDay.contains("10:13") && !sameDay.contains("23"))
        val nextDay = formatOfflineSince(since, since + 86_400_000, ZoneOffset.UTC, Locale.US)
        assertTrue(nextDay, nextDay.contains("11/14/23") && nextDay.contains("10:13"))
    }
}
