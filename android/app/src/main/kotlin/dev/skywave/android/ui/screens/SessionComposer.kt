package dev.skywave.android.ui.screens

import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.widthIn
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.List
import androidx.compose.material.icons.automirrored.filled.Send
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.MutableState
import androidx.compose.runtime.Stable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.TextRange
import androidx.compose.ui.text.input.TextFieldValue
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import androidx.lifecycle.compose.LocalLifecycleOwner
import dev.skywave.android.R
import dev.skywave.core.Action
import dev.skywave.core.Draft
import dev.skywave.core.SessionView
import kotlinx.coroutines.delay

/** A pause in typing this long saves the draft. */
internal const val DRAFT_PAUSE_MS = 1_500L

/**
 * The oldest an unsaved edit gets while typing goes on without a pause:
 * at most this much typing is lost to a crash. Saves are durable writes,
 * so never one per keystroke.
 */
internal const val DRAFT_MAX_UNSAVED_MS = 5_000L

/**
 * The composer's text, kept here while it is written and saved to the core
 * as the session's draft on a pause, on leaving the screen and when the app
 * stops: never per keystroke. The core's draft wins only when it rewrites
 * it itself ([SessionView.draftRev], e.g. after EditQueued).
 */
@Stable
internal class ComposerState(
    private val sessionId: String,
    private val valueState: MutableState<TextFieldValue>,
    private val dirtyState: MutableState<Boolean>,
    private val dispatch: () -> (Action) -> Unit,
) {
    val value: TextFieldValue get() = valueState.value

    /** Edits not yet saved as the draft. */
    val dirty: Boolean get() = dirtyState.value

    /** Bumped on every edit, to restart the pause timer. */
    var edits by mutableIntStateOf(0)
        private set

    fun onValueChange(new: TextFieldValue) {
        val old = valueState.value
        valueState.value = new
        if (new.text != old.text || new.selection != old.selection) {
            dirtyState.value = true
            edits++
        }
    }

    /** Fills the composer as if typed (a recalled prompt); saved like typing. */
    fun fill(text: String) = onValueChange(TextFieldValue(text, TextRange(text.length)))

    /** Takes the core's rewritten draft; nothing to save. */
    fun replace(draft: Draft?) {
        valueState.value = draft.toFieldValue()
        dirtyState.value = false
    }

    /** Saves unsaved edits as the draft; blank text drops it. */
    fun save() {
        if (!dirtyState.value) return
        dirtyState.value = false
        val v = valueState.value
        dispatch()(Action.SaveDraft(sessionId, v.text, v.selection.end.toULong()))
    }

    fun send() {
        val text = valueState.value.text
        if (text.isBlank()) return
        dispatch()(Action.SendMessage(sessionId, text))
        // The core drops the stored draft with the send.
        valueState.value = TextFieldValue()
        dirtyState.value = false
    }

    /**
     * Takes a queued message back into the composer. The core appends the
     * stored draft to it, so unsaved typing is saved first and survives.
     */
    fun editQueued(commandId: String) {
        save()
        dispatch()(Action.EditQueued(commandId))
    }
}

internal fun Draft?.toFieldValue(): TextFieldValue {
    if (this == null) return TextFieldValue()
    val cursor = cursorUtf16.coerceAtMost(text.length.toULong()).toInt()
    return TextFieldValue(text, TextRange(cursor))
}

/**
 * The composer of [view]'s session, filled from its draft, refilled when
 * the core rewrites it, saving on pause, on dispose and on ON_STOP.
 */
@Composable
internal fun rememberComposer(view: SessionView, dispatch: (Action) -> Unit): ComposerState {
    val valueState = rememberSaveable(view.id, stateSaver = TextFieldValue.Saver) {
        mutableStateOf(view.draft.toFieldValue())
    }
    val dirtyState = rememberSaveable(view.id) { mutableStateOf(false) }
    var seenRev by rememberSaveable(view.id) { mutableStateOf(view.draftRev.toString()) }
    val currentDispatch by rememberUpdatedState(dispatch)
    val composer = remember(view.id) {
        ComposerState(view.id, valueState, dirtyState) { currentDispatch }
    }

    LaunchedEffect(view.draftRev) {
        val rev = view.draftRev.toString()
        if (rev != seenRev) {
            seenRev = rev
            composer.replace(view.draft)
        }
    }
    // Save on a pause: every edit restarts this.
    LaunchedEffect(composer, composer.edits) {
        if (composer.dirty) {
            delay(DRAFT_PAUSE_MS)
            composer.save()
        }
    }
    // ...and while typing goes on, once the first unsaved edit is old enough.
    LaunchedEffect(composer, composer.dirty) {
        if (composer.dirty) {
            delay(DRAFT_MAX_UNSAVED_MS)
            composer.save()
        }
    }
    val lifecycle = LocalLifecycleOwner.current.lifecycle
    DisposableEffect(composer, lifecycle) {
        val observer = LifecycleEventObserver { _, event ->
            if (event == Lifecycle.Event.ON_STOP) composer.save()
        }
        lifecycle.addObserver(observer)
        onDispose {
            lifecycle.removeObserver(observer)
            composer.save()
        }
    }
    return composer
}

internal object ComposerTags {
    const val FIELD = "composer_field"
    const val SEND = "composer_send"
    const val RECALL = "composer_recall"
}

/** The composer bar: recall, a multiline field and send, above the keyboard. */
@Composable
internal fun ComposerBar(composer: ComposerState, prompts: List<String>) {
    Surface(tonalElevation = 3.dp) {
        Row(
            Modifier
                .fillMaxWidth()
                .navigationBarsPadding()
                .imePadding()
                .padding(horizontal = 4.dp, vertical = 6.dp),
            verticalAlignment = Alignment.Bottom,
        ) {
            if (prompts.isNotEmpty()) RecallButton(prompts, composer::fill)
            OutlinedTextField(
                value = composer.value,
                onValueChange = composer::onValueChange,
                placeholder = { Text(stringResource(R.string.sv_composer_hint)) },
                maxLines = 6,
                modifier = Modifier
                    .weight(1f)
                    .padding(start = if (prompts.isEmpty()) 8.dp else 0.dp)
                    .testTag(ComposerTags.FIELD),
            )
            IconButton(
                onClick = composer::send,
                enabled = composer.value.text.isNotBlank(),
                modifier = Modifier.testTag(ComposerTags.SEND),
            ) {
                Icon(Icons.AutoMirrored.Filled.Send, stringResource(R.string.sv_send))
            }
        }
    }
}

/** Lists the session's earlier messages, newest first; choosing one fills the composer. */
@Composable
private fun RecallButton(prompts: List<String>, onPick: (String) -> Unit) {
    var open by remember { mutableStateOf(false) }
    Box {
        IconButton(onClick = { open = true }, modifier = Modifier.testTag(ComposerTags.RECALL)) {
            Icon(Icons.AutoMirrored.Filled.List, stringResource(R.string.sv_recall))
        }
        DropdownMenu(expanded = open, onDismissRequest = { open = false }) {
            prompts.forEach { prompt ->
                DropdownMenuItem(
                    text = {
                        Text(
                            prompt,
                            maxLines = 2,
                            overflow = TextOverflow.Ellipsis,
                            modifier = Modifier.widthIn(max = 280.dp),
                        )
                    },
                    onClick = {
                        open = false
                        onPick(prompt)
                    },
                )
            }
        }
    }
}
