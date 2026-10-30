package dev.skywave.android.ui.screens

import androidx.compose.runtime.Stable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import kotlinx.coroutines.withContext
import java.io.File
import java.io.RandomAccessFile
import java.util.concurrent.atomic.AtomicLong
import kotlin.coroutines.CoroutineContext

/** Bytes read per page of the viewer. */
internal const val PAGE_BYTES = 64 * 1024

/** Characters of one line the viewer shows; the rest is counted, not kept. */
internal const val MAX_LINE_CHARS = 2_000

/** Pages the viewer holds at once; the farthest from the one read last go first. */
internal const val MAX_PAGES = 6

/**
 * One window of a file: its lines, from byte [start] to [end]. A line longer
 * than the cap keeps its head and counts the characters it lost in [cut].
 */
internal class Page(val start: Long, val end: Long, val lines: List<String>, val cut: List<Int>)

/**
 * Reads a file in windows of at most [pageBytes], each cut at a line end
 * (or, for a line longer than a window, at a UTF-8 boundary), so a file of
 * any size is read a page at a time and never whole. Blocking; thread-safe.
 */
internal class BlobPages(
    private val file: File,
    private val pageBytes: Int = PAGE_BYTES,
    private val maxLineChars: Int = MAX_LINE_CHARS,
) {
    private val read = AtomicLong()

    /** Bytes read from the file so far, all pages together. */
    val bytesRead: Long get() = read.get()

    /** The page starting at byte [start]; empty at or past the end of the file. */
    fun page(start: Long): Page = RandomAccessFile(file, "r").use { raf ->
        val size = raf.length()
        if (start >= size) return Page(start, start, emptyList(), emptyList())
        val buf = ByteArray(minOf(pageBytes.toLong(), size - start).toInt())
        raf.seek(start)
        raf.readFully(buf)
        read.addAndGet(buf.size.toLong())
        val len = if (start + buf.size >= size) buf.size else cutAt(buf)
        var text = String(buf, 0, len, Charsets.UTF_8)
        if (text.endsWith('\n')) text = text.dropLast(1)
        val lines = ArrayList<String>()
        val cut = ArrayList<Int>()
        for (line in text.split('\n')) {
            val l = line.removeSuffix("\r")
            lines += if (l.length > maxLineChars) l.substring(0, maxLineChars) else l
            cut += (l.length - maxLineChars).coerceAtLeast(0)
        }
        Page(start, start + len, lines, cut)
    }

    /** Where to end a window that is not the file's last: after its last newline, else at a UTF-8 boundary. */
    private fun cutAt(buf: ByteArray): Int {
        val nl = buf.lastIndexOf('\n'.code.toByte())
        if (nl >= 0) return nl + 1
        var at = buf.size
        // Back off continuation bytes (10xxxxxx) so no character is split.
        while (at > 0 && (buf[at - 1].toInt() and 0xC0) == 0x80) at--
        // `at - 1` is now a lead byte (or ASCII); end before a lead byte of a multi-byte sequence.
        if (at > 0 && (buf[at - 1].toInt() and 0x80) != 0) at--
        return if (at == 0) buf.size else at
    }
}

/**
 * The viewer's lines over [pages]: pages are read one at a time as the list
 * reaches them, and at most [MAX_PAGES] are held; a page dropped is read
 * again if scrolled back to. Line counts of pages read stay known, so
 * indices are stable. Mutated on the main thread; reads run on [io].
 */
@Stable
internal class PagedText(private val pages: BlobPages, private val io: CoroutineContext) {
    /** Start of every page found so far; one more than [counts] until the end is reached. */
    private val starts = mutableListOf(0L)
    private val counts = mutableStateListOf<Int>()
    private val held = mutableStateMapOf<Int, Page>()
    private val loading = mutableSetOf<Int>()

    /** Lines of every page read so far. */
    var lineCount by mutableIntStateOf(0)
        private set

    /** Whether the last page has been read. */
    var complete by mutableStateOf(false)
        private set

    /** Index of the page holding line [index] (< [lineCount]), and the line within it. */
    fun locate(index: Int): Pair<Int, Int> {
        var rest = index
        for (p in counts.indices) {
            if (rest < counts[p]) return p to rest
            rest -= counts[p]
        }
        error("line $index past $lineCount")
    }

    /** The page [index] if held, else null (then [load] it). */
    fun held(index: Int): Page? = held[index]

    /** Pages found so far; the next to read at the end has this index. */
    val pageCount: Int get() = counts.size

    /** Reads page [index] (a known one, or the next at the end) unless held or on its way. */
    suspend fun load(index: Int) {
        if (index > counts.size || (index == counts.size && complete) || held.containsKey(index) || !loading.add(index)) return
        try {
            val page = withContext(io) { pages.page(starts[index]) }
            if (index == counts.size) {
                counts += page.lines.size
                lineCount += page.lines.size
                if (page.lines.isEmpty() || page.end <= page.start) complete = true else starts += page.end
            }
            held[index] = page
            while (held.size > MAX_PAGES) held.remove(held.keys.maxBy { kotlin.math.abs(it - index) })
        } finally {
            loading.remove(index)
        }
    }
}
