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
 * with the code's text, or null when cancelled.
 */
class MainActivity : FlutterActivity() {
    private val scan = ScanContract()
    private var scanning: MethodChannel.Result? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // Not again when the activity is only recreated (rotation, theme).
        if (savedInstanceState == null) askForPermissions()
    }

    override fun configureFlutterEngine(flutterEngine: FlutterEngine) {
        super.configureFlutterEngine(flutterEngine)
        MethodChannel(flutterEngine.dartExecutor.binaryMessenger, SCAN_CHANNEL)
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

    private companion object {
        const val PERMISSIONS_REQUEST = 1
        const val SCAN_REQUEST = 2
        const val SCAN_CHANNEL = "dev.skywave.gui/scan"
        const val PREFS = "permissions"
        const val NOTIFICATIONS_ASKED = "notifications_asked"
    }
}
