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
  }) async => done.add(
    'show $id [${notificationDetails?.android?.channelId}]: $body ($payload)',
  );

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
      kind: NotifyKind.approval,
      sessionId: 'sa',
    );
    await notifications.show(
      id: 8,
      title: 'b',
      body: 'approval needed · b',
      kind: NotifyKind.approval,
      sessionId: 'sb',
    );
    await notifications.dismiss(4);

    expect(plugin.done, [
      'show 4 [approvals]: approval needed · a (sa)',
      'show 8 [approvals]: approval needed · b (sb)',
      'cancel 4',
    ]);
  });

  test('each kind of notice goes to its channel', () async {
    final plugin = FakePlugin();
    final notifications = SystemNotifications((GuiIntent _) {}, plugin: plugin);
    await notifications.init();
    for (final kind in NotifyKind.values) {
      await notifications.show(id: 1, title: 'a', body: kind.name, kind: kind);
    }

    expect(plugin.done, [
      'show 1 [approvals]: approval (null)',
      'show 1 [activity]: shell (null)',
      'show 1 [activity]: idle (null)',
      'show 1 [approvals]: stopped (null)',
    ]);
  });

  test('nothing reaches the plugin before it is set up', () async {
    final plugin = FakePlugin();
    final notifications = SystemNotifications((GuiIntent _) {}, plugin: plugin);
    await notifications.show(
      id: 4,
      title: 'a',
      body: 'b',
      kind: NotifyKind.approval,
    );
    await notifications.dismiss(4);
    expect(plugin.done, isEmpty);
  });
}
