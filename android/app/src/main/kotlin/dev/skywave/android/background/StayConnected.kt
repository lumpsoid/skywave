package dev.skywave.android.background

import android.content.Context
import android.content.SharedPreferences
import androidx.core.content.edit
import dev.skywave.android.ui.screens.StayConnectedSetting
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

/** Starts and stops [StayConnectedService]; faked in tests. */
interface ServiceControl {
    fun start()

    fun stop()
}

/**
 * The "Stay connected" choice (A15). It is this device's, not the core's: a
 * platform concern with no rule in it (D2), so it lives in
 * [SharedPreferences] and not in `SettingsView`.
 *
 * The service is only ever started from the foreground: by the toggle, or
 * when the app comes to the foreground with the choice on ([onForeground]).
 * Android 12+ refuses a foreground service started from the background.
 */
class StayConnectedPreference(
    private val prefs: SharedPreferences,
    private val service: ServiceControl,
) : StayConnectedSetting {
    private val state = MutableStateFlow(prefs.getBoolean(KEY, false))
    override val on: StateFlow<Boolean> = state.asStateFlow()

    override fun set(on: Boolean) {
        prefs.edit { putBoolean(KEY, on) }
        state.value = on
        if (on) service.start() else service.stop()
    }

    /** The app is visible: (re)starts the service if the user chose to stay connected. */
    fun onForeground() {
        if (state.value) service.start()
    }

    companion object {
        const val PREFS = "background"
        private const val KEY = "stay_connected"

        fun from(context: Context) = StayConnectedPreference(
            context.getSharedPreferences(PREFS, Context.MODE_PRIVATE),
            StayConnectedService.control(context),
        )
    }
}
