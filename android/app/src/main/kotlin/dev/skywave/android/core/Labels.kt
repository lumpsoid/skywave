package dev.skywave.android.core

import dev.skywave.core.Link
import dev.skywave.core.Screen

// Debug-grade texts for the placeholder UI; A08 replaces them with resources.

internal fun Link.label(): String = when (this) {
    Link.Connecting -> "Connecting"
    is Link.Online -> "Online $latencyMs ms"
    is Link.CatchingUp -> "Catching up $have/$server"
    is Link.Reconnecting -> "Reconnecting #$attempt"
    is Link.Rejected -> "Rejected (${rejection::class.simpleName})"
}

internal fun Screen.label(): String = when (this) {
    is Screen.Pairing -> "Pairing"
    is Screen.SessionList -> "Session list"
    is Screen.Session -> "Session"
    is Screen.Settings -> "Settings"
}
