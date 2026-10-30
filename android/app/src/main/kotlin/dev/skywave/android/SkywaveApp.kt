package dev.skywave.android

import android.app.Application
import dev.skywave.core.initLogging

/** Process-wide setup. A07 makes this the owner of the single `SkywaveCore` (D17). */
class SkywaveApp : Application() {
    override fun onCreate() {
        super.onCreate()
        // Routes the core's tracing output to logcat; null keeps the default filter.
        initLogging(null)
    }
}
