import 'dart:async';

import 'package:flutter/foundation.dart';
import 'package:flutter_local_notifications/flutter_local_notifications.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// System notifications, through flutter_local_notifications on every
/// platform. Rust has decided that an alert is due, or no longer is, and
/// under which id (one per session); this only shows or cancels it, and a
/// tap on one opens the session it names.
class SystemNotifications {
  SystemNotifications(this.send, {FlutterLocalNotificationsPlugin? plugin})
    : _plugin = plugin ?? FlutterLocalNotificationsPlugin();

  final SendIntent send;
  final FlutterLocalNotificationsPlugin _plugin;
  bool _ready = false;

  static const _settings = InitializationSettings(
    // Android asks for POST_NOTIFICATIONS itself, in MainActivity.
    // Named by string, so res/raw/keep.xml stops release shrinking dropping it.
    android: AndroidInitializationSettings('@drawable/ic_stat_skywave'),
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

  static const _approvals = NotificationDetails(
    android: AndroidNotificationDetails(
      'approvals',
      'Approvals',
      channelDescription: 'An agent waits for an approval.',
      importance: Importance.high,
      priority: Priority.high,
    ),
  );

  static const _activity = NotificationDetails(
    android: AndroidNotificationDetails(
      'activity',
      'Agent activity',
      channelDescription: 'An agent went idle, or a long shell run finished.',
      importance: Importance.defaultImportance,
      priority: Priority.defaultPriority,
    ),
  );

  /// The channel of [kind]: an approval, or a stop the hub made, is urgent;
  /// an idle agent or a finished shell run is not.
  static NotificationDetails _detailsFor(NotifyKind kind) => switch (kind) {
    NotifyKind.approval || NotifyKind.stopped => _approvals,
    NotifyKind.shell || NotifyKind.idle => _activity,
  };

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
    await openLaunching();
  }

  /// Shows [body] under [title] as notification [id] on the channel of
  /// [kind], replacing the one up under it; a tap opens [sessionId].
  Future<void> show({
    required int id,
    required String title,
    required String body,
    required NotifyKind kind,
    String? sessionId,
  }) async {
    if (!_ready) return;
    await _plugin.show(
      id: id,
      title: title,
      body: body,
      notificationDetails: _detailsFor(kind),
      payload: sessionId,
    );
  }

  /// Takes notification [id] down, if it is up.
  Future<void> dismiss(int id) async {
    if (!_ready) return;
    await _plugin.cancel(id: id);
  }

  /// A tap on a notification launched the app (mobile, macOS, Windows):
  /// open its session as a tap would. Run once from [init], and again when
  /// a new Android Activity attaches to the live engine, whose launch
  /// intent only this sees. Not on every return to the screen: the intent
  /// stays with the Activity, so its session would open each time.
  Future<void> openLaunching() async {
    if (!_ready) return;
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
