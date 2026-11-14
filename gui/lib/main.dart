import 'dart:async';

import 'package:material_ui/material_ui.dart';
import 'package:rinf/rinf.dart';
import 'package:skywave_gui/app.dart';
import 'package:skywave_gui/boot/boot_holder.dart';
import 'package:skywave_gui/effects/background_service.dart';
import 'package:skywave_gui/effects/effects.dart';
import 'package:skywave_gui/effects/notifications.dart';
import 'package:skywave_gui/holders.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// Starts Rust, subscribes the holders, then boots the core. The GUI shows
/// the startup page until Rust answers [Booted].
Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  await initializeRust(assignRustSignal);

  final messenger = GlobalKey<ScaffoldMessengerState>();
  final notifications = SystemNotifications(sendToRust);
  // Not awaited: on iOS and macOS it waits for the permission prompt.
  unawaited(notifications.init());
  // Listening before runApp: Kotlin may report the service already.
  final background = BackgroundService(sendToRust)..listen();
  final holders = Holders(
    effects: AppEffectSink(messenger, notifications, background),
  )..connect();
  final boot = BootHolder();
  unawaited(boot.connect());

  runApp(
    SkywaveApp(
      boot: boot,
      holders: holders,
      send: sendToRust,
      messenger: messenger,
      onReattach: () => unawaited(notifications.openLaunching()),
    ),
  );
}
