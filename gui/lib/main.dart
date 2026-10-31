import 'dart:async';

import 'package:flutter/material.dart';
import 'package:rinf/rinf.dart';
import 'package:skywave_gui/app.dart';
import 'package:skywave_gui/boot/boot_holder.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// Starts Rust, subscribes the holders, then boots the core. The GUI shows
/// a spinner until Rust answers [Booted].
Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  await initializeRust(assignRustSignal);

  final boot = BootHolder();
  unawaited(boot.connect());

  runApp(SkywaveApp(boot: boot));
}
