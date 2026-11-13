import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:flutter/rendering.dart';
import 'package:skywave_gui/sessions/session_list_page.dart';
import 'package:skywave_gui/sessions/session_row.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

SessionRowView sessionRow(
  String id, {
  bool archived = false,
  bool deletable = false,
  String? blocked,
  String? title,
  bool hasWorkdir = true,
  String? group,
  String? branch,
  bool removable = false,
  bool worktreeRemoved = false,
}) => SessionRowView(
  sessionId: id,
  title: title ?? 'title $id',
  workdir: hasWorkdir ? '/srv/proj-$id' : null,
  workdirName: hasWorkdir ? 'proj-$id' : null,
  group: group ?? (hasWorkdir ? '/srv/proj-$id' : ''),
  groupName:
      group?.split('/').last ?? (hasWorkdir ? 'proj-$id' : 'no directory'),
  branch: branch,
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
  removable: removable,
  worktreeRemoved: worktreeRemoved,
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

/// Text inside a session row, not a group header with the same words.
Finder rowText(String text) =>
    find.descendant(of: find.byType(SessionRow), matching: find.text(text));

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
      '/srv/proj-a',
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

  testWidgets('rows are grouped under a header per repo', (tester) async {
    await pump(
      tester,
      list(
        rows: [
          sessionRow('a', group: '/srv/app'),
          sessionRow('b', group: '/srv/app'),
          sessionRow('c', group: '/srv/other'),
        ],
      ),
    );
    final appHeader = find.byKey(const ValueKey('group:/srv/app'));
    final otherHeader = find.byKey(const ValueKey('group:/srv/other'));
    expect(appHeader, findsOneWidget);
    expect(otherHeader, findsOneWidget);
    expect(find.text('app'), findsOneWidget);
    expect(find.text('other'), findsOneWidget);

    double top(Finder finder) => tester.getTopLeft(finder).dy;
    final a = find.byKey(const ValueKey('a'));
    final b = find.byKey(const ValueKey('b'));
    final c = find.byKey(const ValueKey('c'));
    expect(top(appHeader), lessThan(top(a)));
    expect(top(a), lessThan(top(b)));
    expect(top(b), lessThan(top(otherHeader)));
    expect(top(otherHeader), lessThan(top(c)));
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

  testWidgets('the menu starts a new session in the row\'s workdir', (
    tester,
  ) async {
    await pump(
      tester,
      list(rows: [sessionRow('a'), sessionRow('b', hasWorkdir: false)]),
    );
    await tester.tap(find.byKey(const Key('row-menu-b')));
    await tester.pumpAndSettle();
    expect(find.text('New session here'), findsNothing, reason: 'no workdir');
    await tester.sendKeyEvent(LogicalKeyboardKey.escape);
    await tester.pumpAndSettle();

    await tester.tap(find.byKey(const Key('row-menu-a')));
    await tester.pumpAndSettle();
    await tester.tap(find.text('New session here'));
    await tester.pumpAndSettle();
    expect(sent, [
      const GuiIntentAct(action: ActionIntentCreateSessionLike(sessionId: 'a')),
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

  testWidgets('remove worktree asks first, then removes', (tester) async {
    await pump(
      tester,
      list(
        kind: ListKindView.archived,
        rows: [
          sessionRow('a', archived: true, removable: true, branch: 'feat-x'),
        ],
      ),
    );
    await tester.tap(find.byKey(const Key('row-menu-a')));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Remove worktree'));
    await tester.pumpAndSettle();
    expect(find.textContaining('feat-x'), findsOneWidget);
    expect(find.textContaining('not merged'), findsOneWidget);
    await tester.tap(find.text('Keep'));
    await tester.pumpAndSettle();
    expect(sent, isEmpty);

    await tester.tap(find.byKey(const Key('row-menu-a')));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Remove worktree'));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Remove'));
    await tester.pumpAndSettle();
    expect(sent, [
      const GuiIntentAct(action: ActionIntentRemoveWorktree(sessionId: 'a')),
    ]);
  });

  testWidgets('remove worktree is offered only when removable', (tester) async {
    await pump(
      tester,
      list(
        kind: ListKindView.archived,
        rows: [sessionRow('a', archived: true)],
      ),
    );
    await tester.tap(find.byKey(const Key('row-menu-a')));
    await tester.pumpAndSettle();
    // The menu is open: its archive entry is there.
    expect(find.text('Unarchive'), findsOneWidget);
    expect(find.text('Remove worktree'), findsNothing);
  });

  testWidgets('a removed worktree shows a marker', (tester) async {
    final semantics = tester.ensureSemantics();
    await pump(tester, list(rows: [sessionRow('a', worktreeRemoved: true)]));
    expect(find.byKey(const Key('row-worktree-removed')), findsOneWidget);
    expect(find.text('worktree removed'), findsOneWidget);
    expect(
      find.bySemanticsLabel(RegExp(', worktree removed, ')),
      findsOneWidget,
    );

    await pump(tester, list(rows: [sessionRow('a')]));
    expect(find.byKey(const Key('row-worktree-removed')), findsNothing);
    semantics.dispose();
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
            candidates: [
              CandidateView(path: '/srv/one', label: '/srv/one', recent: true),
            ],
            selected: 0,
            stale: false,
            loading: false,
            statusLabel: 'offline · recent only',
          ),
        ),
      ),
    );
    await tester.pumpAndSettle();
    expect(tester.takeException(), isNull);
  });
  testWidgets('a row whose history is importing says so', (tester) async {
    final semantics = tester.ensureSemantics();
    final plain = sessionRow('a');
    final importing = SessionRowView(
      sessionId: 'b',
      title: 'adopted',
      group: '',
      groupName: 'no directory',
      branch: null,
      state: AgentState.idle,
      badgeLabel: 'idle',
      unread: 0,
      approvals: 0,
      hasDraft: false,
      lastActivityLabel: plain.lastActivityLabel,
      lastActivityMs: 0,
      importingLabel: 'importing history…',
      deletable: false,
      removable: false,
      worktreeRemoved: false,
      archived: false,
    );
    await pump(tester, list(rows: [plain, importing]));
    expect(find.byKey(const Key('row-importing')), findsOneWidget);
    expect(find.text('importing history…'), findsOneWidget);
    expect(
      find.bySemanticsLabel(RegExp('^adopted, idle, importing history…, ')),
      findsOneWidget,
    );
    semantics.dispose();
  });

  testWidgets('a row reads as one sentence, its markers named', (tester) async {
    final semantics = tester.ensureSemantics();
    await pump(tester, list(rows: [sessionRow('a')]));
    expect(
      tester.getSemantics(find.byType(ListTile)),
      matchesSemantics(
        label:
            r'title a, rename queued, 3 unread, needs approval, '
            r'1 approval pending, draft saved, /srv/proj-a, $1.27, '
            'last active 2026-10-31 14:02 UTC',
        isButton: true,
        isFocusable: true,
        hasEnabledState: true,
        isEnabled: true,
        hasSelectedState: true,
        hasTapAction: true,
        hasFocusAction: true,
        // The menu keeps its own node, with its name.
        children: [
          matchesSemantics(
            tooltip: 'Session actions',
            isButton: true,
            isFocusable: true,
            hasEnabledState: true,
            isEnabled: true,
            hasExpandedState: true,
            hasTapAction: true,
            hasFocusAction: true,
          ),
        ],
      ),
    );
    // No fragment is read on its own: not the bare count, not the badge.
    expect(find.bySemanticsLabel('3'), findsNothing);
    expect(find.bySemanticsLabel('approval'), findsNothing);
    semantics.dispose();
  });

  testWidgets('an errored row says the agent errored and why', (tester) async {
    final semantics = tester.ensureSemantics();
    final row = sessionRow('a');
    await pump(
      tester,
      list(
        rows: [
          SessionRowView(
            sessionId: 'e',
            title: 'broken',
            group: '',
            groupName: 'no directory',
            branch: null,
            state: AgentState.errored,
            badgeLabel: 'error',
            unread: 0,
            approvals: 0,
            error: 'crashed',
            hasDraft: false,
            lastActivityLabel: row.lastActivityLabel,
            lastActivityMs: 0,
            deletable: false,
            removable: false,
            worktreeRemoved: false,
            archived: false,
          ),
        ],
      ),
    );
    expect(
      find.bySemanticsLabel(
        'broken, agent error: crashed, last active 2026-10-31 14:02 UTC',
      ),
      findsOneWidget,
    );
    semantics.dispose();
  });

  testWidgets('a wide row keeps the badge, one title line and the path', (
    tester,
  ) async {
    await pump(tester, list(rows: [sessionRow('a')]));
    expect(find.byType(StateBadge), findsOneWidget);
    expect(find.byKey(const Key('row-state-a')), findsNothing);
    expect(find.text('/srv/proj-a'), findsOneWidget);
    // The group header says `proj-a` too; only the row must not.
    expect(rowText('proj-a'), findsNothing);
    expect(tester.widget<Text>(find.textContaining('title a')).maxLines, 1);
  });

  group('on a phone', () {
    Future<void> phone(
      WidgetTester tester,
      ScreenViewSessionList screen,
    ) async {
      tester.view
        ..physicalSize = const Size(360, 780)
        ..devicePixelRatio = 1;
      addTearDown(tester.view.reset);
      await pump(tester, screen);
    }

    Future<void> openSheet(WidgetTester tester) async {
      await tester.tap(find.byKey(const Key('row-menu-a')));
      await tester.pumpAndSettle();
      expect(find.byType(BottomSheet), findsOneWidget);
    }

    Future<void> tapInSheet(WidgetTester tester, String text) async {
      await tester.tap(
        find.descendant(
          of: find.byType(BottomSheet),
          matching: find.text(text),
        ),
      );
      await tester.pumpAndSettle();
    }

    testWidgets('a row shows a state dot, two title lines and the folder '
        'name, with nothing clipped', (tester) async {
      final semantics = tester.ensureSemantics();
      // Wider than the title's room on a phone, not twice as wide.
      const long = 'too long for one';
      await phone(tester, list(rows: [sessionRow('a', title: long)]));
      expect(tester.takeException(), isNull);
      expect(find.byKey(const Key('row-state-a')), findsOneWidget);
      expect(find.byType(StateBadge), findsNothing);
      expect(find.text('approval'), findsNothing);
      expect(rowText('proj-a'), findsOneWidget);
      expect(find.text('/srv/proj-a'), findsNothing);
      for (final text in [
        '3',
        '1 approval pending',
        'draft',
        r'$1.27 · 2026-10-31 14:02 UTC',
      ]) {
        expect(find.text(text), findsOneWidget, reason: text);
      }
      final title = find.textContaining(long);
      expect(tester.widget<Text>(title).maxLines, 2);
      expect(tester.getSize(title).height, greaterThan(30));
      expect(
        tester
            .renderObject<RenderParagraph>(
              find.descendant(of: title, matching: find.byType(RichText)),
            )
            .didExceedMaxLines,
        isFalse,
      );
      // The screen reader still hears the state and the whole path.
      expect(
        find.bySemanticsLabel(RegExp('^$long, rename queued, .*/srv/proj-a, ')),
        findsOneWidget,
      );
      semantics.dispose();
    });

    testWidgets('a first prompt longer than two lines ends in an ellipsis', (
      tester,
    ) async {
      // Rust sends the first prompt whole; the row fits it to its room.
      final long = 'word ' * 40;
      await phone(tester, list(rows: [sessionRow('a', title: long)]));
      expect(tester.takeException(), isNull);
      final title = find.textContaining(long);
      expect(tester.widget<Text>(title).overflow, TextOverflow.ellipsis);
      expect(
        tester
            .renderObject<RenderParagraph>(
              find.descendant(of: title, matching: find.byType(RichText)),
            )
            .didExceedMaxLines,
        isTrue,
      );
    });

    testWidgets('the menu is a sheet with the full name and path; a tap '
        'closes it and runs the action', (tester) async {
      await phone(tester, list(rows: [sessionRow('a')]));
      await openSheet(tester);
      expect(find.widgetWithText(SelectableText, 'title a'), findsOneWidget);
      expect(
        find.widgetWithText(SelectableText, '/srv/proj-a'),
        findsOneWidget,
      );
      expect(find.text('Delete from this device'), findsNothing);
      await tapInSheet(tester, 'Rename');
      expect(find.byType(BottomSheet), findsNothing);

      await openSheet(tester);
      await tapInSheet(tester, 'Archive');
      expect(find.byType(BottomSheet), findsNothing);

      await pump(
        tester,
        list(
          kind: ListKindView.archived,
          rows: [sessionRow('a', archived: true)],
        ),
      );
      await openSheet(tester);
      await tapInSheet(tester, 'Unarchive');
      expect(sent, [
        const GuiIntentRenameOpen(sessionId: 'a'),
        const GuiIntentAct(
          action: ActionIntentSetSessionArchived(
            sessionId: 'a',
            archived: true,
          ),
        ),
        const GuiIntentAct(
          action: ActionIntentSetSessionArchived(
            sessionId: 'a',
            archived: false,
          ),
        ),
      ]);
    });

    testWidgets('delete from the sheet asks first, then deletes', (
      tester,
    ) async {
      await phone(
        tester,
        list(
          kind: ListKindView.archived,
          rows: [sessionRow('a', archived: true, deletable: true)],
        ),
      );
      await openSheet(tester);
      final delete = tester.widget<Text>(
        find.descendant(
          of: find.byType(BottomSheet),
          matching: find.text('Delete from this device'),
        ),
      );
      expect(
        delete.style?.color,
        Theme.of(tester.element(find.byType(BottomSheet))).colorScheme.error,
      );
      await tapInSheet(tester, 'Delete from this device');
      expect(find.byType(BottomSheet), findsNothing);
      expect(find.textContaining('It stays on the server'), findsOneWidget);
      await tester.tap(find.text('Delete'));
      await tester.pumpAndSettle();
      expect(sent, [
        const GuiIntentAct(
          action: ActionIntentDeleteSessionLocally(sessionId: 'a'),
        ),
      ]);
    });

    testWidgets('remove worktree from the sheet asks first, then removes', (
      tester,
    ) async {
      await phone(
        tester,
        list(
          kind: ListKindView.archived,
          rows: [
            sessionRow('a', archived: true, removable: true, branch: 'feat-x'),
          ],
        ),
      );
      await openSheet(tester);
      final remove = tester.widget<Text>(
        find.descendant(
          of: find.byType(BottomSheet),
          matching: find.text('Remove worktree'),
        ),
      );
      expect(
        remove.style?.color,
        Theme.of(tester.element(find.byType(BottomSheet))).colorScheme.error,
      );
      await tapInSheet(tester, 'Remove worktree');
      expect(find.byType(BottomSheet), findsNothing);
      expect(find.textContaining('not merged'), findsOneWidget);
      await tester.tap(find.text('Remove'));
      await tester.pumpAndSettle();
      expect(sent, [
        const GuiIntentAct(action: ActionIntentRemoveWorktree(sessionId: 'a')),
      ]);
    });

    testWidgets('a blocked delete from the sheet says why', (tester) async {
      const why = 'cannot delete title a yet: answer or cancel first';
      await phone(
        tester,
        list(
          kind: ListKindView.archived,
          rows: [sessionRow('a', archived: true, blocked: why)],
        ),
      );
      await openSheet(tester);
      await tapInSheet(tester, 'Delete from this device');
      expect(find.text(why), findsOneWidget);
      await tester.tap(find.text('OK'));
      await tester.pumpAndSettle();
      expect(sent, isEmpty);
    });
  });
}
