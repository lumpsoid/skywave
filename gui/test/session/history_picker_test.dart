import 'package:material_ui/material_ui.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/session/timeline_holder.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

import 'rows.dart';

TimelineHolder _holding() =>
    TimelineHolder()..apply(TimelineUpdateSnapshot(sessionId: 's1', items: []));

/// A composer in [mode] (shell by default), with the picker open on
/// [search] when given.
ComposerView _draft({
  String text = '',
  int rev = 1,
  HistorySearchView? search,
  ComposerModeView mode = ComposerModeView.shell,
}) => ComposerView(
  text: text,
  cursorUtf16: text.length,
  rev: rev,
  mode: mode,
  shellEnabled: true,
  prompt: mode == ComposerModeView.shell
      ? const ShellPromptView(cwd: 'work', sigil: r'$')
      : null,
  canStop: false,
  images: const [],
  files: const [],
  preparing: 0,
  historySearch: search,
);

HistorySearchView _search({
  List<HistoryRowView> rows = const [],
  int? selected,
  String countLabel = '',
  String? emptyLabel,
}) => HistorySearchView(
  rows: rows,
  selected: selected,
  countLabel: countLabel,
  emptyLabel: emptyLabel,
);

const _twoRows = [
  HistoryRowView(command: 'git stash', matched: []),
  HistoryRowView(command: 'git status', matched: []),
];

Finder _composer() => find.byType(TextField).last;

