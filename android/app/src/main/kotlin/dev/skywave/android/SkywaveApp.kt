package dev.skywave.android

import android.app.Application
import android.net.ConnectivityManager
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.provider.Settings
import android.util.Log
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.ProcessLifecycleOwner
import dev.skywave.android.background.BackgroundHost
import dev.skywave.android.background.CatchUpWorker
import dev.skywave.android.background.CoreAwake
import dev.skywave.android.background.StayConnectedPreference
import dev.skywave.android.background.StayConnectedService
import dev.skywave.android.core.CoreLifecycleObserver
import dev.skywave.android.core.CoreRepository
import dev.skywave.android.core.CoreStartup
import dev.skywave.android.core.LinkStatus
import dev.skywave.android.core.NetworkWatcher
import dev.skywave.android.core.startCore
import dev.skywave.android.notifications.AndroidNotifications
import dev.skywave.android.notifications.ApprovalNotifier
import dev.skywave.core.Link
import dev.skywave.core.SkywaveCore
import dev.skywave.core.ViewState
import dev.skywave.core.initLogging
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import java.io.File

/**
 * Process-wide owner of the single core (D17) and the manual DI root (D18):
 * activities read [core] and never open their own, so a configuration
 * change reuses the same core.
 */
class SkywaveApp : Application(), BackgroundHost {
    /** Lives as long as the process, like the core. */
    private val appScope = CoroutineScope(SupervisorJob())

    /**
     * Opened on first use (the Activity's `onCreate`), on that thread: the
     * first snapshot is read synchronously so the first frame is the local
     * replica. Thread-safe; opened once per process.
     */
    val core: CoreStartup by lazy { openCore() }

    /** Who keeps the core's link up: the visible app, a catch-up, the service (D21, A15). */
    override val awake = CoreAwake()

    /** The "Stay connected" choice; this device's, not the core's (A15). */
    override val stayConnected: StayConnectedPreference by lazy { StayConnectedPreference.from(this) }

    override fun openLinks(): StateFlow<LinkStatus>? = (core as? CoreStartup.Ready)?.repository?.linkStatus

    /** The approval notifications' shade; usable before the core opens (A15). */
    private val notifications: AndroidNotifications by lazy { AndroidNotifications(this, MainActivity::class.java) }

    override fun onCreate() {
        super.onCreate()
        notifications.createChannel()
        StayConnectedService.createChannel(this)
        // Registered before any activity starts, so the first ON_START is
        // seen. Coming to the foreground is the one moment the service may
        // be started from besides the toggle (Android 12+).
        ProcessLifecycleOwner.get().lifecycle.addObserver(
            CoreLifecycleObserver(awake, onForeground = { stayConnected.onForeground() }),
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
                // On the main thread, after this one's work: an activity
                // opening the core gets its ON_START in first, so a core
                // about to be shown is not suspended and resumed at once.
                Handler(Looper.getMainLooper()).post { awake.attach(startup.repository) }
                scheduleCatchUpOncePaired(startup.repository)
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

    /** Background catch-ups (A15) from the first time the device is paired; every start re-checks. */
    private fun scheduleCatchUpOncePaired(repository: CoreRepository) {
        appScope.launch {
            repository.state.first { it.link !is Link.Unpaired }
            CatchUpWorker.schedule(this@SkywaveApp)
        }
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
