import 'dart:async';

import 'package:flutter/foundation.dart';
import 'package:flutter_local_notifications/flutter_local_notifications.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// System notifications, through flutter_local_notifications on every
/// platform. Rust has decided that an alert is due; this only shows it, and
/// a tap on one opens the session it names.
class SystemNotifications {
  SystemNotifications(this.send);

  final SendIntent send;
  final _plugin = FlutterLocalNotificationsPlugin();
  bool _ready = false;

  /// One notification at a time: a newer approval replaces the last.
  static const _id = 0;

  static const _settings = InitializationSettings(
    // Android asks for POST_NOTIFICATIONS itself, in MainActivity.
    android: AndroidInitializationSettings('@mipmap/ic_launcher'),
    // iOS and macOS ask for permission here, on first launch.
    iOS: DarwinInitializationSettings(),
    macOS: DarwinInitializationSettings(),
    linux: LinuxInitializationSettings(defaultActionName: 'Open session'),
    windows: WindowsInitializationSettings(
      appName: 'Skywave',
      appUserModelId: 'dev.skywave.gui',
      // Names this app's toast activation callback; fixed for good.
      guid: '3e909f5f-113a-4797-b77b-8c7d0cdb8dfb',
    ),
  );

  static const _details = NotificationDetails(
    android: AndroidNotificationDetails(
      'approvals',
      'Approvals',
      channelDescription: 'An agent waits for an approval.',
      importance: Importance.high,
      priority: Priority.high,
    ),
  );

  /// Sets the plugin up. A platform without a notification service (no
  /// D-Bus daemon on Linux) leaves notifications off; nothing else breaks.
  Future<void> init() async {
    try {
      // The answer is whether permission was granted (iOS, macOS); a
      // denial can be undone in system settings, so show is still tried.
      await _plugin.initialize(
        settings: _settings,
        onDidReceiveNotificationResponse: (response) => _open(response.payload),
      );
      _ready = true;
    } on Exception catch (error) {
      debugPrint('notifications unavailable: $error');
      return;
    }
    await _openLaunching();
  }

  /// Shows [body] under [title]; a tap opens [sessionId].
  Future<void> show({
    required String title,
    required String body,
    String? sessionId,
  }) async {
    if (!_ready) return;
    await _plugin.show(
      id: _id,
      title: title,
      body: body,
      notificationDetails: _details,
      payload: sessionId,
    );
  }

  /// A tap on a notification launched the app (mobile, macOS, Windows):
  /// open its session as a tap would.
  Future<void> _openLaunching() async {
    final NotificationAppLaunchDetails? launch;
    try {
      launch = await _plugin.getNotificationAppLaunchDetails();
    } on UnimplementedError {
      // Linux cannot be launched by a notification.
      return;
    }
    if (launch != null && launch.didNotificationLaunchApp) {
      _open(launch.notificationResponse?.payload);
    }
  }

  void _open(String? sessionId) {
    if (sessionId == null || sessionId.isEmpty) return;
    send.act(ActionIntentOpenSession(sessionId: sessionId));
  }
}