void main() {
  testWidgets('rows render newest at the bottom and a tap picks', (
    tester,
  ) async {
    final sent = <GuiIntent>[];
    await pumpSession(
      tester,
      screen: screenWith(
        composer: _draft(
          search: _search(rows: _twoRows, selected: 0, countLabel: '2/5'),
        ),
      ),
      timeline: _holding(),
      sent: sent,
    );
    expect(find.byKey(const Key('history-picker')), findsOneWidget);
    // Row 0, the newest, sits below row 1.
    final row0 = tester.getTopLeft(find.byKey(const Key('history-row-0')));
    final row1 = tester.getTopLeft(find.byKey(const Key('history-row-1')));
    expect(row0.dy, greaterThan(row1.dy));
    expect(find.text('2/5'), findsOneWidget);

    await tester.tap(find.byKey(const Key('history-row-1')));
    expect(sent, contains(const GuiIntentHistorySearchPick(index: 1)));
  });

  testWidgets('the picker sits above the shell bar', (tester) async {
    final sent = <GuiIntent>[];
    await pumpSession(
      tester,
      screen: screenWith(
        composer: _draft(
          search: _search(rows: _twoRows, selected: 0, countLabel: '2/5'),
        ),
      ),
      timeline: _holding(),
      sent: sent,
    );
    // Positive control: both are on screen, so the comparison is real.
    expect(find.byKey(const Key('shell-cwd')), findsOneWidget);
    final picker = tester.getBottomLeft(
      find.byKey(const Key('history-picker')),
    );
    final bar = tester.getTopLeft(find.byKey(const Key('shell-cwd')));
    expect(picker.dy, lessThanOrEqualTo(bar.dy));
  });

  testWidgets('matched chars are highlighted', (tester) async {
    final sent = <GuiIntent>[];
    await pumpSession(
      tester,
      screen: screenWith(
        composer: _draft(
          search: _search(
            rows: const [
              HistoryRowView(
                command: 'git stash',
                matched: [Utf16Range(start: 0, end: 3)],
              ),
            ],
            selected: 0,
            countLabel: '1/1',
          ),
        ),
      ),
      timeline: _holding(),
      sent: sent,
    );
    final rich = tester.widget<RichText>(
      find.descendant(
        of: find.byKey(const Key('history-row-0')),
        matching: find.byType(RichText),
      ),
    );
    // Text.rich wraps the spans in one outer span.
    final outer = (rich.text as TextSpan).children!.single as TextSpan;
    final spans = outer.children!.cast<TextSpan>();
    final matched = spans.singleWhere((span) => span.text == 'git');
    expect(matched.style?.backgroundColor, isNotNull);
    // Positive control: the unmatched part is not marked.
    final rest = spans.singleWhere((span) => span.text == ' stash');
    expect(rest.style?.backgroundColor, isNull);
  });

  testWidgets('keys drive the open picker', (tester) async {
    final sent = <GuiIntent>[];
    await pumpSession(
      tester,
      screen: screenWith(
        composer: _draft(
          search: _search(rows: _twoRows, selected: 0, countLabel: '2/5'),
        ),
      ),
      timeline: _holding(),
      sent: sent,
    );
    await tester.tap(_composer());
    await tester.pump();
    await tester.sendKeyEvent(LogicalKeyboardKey.arrowUp);
    await tester.sendKeyEvent(LogicalKeyboardKey.arrowDown);
    await tester.sendKeyEvent(LogicalKeyboardKey.enter);
    await tester.sendKeyEvent(LogicalKeyboardKey.escape);
    expect(sent.where((intent) => intent is! GuiIntentDraftEdited).toList(), [
      const GuiIntentHistorySearchMove(older: true),
      const GuiIntentHistorySearchMove(older: false),
      const GuiIntentHistorySearchAccept(),
      const GuiIntentHistorySearchClose(),
    ]);

    // Tab accepts too, if it reaches the composer's key handler at all.
    sent.clear();
    await tester.sendKeyEvent(LogicalKeyboardKey.tab);
    expect(sent.where((intent) => intent is! GuiIntentDraftEdited).toList(), [
      const GuiIntentHistorySearchAccept(),
    ]);
  });

  testWidgets('ctrl-r opens it in shell mode, and closes it again', (
    tester,
  ) async {
    final sent = <GuiIntent>[];
    await pumpSession(
      tester,
      screen: screenWith(composer: _draft()),
      timeline: _holding(),
      sent: sent,
    );
    await tester.tap(_composer());
    await tester.pump();
    await tester.sendKeyDownEvent(LogicalKeyboardKey.controlLeft);
    await tester.sendKeyEvent(LogicalKeyboardKey.keyR);
    // A held key repeats: the repeat must not toggle it again.
    await tester.sendKeyRepeatEvent(LogicalKeyboardKey.keyR);
    await tester.sendKeyUpEvent(LogicalKeyboardKey.controlLeft);
    expect(sent, contains(const GuiIntentHistorySearchOpen()));
    expect(
      sent.whereType<GuiIntentHistorySearchOpen>().length +
          sent.whereType<GuiIntentHistorySearchClose>().length,
      1,
    );
    // Closed, so Up still recalls the prompt.
    await tester.sendKeyEvent(LogicalKeyboardKey.arrowUp);
    expect(sent.last, const GuiIntentHistoryOlder());

    // Open, Ctrl-R closes it.
    await pumpSession(
      tester,
      screen: screenWith(composer: _draft(rev: 2, search: _search())),
      timeline: _holding(),
      sent: sent,
    );
    await tester.tap(_composer());
    await tester.pump();
    await tester.sendKeyDownEvent(LogicalKeyboardKey.controlLeft);
    await tester.sendKeyEvent(LogicalKeyboardKey.keyR);
    await tester.sendKeyUpEvent(LogicalKeyboardKey.controlLeft);
    expect(sent.last, const GuiIntentHistorySearchClose());

    // Control: in agent mode Ctrl-R opens nothing.
    sent.clear();
    await pumpSession(
      tester,
      screen: screenWith(
        composer: _draft(rev: 3, mode: ComposerModeView.agent),
      ),
      timeline: _holding(),
      sent: sent,
    );
    await tester.tap(_composer());
    await tester.pump();
    await tester.sendKeyDownEvent(LogicalKeyboardKey.controlLeft);
    await tester.sendKeyEvent(LogicalKeyboardKey.keyR);
    await tester.sendKeyUpEvent(LogicalKeyboardKey.controlLeft);
    expect(sent.whereType<GuiIntentHistorySearchOpen>(), isEmpty);
  });

  testWidgets('an empty picker shows its label, and no rows', (tester) async {
    final sent = <GuiIntent>[];
    await pumpSession(
      tester,
      screen: screenWith(
        composer: _draft(
          search: _search(countLabel: '0/0', emptyLabel: 'No match'),
        ),
      ),
      timeline: _holding(),
      sent: sent,
    );
    expect(find.text('No match'), findsOneWidget);
    expect(find.byKey(const Key('history-row-0')), findsNothing);
  });

  testWidgets('the input menu opens the picker, and agent mode has no such '
      'action', (tester) async {
    final sent = <GuiIntent>[];
    await pumpSession(
      tester,
      screen: screenWith(composer: _draft()),
      timeline: _holding(),
      sent: sent,
    );
    await tester.tap(find.byKey(const Key('composer-menu')));
    await tester.pumpAndSettle();
    await tester.tap(find.byKey(const Key('history-search')));
    await tester.pumpAndSettle();
    expect(sent, contains(const GuiIntentHistorySearchOpen()));

    // Control: agent mode's menu has the previous-prompt action, not this.
    await pumpSession(
      tester,
      screen: screenWith(composer: _draft(mode: ComposerModeView.agent)),
      timeline: _holding(),
      sent: sent,
    );
    await tester.tap(find.byKey(const Key('composer-menu')));
    await tester.pumpAndSettle();
    expect(find.byKey(const Key('history-older')), findsOneWidget);
    expect(find.byKey(const Key('history-search')), findsNothing);
  });
}
