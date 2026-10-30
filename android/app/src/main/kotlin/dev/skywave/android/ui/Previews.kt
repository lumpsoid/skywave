package dev.skywave.android.ui

import androidx.compose.runtime.Composable
import androidx.compose.ui.tooling.preview.Preview
import dev.skywave.core.Attention
import dev.skywave.core.Link
import dev.skywave.core.LinkRejection
import dev.skywave.core.ListKind
import dev.skywave.core.PairingStatus
import dev.skywave.core.PairingView
import dev.skywave.core.PendingApprovalRef
import dev.skywave.core.Screen
import dev.skywave.core.SessionListView
import dev.skywave.core.SessionRow
import dev.skywave.core.SessionState
import dev.skywave.core.ViewState

/** Hand-made snapshots for previews and UI tests; nothing here talks to a core. */
internal object Samples {
    val noAttention = Attention(approvals = emptyList(), elsewhere = 0u, alerts = 0u)

    val twoElsewhere = Attention(
        approvals = listOf(
            PendingApprovalRef(id = "a1", sessionId = "s2", title = "Run cargo test"),
            PendingApprovalRef(id = "a2", sessionId = "s3", title = "Edit README.md"),
        ),
        elsewhere = 2u,
        alerts = 2u,
    )

    fun row(id: String, title: String) = SessionRow(
        id = id,
        title = title,
        named = true,
        rename = null,
        workdir = "/home/me/src",
        state = SessionState.Idle,
        pendingApprovals = 0u,
        lastActivityMs = 0,
        hasDraft = false,
        unread = 0u,
        costUsd = null,
        deletable = false,
    )

    val sessionList = Screen.SessionList(
        SessionListView(
            kind = ListKind.ACTIVE,
            sessions = listOf(row("s1", "Fix the sync bug"), row("s2", "Write the Android shell")),
            otherCount = 0u,
            creating = emptyList(),
            completion = null,
        ),
    )

    val unpaired = Screen.Pairing(PairingView(paired = null, status = PairingStatus.Idle))

    val reconnecting = Link.Reconnecting(attempt = 3u, nextInMs = 4_000u, offlineSinceMs = 1_700_000_000_000)

    fun state(
        link: Link,
        screen: Screen = sessionList,
        queued: ULong = 0u,
        attention: Attention = noAttention,
    ) = ViewState(link = link, queued = queued, attention = attention, screen = screen)
}

@Composable
private fun ShellPreview(state: ViewState) {
    SkywaveTheme(dynamicColor = false) { AppShell(state, dispatch = {}) }
}

@Preview(name = "Connecting")
@Composable
private fun ConnectingPreview() = ShellPreview(Samples.state(Link.Connecting, Samples.unpaired))

@Preview(name = "Online")
@Composable
private fun OnlinePreview() = ShellPreview(Samples.state(Link.Online(latencyMs = 42u)))

@Preview(name = "Catching up")
@Composable
private fun CatchingUpPreview() = ShellPreview(Samples.state(Link.CatchingUp(have = 120u, server = 480u)))

@Preview(name = "Reconnecting, 3 queued")
@Composable
private fun ReconnectingPreview() = ShellPreview(Samples.state(Samples.reconnecting, queued = 3u))

@Preview(name = "Rejected: revoked")
@Composable
private fun RevokedPreview() = ShellPreview(Samples.state(Link.Rejected(LinkRejection.Revoked)))

@Preview(name = "Rejected: revoked, on pairing")
@Composable
private fun RevokedOnPairingPreview() =
    ShellPreview(Samples.state(Link.Rejected(LinkRejection.Revoked), Samples.unpaired))

@Preview(name = "Rejected: protocol mismatch")
@Composable
private fun MismatchPreview() =
    ShellPreview(Samples.state(Link.Rejected(LinkRejection.ProtocolMismatch(client = 7u, server = 8u))))

@Preview(name = "Approvals elsewhere")
@Composable
private fun AttentionPreview() =
    ShellPreview(Samples.state(Link.Online(latencyMs = 42u), attention = Samples.twoElsewhere))

@Preview(name = "Reconnecting + approvals, dark", uiMode = android.content.res.Configuration.UI_MODE_NIGHT_YES)
@Composable
private fun DarkPreview() =
    ShellPreview(Samples.state(Samples.reconnecting, queued = 1u, attention = Samples.twoElsewhere))
