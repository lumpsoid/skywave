import 'package:flutter/material.dart';
import 'package:skywave_gui/holders.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/link/link_banner.dart';
import 'package:skywave_gui/pairing/pairing_page.dart';
import 'package:skywave_gui/pairing/qr_scan.dart';
import 'package:skywave_gui/screen/repair_page.dart';
import 'package:skywave_gui/session/session_page.dart';
import 'package:skywave_gui/sessions/session_list_page.dart';
import 'package:skywave_gui/settings/settings_page.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';
import 'package:skywave_gui/storage/storage_page.dart';
import 'package:skywave_gui/usage/usage_page.dart';
import 'package:skywave_gui/workdir/workdir_page.dart';

/// The booted app: the banner over a navigator whose one page is the
/// screen Rust sent (D18), but in a subagent's view, which is a second
/// page over its session's. System back is `Back` while Rust says it goes
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
                final repair = update.repair;
                final covered = repair != null;
                final subagent = covered ? null : _subagentCall(update);
                return PopScope(
                  canPop: !update.canGoBack,
                  onPopInvokedWithResult: (didPop, _) {
                    if (!didPop) send.act(const ActionIntentBack());
                  },
                  child: Navigator(
                    pages: [
                      MaterialPage<void>(
                        key: ValueKey((kind, null, covered)),
                        child: repair != null
                            ? RepairPage(notice: repair, send: send)
                            : _page(update, top: subagent == null),
                      ),
                      // A subagent's view goes over its session's, which
                      // stays: its composer keeps what was typed in it,
                      // and back pops to it.
                      if (subagent != null)
                        MaterialPage<void>(
                          key: ValueKey((kind, subagent, covered)),
                          child: _page(update, top: true),
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

  /// The `Agent` call of the subagent whose view is shown, if one is.
  static String? _subagentCall(ScreenUpdate update) => switch (update.screen) {
    ScreenViewSession(:final session) => session.subagent?.call,
    _ => null,
  };

  /// The page for [update]. One under the [top] page is kept for its
  /// state and sends nothing.
  Widget _page(ScreenUpdate update, {required bool top}) =>
      _pageFor(update, top ? send : _quiet);

  static void _quiet(GuiIntent _) {}

  Widget _pageFor(ScreenUpdate update, SendIntent send) =>
      switch (update.screen) {
        final ScreenViewPairing screen => PairingPage(
          screen: screen,
          canGoBack: update.canGoBack,
          send: send,
          scan: platformScanQr,
        ),
        final ScreenViewSessionList screen => SessionListPage(
          screen: screen,
          canGoBack: update.canGoBack,
          send: send,
        ),
        ScreenViewSession(:final session) => SessionPage(
          screen: session,
          timeline: holders.timeline,
          shell: holders.shell,
          search: holders.search,
          send: send,
        ),
        final ScreenViewSettings screen => SettingsPage(
          screen: screen,
          link: holders.link,
          canGoBack: update.canGoBack,
          send: send,
        ),
        final ScreenViewUsage screen => UsagePage(
          screen: screen,
          canGoBack: update.canGoBack,
          send: send,
        ),
        final ScreenViewStorage screen => StoragePage(
          screen: screen,
          canGoBack: update.canGoBack,
          send: send,
        ),
        ScreenViewWorkdir(:final workdir) => WorkdirPage(
          screen: workdir,
          canGoBack: update.canGoBack,
          send: send,
        ),
        _ => const SizedBox.shrink(),
      };
}
