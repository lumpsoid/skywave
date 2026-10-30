package dev.skywave.android.ui.screens

import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.tooling.preview.Preview
import dev.skywave.android.ui.AppShell
import dev.skywave.android.ui.PairInbox
import dev.skywave.android.ui.PairInput
import dev.skywave.android.ui.Samples
import dev.skywave.android.ui.SkywaveTheme
import dev.skywave.core.Candidate
import dev.skywave.core.CandidateSource
import dev.skywave.core.Completion
import dev.skywave.core.CompletionStatus
import dev.skywave.core.ErrorInfo
import dev.skywave.core.FailReason
import dev.skywave.core.Link
import dev.skywave.core.ListKind
import dev.skywave.core.Outgoing
import dev.skywave.core.PendingSession
import dev.skywave.core.Screen
import dev.skywave.core.SessionListView
import dev.skywave.core.SessionRow
import dev.skywave.core.SessionState
import dev.skywave.core.ViewState

/** Session list snapshots for previews and UI tests; nothing here talks to a core. */
internal object SessionListSamples {
    private const val NOW_MS = 1_790_000_000_000

    val working = Samples.row("s1", "Fix the sync bug").copy(
        state = SessionState.AgentWorking,
        unread = 3u,
        costUsd = 0.42,
        lastActivityMs = NOW_MS,
    )
    val waiting = Samples.row("s2", "Write the Android shell").copy(
        state = SessionState.AwaitingApproval(approvalId = "a1"),
        pendingApprovals = 1u,
        hasDraft = true,
        lastActivityMs = NOW_MS,
    )
    val errored = Samples.row("s3", "skywave").copy(
        named = false,
        workdir = "/home/me/src/skywave",
        state = SessionState.Errored(ErrorInfo(message = "agent exited with status 1")),
        rename = Outgoing.Failed(FailReason.UNKNOWN_SESSION),
        lastActivityMs = NOW_MS,
    )

    /** Archived, nothing outstanding: may be deleted from the device. */
    val oldDeletable = Samples.row("s4", "Old experiment").copy(state = SessionState.Stopped, deletable = true)

    /** Archived but its rename is still unanswered: not deletable yet. */
    val oldBusy = Samples.row("s5", "Renamed offline").copy(rename = Outgoing.Queued, deletable = false)

    val queuedCreation = PendingSession(
        commandId = "c1",
        workdir = "/home/me/src/new-thing",
        outgoing = Outgoing.Queued,
        discardable = true,
    )
    val refusedCreation = PendingSession(
        commandId = "c2",
        workdir = "/etc",
        outgoing = Outgoing.Failed(FailReason.WORKDIR_NOT_ALLOWED),
        discardable = true,
    )
    val acceptedCreation = PendingSession(
        commandId = "c3",
        workdir = "/home/me/src/started",
        outgoing = Outgoing.Accepted(seq = 41u),
        discardable = false,
    )

    fun list(
        kind: ListKind = ListKind.ACTIVE,
        sessions: List<SessionRow> = emptyList(),
        otherCount: ULong = 0u,
        creating: List<PendingSession> = emptyList(),
        completion: Completion? = null,
    ) = Screen.SessionList(SessionListView(kind, sessions, otherCount, creating, completion))

    val active = list(sessions = listOf(working, waiting, errored), otherCount = 2u)
    val emptyActive = list()
    val allArchived = list(otherCount = 2u)
    val emptyArchived = list(kind = ListKind.ARCHIVED, otherCount = 3u)
    val archived = list(kind = ListKind.ARCHIVED, sessions = listOf(oldDeletable, oldBusy), otherCount = 3u)

    /** Offline with two creations waiting and one refused. */
    val offlineWithPending = list(
        sessions = listOf(working),
        creating = listOf(refusedCreation, queuedCreation, acceptedCreation),
    )

    fun completion(
        input: String,
        vararg paths: String,
        status: CompletionStatus = CompletionStatus.Ready(truncated = false),
        fill: String? = null,
    ) = Completion(
        input = input,
        candidates = paths.map { Candidate(it, CandidateSource.SERVER) },
        status = status,
        fill = fill,
    )
}

@Composable
private fun ListPreview(screen: Screen, link: Link = Link.Online(latencyMs = 42u), queued: ULong = 0u) {
    val inbox = remember { PairInbox { PairInput.Unreadable("previews read no links") } }
    val state = ViewState(link = link, queued = queued, attention = Samples.noAttention, screen = screen)
    SkywaveTheme(dynamicColor = false) { AppShell(state, dispatch = {}, pairInbox = inbox, about = SettingsSamples.about) }
}

@Preview(name = "Sessions")
@Composable
private fun ActivePreview() = ListPreview(SessionListSamples.active)

@Preview(name = "Sessions: empty")
@Composable
private fun EmptyPreview() = ListPreview(SessionListSamples.emptyActive)

@Preview(name = "Sessions: all archived")
@Composable
private fun AllArchivedPreview() = ListPreview(SessionListSamples.allArchived)

@Preview(name = "Sessions: offline, creations pending")
@Composable
private fun OfflinePendingPreview() =
    ListPreview(SessionListSamples.offlineWithPending, Samples.reconnecting(), queued = 1u)

@Preview(name = "Archived sessions")
@Composable
private fun ArchivedPreview() = ListPreview(SessionListSamples.archived)

@Preview(name = "Archived sessions: empty")
@Composable
private fun EmptyArchivedPreview() = ListPreview(SessionListSamples.emptyArchived)
