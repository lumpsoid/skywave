package dev.skywave.android.ui

import dev.skywave.android.ui.screens.ApprovalBody
import dev.skywave.android.ui.screens.ApprovalSamples
import dev.skywave.android.ui.screens.SessionSamples
import dev.skywave.android.ui.screens.TextBlock
import dev.skywave.android.ui.screens.TimelineRow
import dev.skywave.android.ui.screens.answered
import dev.skywave.android.ui.screens.approvalBody
import dev.skywave.android.ui.screens.copyText
import dev.skywave.android.ui.screens.firstLines
import dev.skywave.android.ui.screens.formatBytes
import dev.skywave.android.ui.screens.formatDuration
import dev.skywave.android.ui.screens.formatTokens
import dev.skywave.android.ui.screens.gatedRowKey
import dev.skywave.android.ui.screens.gatedTool
import dev.skywave.android.ui.screens.inlineCodeSpans
import dev.skywave.android.ui.screens.shortDevice
import dev.skywave.android.ui.screens.textBlocks
import dev.skywave.android.ui.screens.timelineKey
import dev.skywave.android.ui.screens.timelineRows
import dev.skywave.android.ui.screens.usageLine
import dev.skywave.core.ApprovalPrompt
import dev.skywave.core.ApprovalStatus
import dev.skywave.core.BodyFormat
import dev.skywave.core.BodyView
import dev.skywave.core.Entry
import dev.skywave.core.FailReason
import dev.skywave.core.Outgoing
import dev.skywave.core.ToolKind
import dev.skywave.core.TurnUsage
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/** The timeline's pure parts: keys, the divider's place, markdown-lite, formats. */
class SessionTimelineModelTest {
    @Test
    fun aPendingMessageKeepsItsKeyWhenLogged() {
        val pending = SessionSamples.pending("cmd-7", "hello")
        val logged = SessionSamples.message(42, "hello", commandId = "cmd-7")
        assertEquals(timelineKey(pending), timelineKey(logged))
        // Everything else is keyed by its seq.
        assertEquals("s:3", timelineKey(SessionSamples.logged(3, Entry.Interrupted)))
    }

    @Test
    fun rowsKeepTheirKeysAndOrderAsAPendingMessageIsLogged() {
        val before = listOf(
            SessionSamples.message(1, "first"),
            SessionSamples.logged(2, Entry.AgentMessage("reply")),
            SessionSamples.pending("cmd-9", "second"),
        )
        val after = before.take(2) + listOf(
            SessionSamples.message(3, "second", commandId = "cmd-9"),
            SessionSamples.logged(4, Entry.AgentMessage("again")),
        )
        val keysBefore = timelineRows(before, null).map { it.key }
        val keysAfter = timelineRows(after, null).map { it.key }
        assertEquals(keysBefore, keysAfter.take(3))
    }

    @Test
    fun dividerGoesBeforeTheFirstItemAtUnreadFrom() {
        val rows = timelineRows(SessionSamples.everything, 15u)
        val at = rows.indexOf(TimelineRow.Divider)
        assertEquals("s:15", rows[at + 1].key)
        assertEquals(SessionSamples.everything.size + 1, rows.size)
        assertEquals(-1, timelineRows(SessionSamples.everything, null).indexOf(TimelineRow.Divider))
        assertEquals(-1, timelineRows(SessionSamples.everything, 999u).indexOf(TimelineRow.Divider))
    }

    @Test
    fun aRepeatedKeyFallsBackSoTheListStillRenders() {
        val twice = listOf(SessionSamples.message(1, "a", commandId = "x"), SessionSamples.message(2, "a", commandId = "x"))
        val keys = timelineRows(twice, null).map { it.key }
        assertEquals(listOf("m:x", "i:1"), keys)
    }

    @Test
    fun fencesSplitCodeFromProse() {
        val blocks = textBlocks("Intro\n```kotlin\nval x = 1\n\nval y = 2\n```\nOutro\n```\nunclosed")
        assertEquals(
            listOf(
                TextBlock.Prose("Intro"),
                TextBlock.Code("val x = 1\n\nval y = 2"),
                TextBlock.Prose("Outro"),
                TextBlock.Code("unclosed"),
            ),
            blocks,
        )
    }

    @Test
    fun inlineCodeNeedsAPairOnOneLine() {
        assertEquals(listOf(5..7), inlineCodeSpans("use `foo` here"))
        assertEquals(emptyList<IntRange>(), inlineCodeSpans("a ` b\n` c"))
        assertEquals(emptyList<IntRange>(), inlineCodeSpans("empty `` ticks"))
    }

