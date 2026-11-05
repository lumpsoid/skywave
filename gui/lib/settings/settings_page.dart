import 'package:flutter/material.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/link/link_holder.dart';
import 'package:skywave_gui/screen/readable_width.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// Settings: the blob policy, Stay connected where offered (Android), and
/// the server and device this one is
/// paired as, the link to it with its round trip, the fingerprint to
/// compare with what `skywaved` prints, and Re-pair. A paired server's
/// address can be changed in place, without re-pairing.
class SettingsPage extends StatelessWidget {
  const SettingsPage({
    required this.screen,
    required this.link,
    required this.send,
    this.canGoBack = false,
    super.key,
  });

  final ScreenViewSettings screen;
  final LinkHolder link;
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
                    'output. Eager: every tool blob downloads as soon as it '
                    'is synced, which uses more data and disk. Workdir diffs '
                    'and files download only when opened, either way.',
                    style: muted,
                  ),
                ],
              ),
            ),
            if (screen.stayConnected case final on?) ...[
              const _Section('Background'),
              SwitchListTile(
                key: const Key('stay-connected'),
                secondary: const Icon(Icons.sync),
                title: const Text('Stay connected'),
                subtitle: const Text(
                  'Keeps the link open with the app in the background, for '
                  'approval alerts. Shows an ongoing notification.',
                ),
                value: on,
                onChanged: (value) =>
                    send(GuiIntentSetStayConnected(on: value)),
              ),
            ],
            const _Section('This device'),
            ListTile(
              leading: const Icon(Icons.dns),
              title: const Text('Server'),
              subtitle: SelectableText(screen.server),
              trailing: switch (screen.editableAddress) {
                final address? => IconButton(
                  key: const Key('edit-server-address'),
                  icon: const Icon(Icons.edit),
                  tooltip: 'Change address',
                  onPressed: () async {
                    final changed = await showDialog<String>(
                      context: context,
                      builder: (context) => _AddressDialog(initial: address),
                    );
                    if (changed != null) {
                      send.act(ActionIntentSetServerAddress(address: changed));
                    }
                  },
                ),
                null => null,
              },
            ),
            ConnectionTile(holder: link),
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

/// The link to the server as Rust says it, `Connected · 12 ms` while
/// online. The latency changes on every ping, so only this tile listens.
class ConnectionTile extends StatelessWidget {
  const ConnectionTile({required this.holder, super.key});

  final LinkHolder holder;

  @override
  Widget build(BuildContext context) {
    return ListenableBuilder(
      listenable: holder,
      builder: (context, _) => ListTile(
        leading: Icon(
          holder.link?.state == LinkState.online ? Icons.link : Icons.link_off,
        ),
        title: const Text('Connection'),
        subtitle: Text(
          holder.link?.connectionLabel ?? 'Not known yet',
          key: const Key('connection-label'),
        ),
      ),
    );
  }
}

/// Asks where the paired server is now, e.g. after its IP changed: pops
/// the address, or `null` when cancelled. Only the address changes; the
/// pairing stays.
class _AddressDialog extends StatefulWidget {
  const _AddressDialog({required this.initial});

  final String initial;

  @override
  State<_AddressDialog> createState() => _AddressDialogState();
}

class _AddressDialogState extends State<_AddressDialog> {
  late final _address = TextEditingController(text: widget.initial)
    ..selection = TextSelection(
      baseOffset: 0,
      extentOffset: widget.initial.length,
    );

  @override
  void dispose() {
    _address.dispose();
    super.dispose();
  }

  void _submit() => Navigator.of(context).pop(_address.text);

  @override
  Widget build(BuildContext context) {
    return AlertDialog(
      title: const Text('Server address'),
      content: SizedBox(
        width: 420,
        child: TextField(
          key: const Key('server-address-field'),
          controller: _address,
          autofocus: true,
          keyboardType: TextInputType.url,
          decoration: const InputDecoration(
            labelText: 'host:port',
            helperText: 'The same server at a new address; no re-pairing.',
          ),
          onSubmitted: (_) => _submit(),
        ),
      ),
      actions: [
        TextButton(
          onPressed: () => Navigator.of(context).pop(),
          child: const Text('Cancel'),
        ),
        FilledButton(onPressed: _submit, child: const Text('Save')),
      ],
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
