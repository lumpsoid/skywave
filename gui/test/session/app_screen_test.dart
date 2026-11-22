import 'package:material_ui/material_ui.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/session/app_screen_holder.dart';
import 'package:skywave_gui/session/app_screen_view.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// A phone's capture: 1080x2400 pixels, not yet downloaded (its file does
/// not exist, so the image shows its error stand-in).
const phoneCapture = AppScreenCapture(
  captureId: 'c1',
  path: '/nonexistent/skywave-screen.png',
  width: 1080,
  height: 2400,
);

AppScreenUpdate screenUpdate({
  AppScreenCapture? capture,
  List<AppScreenRow> requests = const [],
  List<AppScreenDevice> devices = const [],
  String? device,
}) => AppScreenUpdate(
  sessionId: 's1',
  open: true,
  devices: devices,
  device: device,
  capture: capture,
  requests: requests,
);

AppScreenRow screenRow(
  String key,
  String label,
  String stateLabel, {
  bool waiting = false,
  bool stale = false,
  bool refused = false,
}) => AppScreenRow(
  key: key,
  label: label,
  stateLabel: stateLabel,
  waiting: waiting,
  stale: stale,
  refused: refused,
);

/// Pumps the view over [holder] in a 540x1200 box, sending into [sent].
Future<void> pumpScreen(
  WidgetTester tester,
  AppScreenHolder holder,
  List<GuiIntent> sent,
) => tester.pumpWidget(
  MaterialApp(
    home: Scaffold(
      body: SizedBox(
        width: 540,
        height: 1200,
        child: AppScreenView(sessionId: 's1', holder: holder, send: sent.add),
      ),
    ),
  ),
);

IconButton iconButton(WidgetTester tester, String key) =>
    tester.widget<IconButton>(find.byKey(Key(key)));

void main() {
  test('the holder keeps the last update', () {
    final holder = AppScreenHolder();
    var notified = 0;
    holder.addListener(() => notified++);
    expect(holder.update, isNull);
    holder
      ..apply(screenUpdate(device: 'a'))
      ..apply(screenUpdate(device: 'b'));
    expect(holder.update?.device, 'b');
    expect(notified, 2);
  });

  testWidgets('a tap on the image goes out in capture pixels', (tester) async {
    final holder = AppScreenHolder()
      ..apply(screenUpdate(capture: phoneCapture));
    final sent = <GuiIntent>[];
    await pumpScreen(tester, holder, sent);

    await tester.tap(find.byKey(const Key('app-screen-image')));
    await tester.pump();

    expect(sent, hasLength(1));
    final tap = sent.single as GuiIntentAppScreenTap;
    expect(tap.captureId, 'c1');
    expect((tap.x - 1080 ~/ 2).abs(), lessThanOrEqualTo(1));
    expect((tap.y - 2400 ~/ 2).abs(), lessThanOrEqualTo(1));
  });

  testWidgets('a drag goes out as a swipe', (tester) async {
    final holder = AppScreenHolder()
      ..apply(screenUpdate(capture: phoneCapture));
    final sent = <GuiIntent>[];
    await pumpScreen(tester, holder, sent);

    await tester.drag(
      find.byKey(const Key('app-screen-image')),
      const Offset(0, -200),
    );
    await tester.pump();

    expect(sent, hasLength(1));
    final swipe = sent.single as GuiIntentAppScreenSwipe;
    expect(swipe.captureId, 'c1');
    expect(swipe.fromY, greaterThan(swipe.toY));
  });

  testWidgets('keys are off without a capture', (tester) async {
    final holder = AppScreenHolder()..apply(screenUpdate());
    final sent = <GuiIntent>[];
    await pumpScreen(tester, holder, sent);

    expect(iconButton(tester, 'app-screen-back').onPressed, isNull);
    expect(iconButton(tester, 'app-screen-home').onPressed, isNull);
    expect(iconButton(tester, 'app-screen-recents').onPressed, isNull);
    expect(sent, isEmpty);
  });

  testWidgets('stale requests show in the error colour', (tester) async {
    final holder = AppScreenHolder()
      ..apply(
        screenUpdate(
          capture: phoneCapture,
          requests: [
            screenRow('r1', 'tap 10,20', 'failed', stale: true),
            screenRow('r2', 'capture', 'done'),
          ],
        ),
      );
    final sent = <GuiIntent>[];
    await pumpScreen(tester, holder, sent);

    final error = Theme.of(tester.element(find.byType(AppScreenView)))
        .colorScheme
        .error;
    expect(
      tester.widget<Text>(find.text('tap 10,20 — failed')).style?.color,
      error,
    );
    expect(
      tester.widget<Text>(find.text('capture — done')).style?.color,
      isNot(error),
    );
  });
}
