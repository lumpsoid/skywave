import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/sessions/session_list_page.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

SessionRowView sessionRow(
  String id, {
  bool archived = false,
  bool deletable = false,
  String? blocked,
}) => SessionRowView(
  sessionId: id,
  title: 'title $id',
  workdir: '/srv/$id',
  state: AgentState.awaitingApproval,
  badgeLabel: 'approval',
  unread: 3,
  approvals: 1,
  approvalsLabel: '1 approval pending',
  hasDraft: true,
  costLabel: r'$1.27',
  lastActivityLabel: '2026-10-31 14:02 UTC',
  lastActivityMs: 0,
  renameLabel: 'queued',
  deletable: deletable,
  deleteBlockedLabel: blocked,
  archived: archived,
);

ScreenViewSessionList list({
  ListKindView kind = ListKindView.active,
  List<SessionRowView> rows = const [],
  List<CreatingView> creating = const [],
  String? dismissable,
  CreatePrompt? create,
  RenamePrompt? rename,
}) => ScreenViewSessionList(
  kind: kind,
  title: 'Sessions',
  rows: rows,
  otherCount: 2,
  creating: creating,
  dismissable: dismissable,
  create: create,
  rename: rename,
);

void main() {
  late List<GuiIntent> sent;

  Future<void> pump(WidgetTester tester, ScreenViewSessionList screen) =>
      tester.pumpWidget(
        MaterialApp(
          home: SessionListPage(screen: screen, send: sent.add),
        ),
      );

  setUp(() => sent = []);

  testWidgets('a row shows its labels and a tap opens it', (tester) async {
    await pump(tester, list(rows: [sessionRow('a')]));
    for (final text in [
      'approval',
      '/srv/a',
      '3',
      '1 approval pending',
      'draft',
      r'$1.27 · 2026-10-31 14:02 UTC',
      'Active (1)',
      'Archived (2)',
      'New session',
    ]) {
      expect(find.text(text), findsOneWidget, reason: text);
    }
    expect(find.textContaining('title a'), findsOneWidget);
    expect(find.textContaining('queued'), findsOneWidget);

    await tester.tap(find.textContaining('title a'));
    expect(sent, [
      const GuiIntentAct(action: ActionIntentOpenSession(sessionId: 'a')),
    ]);
  });

  testWidgets('the menu archives and unarchives', (tester) async {
    await pump(tester, list(rows: [sessionRow('a')]));
    await tester.tap(find.byKey(const Key('row-menu-a')));
    await tester.pumpAndSettle();
    expect(find.text('Delete from this device'), findsNothing);
    await tester.tap(find.text('Archive'));
    await tester.pumpAndSettle();

    await pump(
      tester,
      list(
        kind: ListKindView.archived,
        rows: [sessionRow('a', archived: true)],
      ),
    );
    await tester.pumpAndSettle();
    expect(find.text('New session'), findsNothing);
    await tester.tap(find.byKey(const Key('row-menu-a')));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Unarchive'));
    await tester.pumpAndSettle();
    expect(sent, [
      const GuiIntentAct(
        action: ActionIntentSetSessionArchived(sessionId: 'a', archived: true),
      ),
      const GuiIntentAct(
        action: ActionIntentSetSessionArchived(sessionId: 'a', archived: false),
      ),
    ]);
  });

  testWidgets('delete asks first, then deletes', (tester) async {
    await pump(
      tester,
      list(
        kind: ListKindView.archived,
        rows: [sessionRow('a', archived: true, deletable: true)],
      ),
    );
    await tester.tap(find.byKey(const Key('row-menu-a')));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Delete from this device'));
    await tester.pumpAndSettle();
    expect(find.textContaining('It stays on the server'), findsOneWidget);
    await tester.tap(find.text('Keep'));
    await tester.pumpAndSettle();
    expect(sent, isEmpty);

    await tester.tap(find.byKey(const Key('row-menu-a')));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Delete from this device'));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Delete'));
    await tester.pumpAndSettle();
    expect(sent, [
      const GuiIntentAct(
        action: ActionIntentDeleteSessionLocally(sessionId: 'a'),
      ),
    ]);
  });

  testWidgets('a blocked delete says why and sends nothing', (tester) async {
    const why = 'cannot delete title a yet: answer or cancel first';
    await pump(
      tester,
      list(
        kind: ListKindView.archived,
        rows: [sessionRow('a', archived: true, blocked: why)],
      ),
    );
    await tester.tap(find.byKey(const Key('row-menu-a')));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Delete from this device'));
    await tester.pumpAndSettle();
    expect(find.text(why), findsOneWidget);
    await tester.tap(find.text('OK'));
    await tester.pumpAndSettle();
    expect(sent, isEmpty);
  });

  testWidgets('the toggle, new session, rename and settings send intents', (
    tester,
  ) async {
    await pump(tester, list(rows: [sessionRow('a')]));
    await tester.tap(find.text('Archived (2)'));
    await tester.tap(find.text('New session'));
    await tester.tap(find.byTooltip('Settings'));
    await tester.tap(find.byKey(const Key('row-menu-a')));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Rename'));
    await tester.pumpAndSettle();
    expect(sent, [
      const GuiIntentAct(
        action: ActionIntentNavigate(route: RouteView.archivedSessions),
      ),
      const GuiIntentCreateOpen(),
      const GuiIntentAct(
        action: ActionIntentNavigate(route: RouteView.settings),
      ),
      const GuiIntentRenameOpen(sessionId: 'a'),
    ]);
  });

  testWidgets('the creating section dismisses only the dismissable one', (
    tester,
  ) async {
    await pump(
      tester,
      list(
        creating: const [
          CreatingView(
            commandId: 'c1',
            workdir: '/srv/one',
            outgoing: OutgoingView(
              state: OutgoingState.queued,
              label: 'queued',
            ),
            note: 'waits for the server',
            discardable: true,
          ),
          CreatingView(
            commandId: 'c2',
            workdir: '/srv/two',
            outgoing: OutgoingView(
              state: OutgoingState.failed,
              label: 'failed: refused',
            ),
            note: 'refused',
            discardable: false,
          ),
        ],
        dismissable: 'c2',
      ),
    );
    expect(find.text('Creating'), findsOneWidget);
    expect(find.text('waits for the server'), findsOneWidget);
    expect(find.text('Dismiss'), findsOneWidget);
    await tester.tap(find.text('Dismiss'));
    expect(sent, [
      const GuiIntentAct(action: ActionIntentDiscard(commandId: 'c2')),
    ]);
  });

  testWidgets('the rename prompt submits and cancels', (tester) async {
    await pump(
      tester,
      list(
        rows: [sessionRow('a')],
        rename: const RenamePrompt(sessionId: 'a', initial: 'old'),
      ),
    );
    await tester.pump();
    final field = find.byKey(const Key('rename-field'));
    expect(tester.widget<TextField>(field).controller!.text, 'old');
    await tester.enterText(field, 'new name');
    await tester.tap(find.widgetWithText(FilledButton, 'Rename'));
    await tester.sendKeyEvent(LogicalKeyboardKey.escape);
    expect(sent, [
      const GuiIntentRenameSubmit(name: 'new name'),
      const GuiIntentRenameCancel(),
    ]);

    await pump(tester, list(rows: [sessionRow('a')]));
    expect(field, findsNothing, reason: 'it closes when Rust clears it');
  });

  testWidgets('a phone-width list lays out without overflow', (tester) async {
    tester.view.physicalSize = const Size(320, 640);
    tester.view.devicePixelRatio = 1;
    addTearDown(tester.view.reset);
    await pump(
      tester,
      list(
        rows: [sessionRow('a'), sessionRow('b-with-a-much-longer-id-here')],
        create: const CreatePrompt(
          input: '/srv/',
          inputRev: 1,
          popup: CompletionPopup(
            candidates: [CandidateView(path: '/srv/one', recent: true)],
            selected: 0,
            statusLabel: 'offline · recent only',
          ),
        ),
      ),
    );
    await tester.pumpAndSettle();
    expect(tester.takeException(), isNull);
  });
}
