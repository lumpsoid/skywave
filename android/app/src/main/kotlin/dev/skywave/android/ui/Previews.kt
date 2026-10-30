package dev.skywave.android.ui

import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.tooling.preview.Preview
import dev.skywave.android.ui.screens.SettingsSamples
import dev.skywave.core.Attention
import dev.skywave.core.Link
import dev.skywave.core.LinkRejection
import dev.skywave.core.ListKind
import dev.skywave.core.PairedInfo
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
            PendingApprovalRef(id = "a1", sessionId = "s2", title = "Run cargo test", alertedAt = 0u),
            PendingApprovalRef(id = "a2", sessionId = "s3", title = "Edit README.md", alertedAt = 0u),
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

    val pairingInProgress =
        Screen.Pairing(PairingView(paired = null, status = PairingStatus.InProgress(address = "10.0.2.2:7443")))

    val pairingFailed = Screen.Pairing(
        PairingView(
            paired = null,
            status = PairingStatus.Failed(address = "10.0.2.2:7443", reason = "the server's fingerprint does not match"),
        ),
    )

    /** Paired already, e.g. after the device was revoked. */
    val repair = Screen.Pairing(
        PairingView(
            paired = PairedInfo(deviceId = "dev-7f3a", serverAddress = "skywave.home.arpa:7443"),
            status = PairingStatus.Idle,
        ),
    )

    /** Third attempt, due 4 s after [nowMs], offline for the last 10 minutes. */
    fun reconnecting(nowMs: Long = System.currentTimeMillis()) =
        Link.Reconnecting(attempt = 3u, retryAtMs = nowMs + 4_000, offlineSinceMs = nowMs - 600_000)

    fun state(
        link: Link,
        screen: Screen = sessionList,
        queued: ULong = 0u,
        attention: Attention = noAttention,
        // The core's answer; true for screens back leaves (all but the active
        // list and first pairing).
        canGoBack: Boolean = false,
    ) = ViewState(link = link, queued = queued, attention = attention, screen = screen, canGoBack = canGoBack)
}

@Composable
private fun ShellPreview(state: ViewState) {
    val inbox = remember { PairInbox { PairInput.Unreadable("previews read no links") } }
    SkywaveTheme(dynamicColor = false) { AppShell(state, dispatch = {}, pairInbox = inbox, about = SettingsSamples.about, stayConnected = SettingsSamples.stayConnected) }
}

@Preview(name = "Connecting")
@Composable
private fun ConnectingPreview() = ShellPreview(Samples.state(Link.Connecting))

@Preview(name = "Pairing")
@Composable
private fun PairingIdlePreview() = ShellPreview(Samples.state(Link.Unpaired, Samples.unpaired))

@Preview(name = "Pairing in progress")
@Composable
private fun PairingInProgressPreview() = ShellPreview(Samples.state(Link.Unpaired, Samples.pairingInProgress))

@Preview(name = "Pairing failed")
@Composable
private fun PairingFailedPreview() = ShellPreview(Samples.state(Link.Unpaired, Samples.pairingFailed))

@Preview(name = "Pair again")
@Composable
private fun RepairPreview() = ShellPreview(Samples.state(Link.Online(latencyMs = 42u), Samples.repair, canGoBack = true))

@Preview(name = "Online")
@Composable
private fun OnlinePreview() = ShellPreview(Samples.state(Link.Online(latencyMs = 42u)))

@Preview(name = "Catching up")
@Composable
private fun CatchingUpPreview() = ShellPreview(Samples.state(Link.CatchingUp(have = 120u, server = 480u)))

@Preview(name = "Reconnecting, 3 queued")
@Composable
private fun ReconnectingPreview() = ShellPreview(Samples.state(Samples.reconnecting(), queued = 3u))

@Preview(name = "Rejected: revoked")
@Composable
private fun RevokedPreview() = ShellPreview(Samples.state(Link.Rejected(LinkRejection.Revoked)))

@Preview(name = "Rejected: revoked, on pairing")
@Composable
private fun RevokedOnPairingPreview() =
    ShellPreview(Samples.state(Link.Rejected(LinkRejection.Revoked), Samples.repair, canGoBack = true))

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
    ShellPreview(Samples.state(Samples.reconnecting(), queued = 1u, attention = Samples.twoElsewhere))
