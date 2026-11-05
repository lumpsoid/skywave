package dev.skywave.gui

import android.Manifest
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import com.journeyapps.barcodescanner.ScanContract
import com.journeyapps.barcodescanner.ScanOptions
import io.flutter.embedding.android.FlutterActivity
import io.flutter.embedding.engine.FlutterEngine
import io.flutter.embedding.engine.FlutterEngineCache
import io.flutter.embedding.engine.dart.DartExecutor
import io.flutter.plugin.common.MethodChannel

/**
 * The Flutter activity, plus the runtime permissions it asks for at launch,
 * as the Kotlin head does. Android shows one permission request at a time,
 * so both go out in one request.
 *
 * - ACCESS_LOCAL_NETWORK (API 37+): without it a connection to a server on
 *   the LAN just times out, and the app is useless without the server, so it
 *   is asked on every launch until granted. The system stops prompting after
 *   repeated denials; system settings can still grant it.
 * - POST_NOTIFICATIONS (API 33+): approval notifications. Asked once, whatever
 *   the answer; system settings can change it later.
 *
 * It also scans the pairing QR code for Dart (D20): `scan` on [SCAN_CHANNEL]
 * opens the ZXing scanner, which asks for the camera itself, and answers
 * with the code's text, or null when cancelled. `start`/`stop` on
 * [StayConnectedService.BACKGROUND_CHANNEL] start or stop the stay-connected
 * service; [engine] binds that channel, so it answers with no activity too.
 *
 * The Flutter engine, and with it Dart and the Rust core, belongs to the
 * app, not to this activity: [engine] creates it once per process and every
 * activity attaches to it, so the core outlives a destroyed activity. It is
 * never destroyed; the process ending ends it.
 */
class MainActivity : FlutterActivity() {
    private val scan = ScanContract()
    private var scanning: MethodChannel.Result? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // Not again when the activity is only recreated (rotation, theme).
        if (savedInstanceState == null) askForPermissions()
    }

    // Host-provided, so the activity does not destroy it when it goes.
    override fun provideFlutterEngine(context: Context): FlutterEngine = engine(context)

    // Runs for every activity on the shared engine, so the channel answers
    // the activity on screen; [cleanUpFlutterEngine] unbinds it again.
    override fun configureFlutterEngine(flutterEngine: FlutterEngine) {
        super.configureFlutterEngine(flutterEngine)
        flutterEngine.scanChannel()
            .setMethodCallHandler { call, result ->
                when {
                    call.method != "scan" -> result.notImplemented()
                    scanning != null -> result.error("busy", "already scanning", null)
                    else -> {
                        scanning = result
                        val options = ScanOptions()
                            .setDesiredBarcodeFormats(ScanOptions.QR_CODE)
                            .setPrompt("Scan the QR code skywaved pair printed")
                            .setBeepEnabled(false)
                            .setOrientationLocked(false)
                        startActivityForResult(scan.createIntent(this, options), SCAN_REQUEST)
                    }
                }
            }
    }

    // FlutterActivity is no ComponentActivity, so no activity result API.
    @Deprecated("Deprecated in Java")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode != SCAN_REQUEST) return
        val result = scanning ?: return
        scanning = null
        result.success(scan.parseResult(resultCode, data).contents) // null: cancelled
    }

    // The activity leaves the engine, destroyed or evicted by a newer one.
    // The engine outlives it, so the handler, which holds this activity,
    // comes off the channel. A scan still open never gets its result here:
    // answer it as cancelled, so Dart does not wait for good.
    override fun cleanUpFlutterEngine(flutterEngine: FlutterEngine) {
        flutterEngine.scanChannel().setMethodCallHandler(null)
        scanning?.success(null)
        scanning = null
        super.cleanUpFlutterEngine(flutterEngine)
    }

    // A newer MainActivity took the engine: a notification tap starts one
    // whenever this one is not on top (singleTop only reuses the top one),
    // say under the QR scanner. Left in the back stack, this one would come
    // back with no engine: a frozen screen that reports no lifecycle, so the
    // core would stay suspended. Finished, it is not there to go back to.
    override fun detachFromFlutterEngine() {
        super.detachFromFlutterEngine()
        finish()
    }

    private fun FlutterEngine.scanChannel() =
        MethodChannel(dartExecutor.binaryMessenger, SCAN_CHANNEL)

    private fun askForPermissions() {
        val wanted = buildList {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.CINNAMON_BUN &&
                !granted(Manifest.permission.ACCESS_LOCAL_NETWORK)
            ) {
                add(Manifest.permission.ACCESS_LOCAL_NETWORK)
            }
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
                !granted(Manifest.permission.POST_NOTIFICATIONS) &&
                !notificationsAsked()
            ) {
                add(Manifest.permission.POST_NOTIFICATIONS)
                markNotificationsAsked()
            }
        }
        if (wanted.isNotEmpty()) requestPermissions(wanted.toTypedArray(), PERMISSIONS_REQUEST)
    }

    private fun granted(permission: String): Boolean =
        checkSelfPermission(permission) == PackageManager.PERMISSION_GRANTED

    private fun notificationsAsked(): Boolean =
        prefs().getBoolean(NOTIFICATIONS_ASKED, false)

    private fun markNotificationsAsked() {
        prefs().edit().putBoolean(NOTIFICATIONS_ASKED, true).apply()
    }

    private fun prefs() = getSharedPreferences(PREFS, Context.MODE_PRIVATE)

    companion object {
        /** The app's engine in [FlutterEngineCache]; look it up, never create it, elsewhere. */
        const val ENGINE_ID = "skywave"

        /** The app's engine, running Dart's `main`, created on first use. */
        @Synchronized
        fun engine(context: Context): FlutterEngine {
            val cache = FlutterEngineCache.getInstance()
            cache.get(ENGINE_ID)?.let { return it }
            // Registers the plugins itself; the activity does not for a
            // host-provided engine.
            val app = context.applicationContext
            val engine = FlutterEngine(app)
            // Bound for the engine's life, not an activity's: the hub stops
            // the service off screen too, when it gives up after a failure.
            // Only asks; the service reports what happened
            // (StayConnectedService).
            MethodChannel(engine.dartExecutor.binaryMessenger, StayConnectedService.BACKGROUND_CHANNEL)
                .setMethodCallHandler { call, result ->
                    when (call.method) {
                        "start" -> StayConnectedService.start(app)
                        "stop" -> StayConnectedService.stop(app)
                        else -> return@setMethodCallHandler result.notImplemented()
                    }
                    result.success(null)
                }
            engine.dartExecutor.executeDartEntrypoint(DartExecutor.DartEntrypoint.createDefault())
            cache.put(ENGINE_ID, engine)
            return engine
        }

        private const val PERMISSIONS_REQUEST = 1
        private const val SCAN_REQUEST = 2
        private const val SCAN_CHANNEL = "dev.skywave.gui/scan"
        private const val PREFS = "permissions"
        private const val NOTIFICATIONS_ASKED = "notifications_asked"
    }
}
