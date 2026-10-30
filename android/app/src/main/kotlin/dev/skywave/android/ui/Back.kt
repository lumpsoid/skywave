package dev.skywave.android.ui

import dev.skywave.core.ListKind
import dev.skywave.core.Route
import dev.skywave.core.Screen

/**
 * Where system back (and the top bar's up arrow) goes from [screen], or null
 * when back should leave the app. Mirrors the TUI's Esc keys so both heads
 * agree; a candidate for an `Action::Back` in the core (D2).
 */
fun backRoute(screen: Screen): Route? = when (screen) {
    is Screen.Session -> if (screen.view.archived) Route.ARCHIVED_SESSIONS else Route.SESSION_LIST
    is Screen.SessionList -> when (screen.view.kind) {
        ListKind.ARCHIVED -> Route.SESSION_LIST
        ListKind.ACTIVE -> null
    }
    is Screen.Settings -> Route.SESSION_LIST
    is Screen.Pairing -> if (screen.view.paired != null) Route.SESSION_LIST else null
}
