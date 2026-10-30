package dev.skywave.android.ui

import android.content.res.Resources
import dev.skywave.android.R
import dev.skywave.android.core.CoreMessage

/** The text shown for [this]. */
fun CoreMessage.text(resources: Resources): String = when (this) {
    is CoreMessage.Storage -> resources.getString(R.string.message_storage, reason)
    is CoreMessage.Io -> resources.getString(R.string.message_io, reason)
    is CoreMessage.InvalidPairUri -> resources.getString(R.string.message_invalid_pair_uri, reason)
    CoreMessage.ShutDown -> resources.getString(R.string.message_shut_down)
    is CoreMessage.NativeLibrary -> resources.getString(R.string.message_native_library, reason)
    is CoreMessage.Other -> detail
}
