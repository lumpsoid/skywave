package dev.skywave.android.core

import android.net.ConnectivityManager
import android.net.Network
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.LifecycleOwner

/**
 * Follows the whole app's foreground state (D21): register on
 * `ProcessLifecycleOwner`, so rotation and activity switches do not count.
 * [repository] is null when the core failed to open.
 */
class CoreLifecycleObserver(private val repository: () -> CoreRepository?) : DefaultLifecycleObserver {
    override fun onStart(owner: LifecycleOwner) {
        repository()?.resume()
    }

    override fun onStop(owner: LifecycleOwner) {
        repository()?.suspend()
    }
}

/** Nudges a reconnect when a default network appears. Lives as long as the process. */
class NetworkWatcher(private val repository: CoreRepository) : ConnectivityManager.NetworkCallback() {
    fun register(connectivity: ConnectivityManager) = connectivity.registerDefaultNetworkCallback(this)

    override fun onAvailable(network: Network) = repository.onNetworkAvailable()
}
