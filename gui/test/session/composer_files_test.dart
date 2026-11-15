import 'package:material_ui/material_ui.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/session/image_picking.dart';
import 'package:skywave_gui/session/timeline_holder.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

import 'rows.dart';

final _originalPicker = pickAttachments;

TimelineHolder _holding() =>
    TimelineHolder()..apply(TimelineUpdateSnapshot(sessionId: 's1', items: []));

ComposerFile _file(String name, String size, {bool replaces = false}) =>
    ComposerFile(name: name, sizeLabel: size, replaces: replaces);

ComposerView _draft({
  String text = '',
  int rev = 1,
  List<ComposerFile> files = const [],
  ComposerModeView mode = ComposerModeView.agent,
}) => ComposerView(
  text: text,
  cursorUtf16: text.length,
  rev: rev,
  mode: mode,
  shellEnabled: false,
  canStop: false,
  images: const [],
  files: files,
  preparing: 0,
);

IconButton _sendButton(WidgetTester tester) => tester.widget<IconButton>(
  find.ancestor(
    of: find.byIcon(Icons.arrow_upward),
    matching: find.byType(IconButton),
  ),
);

void main() {
  tearDown(() {
    pickAttachments = _originalPicker;
  });

  testWidgets('file chips show the draft\'s files and ✕ detaches', (
    tester,
  ) async {
    final sent = <GuiIntent>[];
    await pumpSession(
      tester,
      screen: screenWith(
        composer: _draft(
          files: [_file('a.pdf', '2 KB'), _file('b.txt', '1 KB')],
        ),
      ),
      timeline: _holding(),
      sent: sent,
    );
    expect(find.byKey(const Key('file-chip-a.pdf')), findsOneWidget);
    expect(find.text('a.pdf · 2 KB'), findsOneWidget);
    expect(find.text('b.txt · 1 KB'), findsOneWidget);

    await tester.tap(
      find.descendant(
        of: find.byKey(const Key('file-chip-a.pdf')),
        matching: find.byIcon(Icons.close),
      ),
    );
    await tester.pump();
    expect(sent.whereType<GuiIntentDetachFile>(), [
      const GuiIntentDetachFile(sessionId: 's1', name: 'a.pdf'),
    ]);
  });

  testWidgets('a file that replaces says so', (tester) async {
    await pumpSession(
      tester,
      screen: screenWith(
        composer: _draft(
          files: [
            _file('a.pdf', '2 KB', replaces: true),
            _file('b.txt', '1 KB'),
          ],
        ),
      ),
      timeline: _holding(),
      sent: <GuiIntent>[],
    );
    expect(find.text('a.pdf · 2 KB · replaces'), findsOneWidget);
    // Control: a file that replaces nothing does not say so.
    expect(find.text('b.txt · 1 KB'), findsOneWidget);
    expect(find.textContaining('replaces'), findsOneWidget);
  });

  testWidgets('a files-only draft can be sent', (tester) async {
    final sent = <GuiIntent>[];
    await pumpSession(
      tester,
      screen: screenWith(composer: _draft(files: [_file('a.pdf', '2 KB')])),
      timeline: _holding(),
      sent: sent,
    );
    expect(_sendButton(tester).onPressed, isNotNull);
    await tester.tap(find.byTooltip('Send'));
    expect(sent.whereType<GuiIntentComposerSubmit>(), [
      const GuiIntentComposerSubmit(sessionId: 's1'),
    ]);

    // Control: with no text, no image and no file, the button is off.
    sent.clear();
    await pumpSession(
      tester,
      screen: screenWith(composer: _draft(rev: 2)),
      timeline: _holding(),
      sent: sent,
    );
    expect(_sendButton(tester).onPressed, isNull);
  });

  testWidgets('the attach button sends a file intent per picked file', (
    tester,
  ) async {
    final sent = <GuiIntent>[];
    await pumpSession(
      tester,
      screen: screenWith(),
      timeline: _holding(),
      sent: sent,
    );
    pickAttachments = (_) async => ['/notes.pdf', '/a.png'];
    await tester.tap(find.byKey(const Key('composer-menu')));
    await tester.pumpAndSettle();
    await tester.tap(find.byKey(const Key('attach-file')));
    await tester.pumpAndSettle();
    expect(sent.whereType<GuiIntentAttachFile>(), [
      const GuiIntentAttachFile(sessionId: 's1', path: '/notes.pdf'),
      const GuiIntentAttachFile(sessionId: 's1', path: '/a.png'),
    ]);
  });

  testWidgets('shell mode shows no file chips', (tester) async {
    await pumpSession(
      tester,
      screen: screenWith(
        composer: _draft(
          mode: ComposerModeView.shell,
          files: [_file('a.pdf', '2 KB')],
        ),
      ),
      timeline: _holding(),
      sent: <GuiIntent>[],
    );
    expect(find.byKey(const Key('file-chip-a.pdf')), findsNothing);

    // Control: the agent composer shows the same file as a chip.
    await pumpSession(
      tester,
      screen: screenWith(
        composer: _draft(rev: 2, files: [_file('a.pdf', '2 KB')]),
      ),
      timeline: _holding(),
      sent: <GuiIntent>[],
    );
    expect(find.byKey(const Key('file-chip-a.pdf')), findsOneWidget);
  });
}
