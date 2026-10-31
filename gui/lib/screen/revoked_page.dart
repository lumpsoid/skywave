import 'package:flutter/material.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/screen/readable_width.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// The device was revoked: a full-screen notice over whatever screen was
/// showing. Re-pair, or browse the local data, which stays readable.
class RevokedPage extends StatelessWidget {
  const RevokedPage({required this.send, super.key});

  final SendIntent send;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Scaffold(
      body: SafeArea(
        child: Center(
          child: SingleChildScrollView(
            padding: const EdgeInsets.all(24),
            child: ReadableWidth(
              maxWidth: 480,
              child: Column(
                mainAxisSize: MainAxisSize.min,
                crossAxisAlignment: CrossAxisAlignment.stretch,
                children: [
                  Icon(Icons.block, size: 56, color: theme.colorScheme.error),
                  const SizedBox(height: 16),
                  Text(
                    'This device was revoked.',
                    style: theme.textTheme.headlineSmall,
                    textAlign: TextAlign.center,
                  ),
                  const SizedBox(height: 12),
                  Text(
                    'The server no longer accepts this device. '
                    'Local data stays readable.',
                    style: theme.textTheme.bodyLarge,
                    textAlign: TextAlign.center,
                  ),
                  const SizedBox(height: 24),
                  FilledButton.icon(
                    icon: const Icon(Icons.qr_code),
                    onPressed: () => send.act(
                      const ActionIntentNavigate(route: RouteView.pairing),
                    ),
                    label: const Text('Re-pair'),
                  ),
                  const SizedBox(height: 8),
                  OutlinedButton.icon(
                    icon: const Icon(Icons.folder_open),
                    onPressed: () => send(const GuiIntentRevokedDismiss()),
                    label: const Text('Browse local data'),
                  ),
                ],
              ),
            ),
          ),
        ),
      ),
    );
  }
}
