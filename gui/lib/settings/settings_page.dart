import 'package:flutter/material.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// Settings: the blob policy, the server and this device. A placeholder
/// for G5's page.
class SettingsPage extends StatelessWidget {
  const SettingsPage({required this.screen, required this.send, super.key});

  final ScreenViewSettings screen;
  final SendIntent send;

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(title: const Text('Settings')),
      body: ListView(
        children: [
          ListTile(
            title: const Text('Blob policy'),
            subtitle: Text(screen.blobPolicyLabel),
          ),
          ListTile(title: const Text('Server'), subtitle: Text(screen.server)),
          ListTile(title: const Text('Device'), subtitle: Text(screen.device)),
          if (screen.fingerprint case final fingerprint?)
            ListTile(
              title: const Text('Fingerprint'),
              subtitle: Text(fingerprint),
            ),
          ListTile(
            title: const Text('Pair again'),
            onTap: () =>
                send.act(const ActionIntentNavigate(route: RouteView.pairing)),
          ),
        ],
      ),
    );
  }
}
