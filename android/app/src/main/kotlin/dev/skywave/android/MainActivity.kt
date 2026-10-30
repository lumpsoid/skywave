package dev.skywave.android

import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import dev.skywave.android.core.CoreStartup
import dev.skywave.android.ui.SkywaveRoot
import dev.skywave.android.ui.SkywaveTheme
import dev.skywave.android.ui.StartupFailedScreen

/**
 * Single Activity (D18). The core lives in [SkywaveApp], so recreating this
 * Activity (rotation) only re-collects its state.
 */
class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        val startup = (application as SkywaveApp).core
        setContent {
            SkywaveTheme {
                when (startup) {
                    is CoreStartup.Ready -> SkywaveRoot(startup.repository)
                    is CoreStartup.Failed -> StartupFailedScreen(startup.reason, appVersion())
                }
            }
        }
    }

    /** This app's version name; no native call, the core may not have loaded. */
    private fun appVersion(): String? =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            packageManager.getPackageInfo(packageName, PackageManager.PackageInfoFlags.of(0))
        } else {
            @Suppress("DEPRECATION")
            packageManager.getPackageInfo(packageName, 0)
        }.versionName
}
