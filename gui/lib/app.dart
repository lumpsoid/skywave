import 'dart:async';
import 'dart:ui' show AppExitResponse;

import 'package:material_ui/material_ui.dart';
import 'package:rinf/rinf.dart';
import 'package:skywave_gui/boot/boot_failure_page.dart';
import 'package:skywave_gui/boot/boot_holder.dart';
import 'package:skywave_gui/boot/startup_page.dart';
import 'package:skywave_gui/holders.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/link/link_banner.dart';
import 'package:skywave_gui/link/link_status.dart';
import 'package:skywave_gui/screen/screen_navigator.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// The app: the startup page until Rust has booted, its failure with a Retry (a
/// Restart after the hub crashed), or the app itself. Holders are created in `main` and passed down from here.
/// Each time Rust answers `Booted`, [AppReady] asks it to send everything
/// again, so a rebuilt widget tree starts whole.
class SkywaveApp extends StatefulWidget {
  const SkywaveApp({
    required this.boot,
    required this.holders,
    required this.send,
    this.messenger,
    this.finalize = finalizeRust,
    this.onReattach,
    super.key,
  });

  final BootHolder boot;
  final Holders holders;
  final SendIntent send;

  /// Where effects show snackbars.
  final GlobalKey<ScaffoldMessengerState>? messenger;

  /// Stops Rust, which closes the core. Tests replace it.
  final VoidCallback finalize;

  /// A new Activity attached to the live engine (Android): whatever its
  /// launch carried, such as a notification tap, is checked here.
  final VoidCallback? onReattach;

  @override
  State<SkywaveApp> createState() => _SkywaveAppState();
}

class _SkywaveAppState extends State<SkywaveApp> {
  late final AppLifecycleListener _lifecycle;
  bool _finalized = false;
  bool _detached = false;

  @override
  void initState() {
    super.initState();
    widget.boot.addListener(_ready);
    _lifecycle = AppLifecycleListener(
      // Desktop: let Rust close the core before the process ends.
      onExitRequested: () async {
        _finalize();
        return AppExitResponse.exit;
      },
      // Android: the Activity went away, not the engine, which the app
      // owns; the core stays open for the next Activity.
      onDetach: () => _detached = true,
      // A new Activity on the live engine: detached goes straight to
      // resumed, past shown, so the return is reported here.
      onResume: _reattached,
      // Leaving the screen is reported, not decided: Rust knows what it
      // means on this platform. Pausing always passes through hidden (also
      // before a detach), and resuming through shown or, after a detach,
      // through [_reattached], so each move is reported once.
      onHide: () => widget.send(const GuiIntentAppBackgrounded()),
      onShow: () => widget.send(const GuiIntentAppForegrounded()),
    );
  }

  /// After a detach only: the app is back on screen, and the new Activity
  /// may have been launched by a notification tap.
  void _reattached() {
    if (!_detached) return;
    _detached = false;
    widget.send(const GuiIntentAppForegrounded());
    widget.onReattach?.call();
  }

  /// Booted: ask Rust for the whole state.
  void _ready() {
    if (widget.boot.status is BootReady) widget.send(const GuiIntentAppReady());
  }

  /// Stops Rust, which closes the core; blocks until it has.
  void _finalize() {
    if (_finalized) return;
    _finalized = true;
    widget.finalize();
  }

  @override
  void dispose() {
    widget.boot.removeListener(_ready);
    _lifecycle.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'Skywave',
      scaffoldMessengerKey: widget.messenger,
      theme: skywaveTheme(Brightness.light),
      darkTheme: skywaveTheme(Brightness.dark),
      // Over the root navigator, so a dialog's app bar finds the link too.
      builder: (context, child) =>
          LinkScope(holder: widget.holders.link, child: child!),
      home: ListenableBuilder(
        listenable: widget.boot,
        builder: (context, _) => switch (widget.boot.status) {
          BootPending() => const StartupPage(),
          BootError(:final message, :final unreadable, :final crashed) =>
            BootFailurePage(
              message: message,
              onRetry: () => unawaited(widget.boot.boot()),
              retryLabel: crashed ? 'Restart' : 'Retry',
              onReset: unreadable
                  ? () => unawaited(widget.boot.boot(resetReplica: true))
                  : null,
            ),
          BootReady() => LinkShortcuts(
            send: widget.send,
            child: ScreenNavigator(holders: widget.holders, send: widget.send),
          ),
        },
      ),
    );
  }
}

/// Material 3 from one seed, light or dark as the system is. Dense on
/// desktop, where a pointer and keys are the input.
ThemeData skywaveTheme(Brightness brightness) => ThemeData(
  colorSchemeSeed: Colors.indigo,
  brightness: brightness,
  visualDensity: VisualDensity.adaptivePlatformDensity,
  inputDecorationTheme: const InputDecorationTheme(
    border: OutlineInputBorder(),
  ),
);
