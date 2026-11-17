package io.github.lumpsoid.skywave

import android.app.Notification
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.util.Log
import androidx.core.app.NotificationChannelCompat
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.app.ServiceCompat
import androidx.core.content.ContextCompat
import io.flutter.embedding.engine.FlutterEngineCache
import io.flutter.plugin.common.MethodChannel

/**
 * "Stay connected": a foreground service whose only job is to exist, so the
 * process keeps running with the app in the background and the core's
 * long-poll alerts on approvals within seconds. The hub decides whether the
 * core stays awake; this service only tells it, through Dart, whether it
 * runs: `running` on [BACKGROUND_CHANNEL] after every successful
 * `startForeground` (a repeated start re-reports, so a fresh hub learns it),
 * `stopped` when it ends, or when its start is refused while no instance
 * runs. No wakelock.
 *
 * Type `specialUse` on API 34+: a user-chosen, indefinite connection to the
 * user's own server fits none of the other types. `dataSync` is cut after
 * 6 hours a day on API 35+, and `remoteMessaging` is for messaging apps.
 *
 * Started only from the foreground (the hub asks when the app is in front).
 * Not sticky: a sticky restart happens in the background, where API 31+
 * refuses `startForeground`. It comes back the next time the app is opened.
 *
 * The notification text is static; showing the link's state is deferred.
 */
class StayConnectedService : Service() {
    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        // Foreground first: a service started with startForegroundService
        // must call it, or the app is killed.
        if (goForeground()) {
            running = true
            report(RUNNING)
        } else {
            report(STOPPED)
            stopSelf()
        }
        return START_NOT_STICKY
    }

    override fun onDestroy() {
        running = false
        report(STOPPED)
        super.onDestroy()
    }

    private fun goForeground(): Boolean = try {
        createChannel()
        val type = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE
        } else {
            0
        }
        ServiceCompat.startForeground(this, NOTIFICATION_ID, notification(), type)
        true
    } catch (e: IllegalStateException) {
        // ForegroundServiceStartNotAllowedException (API 31+): the app is
        // in the background by now.
        Log.w(TAG, "may not run in the foreground now", e)
        false
    }

    /** Creates or updates the "Connection" channel; cheap, so done on every start. */
    private fun createChannel() {
        NotificationManagerCompat.from(this).createNotificationChannel(
            NotificationChannelCompat.Builder(CHANNEL, NotificationManagerCompat.IMPORTANCE_LOW)
                .setName("Connection")
                .setDescription("Shown while Skywave stays connected in the background.")
                .setShowBadge(false)
                .build(),
        )
    }

    private fun notification(): Notification {
        val open = PendingIntent.getActivity(
            this,
            0,
            Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
            PendingIntent.FLAG_IMMUTABLE,
        )
        return NotificationCompat.Builder(this, CHANNEL)
            .setSmallIcon(R.drawable.ic_stat_skywave)
            .setContentTitle("Skywave")
            .setContentText("Keeping the link to your server open")
            .setContentIntent(open)
            .setOngoing(true)
            .setSilent(true)
            .setPriority(NotificationCompat.PRIORITY_LOW)
            .setCategory(NotificationCompat.CATEGORY_SERVICE)
            .setForegroundServiceBehavior(NotificationCompat.FOREGROUND_SERVICE_IMMEDIATE)
            .setShowWhen(false)
            .build()
    }

    companion object {
        private const val TAG = "StayConnected"
        private const val CHANNEL = "connection"

        /**
         * Negative: the hub's notification ids (notify.rs) span every
         * positive `int`, untagged, and startForeground takes no tag. Not 0.
         */
        private const val NOTIFICATION_ID = -1

        /**
         * Dart asks `start`/`stop` on it (MainActivity); Kotlin reports
         * `running`/`stopped` back on it. One handler per direction.
         */
        const val BACKGROUND_CHANNEL = "io.github.lumpsoid.skywave/background"
        private const val RUNNING = "running"
        private const val STOPPED = "stopped"

        /**
         * An instance is in the foreground: set after its `startForeground`
         * succeeded, cleared in its `onDestroy`. Main thread only, like
         * every service callback; volatile all the same.
         */
        @Volatile
        private var running = false

        /**
         * Starts the service, or asks a running one to report again. A
         * refused start is reported as stopped only if no instance runs.
         */
        fun start(context: Context) {
            val app = context.applicationContext
            try {
                ContextCompat.startForegroundService(app, Intent(app, StayConnectedService::class.java))
            } catch (e: IllegalStateException) {
                // ForegroundServiceStartNotAllowedException: only started
                // from the foreground, so a race with leaving it. A refused
                // re-start leaves a running service running: reporting
                // `stopped` then would let the hub suspend a core the
                // service still holds. A running instance's onDestroy is
                // the definitive `stopped`.
                Log.w(TAG, "could not start", e)
                if (!running) {
                    report(STOPPED)
                }
            }
        }

        fun stop(context: Context) {
            val app = context.applicationContext
            app.stopService(Intent(app, StayConnectedService::class.java))
        }

        /**
         * Tells Dart, on the main thread, if the app's engine exists. Looks
         * it up only: creating it here would boot a core with no activity.
         */
        private fun report(method: String) {
            Handler(Looper.getMainLooper()).post {
                val engine = FlutterEngineCache.getInstance().get(MainActivity.ENGINE_ID) ?: return@post
                MethodChannel(engine.dartExecutor.binaryMessenger, BACKGROUND_CHANNEL)
                    .invokeMethod(method, null)
            }
        }
    }
}
