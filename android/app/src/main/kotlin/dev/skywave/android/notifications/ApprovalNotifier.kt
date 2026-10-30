package dev.skywave.android.notifications

import dev.skywave.core.Screen
import dev.skywave.core.ViewState
import kotlinx.coroutines.flow.Flow

/** One approval to tell the user about: [sessionTitle] is already sanitised. */
data class ApprovalAlert(val approvalId: String, val sessionId: String, val sessionTitle: String)

/**
 * The system's notification shade, one notification per approval. Faked in
 * tests; [AndroidNotifications] is the real one.
 */
interface Notifications {
    fun post(alert: ApprovalAlert)

    fun cancel(approvalId: String)

    /** Approvals with a notification up now, including ones a previous process posted. */
    fun active(): Set<String>
}

/**
 * Turns snapshots into approval notifications (architecture.md, "New approval
 * while Online"). The core decides what to alert for (D2): each pending
 * approval carries the `attention.alerts` value it alerted at in this core
 * (`alertedAt`, 0 if it has not alerted here), so this posts every approval
 * whose `alertedAt` is above the last `alerts` it saw, and takes each
 * notification down once its approval leaves `attention.approvals` (answered
 * here or elsewhere, or cancelled). That includes an approval already in the
 * first snapshot that alerts later (pulled by a catch-up an earlier process
 * cut short), and never one an earlier process alerted for: the core reports
 * those at 0.
 *
 * The last seen `alerts` is kept in memory, not persisted, and starts at 0:
 * the core counts from zero in every process and its first snapshot is
 * always 0, so this may attach after that snapshot and still miss nothing,
 * while a persisted count would swallow the next process's first alerts.
 * There is one core per process, so one count per notifier is enough. What
 * does outlive the process is the shade itself, read back through
 * [Notifications.active] so stale notifications still go.
 *
 * Not thread-safe: feed it from one coroutine ([follow]).
 *
 * @param inForeground whether the app is visible now; a new approval in the
 *   session on screen is not posted, and that session's notifications go.
 */
class ApprovalNotifier(
    private val notifications: Notifications,
    private val inForeground: () -> Boolean,
) {
    /** `alerts` of the last snapshot; the core's count starts at 0. */
    private var seenAlerts: ULong = 0u

    /** Whether the shade was read back yet. */
    private var started = false

    /** Approvals this notifier believes have a notification up. */
    private val posted = mutableSetOf<String>()

    suspend fun follow(views: Flow<ViewState>) = views.collect(::onView)

    fun onView(view: ViewState) {
        if (!started) {
            // The shade may still hold what an earlier process posted.
            posted += notifications.active()
            started = true
        }
        val approvals = view.attention.approvals
        val pending = approvals.mapTo(HashSet()) { it.id }
        val onScreen = (view.screen as? Screen.Session)?.view?.id?.takeIf { inForeground() }
        for (approval in approvals) {
            // One in the session on screen was seen: it counts as handled.
            if (approval.alertedAt <= seenAlerts || approval.id in posted || approval.sessionId == onScreen) continue
            notifications.post(ApprovalAlert(approval.id, approval.sessionId, sanitiseTitle(approval.title)))
            posted += approval.id
        }
        seenAlerts = maxOf(seenAlerts, view.attention.alerts)
        val onScreenIds = approvals.filter { it.sessionId == onScreen }.mapTo(HashSet()) { it.id }
        val gone = posted.filter { it !in pending || it in onScreenIds }
        for (id in gone) {
            notifications.cancel(id)
            posted -= id
        }
    }
}

/** Longest session title a notification shows, in UTF-16 units. */
internal const val MAX_TITLE = 80

/**
 * A server-provided title made safe to show: control and format characters
 * (newlines, bidi overrides) dropped, whitespace runs collapsed, capped at
 * [MAX_TITLE]. Empty when nothing printable is left.
 */
fun sanitiseTitle(title: String): String {
    val printable = buildString {
        for (c in title) {
            when {
                c.isWhitespace() -> append(' ')
                Character.isISOControl(c) || Character.getType(c) == Character.FORMAT.toInt() -> Unit
                else -> append(c)
            }
        }
    }
    val text = printable.trim().replace(Regex(" {2,}"), " ")
    if (text.length <= MAX_TITLE) return text
    var end = MAX_TITLE - 1
    if (Character.isHighSurrogate(text[end - 1])) end -= 1
    return text.substring(0, end).trimEnd() + "…"
}
