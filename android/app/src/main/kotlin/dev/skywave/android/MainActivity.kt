package dev.skywave.android

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.tooling.preview.Preview
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import dev.skywave.android.core.CoreStartup
import dev.skywave.android.core.label
import dev.skywave.core.ViewState
import dev.skywave.core.coreVersion

/**
 * Single Activity (D18). The core lives in [SkywaveApp], so recreating this
 * Activity (rotation) only re-collects its state. Placeholder until A08.
 */
class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        val version = coreVersion()
        val startup = (application as SkywaveApp).core
        setContent {
            SkywaveTheme {
                when (startup) {
                    is CoreStartup.Ready -> {
                        val view by startup.repository.state.collectAsStateWithLifecycle()
                        Placeholder(version, view.toLines())
                    }
                    is CoreStartup.Failed -> Placeholder(version, listOf("Could not start", startup.reason))
                }
            }
        }
    }
}

private fun ViewState.toLines(): List<String> = listOf(
    "screen ${screen.label()}",
    "link ${link.label()}",
    "queued $queued",
)

@Composable
private fun SkywaveTheme(content: @Composable () -> Unit) {
    MaterialTheme(
        colorScheme = if (isSystemInDarkTheme()) darkColorScheme() else lightColorScheme(),
        content = content,
    )
}

@Composable
private fun Placeholder(version: String, lines: List<String>) {
    Scaffold { padding ->
        Column(
            modifier = Modifier.fillMaxSize().padding(padding),
            verticalArrangement = Arrangement.Center,
            horizontalAlignment = Alignment.CenterHorizontally,
        ) {
            Text("Skywave", style = MaterialTheme.typography.headlineLarge)
            Text("core $version", style = MaterialTheme.typography.bodyMedium)
            lines.forEach { Text(it, style = MaterialTheme.typography.bodyLarge) }
        }
    }
}

@Preview
@Composable
private fun PlaceholderPreview() {
    SkywaveTheme { Placeholder("0.0.0 (protocol 0)", listOf("screen Pairing", "link Connecting", "queued 0")) }
}
