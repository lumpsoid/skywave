import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/session/image_picking.dart';
import 'package:skywave_gui/session/session_page.dart';
import 'package:skywave_gui/session/timeline_holder.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

import 'rows.dart';

final _originalPicker = pickImages;

TimelineHolder _holding() =>
    TimelineHolder()..apply(TimelineUpdateSnapshot(sessionId: 's1', items: []));

/// The session page pumped once, as `pumpSession` does but without
/// settling: a spinner never settles, so `pumpAndSettle` would time out.
Future<void> _pumpOnce(
  WidgetTester tester,
  SessionScreen screen,
  TimelineHolder timeline,
  List<GuiIntent> sent,
) async {
  await tester.pumpWidget(
    MaterialApp(
      home: SessionPage(screen: screen, timeline: timeline, send: sent.add),
    ),
  );
  await tester.pump();
}

ComposerImage _image(String sha, String name) =>
    ComposerImage(sha256: sha, path: '/no/such/$name', sizeLabel: '1 KB');

ComposerView _draft({
  String text = '',
  int rev = 1,
  List<ComposerImage> images = const [],
  int preparing = 0,
  ComposerModeView mode = ComposerModeView.agent,
}) => ComposerView(
  text: text,
  cursorUtf16: text.length,
  rev: rev,
  mode: mode,
  shellEnabled: false,
  canStop: false,
  images: images,
  preparing: preparing,
);

Finder _composer() => find.byType(TextField).last;

IconButton _sendButton(WidgetTester tester) => tester.widget<IconButton>(
  find.ancestor(
    of: find.byIcon(Icons.arrow_upward),
    matching: find.byType(IconButton),
  ),
);

void main() {
  tearDown(() => pickImages = _originalPicker);

  testWidgets('chips show the draft\'s images and ✕ detaches', (tester) async {
    final sent = <GuiIntent>[];
    await pumpSession(
      tester,
      screen: screenWith(
        composer: _draft(
          images: [_image('aaa', 'a.png'), _image('bbb', 'b.jpg')],
        ),
      ),
      timeline: _holding(),
      sent: sent,
    );
    expect(find.byTooltip('Remove image'), findsNWidgets(2));
    expect(find.byKey(const Key('detach-aaa')), findsOneWidget);

    await tester.tap(find.byKey(const Key('detach-bbb')));
    await tester.pump();
    expect(sent.whereType<GuiIntentDetachImage>(), [
      const GuiIntentDetachImage(sessionId: 's1', sha256: 'bbb'),
    ]);
  });

  testWidgets('the attach button sends a file intent per picked image', (
    tester,
  ) async {
    final sent = <GuiIntent>[];
    await pumpSession(
      tester,
      screen: screenWith(),
      timeline: _holding(),
      sent: sent,
    );
    pickImages = (_) async => ['/a.png', '/b.jpg'];
    await tester.tap(find.byKey(const Key('attach-image')));
    await tester.pumpAndSettle();
    expect(sent.whereType<GuiIntentAttachImageFile>(), [
      const GuiIntentAttachImageFile(sessionId: 's1', path: '/a.png'),
      const GuiIntentAttachImageFile(sessionId: 's1', path: '/b.jpg'),
    ]);

    // Control: a cancelled pick attaches nothing.
    sent.clear();
    pickImages = (_) async => [];
    await tester.tap(find.byKey(const Key('attach-image')));
    await tester.pumpAndSettle();
    expect(sent.whereType<GuiIntentAttachImageFile>(), isEmpty);
  });

  testWidgets(
    'an image-only draft can be sent, but not while one is preparing',
    (tester) async {
      debugDefaultTargetPlatformOverride = TargetPlatform.linux;
      try {
        final sent = <GuiIntent>[];
        final image = _image('aaa', 'a.png');
        final timeline = _holding();
        await pumpSession(
          tester,
          screen: screenWith(composer: _draft(rev: 1, images: [image])),
          timeline: timeline,
          sent: sent,
        );
        expect(_sendButton(tester).onPressed, isNotNull);
        await tester.tap(find.byTooltip('Send'));
        expect(sent.whereType<GuiIntentComposerSubmit>(), [
          const GuiIntentComposerSubmit(sessionId: 's1'),
        ]);

        // Enter sends it too.
        sent.clear();
        await tester.tap(_composer());
        await tester.sendKeyEvent(LogicalKeyboardKey.enter);
        expect(sent.whereType<GuiIntentComposerSubmit>(), hasLength(1));

        // One still being prepared: off, the spinner shown, and nothing sent.
        // The spinner never settles, so this pumps once rather than settling.
        sent.clear();
        await _pumpOnce(
          tester,
          screenWith(composer: _draft(rev: 2, images: [image], preparing: 1)),
          timeline,
          sent,
        );
        expect(_sendButton(tester).onPressed, isNull);
        expect(find.byKey(const ValueKey('image-preparing-0')), findsOneWidget);
        await tester.tap(find.byTooltip('Send'));
        await tester.sendKeyEvent(LogicalKeyboardKey.enter);
        expect(sent.whereType<GuiIntentComposerSubmit>(), isEmpty);
      } finally {
        debugDefaultTargetPlatformOverride = null;
      }
    },
  );

  testWidgets('shell mode shows no attach button', (tester) async {
    final sent = <GuiIntent>[];
    await pumpSession(
      tester,
      screen: screenWith(composer: _draft(mode: ComposerModeView.shell)),
      timeline: _holding(),
      sent: sent,
    );
    expect(find.byKey(const Key('attach-image')), findsNothing);

    // Control: the agent composer has it.
    await pumpSession(
      tester,
      screen: screenWith(composer: _draft(rev: 2)),
      timeline: _holding(),
      sent: sent,
    );
    expect(find.byKey(const Key('attach-image')), findsOneWidget);
  });
}
