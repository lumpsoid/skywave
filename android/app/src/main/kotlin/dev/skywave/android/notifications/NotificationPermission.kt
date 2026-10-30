package dev.skywave.android.notifications

import android.Manifest
import android.content.Context
import android.content.SharedPreferences
import android.content.pm.PackageManager
import android.os.Build
import androidx.core.content.ContextCompat
import androidx.core.content.edit

/**
 * POST_NOTIFICATIONS (API 33+), asked once: the first time the user opens a
 * session, not on first launch. Whatever the answer, it is not asked again
 * (no nagging); system settings can still turn notifications on.
 */
class NotificationPermission(private val prefs: SharedPreferences) {
    /** Whether to show the system prompt now; true at most once per install. */
    fun shouldAsk(context: Context): Boolean =
        Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            !granted(context) &&
            !prefs.getBoolean(KEY_ASKED, false)

    /** Records that the prompt was shown, before its answer comes back. */
    fun markAsked() = prefs.edit { putBoolean(KEY_ASKED, true) }

    companion object {
        const val PREFS = "notifications"
        private const val KEY_ASKED = "post_notifications_asked"

        /** Whether this app may post notifications; always true below API 33. */
        fun granted(context: Context): Boolean =
            Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU ||
                ContextCompat.checkSelfPermission(context, Manifest.permission.POST_NOTIFICATIONS) ==
                PackageManager.PERMISSION_GRANTED

        fun from(context: Context) =
            NotificationPermission(context.getSharedPreferences(PREFS, Context.MODE_PRIVATE))
    }
}
