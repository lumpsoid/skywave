package dev.skywave.android.ui

import dev.skywave.core.BlobPolicy
import dev.skywave.core.Entry
import dev.skywave.core.ListKind
import dev.skywave.core.PairedInfo
import dev.skywave.core.PairingStatus
import dev.skywave.core.PairingView
import dev.skywave.core.Route
import dev.skywave.core.Screen
import dev.skywave.core.SessionListView
import dev.skywave.core.SessionState
import dev.skywave.core.SessionView
import dev.skywave.core.SettingsView
import dev.skywave.core.TimelineItem
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

internal fun sessionView(archived: Boolean, items: Int = 0) = SessionView(
    id = "s1",
    title = "Fix the sync bug",
    named = true,
    rename = null,
    archived = archived,
    workdir = null,
    state = SessionState.Idle,
    timeline = List(items) {
        TimelineItem.Logged(seq = it.toULong(), atMs = 0, entry = Entry.Interrupted)
    },
    pendingApproval = null,
    rules = emptyList(),
    usage = null,
    interrupt = null,
    stop = null,
    draft = null,
    draftRev = 0u,
    lastSeq = 0u,
    unreadFrom = null,
    prompts = emptyList(),
)

class BackRouteTest {
    private fun list(kind: ListKind) =
        Screen.SessionList(SessionListView(kind, emptyList(), 0u, emptyList(), null))

    @Test
    fun sessionGoesBackToTheListItCameFrom() {
        assertEquals(Route.SESSION_LIST, backRoute(Screen.Session(sessionView(archived = false))))
        assertEquals(Route.ARCHIVED_SESSIONS, backRoute(Screen.Session(sessionView(archived = true))))
    }

    @Test
    fun archivedListAndSettingsGoToTheActiveList() {
        assertEquals(Route.SESSION_LIST, backRoute(list(ListKind.ARCHIVED)))
        assertEquals(Route.SESSION_LIST, backRoute(Screen.Settings(SettingsView(BlobPolicy.EAGER, null, null))))
    }

    @Test
    fun activeListLeavesTheApp() {
        assertNull(backRoute(list(ListKind.ACTIVE)))
    }

    @Test
    fun pairingGoesBackOnlyWhenAlreadyPaired() {
        assertNull(backRoute(Screen.Pairing(PairingView(null, PairingStatus.Idle))))
        val paired = PairingView(PairedInfo(deviceId = "d", serverAddress = "h:1"), PairingStatus.Idle)
        assertEquals(Route.SESSION_LIST, backRoute(Screen.Pairing(paired)))
    }
}
