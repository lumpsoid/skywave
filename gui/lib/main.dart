import 'dart:async';

import 'package:flutter/material.dart';
import 'package:rinf/rinf.dart';
import 'package:skywave_gui/app.dart';
import 'package:skywave_gui/boot/boot_holder.dart';
import 'package:skywave_gui/effects/effects.dart';
import 'package:skywave_gui/holders.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// Starts Rust, subscribes the holders, then boots the core. The GUI shows
/// a spinner until Rust answers [Booted].
Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  await initializeRust(assignRustSignal);

  final messenger = GlobalKey<ScaffoldMessengerState>();
  final holders = Holders(effects: AppEffectSink(messenger))..connect();
  final boot = BootHolder();
  unawaited(boot.connect());

  runApp(
    SkywaveApp(
      boot: boot,
      holders: holders,
      send: sendToRust,
      messenger: messenger,
    ),
  );
}
