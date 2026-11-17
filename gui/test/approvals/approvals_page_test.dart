import 'package:material_ui/material_ui.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/approvals/approvals_page.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// A shell approval that can be answered here.
ApprovalPanel panelOf(
  String approvalId, {
  List<List<String>> ruleCandidates = const [],
}) => ApprovalPanel(
  approvalId: approvalId,
  kind: ToolKindView.shell,
  tool: 'Bash',
  title: r'$ rm -rf build',
  canAnswer: true,
  suggestions: const [],
  ruleCandidates: ruleCandidates,
  alwaysPicks: false,
);

GuiIntent resolve(String approvalId, DecisionView decision) => GuiIntentAct(
  action: ActionIntentResolveApproval(
    approvalId: approvalId,
    decision: decision,
    rule: null,
  ),
);

Future<void> pumpApprovals(
  WidgetTester tester,
  List<ApprovalsItem> items,
  SendIntent send,
) => tester.pumpWidget(
  MaterialApp(
    home: ApprovalsPage(
      screen: ScreenViewApprovals(items: items),
      canGoBack: true,
      send: send,
    ),
  ),
);

void main() {
  testWidgets('each waiting approval answers in place', (tester) async {
    final sent = <GuiIntent>[];
    await pumpApprovals(tester, [
      ApprovalsItem(
        sessionId: 's1',
        sessionTitle: 'fix the build',
        approvalId: 'a1',
        panel: panelOf('a1'),
      ),
      const ApprovalsItem(
        sessionId: 's2',
        sessionTitle: 'docs',
        approvalId: 'q2',
        note: 'a question · answer it in its session',
      ),
    ], sent.add);
    expect(find.text('fix the build'), findsOneWidget);
    expect(find.text('docs'), findsOneWidget);
    expect(find.text('a question · answer it in its session'), findsOneWidget);

    await tester.tap(
      find.descendant(
        of: find.byKey(const ValueKey('panel-a1')),
        matching: find.text('Approve'),
      ),
    );
    await tester.tap(find.byKey(const Key('approvals-open-q2')));
    await tester.tap(find.byType(BackButton));
    expect(sent, [
      resolve('a1', DecisionView.approve),
      const GuiIntentAct(action: ActionIntentOpenSession(sessionId: 's2')),
      const GuiIntentAct(action: ActionIntentBack()),
    ]);
  });

  testWidgets('trust from the list sets the rule in the approval\'s session '
      'and approves', (tester) async {
    final sent = <GuiIntent>[];
    await pumpApprovals(tester, [
      ApprovalsItem(
        sessionId: 's7',
        sessionTitle: 'lint · subagent',
        approvalId: 'a7',
        panel: panelOf(
          'a7',
          ruleCandidates: const [
            ['git', 'status'],
          ],
        ),
      ),
    ], sent.add);
    await tester.tap(find.byKey(const Key('approval-trust')));
    await tester.pumpAndSettle();
    await tester.tap(find.byKey(const Key('trust-confirm')));
    await tester.pumpAndSettle();
    expect(sent, [
      const GuiIntentAct(
        action: ActionIntentSetSafetyRule(
          sessionId: 's7',
          words: ['git', 'status'],
          on: true,
        ),
      ),
      resolve('a7', DecisionView.approve),
    ]);
  });

  testWidgets('nothing waiting says so', (tester) async {
    await pumpApprovals(tester, const [], (_) {});
    expect(find.byKey(const Key('approvals-empty')), findsOneWidget);
    expect(find.text('Nothing waits for you'), findsOneWidget);
  });

  testWidgets('two panels lay out at a phone width', (tester) async {
    tester.view.physicalSize = const Size(360 * 3, 760 * 3);
    tester.view.devicePixelRatio = 3;
    addTearDown(tester.view.reset);
    await pumpApprovals(tester, [
      ApprovalsItem(
        sessionId: 's1',
        sessionTitle: 'one',
        approvalId: 'a1',
        panel: panelOf('a1'),
      ),
      ApprovalsItem(
        sessionId: 's2',
        sessionTitle: 'two',
        approvalId: 'a2',
        panel: panelOf('a2'),
      ),
    ], (_) {});
    expect(tester.takeException(), isNull);
    expect(find.byKey(const ValueKey('panel-a1')), findsOneWidget);
    expect(find.byKey(const ValueKey('panel-a2')), findsOneWidget);
  });
}
