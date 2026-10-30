package dev.skywave.android

import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.remember
import dev.skywave.android.core.CoreClient
import dev.skywave.android.core.CoreStartup
import dev.skywave.android.core.coreBlobHeadReader
import dev.skywave.android.core.corePairUriParser
import dev.skywave.android.ui.PairInbox
import dev.skywave.android.ui.SkywaveRoot
import dev.skywave.android.ui.pairLinkInput
import dev.skywave.android.ui.screens.AboutInfo
import dev.skywave.android.ui.screens.LocalBlobHeadReader
import dev.skywave.core.Action
import dev.skywave.core.Route
import dev.skywave.core.coreVersion
import dev.skywave.android.ui.SkywaveTheme
import dev.skywave.android.ui.StartupFailedScreen

/**
 * Single Activity (D18). The core lives in [SkywaveApp], so recreating this
 * Activity (rotation) only re-collects its state.
 */
class MainActivity : ComponentActivity() {
    /** A pairing link waiting for the pairing form; saved with the Activity. */
    private val pairInbox = PairInbox(corePairUriParser)

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        val app = application as SkywaveApp
        val startup = app.core
        if (startup is CoreStartup.Ready) {
            if (savedInstanceState != null) {
                pairInbox.restoreFrom(savedInstanceState)
            } else if (intent.flags and Intent.FLAG_ACTIVITY_LAUNCHED_FROM_HISTORY == 0) {
                // A recreated Activity, or one reopened from recents, still
                // carries the link it was first opened with: handled then.
                openPairLink(intent, startup.repository)
            }
        }
        setContent {
            SkywaveTheme {
                when (startup) {
                    // The core reads blob previews: native, so only once it is Ready.
                    is CoreStartup.Ready -> CompositionLocalProvider(LocalBlobHeadReader provides coreBlobHeadReader) {
                        SkywaveRoot(startup.repository, pairInbox, remember { about(app) })
                    }
                    is CoreStartup.Failed -> StartupFailedScreen(startup.reason, appVersion())
                }
            }
        }
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        val startup = (application as SkywaveApp).core
        if (startup is CoreStartup.Ready) openPairLink(intent, startup.repository)
    }

    override fun onSaveInstanceState(outState: Bundle) {
        super.onSaveInstanceState(outState)
        pairInbox.saveTo(outState)
    }

    /** Shows pairing prefilled from [intent]'s link, if it has one; never pairs by itself. */
    private fun openPairLink(intent: Intent?, client: CoreClient) {
        val input = pairLinkInput(intent, pairInbox.parser) ?: return
        pairInbox.offer(input)
        client.dispatch(Action.Navigate(Route.PAIRING))
    }

    /** What Settings shows of this build. Call only once the core is Ready: [coreVersion] is native. */
    private fun about(app: SkywaveApp) =
        AboutInfo(appVersion = appVersion(), coreVersion = coreVersion(), deviceName = app.deviceName)

    /** This app's version name; no native call, the core may not have loaded. */
    private fun appVersion(): String? =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            packageManager.getPackageInfo(packageName, PackageManager.PackageInfoFlags.of(0))
        } else {
            @Suppress("DEPRECATION")
            packageManager.getPackageInfo(packageName, 0)
        }.versionName
}
