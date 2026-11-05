import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/effects/background_service.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();
  final messenger =
      TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger;
  const channel = MethodChannel('test/background');

  late List<GuiIntent> sent;
  late BackgroundService service;

  setUp(() {
    sent = [];
    service = BackgroundService(sent.add, channel: channel);
  });

  tearDown(() {
    messenger.setMockMethodCallHandler(channel, null);
    channel.setMethodCallHandler(null);
  });

  /// Delivers [method] from the platform side, as Kotlin's service does.
  Future<void> report(String method) => messenger.handlePlatformMessage(
    channel.name,
    const StandardMethodCodec().encodeMethodCall(MethodCall(method)),
    (_) {},
  );

  test('start and stop ask the platform channel', () async {
    final calls = <String>[];
    messenger.setMockMethodCallHandler(channel, (call) async {
      calls.add(call.method);
      return null;
    });

    await service.start();
    await service.stop();

    expect(calls, ['start', 'stop']);
  });

  test('without the channel (not Android) start and stop do nothing', () async {
    await service.start();
    await service.stop();
    expect(sent, isEmpty);
  });

  test('a report from Kotlin goes to Rust as the service state', () async {
    service.listen();

    await report('running');
    await report('stopped');

    expect(
      sent.map((intent) => (intent as GuiIntentBackgroundService).running),
      [true, false],
    );
  });

  test('reports are dropped until listening', () async {
    await report('running');
    expect(sent, isEmpty);
  });
}
