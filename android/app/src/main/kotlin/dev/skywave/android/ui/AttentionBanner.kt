package dev.skywave.android.ui

import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.pluralStringResource
import androidx.compose.ui.res.stringResource
import dev.skywave.android.R
import dev.skywave.core.Attention

/**
 * "N approvals elsewhere" while agents in other sessions wait on an answer;
 * a tap opens the next one. Nothing when [Attention.elsewhere] is zero.
 */
@Composable
fun AttentionBanner(attention: Attention, onOpenNext: () -> Unit, modifier: Modifier = Modifier) {
    if (attention.elsewhere == 0uL) return
    BannerSurface(
        container = MaterialTheme.colorScheme.tertiaryContainer,
        content = MaterialTheme.colorScheme.onTertiaryContainer,
        modifier = modifier,
        onClick = onOpenNext,
    ) {
        Text(
            pluralStringResource(R.plurals.attention_elsewhere, attention.elsewhere.toCount(), attention.elsewhere.toLong()),
            style = MaterialTheme.typography.bodyMedium,
            modifier = Modifier.weight(1f),
        )
        TextButton(onClick = onOpenNext) { Text(stringResource(R.string.attention_open_next)) }
    }
}
