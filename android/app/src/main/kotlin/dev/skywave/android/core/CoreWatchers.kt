package dev.skywave.android.core

import android.net.ConnectivityManager
import android.net.Network
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.LifecycleOwner
import dev.skywave.android.background.CoreAwake
import dev.skywave.android.background.Holder

/**
 * Follows the whole app's foreground state (D21): register on
 * `ProcessLifecycleOwner`, so rotation and activity switches do not count.
 * Being visible is one [Holder] of the core's link; going to the background
 * suspends the core only if nothing else (a catch-up, the service) holds it.
 * [onForeground] runs on every start, after the hold.
 */
class CoreLifecycleObserver(
    private val awake: CoreAwake,
    private val onForeground: () -> Unit = {},
) : DefaultLifecycleObserver {
    override fun onStart(owner: LifecycleOwner) {
        awake.hold(Holder.FOREGROUND)
        onForeground()
    }

    override fun onStop(owner: LifecycleOwner) {
        awake.release(Holder.FOREGROUND)
    }
}

/** Nudges a reconnect when a default network appears. Lives as long as the process. */
class NetworkWatcher(private val repository: CoreRepository) : ConnectivityManager.NetworkCallback() {
    fun register(connectivity: ConnectivityManager) = connectivity.registerDefaultNetworkCallback(this)

    override fun onAvailable(network: Network) = repository.onNetworkAvailable()
}
