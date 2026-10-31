package dev.skywave.android.core

import android.Manifest
import android.app.Application
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config

/**
 * ACCESS_LOCAL_NETWORK only gates API 37+. The API 37 side is untested here:
 * Robolectric's SDK 37 sandbox needs Java 21, the tests run on 17.
 */
@RunWith(RobolectricTestRunner::class)
class LocalNetworkPermissionTest {
    private val app: Application = RuntimeEnvironment.getApplication()

    @Test
    @Config(sdk = [35])
    fun grantedBelowApi37EvenWhenDenied() {
        shadowOf(app).denyPermissions(Manifest.permission.ACCESS_LOCAL_NETWORK)
        assertTrue(LocalNetworkPermission.granted(app))
    }
}
