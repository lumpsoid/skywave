package dev.skywave.android.ui.screens

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.KeyboardArrowRight
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.LocalTextStyle
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
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
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.TextFieldValue
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import dev.skywave.android.R
import dev.skywave.core.Action
import dev.skywave.core.CandidateSource
import dev.skywave.core.Completion
import dev.skywave.core.CompletionStatus

/** Test tags of the new-session dialog. */
internal object NewSessionTags {
    const val WORKDIR = "new_session_workdir"
    const val COMPLETE = "new_session_complete"
    const val CREATE = "new_session_create"
    const val CANCEL = "new_session_cancel"

    fun candidate(path: String) = "new_session_candidate:$path"
}

/**
 * Asks for the server folder of a new session, with the core's workdir
 * completion. "Complete" (the field's button or the keyboard's action key)
 * is the TUI's Tab: it shows the candidates and, once the core has them for
 * what is typed, fills in [Completion.fill]. While candidates show, every
 * change to the field asks the core again, so tapping a candidate fills it
 * and lists what is inside. Whether a folder is acceptable is the server's
 * call (D14). Create sends [Action.CreateSession], which also ends the
 * completion; closing any other way sends [Action.EndCompletion].
 */
@Composable
internal fun NewSessionDialog(completion: Completion?, dispatch: (Action) -> Unit, onClose: () -> Unit) {
    var workdir by rememberSaveable(stateSaver = TextFieldValue.Saver) { mutableStateOf(TextFieldValue("")) }
    // The TUI's popup: candidates are showing and edits complete again.
    var completing by rememberSaveable { mutableStateOf(false) }
    // Complete was asked for; fill in once the core's answer is settled.
    var fillPending by rememberSaveable { mutableStateOf(false) }
    val created = remember { mutableStateOf(false) }

    val currentDispatch by rememberUpdatedState(dispatch)
    DisposableEffect(Unit) {
        onDispose { if (!created.value) currentDispatch(Action.EndCompletion) }
    }

    fun edit(value: TextFieldValue) {
        val changed = value.text != workdir.text
        workdir = value
        if (changed) {
            fillPending = false
            if (completing) dispatch(Action.CompleteWorkdir(value.text))
        }
    }

    fun setText(text: String) = edit(TextFieldValue(text, TextRange(text.length)))

    fun complete() {
        completing = true
        fillPending = true
        dispatch(Action.CompleteWorkdir(workdir.text))
    }

    // Tab's fill, applied when the candidates for what is typed are in.
    LaunchedEffect(completion, fillPending) {
        val c = completion ?: return@LaunchedEffect
        if (!fillPending || c.input != workdir.text || c.status == CompletionStatus.Loading) return@LaunchedEffect
        fillPending = false
        c.fill?.let(::setText)
    }

    val create = {
        created.value = true
        dispatch(Action.CreateSession(workdir.text))
        onClose()
    }

    AlertDialog(
        onDismissRequest = onClose,
        title = { Text(stringResource(R.string.new_session_title)) },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                OutlinedTextField(
                    value = workdir,
                    onValueChange = ::edit,
                    label = { Text(stringResource(R.string.new_session_workdir)) },
                    placeholder = { Text(stringResource(R.string.new_session_workdir_hint)) },
                    singleLine = true,
                    textStyle = LocalTextStyle.current.copy(fontFamily = FontFamily.Monospace),
                    trailingIcon = {
                        IconButton(onClick = ::complete, modifier = Modifier.testTag(NewSessionTags.COMPLETE)) {
                            Icon(
                                Icons.AutoMirrored.Filled.KeyboardArrowRight,
                                stringResource(R.string.new_session_complete),
                            )
                        }
                    },
                    keyboardOptions = KeyboardOptions(
                        keyboardType = KeyboardType.Uri,
                        autoCorrectEnabled = false,
                        imeAction = ImeAction.Next,
                    ),
                    keyboardActions = KeyboardActions(onNext = { complete() }),
                    modifier = Modifier.fillMaxWidth().testTag(NewSessionTags.WORKDIR),
                )
                if (completing && completion != null) Candidates(completion, onPick = { setText("$it/") })
            }
        },
        confirmButton = {
            TextButton(
                onClick = create,
                enabled = workdir.text.isNotBlank(),
                modifier = Modifier.testTag(NewSessionTags.CREATE),
            ) { Text(stringResource(R.string.new_session_create)) }
        },
        dismissButton = {
            TextButton(onClick = onClose, modifier = Modifier.testTag(NewSessionTags.CANCEL)) {
                Text(stringResource(R.string.sessions_cancel))
            }
        },
    )
}

@Composable
private fun Candidates(completion: Completion, onPick: (String) -> Unit) {
    val muted = MaterialTheme.colorScheme.onSurfaceVariant
    when (val status = completion.status) {
        CompletionStatus.Loading -> Row(
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            CircularProgressIndicator(Modifier.size(14.dp), strokeWidth = 2.dp)
            Text(stringResource(R.string.new_session_loading), style = MaterialTheme.typography.bodySmall, color = muted)
        }
        CompletionStatus.Unavailable ->
            Text(stringResource(R.string.new_session_unavailable), style = MaterialTheme.typography.bodySmall, color = muted)
        is CompletionStatus.Ready -> if (status.truncated) {
            Text(stringResource(R.string.new_session_truncated), style = MaterialTheme.typography.bodySmall, color = muted)
        }
    }
    if (completion.candidates.isEmpty()) {
        if (completion.status != CompletionStatus.Loading) {
            Text(stringResource(R.string.new_session_no_candidates), style = MaterialTheme.typography.bodyMedium, color = muted)
        }
        return
    }
    LazyColumn(Modifier.fillMaxWidth().heightIn(max = 280.dp)) {
        items(completion.candidates, key = { it.path }) { candidate ->
            Row(
                Modifier
                    .fillMaxWidth()
                    .clickable { onPick(candidate.path) }
                    .testTag(NewSessionTags.candidate(candidate.path))
                    .padding(vertical = 10.dp),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                Text(
                    candidate.path,
                    fontFamily = FontFamily.Monospace,
                    maxLines = 1,
                    overflow = TextOverflow.StartEllipsis,
                    modifier = Modifier.weight(1f),
                )
                if (candidate.source == CandidateSource.RECENT) {
                    Text(stringResource(R.string.new_session_recent), style = MaterialTheme.typography.labelSmall, color = muted)
                }
            }
        }
    }
}
