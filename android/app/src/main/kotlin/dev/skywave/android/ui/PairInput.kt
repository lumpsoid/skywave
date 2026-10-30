package dev.skywave.android.ui

import android.content.Intent
import android.os.Bundle
import androidx.compose.runtime.Stable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue

/**
 * Pairing details read from a `skywave://pair` link or QR code, for the
 * pairing form to show. Never paired from directly: the user checks them
 * first (the fingerprint above all) and taps Pair.
 */
sealed interface PairInput {
    /** The link's fields, as the core read them. */
    data class Details(val address: String, val code: String, val fingerprint: String) : PairInput

    /** Not a pairing link; [reason] is the core's. */
    data class Unreadable(val reason: String) : PairInput
}

/**
 * Reads a pairing link. In the app this is the core's `parsePairUri`
 * (`core/PairUri.kt`); tests use a fake, as the native core does not load
 * on the JVM.
 */
fun interface PairUriParser {
    fun parse(uri: String): PairInput
}

/**
 * The pairing link the Activity was opened with, if [intent] is one
 * (`VIEW skywave://pair…`), read with [parser]. Null for anything else.
 */
fun pairLinkInput(intent: Intent?, parser: PairUriParser): PairInput? {
    if (intent?.action != Intent.ACTION_VIEW) return null
    val uri = intent.data ?: return null
    if (!uri.scheme.equals(PAIR_SCHEME, ignoreCase = true) || !uri.host.equals(PAIR_HOST, ignoreCase = true)) return null
    return parser.parse(uri.toString())
}

private const val PAIR_SCHEME = "skywave"
private const val PAIR_HOST = "pair"

/**
 * Holds at most one [PairInput] until the pairing form takes it: a one-shot
 * event from a link or a scan. Lives with the Activity and is written to its
 * saved state, so an input not yet shown survives rotation; once taken, the
 * form's own saved fields carry it.
 */
@Stable
class PairInbox(val parser: PairUriParser) {
    var pending: PairInput? by mutableStateOf(null)
        private set

    /** Replaces whatever was waiting: the latest link or scan wins. */
    fun offer(input: PairInput) {
        pending = input
    }

    /** Reads [uri] with [parser] and offers the result. */
    fun offerUri(uri: String) = offer(parser.parse(uri))

    /** Clears [input] if it is still the pending one. */
    fun consume(input: PairInput) {
        if (pending == input) pending = null
    }

    fun saveTo(out: Bundle) {
        when (val p = pending) {
            null -> Unit
            is PairInput.Details -> out.putStringArray(KEY_DETAILS, arrayOf(p.address, p.code, p.fingerprint))
            is PairInput.Unreadable -> out.putString(KEY_UNREADABLE, p.reason)
        }
    }

    fun restoreFrom(saved: Bundle) {
        val details = saved.getStringArray(KEY_DETAILS)
        val unreadable = saved.getString(KEY_UNREADABLE)
        pending = when {
            details != null && details.size == 3 -> PairInput.Details(details[0], details[1], details[2])
            unreadable != null -> PairInput.Unreadable(unreadable)
            else -> null
        }
    }

    private companion object {
        const val KEY_DETAILS = "skywave.pair_input.details"
        const val KEY_UNREADABLE = "skywave.pair_input.unreadable"
    }
}
