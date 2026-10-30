package dev.skywave.android.background

import android.app.Application
import android.content.Context
import androidx.test.core.app.ApplicationProvider
import androidx.work.NetworkType
import androidx.work.WorkInfo
import androidx.work.WorkManager
import androidx.work.testing.WorkManagerTestInitHelper
import dev.skywave.core.Link
import dev.skywave.core.LinkRejection
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.Shadows.shadowOf
import java.util.concurrent.TimeUnit

/** The preference, the service control it drives, the notification labels, the schedule (Robolectric). */
@RunWith(RobolectricTestRunner::class)
class StayConnectedTest {
    private val app: Application = ApplicationProvider.getApplicationContext()
    private val shadowApp = shadowOf(app)

    private fun preference() = StayConnectedPreference.from(app)

    @Test
    fun offByDefaultAndStartsNothing() {
        val preference = preference()
        assertFalse(preference.on.value)
        preference.onForeground()
        assertNull(shadowApp.nextStartedService)
    }

    @Test
    fun turningOnStartsTheServiceAndIsRemembered() {
        preference().set(true)
        assertEquals(StayConnectedService::class.java.name, shadowApp.nextStartedService.component?.className)
        assertTrue("a new process reads it back", preference().on.value)
    }

    @Test
    fun turningOffStopsTheService() {
        val preference = preference()
        preference.set(true)
        shadowApp.clearStartedServices()
        preference.set(false)
        assertEquals(StayConnectedService::class.java.name, shadowApp.nextStoppedService.component?.className)
        assertNull(shadowApp.nextStartedService)
        assertFalse(preference().on.value)
    }

    @Test
    fun comingToTheForegroundRestartsItWhenOn() {
        preference().set(true)
        shadowApp.clearStartedServices()
        preference().onForeground()
        assertEquals(StayConnectedService::class.java.name, shadowApp.nextStartedService.component?.className)
    }

    @Test
    fun theNotificationOnlyChangesWhenTheUserWouldCare() {
        assertEquals(ConnectionLabel.Connected, connectionLabel(Link.Online(5u)))
        assertEquals(connectionLabel(Link.Online(5u)), connectionLabel(Link.Online(900u)))
        assertEquals(ConnectionLabel.Connecting, connectionLabel(Link.Connecting))
        assertEquals(ConnectionLabel.Connecting, connectionLabel(Link.CatchingUp(have = 3u, server = 9u)))
        assertEquals(
            ConnectionLabel.Offline(retryAtMs = 4_000),
            connectionLabel(Link.Reconnecting(attempt = 2u, retryAtMs = 4_000, offlineSinceMs = 0)),
        )
        assertEquals(ConnectionLabel.Refused, connectionLabel(Link.Rejected(LinkRejection.Revoked)))
        assertEquals(ConnectionLabel.NotPaired, connectionLabel(Link.Unpaired))
    }

    @Test
    fun catchUpIsScheduledOnceEveryFifteenMinutesOnAnyNetwork() {
        WorkManagerTestInitHelper.initializeTestWorkManager(app)
        CatchUpWorker.schedule(app)
        val first = infos(app).single()
        CatchUpWorker.schedule(app) // every start: keeps the existing schedule
        val info = infos(app).single()
        assertEquals(first.id, info.id)
        assertEquals(WorkInfo.State.ENQUEUED, info.state)
        assertEquals(NetworkType.CONNECTED, info.constraints.requiredNetworkType)
        assertEquals(TimeUnit.MINUTES.toMillis(15), info.periodicityInfo?.repeatIntervalMillis)
    }

    private fun infos(context: Context): List<WorkInfo> =
        WorkManager.getInstance(context).getWorkInfosForUniqueWork("catch-up").get()
}
