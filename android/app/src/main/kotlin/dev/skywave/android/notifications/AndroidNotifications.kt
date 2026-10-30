package dev.skywave.android.notifications

import android.app.PendingIntent
import android.content.Context
import androidx.core.app.NotificationChannelCompat
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import dev.skywave.android.R

/**
 * [Notifications] on the system shade: one notification per approval,
 * tagged with its id, on the high-importance "Approvals" channel.
 * Tapping one starts [activity] with an [ApprovalTap] intent.
 */
class AndroidNotifications(private val context: Context, private val activity: Class<*>) : Notifications {
    private val manager = NotificationManagerCompat.from(context)

    /** Creates or updates the channel; cheap, so done on every start. */
    fun createChannel() {
        manager.createNotificationChannel(
            NotificationChannelCompat.Builder(CHANNEL, NotificationManagerCompat.IMPORTANCE_HIGH)
                .setName(context.getString(R.string.notif_channel_approvals))
                .setDescription(context.getString(R.string.notif_channel_approvals_description))
                .build(),
        )
    }

    override fun post(alert: ApprovalAlert) {
        // Without the permission (API 33+, denied or never asked) nothing shows.
        if (!NotificationPermission.granted(context)) return
        val tap = PendingIntent.getActivity(
            context,
            0,
            ApprovalTap.intent(context, activity, alert),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val title = alert.sessionTitle.ifEmpty { context.getString(R.string.notif_untitled_session) }
        val notification = NotificationCompat.Builder(context, CHANNEL)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(context.getString(R.string.notif_approval_title))
            .setContentText(title)
            .setPriority(NotificationCompat.PRIORITY_HIGH)
            .setCategory(NotificationCompat.CATEGORY_REMINDER)
            .setContentIntent(tap)
            .setAutoCancel(true)
            .setGroup(GROUP)
            .build()
        try {
            manager.notify(alert.approvalId, ID, notification)
        } catch (_: SecurityException) {
            // Revoked between the check and here: same as never granted.
        }
    }

    override fun cancel(approvalId: String) = manager.cancel(approvalId, ID)

    override fun active(): Set<String> =
        manager.activeNotifications.filter { it.id == ID && it.tag != null }.mapTo(HashSet()) { it.tag }

    private companion object {
        const val CHANNEL = "approvals"
        const val GROUP = "dev.skywave.android.approvals"

        /** Every approval notification shares this id; the tag tells them apart. */
        const val ID = 1
    }
}
