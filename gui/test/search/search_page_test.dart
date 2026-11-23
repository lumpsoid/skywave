import 'package:material_ui/material_ui.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/search/search_page.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

const _rows = [
  SearchRowView(
    key: 'a',
    sessionTitle: 'Fix login',
    kindLabel: 'Message',
    timeLabel: '10:02',
    snippet: 'Fixed the login bug',
    matched: [Utf16Range(start: 10, end: 15)],
  ),
  SearchRowView(
    key: 'b',
    sessionTitle: 'Docs',
    kindLabel: 'Tool',
    timeLabel: '09:40',
    snippet: 'Read login.md',
    matched: [Utf16Range(start: 5, end: 10)],
  ),
];

const _screen = ScreenViewSearch(
  query: 'login',
  countLabel: '2 hits',
  partialLabel: 'Still searching',
  rows: _rows,
);

const _scoped = ScreenViewSearch(
  scopeId: 's1',
  scopeTitle: 'Fix login',
  query: 'login',
  rows: _rows,
);

/// The snippet's title, found by its plain text.
Finder _snippet(String text) => find.byWidgetPredicate(
  (widget) => widget is Text && widget.textSpan?.toPlainText() == text,
);

void main() {
  testWidgets('rows show their snippet, kind and session title', (
    tester,
  ) async {
    await tester.pumpWidget(
      MaterialApp(
        home: SearchPage(screen: _screen, send: (_) {}),
      ),
    );
    expect(find.text('2 hits · Still searching'), findsOneWidget);
    expect(_snippet('Fixed the login bug'), findsOneWidget);
    expect(_snippet('Read login.md'), findsOneWidget);
    expect(find.text('Message · Fix login · 10:02'), findsOneWidget);
    expect(find.text('Tool · Docs · 09:40'), findsOneWidget);
    expect(find.byKey(const Key('hit-a')), findsOneWidget);
  });

  testWidgets('the matched part of a snippet is its own styled span', (
    tester,
  ) async {
    await tester.pumpWidget(
      MaterialApp(
        home: SearchPage(screen: _screen, send: (_) {}),
      ),
    );
    final title = tester.widget<Text>(_snippet('Fixed the login bug'));
    final spans = (title.textSpan! as TextSpan).children!.cast<TextSpan>();
    expect(
      [for (final span in spans) span.text],
      ['Fixed the ', 'login', ' bug'],
    );
    expect(spans.first.style, isNull);
    expect(spans[1].style?.backgroundColor, isNotNull);
  });

  testWidgets('typing sends the query', (tester) async {
    final sent = <GuiIntent>[];
    await tester.pumpWidget(
      MaterialApp(
        home: SearchPage(screen: _screen, send: sent.add),
      ),
    );
    await tester.enterText(find.byType(TextField), 'logi');
    expect(sent, [const GuiIntentGlobalSearchTyped(query: 'logi')]);
  });

  testWidgets('tapping a hit opens it by its key', (tester) async {
    final sent = <GuiIntent>[];
    await tester.pumpWidget(
      MaterialApp(
        home: SearchPage(screen: _screen, send: sent.add),
      ),
    );
    await tester.tap(find.byKey(const Key('hit-b')));
    expect(sent, [const GuiIntentGlobalSearchOpen(key: 'b')]);
  });

  testWidgets('a scoped page names its session and hides session titles', (
    tester,
  ) async {
    await tester.pumpWidget(
      MaterialApp(
        home: SearchPage(screen: _scoped, send: (_) {}),
      ),
    );
    expect(find.text('In: Fix login'), findsOneWidget);
    expect(find.text('Message · 10:02'), findsOneWidget);
    expect(find.text('Message · Fix login · 10:02'), findsNothing);
  });

  testWidgets('the scope chip drops the scope', (tester) async {
    final sent = <GuiIntent>[];
    await tester.pumpWidget(
      MaterialApp(
        home: SearchPage(screen: _scoped, send: sent.add),
      ),
    );
    await tester.tap(find.byTooltip('Search all sessions'));
    expect(sent, [
      const GuiIntentAct(action: ActionIntentOpenSearch(sessionId: null)),
    ]);
  });
}
