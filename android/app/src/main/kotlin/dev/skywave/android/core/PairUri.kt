package dev.skywave.android.core

import dev.skywave.android.ui.PairInput
import dev.skywave.android.ui.PairUriParser
import dev.skywave.core.SkywaveException
import dev.skywave.core.parsePairUri

/** The core reads pairing links (spec §6, D2): the app never parses one itself. */
val corePairUriParser = PairUriParser { uri ->
    try {
        val p = parsePairUri(uri)
        PairInput.Details(p.address, p.code, p.fingerprint)
    } catch (e: SkywaveException.InvalidPairUri) {
        PairInput.Unreadable(e.reason)
    } catch (e: SkywaveException) {
        PairInput.Unreadable(e.message ?: e::class.simpleName ?: e.toString())
    }
}
