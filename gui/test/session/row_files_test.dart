import 'package:material_ui/material_ui.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/session/timeline_holder.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

import 'rows.dart';

TimelineHolder _holding(List<TimelineRow> rows) =>
    TimelineHolder()
      ..apply(TimelineUpdateSnapshot(sessionId: 's1', items: rows));

void _tall(WidgetTester tester) {
  tester.view
    ..physicalSize = const Size(1200, 3000)
    ..devicePixelRatio = 1;
  addTearDown(tester.view.reset);
}

/// A queued message with [files] that replaces what it found, if [allowed].
TimelineRow _queued(List<RowFile> files, {bool allowed = false}) => anyRow(
  'p:2',
  RowKind.pendingMessage,
  'replace it',
  author: 'me',
  own: true,
  commandId: 'c1',
  outgoing: const OutgoingView(
    state: OutgoingState.failed,
    label: 'failed: refused',
  ),
  files: files,
  canResendReplacing: allowed,
);

void main() {
  testWidgets('a queued message lists its files', (tester) async {
    _tall(tester);
    final sent = <GuiIntent>[];
    await pumpSession(
      tester,
      screen: screenWith(),
      timeline: _holding([
        anyRow(
          'p:1',
          RowKind.pendingMessage,
          'two files',
          author: 'me',
          own: true,
          files: const [
            RowFile(name: 'a.pdf', sizeLabel: '2 KB', found: false),
            RowFile(name: 'x.txt', sizeLabel: '1 KB', found: true),
          ],
        ),
      ]),
      sent: sent,
    );

    expect(find.byKey(const Key('row-file-0')), findsOneWidget);
    expect(find.byKey(const Key('row-file-1')), findsOneWidget);
    expect(find.text('a.pdf · 2 KB'), findsOneWidget);
    expect(find.text('x.txt · 1 KB'), findsOneWidget);
    // Only the file found in the workdir says so.
    expect(find.textContaining('already in the workdir'), findsOneWidget);
  });

  testWidgets('no Replace & resend unless the row allows it', (tester) async {
    _tall(tester);
    final found = [
      const RowFile(name: 'x.txt', sizeLabel: '1 KB', found: true),
    ];
    await pumpSession(
      tester,
      screen: screenWith(),
      timeline: _holding([_queued(found)]),
      sent: <GuiIntent>[],
    );
    expect(find.byKey(const Key('resend-replacing')), findsNothing);

    // Control: the same row, allowed, offers it.
    await pumpSession(
      tester,
      screen: screenWith(),
      timeline: _holding([_queued(found, allowed: true)]),
      sent: <GuiIntent>[],
    );
    expect(find.byKey(const Key('resend-replacing')), findsOneWidget);
  });

  testWidgets('Replace & resend asks first and resends on confirm', (
    tester,
  ) async {
    _tall(tester);
    final sent = <GuiIntent>[];
    await pumpSession(
      tester,
      screen: screenWith(),
      timeline: _holding([
        _queued(const [
          RowFile(name: 'x.txt', sizeLabel: '1 KB', found: true),
        ], allowed: true),
      ]),
      sent: sent,
    );
    sent.clear();

    await tester.tap(find.byKey(const Key('resend-replacing')));
    await tester.pumpAndSettle();
    expect(
      find.descendant(
        of: find.byType(AlertDialog),
        matching: find.textContaining('x.txt'),
      ),
      findsOneWidget,
    );
    expect(sent, isEmpty, reason: 'nothing is sent before the user confirms');

    await tester.tap(find.byKey(const Key('confirm-ok')));
    await tester.pumpAndSettle();
    expect(sent, [
      const GuiIntentAct(action: ActionIntentResendReplacing(commandId: 'c1')),
    ]);
  });

  testWidgets('cancelling Replace & resend sends nothing', (tester) async {
    _tall(tester);
    final sent = <GuiIntent>[];
    await pumpSession(
      tester,
      screen: screenWith(),
      timeline: _holding([
        _queued(const [
          RowFile(name: 'x.txt', sizeLabel: '1 KB', found: true),
        ], allowed: true),
      ]),
      sent: sent,
    );
    sent.clear();

    await tester.tap(find.byKey(const Key('resend-replacing')));
    await tester.pumpAndSettle();
    await tester.tap(find.byKey(const Key('confirm-cancel')));
    await tester.pumpAndSettle();
    expect(sent, isEmpty);
  });
}
