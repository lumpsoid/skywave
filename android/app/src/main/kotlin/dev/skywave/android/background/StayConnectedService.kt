package dev.skywave.android.background

import android.app.Notification
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import android.util.Log
import androidx.core.app.NotificationChannelCompat
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.app.ServiceCompat
import androidx.core.content.ContextCompat
import dev.skywave.android.MainActivity
import dev.skywave.android.R
import dev.skywave.core.Link
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.launch

/**
 * What the ongoing notification says about the link: coarse on purpose, so
 * it changes only when the user would care (not on every latency or
 * catch-up count).
 */
sealed interface ConnectionLabel {
    data object Connected : ConnectionLabel

    data object Connecting : ConnectionLabel

    /** Down; the next attempt is due at [retryAtMs] (wall clock), counted down by the system. */
    data class Offline(val retryAtMs: Long) : ConnectionLabel

    data object Refused : ConnectionLabel

    data object NotPaired : ConnectionLabel
}

fun connectionLabel(link: Link): ConnectionLabel = when (link) {
    is Link.Online -> ConnectionLabel.Connected
    Link.Connecting, is Link.CatchingUp -> ConnectionLabel.Connecting
    is Link.Reconnecting -> ConnectionLabel.Offline(link.retryAtMs)
    is Link.Rejected -> ConnectionLabel.Refused
    Link.Unpaired -> ConnectionLabel.NotPaired
}

/**
 * "Stay connected" (A15, D21): a foreground service that holds the core
 * awake as [Holder.SERVICE], so the core's long-poll keeps running with the
 * app in the background and approvals alert within seconds. It does no work
 * of its own: no wakelock, no polling; its ongoing notification on the
 * low-importance "Connection" channel shows the link and changes only when
 * [connectionLabel] does.
 *
 * Type `specialUse` on API 34+: a user-chosen, indefinite connection to the
 * user's own server fits none of the other types; `dataSync` would also be
 * cut after 6 hours a day on API 35+. Started only from the foreground
 * ([StayConnectedPreference]); `START_STICKY` so the system brings it back if
 * it has to kill it, and it stops itself if the choice is off by then. Not
 * restarted on boot: it comes back the next time the app is opened.
 */
class StayConnectedService : Service() {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    private var holding = false

    /** What the notification shows now; a repeated start re-posts it unchanged. */
    private var shown: ConnectionLabel = ConnectionLabel.Connecting

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val host = application as BackgroundHost
        // Foreground first, even when about to stop: a service started with
        // startForegroundService must call it or the app is killed.
        if (!goForeground(notification(shown))) {
            stopSelf()
            return START_NOT_STICKY
        }
        // Turned off since (a sticky restart), or no core to hold.
        val links = if (host.stayConnected.on.value) host.openLinks() else null
        if (links == null) {
            stopSelf()
            return START_NOT_STICKY
        }
        if (!holding) {
            holding = true
            host.awake.hold(Holder.SERVICE)
            scope.launch {
                links.map { connectionLabel(it.link) }.distinctUntilChanged().collect {
                    shown = it
                    NotificationManagerCompat.from(this@StayConnectedService).notifyIfAllowed(it)
                }
            }
        }
        return START_STICKY
    }

    override fun onDestroy() {
        scope.cancel()
        if (holding) (application as BackgroundHost).awake.release(Holder.SERVICE)
        super.onDestroy()
    }

    private fun goForeground(notification: Notification): Boolean = try {
        val type = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE
        } else {
            0
        }
        ServiceCompat.startForeground(this, NOTIFICATION_ID, notification, type)
        true
    } catch (e: IllegalStateException) {
        // ForegroundServiceStartNotAllowedException (API 31+): a sticky
        // restart while the app is in the background.
        Log.w(TAG, "may not run in the foreground now", e)
        false
    }

    private fun NotificationManagerCompat.notifyIfAllowed(label: ConnectionLabel) {
        try {
            notify(NOTIFICATION_ID, notification(label))
        } catch (_: SecurityException) {
            // No POST_NOTIFICATIONS: the service runs, the shade shows nothing.
        }
    }

    private fun notification(label: ConnectionLabel): Notification {
        val open = PendingIntent.getActivity(
            this,
            0,
            Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
            PendingIntent.FLAG_IMMUTABLE,
        )
        val builder = NotificationCompat.Builder(this, CHANNEL)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(getString(R.string.connection_title))
            .setContentIntent(open)
            .setOngoing(true)
            .setSilent(true)
            .setPriority(NotificationCompat.PRIORITY_LOW)
            .setCategory(NotificationCompat.CATEGORY_SERVICE)
            .setForegroundServiceBehavior(NotificationCompat.FOREGROUND_SERVICE_IMMEDIATE)
            .setShowWhen(false)
        val text = when (label) {
            ConnectionLabel.Connected -> R.string.connection_connected
            ConnectionLabel.Connecting -> R.string.connection_connecting
            is ConnectionLabel.Offline -> {
                // The system counts down; nothing here ticks.
                builder.setShowWhen(true).setWhen(label.retryAtMs).setUsesChronometer(true).setChronometerCountDown(true)
                R.string.connection_offline
            }
            ConnectionLabel.Refused -> R.string.connection_refused
            ConnectionLabel.NotPaired -> R.string.connection_not_paired
        }
        return builder.setContentText(getString(text)).build()
    }

    companion object {
        private const val TAG = "StayConnected"
        private const val CHANNEL = "connection"

        /** Distinct from the approvals' id 1 (those are also tagged). */
        private const val NOTIFICATION_ID = 2

        /** Creates or updates the "Connection" channel; cheap, so done on every start. */
        fun createChannel(context: Context) {
            NotificationManagerCompat.from(context).createNotificationChannel(
                NotificationChannelCompat.Builder(CHANNEL, NotificationManagerCompat.IMPORTANCE_LOW)
                    .setName(context.getString(R.string.notif_channel_connection))
                    .setDescription(context.getString(R.string.notif_channel_connection_description))
                    .setShowBadge(false)
                    .build(),
            )
        }

        fun control(context: Context): ServiceControl {
            val app = context.applicationContext
            val intent = Intent(app, StayConnectedService::class.java)
            return object : ServiceControl {
                override fun start() {
                    try {
                        ContextCompat.startForegroundService(app, intent)
                    } catch (e: IllegalStateException) {
                        // Only started from the foreground; this is a race with leaving it.
                        Log.w(TAG, "could not start", e)
                    }
                }

                override fun stop() {
                    app.stopService(intent)
                }
            }
        }
    }
}
