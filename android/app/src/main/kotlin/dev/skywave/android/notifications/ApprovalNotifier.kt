package dev.skywave.android.notifications

import dev.skywave.core.Link
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
 * while Online"). The core decides when to alert (D2): this only posts when
 * `attention.alerts` grows, for the approvals it has not accounted for yet,
 * and takes each notification down once its approval leaves
 * `attention.approvals` (answered here or elsewhere, or cancelled).
 *
 * The last seen `alerts` is kept in memory, not persisted: the core counts
 * from zero in every process and never alerts for what was pending at start,
 * so a restart cannot re-alert, and a persisted count would swallow the next
 * process's first alerts. What does outlive the process is the shade itself,
 * read back through [Notifications.active] so stale notifications still go.
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
    /** `alerts` of the last snapshot; null before the first. */
    private var seenAlerts: ULong? = null

    /**
     * Approvals that need no notification from now on: posted, pending at
     * start, or pending while online without `alerts` growing (the core
     * judged them no news, e.g. answered here and the answer refused).
     */
    private val accounted = mutableSetOf<String>()

    /** Approvals this notifier believes have a notification up. */
    private val posted = mutableSetOf<String>()

    suspend fun follow(views: Flow<ViewState>) = views.collect(::onView)

    fun onView(view: ViewState) {
        val approvals = view.attention.approvals
        val pending = approvals.mapTo(HashSet()) { it.id }
        val last = seenAlerts
        val onScreen = (view.screen as? Screen.Session)?.view?.id?.takeIf { inForeground() }
        if (last == null) {
            // Pending at start is not news; the shade may still hold what an
            // earlier process posted.
            accounted += pending
            posted += notifications.active()
        } else if (view.attention.alerts > last) {
            for (approval in approvals) {
                if (!accounted.add(approval.id) || approval.sessionId == onScreen) continue
                notifications.post(ApprovalAlert(approval.id, approval.sessionId, sanitiseTitle(approval.title)))
                posted += approval.id
            }
        }
        seenAlerts = maxOf(last ?: 0u, view.attention.alerts)
        // The core only decides while online: an approval that shows up while
        // catching up alerts at the end if still pending, so it stays open.
        if (view.link is Link.Online) accounted += pending
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
