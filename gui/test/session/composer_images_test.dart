import 'package:desktop_drop/desktop_drop.dart';
import 'package:flutter/foundation.dart';
import 'package:material_ui/material_ui.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/session/image_picking.dart';
import 'package:skywave_gui/session/session_page.dart';
import 'package:skywave_gui/session/timeline_holder.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

import 'rows.dart';

final _originalPicker = pickImages;
final _originalClipboard = readClipboardImage;

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

/// Ctrl+V, as a key down and up with Ctrl held around it.
Future<void> _pressCtrlV(WidgetTester tester) async {
  await tester.sendKeyDownEvent(LogicalKeyboardKey.controlLeft);
  await tester.sendKeyEvent(LogicalKeyboardKey.keyV);
  await tester.sendKeyUpEvent(LogicalKeyboardKey.controlLeft);
}

/// Runs [body] as a Linux desktop. The override is reset inside the test:
/// the binding checks foundation variables before any tearDown runs.
Future<void> _asLinux(Future<void> Function() body) async {
  debugDefaultTargetPlatformOverride = TargetPlatform.linux;
  try {
    await body();
  } finally {
    debugDefaultTargetPlatformOverride = null;
  }
}

void main() {
  tearDown(() {
    pickImages = _originalPicker;
    readClipboardImage = _originalClipboard;
    TestWidgetsFlutterBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(SystemChannels.platform, null);
  });

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
    await tester.tap(find.byKey(const Key('composer-menu')));
    await tester.pumpAndSettle();
    await tester.tap(find.byKey(const Key('attach-image')));
    await tester.pumpAndSettle();
    expect(sent.whereType<GuiIntentAttachImageFile>(), [
      const GuiIntentAttachImageFile(sessionId: 's1', path: '/a.png'),
      const GuiIntentAttachImageFile(sessionId: 's1', path: '/b.jpg'),
    ]);

    // Control: a cancelled pick attaches nothing.
    sent.clear();
    pickImages = (_) async => [];
    await tester.tap(find.byKey(const Key('composer-menu')));
    await tester.pumpAndSettle();
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
    await tester.tap(find.byKey(const Key('composer-menu')));
    await tester.pumpAndSettle();
    expect(find.byKey(const Key('attach-image')), findsNothing);

    // Control: the agent composer has it. Close the menu first.
    await tester.tapAt(Offset.zero);
    await tester.pumpAndSettle();
    await pumpSession(
      tester,
      screen: screenWith(composer: _draft(rev: 2)),
      timeline: _holding(),
      sent: sent,
    );
    await tester.tap(find.byKey(const Key('composer-menu')));
    await tester.pumpAndSettle();
    expect(find.byKey(const Key('attach-image')), findsOneWidget);
  });

  testWidgets(
    'Ctrl+V with an image in the clipboard attaches it',
    (tester) => _asLinux(() async {
      readClipboardImage = () async => Uint8List.fromList([1, 2, 3]);
      final sent = <GuiIntent>[];
      await pumpSession(
        tester,
        screen: screenWith(composer: _draft(text: 'look')),
        timeline: _holding(),
        sent: sent,
      );
      await tester.tap(_composer());
      await tester.pump();
      await _pressCtrlV(tester);
      await tester.pumpAndSettle();
      expect(sent.whereType<GuiIntentAttachImageBytes>(), [
        GuiIntentAttachImageBytes(
          sessionId: 's1',
          bytes: Uint8List.fromList([1, 2, 3]),
          mime: 'image/png',
        ),
      ]);
      expect(tester.widget<TextField>(_composer()).controller!.text, 'look');
    }),
  );

  testWidgets(
    'Ctrl+V with text only pastes the text',
    (tester) => _asLinux(() async {
      readClipboardImage = () async => null;
      TestWidgetsFlutterBinding.instance.defaultBinaryMessenger
          .setMockMethodCallHandler(SystemChannels.platform, (call) async {
            if (call.method == 'Clipboard.getData') {
              return <String, Object?>{'text': 'hello'};
            }
            return null;
          });
      final sent = <GuiIntent>[];
      await pumpSession(
        tester,
        screen: screenWith(),
        timeline: _holding(),
        sent: sent,
      );
      await tester.tap(_composer());
      await tester.pump();
      await _pressCtrlV(tester);
      await tester.pumpAndSettle();
      expect(tester.widget<TextField>(_composer()).controller!.text, 'hello');
      expect(sent.whereType<GuiIntentAttachImageBytes>(), isEmpty);
    }),
  );

  testWidgets(
    'a dropped file is attached, in drop order',
    (tester) => _asLinux(() async {
      final sent = <GuiIntent>[];
      await pumpSession(
        tester,
        screen: screenWith(),
        timeline: _holding(),
        sent: sent,
      );
      tester.widget<DropTarget>(find.byType(DropTarget)).onDragDone!(
        DropDoneDetails(
          files: [DropItemFile('/a.png'), DropItemFile('/b.txt')],
          localPosition: Offset.zero,
          globalPosition: Offset.zero,
        ),
      );
      await tester.pump();
      expect(sent.whereType<GuiIntentAttachImageFile>(), [
        const GuiIntentAttachImageFile(sessionId: 's1', path: '/a.png'),
        const GuiIntentAttachImageFile(sessionId: 's1', path: '/b.txt'),
      ]);

      // Control: in shell mode the drop target is off.
      sent.clear();
      await pumpSession(
        tester,
        screen: screenWith(composer: _draft(mode: ComposerModeView.shell)),
        timeline: _holding(),
        sent: sent,
      );
      expect(
        tester.widget<DropTarget>(find.byType(DropTarget)).enable,
        isFalse,
      );
    }),
  );

  testWidgets('keyboard image insertion attaches it', (tester) async {
    final sent = <GuiIntent>[];
    await pumpSession(
      tester,
      screen: screenWith(),
      timeline: _holding(),
      sent: sent,
    );
    final insertion = tester
        .widget<TextField>(_composer())
        .contentInsertionConfiguration!;
    expect(insertion.allowedMimeTypes, contains('image/png'));
    insertion.onContentInserted(
      KeyboardInsertedContent(
        mimeType: 'image/png',
        uri: 'content://x',
        data: Uint8List.fromList([9]),
      ),
    );
    await tester.pump();
    expect(sent.whereType<GuiIntentAttachImageBytes>(), [
      GuiIntentAttachImageBytes(
        sessionId: 's1',
        bytes: Uint8List.fromList([9]),
        mime: 'image/png',
      ),
    ]);
  });
}
