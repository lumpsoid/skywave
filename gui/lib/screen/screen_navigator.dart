import 'package:flutter/material.dart';
import 'package:skywave_gui/holders.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/link/link_banner.dart';
import 'package:skywave_gui/pairing/pairing_page.dart';
import 'package:skywave_gui/session/session_page.dart';
import 'package:skywave_gui/sessions/session_list_page.dart';
import 'package:skywave_gui/settings/settings_page.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// The booted app: the banner over a navigator whose one page is the
/// screen Rust sent (D18). System back is `Back` while Rust says it goes
/// somewhere; otherwise it leaves the app.
class ScreenNavigator extends StatelessWidget {
  const ScreenNavigator({required this.holders, required this.send, super.key});

  final Holders holders;
  final SendIntent send;

  @override
  Widget build(BuildContext context) {
    return Column(
      children: [
        SafeArea(
          bottom: false,
          child: LinkBanner(holder: holders.link, send: send),
        ),
        Expanded(
          child: ListenableBuilder(
            listenable: holders.screen,
            builder: (context, _) {
              final update = holders.screen.screen;
              final kind = holders.screen.kind;
              if (update == null || kind == null) {
                return const Scaffold(
                  body: Center(child: CircularProgressIndicator()),
                );
              }
              return PopScope(
                canPop: !update.canGoBack,
                onPopInvokedWithResult: (didPop, _) {
                  if (!didPop) send.act(const ActionIntentBack());
                },
                child: Navigator(
                  pages: [
                    MaterialPage<void>(
                      key: ValueKey(kind),
                      child: update.revokedFullScreen
                          ? RevokedPage(send: send)
                          : _page(update.screen),
                    ),
                  ],
                  onDidRemovePage: (_) {},
                ),
              );
            },
          ),
        ),
      ],
    );
  }

  Widget _page(ScreenView screen) => switch (screen) {
    ScreenViewPairing() => PairingPage(screen: screen, send: send),
    ScreenViewSessionList() => SessionListPage(screen: screen, send: send),
    ScreenViewSession(:final session) => SessionPage(
      screen: session,
      timeline: holders.timeline,
      send: send,
    ),
    ScreenViewSettings() => SettingsPage(screen: screen, send: send),
    _ => const SizedBox.shrink(),
  };
}

/// The device was revoked: re-pair, or browse what is here.
class RevokedPage extends StatelessWidget {
  const RevokedPage({required this.send, super.key});

  final SendIntent send;

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      body: Center(
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            const Text('This device was revoked.'),
            const SizedBox(height: 16),
            FilledButton(
              onPressed: () => send.act(
                const ActionIntentNavigate(route: RouteView.pairing),
              ),
              child: const Text('Pair again'),
            ),
            TextButton(
              onPressed: () => send(const GuiIntentRevokedDismiss()),
              child: const Text('Browse local data'),
            ),
          ],
        ),
      ),
    );
  }
}
