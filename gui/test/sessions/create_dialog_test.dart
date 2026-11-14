import 'package:material_ui/material_ui.dart';
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

  testWidgets('the worktree field sends its edits; Enter in it creates', (
    tester,
  ) async {
    await pump(tester, const CreatePrompt(input: '', inputRev: 0));
    expect(find.text('New worktree on branch (optional)'), findsOneWidget);
    await tester.enterText(find.byKey(const Key('create-worktree')), 'feat/x');
    await tester.sendKeyEvent(LogicalKeyboardKey.enter);
    expect(sent, [
      const GuiIntentCreateWorktreeTyped(worktree: 'feat/x'),
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
            CandidateView(path: '/srv/one', label: '/srv/one', recent: true),
            CandidateView(path: '/srv/two', label: '/srv/two', recent: false),
          ],
          selected: 0,
          stale: false,
          loading: false,
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
          stale: false,
          loading: false,
          emptyLabel: 'no matching directories',
        ),
      ),
    );
    expect(find.text('no matching directories'), findsOneWidget);
    await pump(tester, null);
    expect(find.byKey(_field), findsNothing);
  });

  testWidgets('the dialog keeps its size while the list changes', (
    tester,
  ) async {
    const input = '/srv/';
    CandidateView row(int i) =>
        CandidateView(path: '/srv/d$i', label: 'd$i', recent: false);
    final sizes = <Size>[];
    Future<void> show(CompletionPopup? popup) async {
      await pump(tester, CreatePrompt(input: input, inputRev: 1, popup: popup));
      expect(tester.takeException(), isNull, reason: 'no overflow');
      sizes.add(tester.getSize(find.byType(AlertDialog)));
    }

    await show(null);
    await show(
      const CompletionPopup(candidates: [], stale: false, loading: true),
    );
    await show(
      const CompletionPopup(
        candidates: [],
        stale: false,
        loading: false,
        emptyLabel: 'no matching directories',
      ),
    );
    await show(
      CompletionPopup(
        candidates: [row(0), row(1)],
        stale: false,
        loading: false,
      ),
    );
    await show(
      CompletionPopup(
        candidates: [for (var i = 0; i < 10; i++) row(i)],
        selected: 3,
        stale: false,
        loading: false,
      ),
    );
    await show(
      CompletionPopup(candidates: [row(0), row(1)], stale: true, loading: true),
    );
    await show(
      CompletionPopup(
        candidates: [row(0)],
        stale: false,
        loading: false,
        statusLabel: 'more not shown · keep typing',
      ),
    );
    expect(sizes, hasLength(7));
    expect(sizes, everyElement(sizes.first));
  });

  testWidgets('the list fits a short window', (tester) async {
    tester.view.physicalSize = const Size(800, 600);
    tester.view.devicePixelRatio = 1;
    addTearDown(tester.view.reset);

    Future<double> boxHeight(List<CandidateView> candidates) async {
      await pump(
        tester,
        CreatePrompt(
          input: '/srv/',
          inputRev: 1,
          popup: CompletionPopup(
            candidates: candidates,
            stale: false,
            loading: false,
          ),
        ),
      );
      expect(tester.takeException(), isNull, reason: 'no overflow');
      return tester.getSize(find.byKey(const Key('completion-popup'))).height;
    }

    final none = await boxHeight(<CandidateView>[]);
    final many = await boxHeight([
      for (var i = 0; i < 10; i++)
        CandidateView(path: '/srv/d$i', label: 'd$i', recent: false),
    ]);
    expect(many, none, reason: 'the height never follows the candidates');
    expect(many, greaterThan(0));
    expect(many, lessThanOrEqualTo(240));
  });

  testWidgets('loading shows a progress bar', (tester) async {
    await pump(
      tester,
      const CreatePrompt(
        input: '/srv/',
        inputRev: 1,
        popup: CompletionPopup(candidates: [], stale: false, loading: true),
      ),
    );
    expect(find.byType(LinearProgressIndicator), findsOneWidget);
    await pump(
      tester,
      const CreatePrompt(
        input: '/srv/',
        inputRev: 1,
        popup: CompletionPopup(candidates: [], stale: false, loading: false),
      ),
    );
    expect(find.byType(LinearProgressIndicator), findsNothing);
  });

  testWidgets('stale rows are dimmed and ignore taps', (tester) async {
    const row = CandidateView(path: '/srv/one', label: 'one', recent: false);
    Finder dimmed() => find.ancestor(
      of: find.text('one'),
      matching: find.byWidgetPredicate(
        (widget) => widget is Opacity && widget.opacity == 0.5,
      ),
    );

    await pump(
      tester,
      const CreatePrompt(
        input: '/srv/',
        inputRev: 1,
        popup: CompletionPopup(candidates: [row], stale: true, loading: true),
      ),
    );
    expect(dimmed(), findsOneWidget);
    // The tap is meant to miss: IgnorePointer lets it through to nothing.
    await tester.tap(find.text('one'), warnIfMissed: false);
    expect(sent, isEmpty, reason: 'a stale row is not taken');

    await pump(
      tester,
      const CreatePrompt(
        input: '/srv/',
        inputRev: 1,
        popup: CompletionPopup(candidates: [row], stale: false, loading: false),
      ),
    );
    expect(dimmed(), findsNothing);
    await tester.tap(find.text('one'));
    expect(sent, [const GuiIntentCompletionPick(index: 0)]);
  });

  testWidgets('rows show their label', (tester) async {
    await pump(
      tester,
      const CreatePrompt(
        input: '/srv/',
        inputRev: 1,
        popup: CompletionPopup(
          candidates: [
            CandidateView(path: '/srv/api', label: 'api', recent: false),
          ],
          stale: false,
          loading: false,
        ),
      ),
    );
    expect(find.text('api'), findsOneWidget);
    expect(find.text('/srv/api'), findsNothing);
  });

  testWidgets('the hint follows the highlight', (tester) async {
    const row = CandidateView(path: '/srv/one', label: 'one', recent: false);
    Future<void> showPopup({int? selected, String? statusLabel}) => pump(
      tester,
      CreatePrompt(
        input: '/srv/',
        inputRev: 1,
        popup: CompletionPopup(
          candidates: const [row],
          selected: selected,
          stale: false,
          loading: false,
          statusLabel: statusLabel,
        ),
      ),
    );

    await showPopup();
    expect(find.textContaining('Enter create'), findsOneWidget);
    await showPopup(selected: 0);
    expect(find.textContaining('Esc unselect'), findsOneWidget);
    expect(find.textContaining('Enter create'), findsNothing);
    await showPopup(selected: 0, statusLabel: 'offline · recent only');
    expect(find.text('offline · recent only'), findsOneWidget);
    expect(find.textContaining('Esc unselect'), findsNothing);
  });
}
