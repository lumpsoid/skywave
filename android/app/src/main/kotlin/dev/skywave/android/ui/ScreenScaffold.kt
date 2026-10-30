package dev.skywave.android.ui

import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.RowScope
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.Immutable
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.style.TextOverflow
import dev.skywave.android.R

/**
 * What the shell puts around every screen: the banners under the top bar,
 * the snackbar host, and where up goes. Screens hand it to [ScreenScaffold]
 * and need not know what is inside.
 */
@Immutable
class ShellChrome(
    val banners: @Composable ColumnScope.() -> Unit,
    val snackbarHost: @Composable () -> Unit,
    /** The top bar's up arrow; null hides it. Same target as system back. */
    val onUp: (() -> Unit)?,
)

/**
 * The frame of every screen: a top app bar titled [title] with [actions],
 * the shell's banners under it, then [content]. Edge to edge: the top bar
 * takes the status bar inset and [content] gets the rest as its padding.
 * [titleContent], when given, takes the title's place (e.g. a search field).
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ScreenScaffold(
    title: String,
    chrome: ShellChrome,
    modifier: Modifier = Modifier,
    actions: @Composable RowScope.() -> Unit = {},
    bottomBar: @Composable () -> Unit = {},
    floatingActionButton: @Composable () -> Unit = {},
    titleContent: (@Composable () -> Unit)? = null,
    content: @Composable (PaddingValues) -> Unit,
) {
    Scaffold(
        modifier = modifier,
        topBar = {
            Column {
                TopAppBar(
                    title = titleContent ?: { Text(title, maxLines = 1, overflow = TextOverflow.Ellipsis) },
                    navigationIcon = {
                        chrome.onUp?.let { up ->
                            IconButton(onClick = up) {
                                Icon(Icons.AutoMirrored.Filled.ArrowBack, stringResource(R.string.action_back))
                            }
                        }
                    },
                    actions = actions,
                )
                chrome.banners(this)
            }
        },
        bottomBar = bottomBar,
        floatingActionButton = floatingActionButton,
        snackbarHost = chrome.snackbarHost,
        content = content,
    )
}
