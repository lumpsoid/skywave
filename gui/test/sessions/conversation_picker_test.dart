import 'package:material_ui/material_ui.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/sessions/session_list_page.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

const _picker = Key('conversation-picker');

ScreenViewSessionList withPicker(
  ConversationsView? conversations, {
  CreatePrompt? create = const CreatePrompt(input: '', inputRev: 0),
}) => ScreenViewSessionList(
  kind: ListKindView.active,
  title: 'Sessions',
  rows: const [],
  otherCount: 0,
  creating: const [],
  create: create,
  conversations: conversations,
);

const _listed = ConversationsView(
  rows: [
    ConversationRowView(
      conversationId: 'aaa',
      title: 'fix the flaky test',
      detail: '…/personal/skywave · 3 h ago · 42 messages',
      linkLabel: 'already in Skywave: “flaky”',
      sessionId: 's-1',
    ),
    ConversationRowView(
      conversationId: 'bbb',
      title: 'write the docs',
      detail: '…/personal/docs · 2 d ago · 7 messages',
    ),
  ],
  statusLabel: 'only the newest are listed',
  unavailable: false,
);

void main() {
  late List<GuiIntent> sent;

  Future<void> pump(WidgetTester tester, ScreenViewSessionList screen) async {
    await tester.pumpWidget(
      MaterialApp(
        home: SessionListPage(screen: screen, send: sent.add),
      ),
    );
    await tester.pump();
  }

  setUp(() => sent = []);

  testWidgets('the dialog button opens the picker', (tester) async {
    await pump(tester, withPicker(null));
    expect(find.byKey(_picker), findsNothing);
    await tester.tap(find.byKey(const Key('create-pick-conversation')));
    expect(find.text('Pick a conversation…'), findsOneWidget);
    expect(sent, [const GuiIntentConversationsOpen()]);
    // The paste field stays.
    expect(find.byKey(const Key('create-conversation')), findsOneWidget);
  });

  testWidgets('the sheet lists rows; a tap continues one, Open opens its '
      'session', (tester) async {
    await pump(tester, withPicker(_listed));
    expect(find.byKey(_picker), findsOneWidget);
    for (final text in [
      'fix the flaky test',
      '…/personal/skywave · 3 h ago · 42 messages',
      'already in Skywave: “flaky”',
      'write the docs',
      'only the newest are listed',
    ]) {
      expect(find.text(text), findsOneWidget, reason: text);
    }
    // Only the row running in a session offers Open.
    expect(find.text('Open'), findsOneWidget);

    await tester.tap(find.text('write the docs'));
    await tester.tap(find.text('Open'));
    await tester.tap(find.byTooltip('List again'));
    expect(sent, [
      const GuiIntentConversationPick(conversationId: 'bbb'),
      const GuiIntentAct(action: ActionIntentOpenSession(sessionId: 's-1')),
      const GuiIntentConversationsOpen(),
    ]);
  });

  testWidgets('Close, Esc, a tap outside and paste-instead close the sheet', (
    tester,
  ) async {
    await pump(tester, withPicker(_listed));
    await tester.tap(find.byTooltip('Close'));
    await tester.tap(find.text('Paste an id instead'));
    // Esc goes to the sheet, not the prompt under it.
    await tester.sendKeyEvent(LogicalKeyboardKey.escape);
    await tester.tapAt(const Offset(10, 10));
    expect(sent, List.filled(4, const GuiIntentConversationsClose()));
  });

  testWidgets('an empty picker says why, a refusal as a warning', (
    tester,
  ) async {
    await pump(
      tester,
      withPicker(
        const ConversationsView(
          rows: [],
          statusLabel: 'listing…',
          emptyLabel: "Asking the server for its agent's conversations…",
          unavailable: false,
        ),
      ),
    );
    expect(find.text('listing…'), findsOneWidget);
    expect(
      find.text("Asking the server for its agent's conversations…"),
      findsOneWidget,
    );
    const offline =
        'offline · the server lists its conversations once connected';
    Future<Color?> colorWhen({required bool unavailable}) async {
      await pump(
        tester,
        withPicker(
          ConversationsView(
            rows: const [],
            emptyLabel: offline,
            unavailable: unavailable,
          ),
        ),
      );
      return tester.widget<Text>(find.text(offline)).style?.color;
    }

    final plain = await colorWhen(unavailable: false);
    final warning = await colorWhen(unavailable: true);
    expect(plain, isNotNull);
    expect(warning, isNot(plain));
    expect(find.text('Open'), findsNothing);
  });

  testWidgets('the sheet goes once Rust clears it', (tester) async {
    await pump(tester, withPicker(_listed));
    expect(find.byKey(_picker), findsOneWidget);
    await pump(tester, withPicker(null));
    expect(find.byKey(_picker), findsNothing);
    expect(
      find.byKey(const Key('create-workdir')),
      findsOneWidget,
      reason: 'the prompt stays',
    );
  });
}
