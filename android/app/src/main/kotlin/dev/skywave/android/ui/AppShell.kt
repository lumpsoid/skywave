package dev.skywave.android.ui

import androidx.activity.compose.BackHandler
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.platform.LocalResources
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import dev.skywave.android.core.CoreClient
import dev.skywave.android.ui.screens.AboutInfo
import dev.skywave.android.ui.screens.PairingScreen
import dev.skywave.android.ui.screens.SessionListScreen
import dev.skywave.android.ui.screens.SessionScreen
import dev.skywave.android.ui.screens.SettingsScreen
import dev.skywave.android.ui.screens.StayConnectedSetting
import dev.skywave.core.Action
import dev.skywave.core.Link
import dev.skywave.core.Route
import dev.skywave.core.Screen
import dev.skywave.core.ViewState

/**
 * [AppShell] fed from [client]: its snapshots, its actions, its failures as
 * snackbars. [pairInbox] carries pairing links opened from outside; [about]
 * is what Settings shows of this build and device; [stayConnected] is the
 * device-local choice Settings toggles.
 */
@Composable
fun SkywaveRoot(client: CoreClient, pairInbox: PairInbox, about: AboutInfo, stayConnected: StayConnectedSetting) {
    val state by client.state.collectAsStateWithLifecycle()
    val snackbar = remember { SnackbarHostState() }
    val resources = LocalResources.current
    LaunchedEffect(client, resources) {
        // One at a time: the channel holds the rest until this one is gone.
        client.messages.collect { snackbar.showSnackbar(it.text(resources)) }
    }
    val dispatch = remember(client) { { action: Action -> client.dispatch(action) } }
    AppShell(state, dispatch, pairInbox, about, stayConnected, snackbar)
}

/**
 * Renders whatever [ViewState.screen] names (D18): the core owns the route,
 * so there is no navigation state here. Every screen gets the link and
 * approval banners; a refused link takes the whole screen until the user
 * pairs again or chooses to keep reading.
 */
@Composable
fun AppShell(
    state: ViewState,
    dispatch: (Action) -> Unit,
    pairInbox: PairInbox,
    about: AboutInfo,
    stayConnected: StayConnectedSetting,
    snackbarHostState: SnackbarHostState = remember { SnackbarHostState() },
) {
    val screen = state.screen
    // Where back goes is the core's rule; at its root back leaves the app.
    val goBack = if (state.canGoBack) ({ dispatch(Action.Back) }) else null

    val onPairing = screen is Screen.Pairing
    val pairAgain = { dispatch(Action.Navigate(Route.PAIRING)) }
    val snackbarHost = @Composable { SnackbarHost(snackbarHostState) }

    // Dismissing the full-screen state is presentation only; it comes back
    // when the link is refused anew (a different rejection, or after a
    // reconnect). System back on it means "keep reading" too: navigating
    // behind a screen that hides the result would surprise the user.
    val rejection = (state.link as? Link.Rejected)?.rejection
    var keepReading by rememberSaveable(rejection?.toString()) { mutableStateOf(false) }
    if (rejection != null && !onPairing && !keepReading) {
        BackHandler { keepReading = true }
        Scaffold(snackbarHost = snackbarHost) { padding ->
            RejectedScreen(rejection, onPairAgain = pairAgain, onKeepReading = { keepReading = true }, padding)
        }
        return
    }

    // After the early return, so it is only registered while the screen it
    // navigates away from is the one showing.
    BackHandler(enabled = state.canGoBack) { dispatch(Action.Back) }

    val chrome = ShellChrome(
        banners = {
            LinkBanner(
                link = state.link,
                queued = state.queued,
                onRetryNow = { dispatch(Action.RetryNow) },
                onPairAgain = if (onPairing) null else pairAgain,
            )
            AttentionBanner(state.attention, onOpenNext = { dispatch(Action.OpenNextApproval) })
        },
        snackbarHost = snackbarHost,
        onUp = goBack,
    )
    when (screen) {
        is Screen.Pairing -> PairingScreen(screen.view, chrome, dispatch, pairInbox)
        is Screen.SessionList -> SessionListScreen(screen.view, chrome, dispatch)
        is Screen.Session -> SessionScreen(screen.view, state.attention, chrome, dispatch)
        is Screen.Settings -> SettingsScreen(screen.view, chrome, dispatch, about, stayConnected)
    }
}