    @Test
    fun previewsKeepWholeLinesUpToTheLimit() {
        assertEquals("a\nb", firstLines("a\nb", 2))
        assertEquals("a\nb\n…", firstLines("a\nb\nc", 2))
        assertEquals("a\nb\nc", firstLines("a\nb\nc", Int.MAX_VALUE))
    }

    @Test
    fun formats() {
        assertEquals("512 B", formatBytes(512u))
        assertEquals("4.1 kB", formatBytes(4096u))
        assertEquals("1.2s", formatDuration(1200u))
        assertEquals("12s", formatDuration(12_400u))
        assertEquals("2m05s", formatDuration(125_000u))
        assertEquals("1h02m", formatDuration(3_720_000u))
        assertEquals("950", formatTokens(950u))
        assertEquals("12.3k", formatTokens(12_300u))
        assertEquals(
            "12.3k in · 840 out · $0.0123 · 4.1s · sonnet",
            usageLine(SessionSamples.usage, "in", "out"),
        )
        assertEquals("", usageLine(TurnUsage(null, null, null, null, null), "in", "out"))
    }

    @Test
    fun copyTextIsWhatIsShown() {
        assertEquals("hello", copyText(SessionSamples.pending("p", "hello")))
        assertEquals("rate limited", copyText(SessionSamples.logged(1, Entry.AgentErrored("rate limited"))))
        assertEquals(null, copyText(SessionSamples.logged(1, Entry.Interrupted)))
    }

    @Test
    fun theGatedRowIsTheCallThePromptNames() {
        val view = ApprovalSamples.blocked(ApprovalSamples.editTool)
        val prompt = view.pendingApproval!!
        assertEquals("s:3", gatedRowKey(timelineRows(view.timeline, null), prompt))
        assertEquals(ApprovalSamples.editTool.tool, gatedTool(view.timeline, prompt))
        assertEquals(null, gatedRowKey(timelineRows(view.timeline, null), null))
        // A call that is not in the timeline gates nothing there.
        assertEquals(null, gatedTool(view.timeline, prompt.copy(call = "elsewhere")))
        // With no logged call, the standalone approval is the gated item.
        val v1 = prompt.copy(call = null)
        val entry = SessionSamples.logged(7, Entry.Approval(prompt.id, "Bash", "ls", null, ApprovalStatus.Pending))
        assertEquals("s:7", gatedRowKey(timelineRows(listOf(entry), null), v1))
        assertEquals(null, gatedTool(listOf(entry), v1))
    }

    @Test
    fun theApprovalBodyIsTheDiffTheContentOrTheCommand() {
        fun body(tool: Entry.Tool) = approvalBody(ApprovalSamples.prompt(tool), tool.tool)
        assertEquals(ApprovalBody.Diff(ApprovalSamples.diff, 6u, null), body(ApprovalSamples.editTool))
        assertTrue(body(ApprovalSamples.writeTool) is ApprovalBody.Content)
        assertEquals(ApprovalBody.Command("cargo test -p sync -- --nocapture"), body(ApprovalSamples.commandTool))
        // A command too long for the title comes whole in the body.
        val long = SessionSamples.tool(ToolKind.SHELL, "$ make all…", body = BodyView(BodyFormat.TEXT, "make all\n  -j8", 2u, null))
        assertEquals(ApprovalBody.Command("make all\n  -j8"), body(long))
        assertEquals(ApprovalBody.None, body(SessionSamples.tool(ToolKind.FETCH, "https://example.com")))
        // No logged call: the prompt's summary is all there is.
        val v1 = ApprovalPrompt("a", null, "Bash", "rm -rf build", null, emptyList(), null)
        assertEquals(ApprovalBody.Content("rm -rf build", 1u, null), approvalBody(v1, null))
    }

    @Test
    fun anAnswerStandsUnlessRefused() {
        assertFalse(answered(null))
        assertTrue(answered(ApprovalSamples.answer()))
        assertTrue(answered(ApprovalSamples.answer(outgoing = Outgoing.Accepted(3u))))
        assertFalse(answered(ApprovalSamples.answer(outgoing = Outgoing.Failed(FailReason.APPROVAL_ALREADY_RESOLVED))))
        assertEquals("…000002", shortDevice(SessionSamples.OTHER))
    }
}
