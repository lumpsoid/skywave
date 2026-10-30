package dev.skywave.android.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import java.time.ZoneOffset
import java.util.Locale

class LinkFormatTest {
    @Test
    fun countdownRoundsUpToWholeSeconds() {
        assertEquals(Countdown(0, 4), countdown(4_000u, 0))
        assertEquals(Countdown(0, 4), countdown(4_000u, 1))
        assertEquals(Countdown(0, 3), countdown(4_000u, 1_000))
        assertEquals(Countdown(0, 1), countdown(4_000u, 3_999))
    }

    @Test
    fun countdownStopsAtZero() {
        assertTrue(countdown(4_000u, 4_000).isOver)
        assertTrue(countdown(4_000u, 60_000).isOver)
        assertTrue(countdown(0u, 0).isOver)
    }

    @Test
    fun countdownSplitsMinutes() {
        assertEquals(Countdown(1, 5), countdown(65_000u, 0))
        assertEquals(Countdown(2, 0), countdown(119_001u, 0))
    }

    @Test
    fun nextTickLandsOnTheNextSecondBoundary() {
        assertEquals(1_000, nextTickInMs(4_000u, 0))
        assertEquals(500, nextTickInMs(4_000u, 500))
        assertEquals(1, nextTickInMs(4_000u, 3_999))
        assertEquals(0, nextTickInMs(4_000u, 4_000))
        assertEquals(0, nextTickInMs(4_000u, 9_000))
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
