package dev.skywave.android.core

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.os.Build
import androidx.core.content.ContextCompat

/**
 * ACCESS_LOCAL_NETWORK (API 37+): without it, connecting to a server on the
 * LAN times out. The app is useless without the server, so it is asked on
 * every launch until granted; the system stops showing the prompt after
 * repeated denials, and system settings can still grant it.
 */
object LocalNetworkPermission {
    /** Whether the app may reach local addresses; always true below API 37. */
    fun granted(context: Context): Boolean =
        Build.VERSION.SDK_INT < Build.VERSION_CODES.CINNAMON_BUN ||
            ContextCompat.checkSelfPermission(context, Manifest.permission.ACCESS_LOCAL_NETWORK) ==
            PackageManager.PERMISSION_GRANTED
}
