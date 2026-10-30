package dev.skywave.android.background

import android.app.Application
import android.app.Notification
import android.app.NotificationManager
import android.content.pm.ServiceInfo
import androidx.test.core.app.ApplicationProvider
import dev.skywave.android.core.LinkStatus
import dev.skywave.core.Link
import kotlinx.coroutines.flow.MutableStateFlow
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.Robolectric
import org.robolectric.RobolectricTestRunner
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config
import org.robolectric.shadows.ShadowLooper

/** A process without the native core: the link is a flow the test drives. */
class FakeHost : Application(), BackgroundHost {
    override val awake = CoreAwake()
    override val stayConnected by lazy { StayConnectedPreference.from(this) }
    val links = MutableStateFlow(LinkStatus(Link.Connecting, stale = false))
    val core = RecordingCore()

    override fun onCreate() {
        super.onCreate()
        StayConnectedService.createChannel(this)
        awake.attach(core)
    }

    override fun openLinks() = links
}

@RunWith(RobolectricTestRunner::class)
@Config(application = FakeHost::class)
class StayConnectedServiceTest {
    private val host: FakeHost = ApplicationProvider.getApplicationContext()
    private val controller = Robolectric.buildService(StayConnectedService::class.java).create()
    private val service get() = controller.get()

    private fun shownText(): String? {
        val manager = host.getSystemService(NotificationManager::class.java)
        return shadowOf(manager).allNotifications.single().extras.getString(Notification.EXTRA_TEXT)
    }

    @Test
    fun withTheChoiceOnItRunsInTheForegroundAndHoldsTheCore() {
        host.stayConnected.set(true)
        controller.startCommand(0, 1)
        ShadowLooper.idleMainLooper()
        assertFalse(shadowOf(service).isStoppedBySelf)
        assertEquals(ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE, service.foregroundServiceType)
        assertEquals("Staying connected", shadowOf(service).lastForegroundNotification.extras.getString(Notification.EXTRA_TITLE))
        assertEquals(listOf("suspend", "resume"), host.core.calls)
        assertEquals("Connecting…", shownText())

        host.links.value = LinkStatus(Link.Online(5u), stale = false)
        ShadowLooper.idleMainLooper()
        assertEquals("Connected", shownText())

        // The app coming to the foreground again starts it again: one hold.
        controller.startCommand(0, 2)
        ShadowLooper.idleMainLooper()
        assertEquals("Connected", shownText())
        assertEquals(listOf("suspend", "resume"), host.core.calls)

        controller.destroy()
        assertEquals(listOf("suspend", "resume", "suspend"), host.core.calls)
    }

    @Test
    fun aStartWithTheChoiceOffGoesForegroundThenStops() {
        // A sticky restart after the user turned it off: foreground first, as
        // startForegroundService requires, then gone without holding the core.
        controller.startCommand(0, 1)
        assertTrue(shadowOf(service).isStoppedBySelf)
        assertEquals(ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE, service.foregroundServiceType)
        controller.destroy()
        assertEquals(listOf("suspend"), host.core.calls)
    }
}
