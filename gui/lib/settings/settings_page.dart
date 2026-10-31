import 'package:flutter/material.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/screen/readable_width.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// Settings: the blob policy, and the server and device this one is
/// paired as, with the fingerprint to compare with what `skywaved`
/// prints, and Re-pair.
class SettingsPage extends StatelessWidget {
  const SettingsPage({
    required this.screen,
    required this.send,
    this.canGoBack = false,
    super.key,
  });

  final ScreenViewSettings screen;
  final bool canGoBack;
  final SendIntent send;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final muted = theme.textTheme.bodyMedium?.copyWith(
      color: theme.colorScheme.onSurfaceVariant,
    );
    return Scaffold(
      appBar: AppBar(
        leading: backButton(
          canGoBack: canGoBack,
          onBack: () => send.act(const ActionIntentBack()),
        ),
        title: const Text('Settings'),
      ),
      body: ReadableWidth(
        child: ListView(
          children: [
            const _Section('Blobs'),
            Padding(
              padding: const EdgeInsets.symmetric(horizontal: 16),
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  Text(
                    'Blobs hold full tool inputs and outputs; sessions sync '
                    'without them.',
                    style: muted,
                  ),
                  const SizedBox(height: 12),
                  SegmentedButton<BlobPolicyView>(
                    key: const Key('blob-policy'),
                    segments: const [
                      ButtonSegment(
                        value: BlobPolicyView.onDemand,
                        icon: Icon(Icons.download_for_offline_outlined),
                        label: Text('On demand'),
                      ),
                      ButtonSegment(
                        value: BlobPolicyView.eager,
                        icon: Icon(Icons.downloading),
                        label: Text('Eager'),
                      ),
                    ],
                    selected: {screen.blobPolicy},
                    onSelectionChanged: (selected) => send.act(
                      ActionIntentSetBlobPolicy(
                        policy: switch (selected.first) {
                          BlobPolicyView.onDemand => BlobPolicyChoice.onDemand,
                          BlobPolicyView.eager => BlobPolicyChoice.eager,
                        },
                      ),
                    ),
                  ),
                  const SizedBox(height: 8),
                  Text(
                    screen.blobPolicyLabel,
                    key: const Key('blob-policy-label'),
                  ),
                  const SizedBox(height: 4),
                  Text(
                    'On demand: a blob downloads when you expand its tool '
                    'output. Eager: every blob downloads as soon as it is '
                    'synced, which uses more data and disk.',
                    style: muted,
                  ),
                ],
              ),
            ),
            const _Section('This device'),
            ListTile(
              leading: const Icon(Icons.dns),
              title: const Text('Server'),
              subtitle: SelectableText(screen.server),
            ),
            ListTile(
              leading: const Icon(Icons.devices),
              title: const Text('Device'),
              subtitle: SelectableText(screen.device),
            ),
            if (screen.fingerprint case final fingerprint?)
              ListTile(
                leading: const Icon(Icons.fingerprint),
                title: const Text('Server fingerprint'),
                subtitle: SelectableText(
                  fingerprint,
                  style: const TextStyle(fontFamily: 'monospace'),
                ),
              ),
            Padding(
              padding: const EdgeInsets.all(16),
              child: Align(
                alignment: Alignment.centerLeft,
                child: OutlinedButton.icon(
                  icon: const Icon(Icons.qr_code),
                  onPressed: () => send.act(
                    const ActionIntentNavigate(route: RouteView.pairing),
                  ),
                  label: const Text('Re-pair'),
                ),
              ),
            ),
          ],
        ),
      ),
    );
  }
}

class _Section extends StatelessWidget {
  const _Section(this.text);

  final String text;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Padding(
      padding: const EdgeInsets.fromLTRB(16, 20, 16, 8),
      child: Text(
        text,
        style: theme.textTheme.titleSmall?.copyWith(
          color: theme.colorScheme.primary,
        ),
      ),
    );
  }
}
