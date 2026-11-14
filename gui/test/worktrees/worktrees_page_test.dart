import 'package:material_ui/material_ui.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';
import 'package:skywave_gui/worktrees/worktrees_page.dart';

const _screen = ScreenViewWorktrees(
  statusLabel: 'Scanned 2 repositories',
  repos: [
    WorktreeRepoView(
      repo: '/srv/gone',
      note: 'Not a repository any more.',
      worktrees: [],
    ),
    WorktreeRepoView(
      repo: '/srv/app',
      worktrees: [
        WorktreeView(
          path: '/srv/app-fix',
          branch: 'fix-login',
          tags: ['uncommitted changes', 'not merged'],
          sessions: [
            WorktreeSessionView(
              sessionId: 'a',
              title: 'Fix login',
              archived: false,
              removable: true,
              worktreeRemoved: false,
            ),
            WorktreeSessionView(
              sessionId: 'b',
              title: 'Old spike',
              archived: true,
              removable: false,
              worktreeRemoved: false,
              removalLabel: 'queued',
            ),
          ],
        ),
      ],
    ),
  ],
);

void main() {
  testWidgets('a worktree shows its repo, branch, tags and sessions', (
    tester,
  ) async {
    await tester.pumpWidget(
      MaterialApp(
        home: WorktreesPage(screen: _screen, send: (_) {}),
      ),
    );
    for (final text in [
      'Scanned 2 repositories',
      '/srv/gone',
      'Not a repository any more.',
      '/srv/app',
      '/srv/app-fix',
      'fix-login',
      'uncommitted changes',
      'not merged',
      'Fix login',
      'Old spike',
      'archived',
      'queued',
    ]) {
      expect(find.text(text), findsOneWidget, reason: text);
    }
    // Only the removable session has the button.
    expect(find.byKey(const Key('remove-worktree-a')), findsOneWidget);
    expect(find.byKey(const Key('remove-worktree-b')), findsNothing);
    expect(find.byTooltip('Remove worktree'), findsOneWidget);
  });

  testWidgets('refresh then back send their intents', (tester) async {
    final sent = <GuiIntent>[];
    await tester.pumpWidget(
      MaterialApp(
        home: WorktreesPage(screen: _screen, canGoBack: true, send: sent.add),
      ),
    );
    await tester.tap(find.byTooltip('Refresh'));
    await tester.tap(find.byType(BackButton));
    expect(sent, [
      const GuiIntentAct(action: ActionIntentRefreshWorktrees()),
      const GuiIntentAct(action: ActionIntentBack()),
    ]);
  });

  testWidgets('remove worktree asks first, then removes', (tester) async {
    final sent = <GuiIntent>[];
    await tester.pumpWidget(
      MaterialApp(
        home: WorktreesPage(screen: _screen, send: sent.add),
      ),
    );
    await tester.tap(find.byKey(const Key('remove-worktree-a')));
    await tester.pumpAndSettle();
    expect(find.text('Remove worktree?'), findsOneWidget);
    expect(find.textContaining('delete its branch fix-login?'), findsOneWidget);
    await tester.tap(find.text('Keep'));
    await tester.pumpAndSettle();
    expect(sent, isEmpty);

    await tester.tap(find.byKey(const Key('remove-worktree-a')));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Remove'));
    await tester.pumpAndSettle();
    expect(sent, [
      const GuiIntentAct(action: ActionIntentRemoveWorktree(sessionId: 'a')),
    ]);
  });
}
