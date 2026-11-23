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
  int typed = 0,
}) => AppScreenUpdate(
  sessionId: 's1',
  open: true,
  devices: devices,
  device: device,
  capture: capture,
  requests: requests,
  typed: typed,
);

AppScreenRow screenRow(
  String key,
  String label,
  String stateLabel, {
  bool waiting = false,
  bool stale = false,
  bool refused = false,
  bool discardable = false,
}) => AppScreenRow(
  key: key,
  label: label,
  stateLabel: stateLabel,
  waiting: waiting,
  stale: stale,
  refused: refused,
  discardable: discardable,
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

/// What the app screen's text field holds.
String _fieldText(WidgetTester tester) => tester
    .widget<TextField>(find.byKey(const Key('app-screen-text')))
    .controller!
    .text;

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

    final image = find.byKey(const Key('app-screen-image'));
    final down = tester.getCenter(image);
    await tester.drag(image, const Offset(0, -200));
    await tester.pump();

    expect(sent, hasLength(1));
    final swipe = sent.single as GuiIntentAppScreenSwipe;
    expect(swipe.captureId, 'c1');
    expect(swipe.fromY, greaterThan(swipe.toY));
    // It starts where the finger went down, within 2 capture pixels.
    final box = tester.getRect(image);
    final downY = ((down.dy - box.top) / box.height * 2400).floor();
    expect((swipe.fromY - downY).abs(), lessThanOrEqualTo(2));
  });

  testWidgets('a refused request can be dismissed', (tester) async {
    final holder = AppScreenHolder()
      ..apply(
        screenUpdate(
          capture: phoneCapture,
          requests: [
            screenRow(
              'r1',
              'tap 10,20',
              'failed',
              refused: true,
              discardable: true,
            ),
            // Refused but not discardable: no dismiss button.
            screenRow('r2', 'tap 5,5', 'failed', refused: true),
          ],
        ),
      );
    final sent = <GuiIntent>[];
    await pumpScreen(tester, holder, sent);

    expect(find.byKey(const Key('app-screen-dismiss-r2')), findsNothing);
    await tester.tap(find.byKey(const Key('app-screen-dismiss-r1')));
    await tester.pump();

    expect(sent, hasLength(1));
    final dismiss = sent.single as GuiIntentAppScreenDismiss;
    expect(dismiss.sessionId, 's1');
    expect(dismiss.requestId, 'r1');
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

  testWidgets('typed text stays until the hub says it was queued', (
    tester,
  ) async {
    final holder = AppScreenHolder()
      ..apply(screenUpdate(capture: phoneCapture));
    final sent = <GuiIntent>[];
    await pumpScreen(tester, holder, sent);

    await tester.enterText(find.byKey(const Key('app-screen-text')), 'hello');
    await tester.tap(find.byKey(const Key('app-screen-send')));
    await tester.pump();

    final text = sent.single as GuiIntentAppScreenText;
    expect(text.text, 'hello');
    expect(text.captureId, 'c1');
    expect(_fieldText(tester), 'hello');

    // The hub queued it: the count moved, and the field clears.
    holder.apply(screenUpdate(capture: phoneCapture, typed: 1));
    await tester.pump();
    expect(_fieldText(tester), '');

    // Text edited after the send is the user's: a count that moves while the
    // field holds other text keeps it.
    await tester.enterText(find.byKey(const Key('app-screen-text')), 'a');
    await tester.tap(find.byKey(const Key('app-screen-send')));
    await tester.pump();
    await tester.enterText(find.byKey(const Key('app-screen-text')), 'ab');
    holder.apply(screenUpdate(capture: phoneCapture, typed: 2));
    await tester.pump();
    expect(_fieldText(tester), 'ab');
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
