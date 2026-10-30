package dev.skywave.android.ui

import dev.skywave.android.ui.screens.BlobPages
import dev.skywave.android.ui.screens.PAGE_BYTES
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.File

/** Reading a blob a window at a time: never whole, nothing lost or split. */
class BlobPagesTest {
    @get:Rule
    val tmp = TemporaryFolder()

    private fun file(text: String): File = tmp.newFile().apply { writeText(text) }

    /** Every page from the start, joined. */
    private fun BlobPages.all(): List<String> {
        val lines = mutableListOf<String>()
        var at = 0L
        while (true) {
            val page = page(at)
            if (page.lines.isEmpty()) return lines
            lines += page.lines
            at = page.end
        }
    }

    @Test
    fun aPageOfAHugeFileReadsOnlyThatPage() {
        val big = tmp.newFile()
        big.bufferedWriter().use { w -> for (i in 1..150_000) w.write("line ${"%06d".format(i)} of a five megabyte blob\n") }
        assertTrue(big.length() > 5_000_000)
        val pages = BlobPages(big)
        val first = pages.page(0)
        assertEquals("line 000001 of a five megabyte blob", first.lines.first())
        assertTrue(first.lines.last().startsWith("line "))
        assertEquals(PAGE_BYTES.toLong(), pages.bytesRead)
        // Pages end on a line end, so the next one starts on a whole line.
        assertTrue(pages.page(first.end).lines.first().startsWith("line "))
        assertEquals(2L * PAGE_BYTES, pages.bytesRead)
    }

    @Test
    fun pagesTogetherAreTheWholeFile() {
        val lines = (1..500).map { "row $it: ${"é".repeat(it % 7)}" }
        val pages = BlobPages(file(lines.joinToString("\n") + "\n"), pageBytes = 300)
        assertEquals(lines, pages.all())
    }

    @Test
    fun aLineLongerThanAPageIsSplitOnACharacterBoundary() {
        val long = "€".repeat(1_000) // 3 bytes each
        val pages = BlobPages(file("$long\nend"), pageBytes = 100, maxLineChars = 10_000)
        val all = pages.all()
        assertEquals(long, all.dropLast(1).joinToString(""))
        assertEquals("end", all.last())
    }

    @Test
    fun aLongLineKeepsItsHeadAndCountsTheRest() {
        val page = BlobPages(file("x".repeat(50) + "\r\nshort\n"), maxLineChars = 20).page(0)
        assertEquals(listOf("x".repeat(20), "short"), page.lines)
        assertEquals(listOf(30, 0), page.cut)
    }

    @Test
    fun anEmptyFileHasNoLines() {
        assertEquals(emptyList<String>(), BlobPages(file("")).page(0).lines)
    }
}
