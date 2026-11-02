import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/sessions/session_list_page.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

ScreenViewSessionList withPrompt(CreatePrompt? create) => ScreenViewSessionList(
  kind: ListKindView.active,
  title: 'Sessions',
  rows: const [],
  otherCount: 0,
  creating: const [],
  create: create,
);

const _field = Key('create-workdir');

void main() {
  late List<GuiIntent> sent;

  Future<void> pump(WidgetTester tester, CreatePrompt? create) async {
    await tester.pumpWidget(
      MaterialApp(
        home: SessionListPage(screen: withPrompt(create), send: sent.add),
      ),
    );
    await tester.pump();
  }

  setUp(() => sent = []);

  testWidgets('typing and the keys send their intents', (tester) async {
    await pump(tester, const CreatePrompt(input: '', inputRev: 0));
    await tester.enterText(find.byKey(_field), '/srv/');
    await tester.sendKeyEvent(LogicalKeyboardKey.tab);
    await tester.sendKeyEvent(LogicalKeyboardKey.enter);
    await tester.sendKeyEvent(LogicalKeyboardKey.escape);
    expect(sent, [
      const GuiIntentCompletionTyped(input: '/srv/'),
      const GuiIntentCompletionComplete(),
      const GuiIntentCompletionAccept(),
      const GuiIntentCompletionCancel(),
    ]);
  });

  testWidgets('the id field sends its edits; Enter in it creates', (
    tester,
  ) async {
    await pump(tester, const CreatePrompt(input: '', inputRev: 0));
    expect(
      find.text('Continue a Claude Code session (paste its id)'),
      findsOneWidget,
    );
    await tester.enterText(find.byKey(const Key('create-conversation')), 'abc');
    await tester.sendKeyEvent(LogicalKeyboardKey.enter);
    expect(sent, [
      const GuiIntentCreateConversationTyped(conversation: 'abc'),
      const GuiIntentCompletionAccept(),
    ]);
  });

  testWidgets('the popup lists candidates; arrows move, a tap takes one', (
    tester,
  ) async {
    await pump(
      tester,
      const CreatePrompt(
        input: '/srv/',
        inputRev: 1,
        popup: CompletionPopup(
          candidates: [
            CandidateView(path: '/srv/one', recent: true),
            CandidateView(path: '/srv/two', recent: false),
          ],
          selected: 0,
          statusLabel: 'more not shown · keep typing',
        ),
      ),
    );
    expect(find.text('/srv/one'), findsOneWidget);
    expect(find.text('/srv/two'), findsOneWidget);
    expect(find.text('recent'), findsOneWidget);
    expect(find.text('more not shown · keep typing'), findsOneWidget);
    expect(
      tester.widget<TextField>(find.byKey(_field)).controller!.text,
      '/srv/',
    );

    await tester.sendKeyEvent(LogicalKeyboardKey.arrowDown);
    await tester.sendKeyEvent(LogicalKeyboardKey.arrowUp);
    await tester.tap(find.text('/srv/two'));
    expect(sent, [
      const GuiIntentCompletionMove(up: false),
      const GuiIntentCompletionMove(up: true),
      const GuiIntentCompletionPick(index: 1),
    ]);
  });

  testWidgets('a new input rev fills the field; the same rev does not', (
    tester,
  ) async {
    await pump(tester, const CreatePrompt(input: '', inputRev: 0));
    await tester.enterText(find.byKey(_field), '/sr');
    await pump(tester, const CreatePrompt(input: '', inputRev: 0));
    String text() =>
        tester.widget<TextField>(find.byKey(_field)).controller!.text;
    expect(text(), '/sr');
    await pump(tester, const CreatePrompt(input: '/srv/one/', inputRev: 1));
    expect(text(), '/srv/one/');
  });

  testWidgets('an empty listing says so; the dialog goes when Rust closes it', (
    tester,
  ) async {
    await pump(
      tester,
      const CreatePrompt(
        input: '/nope',
        inputRev: 0,
        popup: CompletionPopup(
          candidates: [],
          selected: 0,
          emptyLabel: 'no matching directories',
        ),
      ),
    );
    expect(find.text('no matching directories'), findsOneWidget);
    await pump(tester, null);
    expect(find.byKey(_field), findsNothing);
  });
}
