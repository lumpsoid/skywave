package dev.skywave.android.notifications

import android.Manifest
import android.app.Activity
import android.app.Application
import android.app.NotificationManager
import android.content.Context
import android.content.Intent
import androidx.core.app.NotificationCompat
import org.robolectric.RuntimeEnvironment
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.Shadows.shadowOf

/** The real [Notifications] against Robolectric's notification manager. */
@RunWith(RobolectricTestRunner::class)
class AndroidNotificationsTest {
    private val app: Application = RuntimeEnvironment.getApplication()
    private val manager = app.getSystemService(NotificationManager::class.java)
    private val shade = AndroidNotifications(app, Activity::class.java)
    private val alert = ApprovalAlert("ap-1", "s-1", "Fix the bug")

    private fun grant() = shadowOf(app).grantPermissions(Manifest.permission.POST_NOTIFICATIONS)

    @Test
    fun theChannelIsHighImportance() {
        shade.createChannel()
        val channel = manager.getNotificationChannel("approvals")
        assertEquals(NotificationManager.IMPORTANCE_HIGH, channel.importance)
        assertEquals("Approvals", channel.name)
    }

    @Test
    fun postsOnePerApprovalTaggedWithItsId() {
        grant()
        shade.createChannel()
        shade.post(alert)
        shade.post(ApprovalAlert("ap-2", "s-2", ""))
        assertEquals(setOf("ap-1", "ap-2"), shade.active())

        val posted = manager.activeNotifications.single { it.tag == "ap-1" }.notification
        assertEquals("Approval needed", posted.extras.getString(NotificationCompat.EXTRA_TITLE))
        assertEquals("Fix the bug", posted.extras.getCharSequence(NotificationCompat.EXTRA_TEXT).toString())
        val untitled = manager.activeNotifications.single { it.tag == "ap-2" }.notification
        assertEquals("A session", untitled.extras.getCharSequence(NotificationCompat.EXTRA_TEXT).toString())

        val tap = shadowOf(posted.contentIntent).savedIntent
        assertEquals("s-1", ApprovalTap.sessionId(tap))
        assertEquals(Activity::class.java.name, tap.component?.className)
    }

    @Test
    fun cancelTakesOnlyThatApprovalDown() {
        grant()
        shade.post(alert)
        shade.post(ApprovalAlert("ap-2", "s-2", "Other"))
        shade.cancel("ap-1")
        assertEquals(setOf("ap-2"), shade.active())
    }

    @Test
    fun nothingIsPostedWithoutThePermission() {
        shadowOf(app).denyPermissions(Manifest.permission.POST_NOTIFICATIONS)
        shade.post(alert)
        assertEquals(emptySet<String>(), shade.active())
    }
}

/** Reading a tapped notification's intent back. */
@RunWith(RobolectricTestRunner::class)
class ApprovalTapTest {
    private val context: Context = RuntimeEnvironment.getApplication()

    @Test
    fun theTapIntentNamesTheSession() {
        val intent = ApprovalTap.intent(context, Activity::class.java, ApprovalAlert("ap-1", "s-1", "t"))
        assertEquals("s-1", ApprovalTap.sessionId(intent))
        assertTrue(intent.flags and Intent.FLAG_ACTIVITY_NEW_TASK != 0)
    }

    @Test
    fun eachApprovalGetsADistinctIntent() {
        val a = ApprovalTap.intent(context, Activity::class.java, ApprovalAlert("ap-1", "s-1", "t"))
        val b = ApprovalTap.intent(context, Activity::class.java, ApprovalAlert("ap-2", "s-1", "t"))
        assertFalse(a.filterEquals(b))
    }

    @Test
    fun otherIntentsOpenNothing() {
        assertNull(ApprovalTap.sessionId(null))
        assertNull(ApprovalTap.sessionId(Intent(Intent.ACTION_MAIN).putExtra(ApprovalTap.EXTRA_SESSION_ID, "s-1")))
        assertNull(ApprovalTap.sessionId(Intent(ApprovalTap.ACTION)))
    }
}

/** POST_NOTIFICATIONS is asked at most once (Robolectric runs API 35). */
@RunWith(RobolectricTestRunner::class)
class NotificationPermissionTest {
    private val app: Application = RuntimeEnvironment.getApplication()
    private val permission = NotificationPermission.from(app)

    @Test
    fun askedOnceThenNeverAgain() {
        shadowOf(app).denyPermissions(Manifest.permission.POST_NOTIFICATIONS)
        assertTrue(permission.shouldAsk(app))
        permission.markAsked()
        assertFalse(permission.shouldAsk(app))
        assertFalse(NotificationPermission.from(app).shouldAsk(app))
    }

    @Test
    fun notAskedWhenAlreadyGranted() {
        shadowOf(app).grantPermissions(Manifest.permission.POST_NOTIFICATIONS)
        assertFalse(permission.shouldAsk(app))
        assertTrue(NotificationPermission.granted(app))
    }
}
