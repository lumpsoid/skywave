import 'package:flutter_local_notifications/flutter_local_notifications.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/effects/notifications.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// The plugin, recording what is shown and cancelled.
class FakePlugin extends Fake implements FlutterLocalNotificationsPlugin {
  final done = <String>[];

  @override
  Future<bool?> initialize({
    required InitializationSettings settings,
    DidReceiveNotificationResponseCallback? onDidReceiveNotificationResponse,
    DidReceiveBackgroundNotificationResponseCallback?
    onDidReceiveBackgroundNotificationResponse,
  }) async => true;

  @override
  Future<NotificationAppLaunchDetails?>
  getNotificationAppLaunchDetails() async => null;

  @override
  Future<void> show({
    required int id,
    String? title,
    String? body,
    NotificationDetails? notificationDetails,
    String? payload,
  }) async => done.add('show $id: $body ($payload)');

  @override
  Future<void> cancel({required int id, String? tag}) async =>
      done.add('cancel $id');
}

void main() {
  test('show and dismiss go to the plugin under the id Rust gave', () async {
    final plugin = FakePlugin();
    final notifications = SystemNotifications((GuiIntent _) {}, plugin: plugin);
    await notifications.init();
    await notifications.show(
      id: 4,
      title: 'a',
      body: 'approval needed · a',
      sessionId: 'sa',
    );
    await notifications.show(
      id: 8,
      title: 'b',
      body: 'approval needed · b',
      sessionId: 'sb',
    );
    await notifications.dismiss(4);

    expect(plugin.done, [
      'show 4: approval needed · a (sa)',
      'show 8: approval needed · b (sb)',
      'cancel 4',
    ]);
  });

  test('nothing reaches the plugin before it is set up', () async {
    final plugin = FakePlugin();
    final notifications = SystemNotifications((GuiIntent _) {}, plugin: plugin);
    await notifications.show(id: 4, title: 'a', body: 'b');
    await notifications.dismiss(4);
    expect(plugin.done, isEmpty);
  });
}
