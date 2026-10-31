import 'package:flutter/material.dart';
import 'package:skywave_gui/holders.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/link/link_banner.dart';
import 'package:skywave_gui/pairing/pairing_page.dart';
import 'package:skywave_gui/screen/revoked_page.dart';
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
        LinkBanner(holder: holders.link, send: send),
        Expanded(
          // The banner takes the top inset while it shows.
          child: ListenableBuilder(
            listenable: holders.link,
            builder: (context, child) => MediaQuery.removePadding(
              context: context,
              removeTop: holders.link.link?.label.isNotEmpty ?? false,
              child: child!,
            ),
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
                        key: ValueKey((kind, update.revokedFullScreen)),
                        child: update.revokedFullScreen
                            ? RevokedPage(send: send)
                            : _page(update),
                      ),
                    ],
                    onDidRemovePage: (_) {},
                  ),
                );
              },
            ),
          ),
        ),
      ],
    );
  }

  Widget _page(ScreenUpdate update) => switch (update.screen) {
    final ScreenViewPairing screen => PairingPage(
      screen: screen,
      canGoBack: update.canGoBack,
      send: send,
    ),
    final ScreenViewSessionList screen => SessionListPage(
      screen: screen,
      canGoBack: update.canGoBack,
      send: send,
    ),
    ScreenViewSession(:final session) => SessionPage(
      screen: session,
      timeline: holders.timeline,
      send: send,
    ),
    final ScreenViewSettings screen => SettingsPage(
      screen: screen,
      canGoBack: update.canGoBack,
      send: send,
    ),
    _ => const SizedBox.shrink(),
  };
}
