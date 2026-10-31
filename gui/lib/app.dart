import 'dart:async';
import 'dart:ui' show AppExitResponse;

import 'package:flutter/material.dart';
import 'package:rinf/rinf.dart';
import 'package:skywave_gui/boot/boot_failure_page.dart';
import 'package:skywave_gui/boot/boot_holder.dart';
import 'package:skywave_gui/holders.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/screen/screen_navigator.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// The app: a spinner until Rust has booted, its failure with a Retry, or
/// the app itself. Holders are created in `main` and passed down from here.
/// Each time Rust answers `Booted`, [AppReady] asks it to send everything
/// again, so a rebuilt widget tree starts whole.
class SkywaveApp extends StatefulWidget {
  const SkywaveApp({
    required this.boot,
    required this.holders,
    required this.send,
    this.messenger,
    super.key,
  });

  final BootHolder boot;
  final Holders holders;
  final SendIntent send;

  /// Where effects show snackbars.
  final GlobalKey<ScaffoldMessengerState>? messenger;

  @override
  State<SkywaveApp> createState() => _SkywaveAppState();
}

class _SkywaveAppState extends State<SkywaveApp> {
  late final AppLifecycleListener _lifecycle;
  bool _finalized = false;

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
      // Mobile: the engine is going away without an exit request.
      onDetach: _finalize,
    );
  }

  /// Booted: ask Rust for the whole state.
  void _ready() {
    if (widget.boot.status is BootReady) widget.send(const GuiIntentAppReady());
  }

  /// Stops Rust, which closes the core; blocks until it has.
  void _finalize() {
    if (_finalized) return;
    _finalized = true;
    finalizeRust();
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
      theme: ThemeData(colorSchemeSeed: Colors.indigo),
      darkTheme: ThemeData(
        colorSchemeSeed: Colors.indigo,
        brightness: Brightness.dark,
      ),
      home: ListenableBuilder(
        listenable: widget.boot,
        builder: (context, _) => switch (widget.boot.status) {
          BootPending() => const Scaffold(
            body: Center(child: CircularProgressIndicator()),
          ),
          BootError(:final message) => BootFailurePage(
            message: message,
            onRetry: () => unawaited(widget.boot.boot()),
          ),
          BootReady() => ScreenNavigator(
            holders: widget.holders,
            send: widget.send,
          ),
        },
      ),
    );
  }
}
