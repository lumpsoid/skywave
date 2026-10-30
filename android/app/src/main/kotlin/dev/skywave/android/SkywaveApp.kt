package dev.skywave.android

import android.app.Application
import android.net.ConnectivityManager
import android.os.Build
import android.provider.Settings
import android.util.Log
import androidx.lifecycle.ProcessLifecycleOwner
import dev.skywave.android.core.CoreLifecycleObserver
import dev.skywave.android.core.CoreStartup
import dev.skywave.android.core.NetworkWatcher
import dev.skywave.android.core.startCore
import dev.skywave.core.SkywaveCore
import dev.skywave.core.initLogging
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import java.io.File

/**
 * Process-wide owner of the single core (D17) and the manual DI root (D18):
 * activities read [core] and never open their own, so a configuration
 * change reuses the same core.
 */
class SkywaveApp : Application() {
    /** Lives as long as the process, like the core. */
    private val appScope = CoroutineScope(SupervisorJob())

    /**
     * Opened on first use (the Activity's `onCreate`), on that thread: the
     * first snapshot is read synchronously so the first frame is the local
     * replica. Thread-safe; opened once per process.
     */
    val core: CoreStartup by lazy { openCore() }

    override fun onCreate() {
        super.onCreate()
        // Routes the core's tracing output to logcat; null keeps the default filter.
        initLogging(null)
        // Registered before any activity starts, so the first ON_START is seen.
        ProcessLifecycleOwner.get().lifecycle.addObserver(
            CoreLifecycleObserver { (core as? CoreStartup.Ready)?.repository },
        )
    }

    private fun openCore(): CoreStartup {
        val dataDir = File(filesDir, "skywave") // D22
        Log.i(TAG, "opening core at $dataDir")
        val startup = startCore(
            open = { SkywaveCore.open(dataDir.path, deviceName()) },
            scope = appScope,
            dispatchContext = Dispatchers.IO,
        )
        when (startup) {
            is CoreStartup.Ready ->
                NetworkWatcher(startup.repository).register(getSystemService(ConnectivityManager::class.java))
            is CoreStartup.Failed -> Log.e(TAG, "core failed to open: ${startup.reason}")
        }
        return startup
    }

    /** D17: the user-visible device name if set, else the model. */
    private fun deviceName(): String =
        Settings.Global.getString(contentResolver, Settings.Global.DEVICE_NAME)
            ?.takeIf { it.isNotBlank() }
            ?: Build.MODEL

    private companion object {
        const val TAG = "SkywaveApp"
    }
}
