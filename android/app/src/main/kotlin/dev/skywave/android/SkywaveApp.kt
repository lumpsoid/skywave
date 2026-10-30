package dev.skywave.android

import android.app.Application
import android.net.ConnectivityManager
import android.os.Build
import android.provider.Settings
import android.util.Log
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.ProcessLifecycleOwner
import dev.skywave.android.core.CoreLifecycleObserver
import dev.skywave.android.core.CoreStartup
import dev.skywave.android.core.NetworkWatcher
import dev.skywave.android.core.startCore
import dev.skywave.android.notifications.AndroidNotifications
import dev.skywave.android.notifications.ApprovalNotifier
import dev.skywave.core.SkywaveCore
import dev.skywave.core.ViewState
import dev.skywave.core.initLogging
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.launch
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

    /** The approval notifications' shade; usable before the core opens (A15). */
    private val notifications: AndroidNotifications by lazy { AndroidNotifications(this, MainActivity::class.java) }

    override fun onCreate() {
        super.onCreate()
        notifications.createChannel()
        // Registered before any activity starts, so the first ON_START is seen.
        ProcessLifecycleOwner.get().lifecycle.addObserver(
            CoreLifecycleObserver { (core as? CoreStartup.Ready)?.repository },
        )
    }

    private fun openCore(): CoreStartup {
        val dataDir = File(filesDir, "skywave") // D22
        Log.i(TAG, "opening core at $dataDir")
        val startup = startCore(
            open = {
                // The first native calls, inside open so a library that will
                // not load ends at the Failed screen. Routes the core's
                // tracing to logcat; null keeps the default filter.
                initLogging(null)
                SkywaveCore.open(dataDir.path, deviceName)
            },
            scope = appScope,
            dispatchContext = Dispatchers.IO,
        )
        when (startup) {
            is CoreStartup.Ready -> {
                NetworkWatcher(startup.repository).register(getSystemService(ConnectivityManager::class.java))
                followApprovals(startup.repository.state)
            }
            is CoreStartup.Failed -> Log.e(TAG, "core failed to open: ${startup.reason}")
        }
        return startup
    }

    /**
     * Posts approval notifications for as long as the process lives. The
     * first snapshot (the replica as stored) is fed here, synchronously, so
     * it is the notifier's baseline whatever the collector sees first.
     */
    private fun followApprovals(state: StateFlow<ViewState>) {
        val notifier = ApprovalNotifier(notifications) {
            ProcessLifecycleOwner.get().lifecycle.currentState.isAtLeast(Lifecycle.State.STARTED)
        }
        notifier.onView(state.value)
        appScope.launch(Dispatchers.Main) { notifier.follow(state) }
    }

    /**
     * D17: the user-visible device name if set, else the model. Read once,
     * so the name Settings shows is the one this process pairs with.
     */
    val deviceName: String by lazy {
        Settings.Global.getString(contentResolver, Settings.Global.DEVICE_NAME)
            ?.takeIf { it.isNotBlank() }
            ?: Build.MODEL
    }

    private companion object {
        const val TAG = "SkywaveApp"
    }
}
