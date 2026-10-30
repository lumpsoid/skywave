package dev.skywave.android.notifications

import android.content.Context
import android.content.Intent
import android.net.Uri

/**
 * The intent a tapped approval notification starts [activity] with, and its
 * reading back. The approval rides in the data URI so each notification's
 * PendingIntent is distinct; nothing filters for it, so only an explicit
 * intent reaches the activity this way.
 */
object ApprovalTap {
    const val ACTION = "dev.skywave.android.action.OPEN_APPROVAL"
    const val EXTRA_SESSION_ID = "dev.skywave.android.extra.SESSION_ID"
    private const val SCHEME = "skywave-approval"

    fun intent(context: Context, activity: Class<*>, alert: ApprovalAlert): Intent =
        Intent(context, activity)
            .setAction(ACTION)
            .setData(Uri.fromParts(SCHEME, alert.approvalId, null))
            .putExtra(EXTRA_SESSION_ID, alert.sessionId)
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_SINGLE_TOP)

    /** The session to open if [intent] came from an approval notification. */
    fun sessionId(intent: Intent?): String? {
        if (intent?.action != ACTION) return null
        return intent.getStringExtra(EXTRA_SESSION_ID)?.takeIf { it.isNotEmpty() }
    }
}
