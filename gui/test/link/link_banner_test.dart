import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/link/link_banner.dart';
import 'package:skywave_gui/link/link_holder.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

LinkUpdate link(
  LinkState state,
  String label, {
  bool canRetryNow = false,
  int elsewhere = 0,
  String? elsewhereLabel,
  String? loginLabel,
}) => LinkUpdate(
  state: state,
  label: label,
  connectionLabel: '',
  canRetryNow: canRetryNow,
  rejected:
      state == LinkState.repairNeeded ||
      state == LinkState.protocolMismatch ||
      state == LinkState.historyLost,
  canResetReplica: state == LinkState.historyLost,
  queued: 0,
  approvalsElsewhere: elsewhere,
  elsewhereLabel: elsewhereLabel,
  loginLabel: loginLabel,
);

void main() {
  late LinkHolder holder;
  late List<GuiIntent> sent;

  Future<void> pump(WidgetTester tester) async {
    holder = LinkHolder();
    sent = [];
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: LinkShortcuts(
            send: sent.add,
            child: LinkBanner(holder: holder, send: sent.add),
          ),
        ),
      ),
    );
  }

  testWidgets('the login warning shows even while online with nothing else', (
    tester,
  ) async {
    const warning = 'agent login expires in 2 days · log in again on the server';
    await pump(tester);
    holder.apply(link(LinkState.online, ''));
    await tester.pump();
    expect(find.byKey(const Key('login-label')), findsNothing);

    holder.apply(link(LinkState.online, '', loginLabel: warning));
    await tester.pumpAndSettle();
    expect(find.text(warning), findsOneWidget);
    expect(find.byKey(const Key('link-label')), findsNothing);

    holder.apply(
      link(LinkState.reconnecting, 'offline · timed out', loginLabel: warning),
    );
    await tester.pumpAndSettle();
    expect(find.text('offline · timed out'), findsOneWidget);
    expect(find.text(warning), findsOneWidget);
  });

  testWidgets('the banner shows each state\'s label, and nothing unpaired', (
    tester,
  ) async {
    await pump(tester);
    holder.apply(link(LinkState.unpaired, ''));
    await tester.pump();
    expect(find.byKey(const Key('link-label')), findsNothing);

    for (final (state, label) in [
      (LinkState.connecting, 'connecting…'),
      (LinkState.online, 'online · 2 queued'),
      (LinkState.catchingUp, 'syncing · 4/9'),
      (
        LinkState.repairNeeded,
        'server identity changed · re-pair to reconnect',
      ),
      (
        LinkState.protocolMismatch,
        'protocol mismatch · client v2, server v3 · update, then retry',
      ),
    ]) {
      holder.apply(link(state, label));
      await tester.pump();
      expect(find.text(label), findsOneWidget, reason: '$state');
    }
    expect(find.text('Retry now'), findsNothing);

    holder.apply(
      link(
        LinkState.reconnecting,
        'offline · server unreachable\nattempt 3 · retry in 07s',
      ),
    );
    await tester.pump();
    expect(find.text('offline · server unreachable'), findsOneWidget);
    expect(find.text('attempt 3 · retry in 07s'), findsOneWidget);
  });

  testWidgets('a phone banner keeps its height as the countdown ticks', (
    tester,
  ) async {
    tester.view.physicalSize = const Size(320, 640);
    tester.view.devicePixelRatio = 1;
    addTearDown(tester.view.reset);
    await pump(tester);
    final heights = <double>[];
    for (final retry in ['retry in 10s', 'retry in 07s', 'retrying…']) {
      holder.apply(
        link(
          LinkState.reconnecting,
          'offline · server unreachable · 4 queued\nattempt 3 · $retry',
          canRetryNow: true,
        ),
      );
      await tester.pumpAndSettle();
      heights.add(tester.getSize(find.byType(LinkBanner)).height);
    }
    expect(heights.toSet(), hasLength(1), reason: '$heights');
  });

  testWidgets('a wrapped approvals button is no wider than its words', (
    tester,
  ) async {
    tester.view.physicalSize = const Size(320, 640);
    tester.view.devicePixelRatio = 1;
    addTearDown(tester.view.reset);
    await pump(tester);
    holder.apply(
      link(
        LinkState.reconnecting,
        'offline\nattempt 3 · retry in 07s',
        canRetryNow: true,
        elsewhere: 12,
        elsewhereLabel: '12 approvals elsewhere',
      ),
    );
    await tester.pumpAndSettle();
    final words = find.text('12 approvals elsewhere');
    final button = find.ancestor(of: words, matching: find.byType(TextButton));
    final bar = find.byType(OverflowBar);
    expect(tester.getSize(words).height, greaterThan(20), reason: 'wraps');
    expect(
      tester.getSize(button).width,
      lessThan(tester.getSize(bar).width - 8),
    );
  });

  testWidgets('the banner slides away once online with nothing waiting', (
    tester,
  ) async {
    await pump(tester);
    holder.apply(link(LinkState.connecting, 'connecting…'));
    await tester.pumpAndSettle();
    expect(find.text('connecting…'), findsOneWidget);

    // Rust says nothing while online with nothing queued or elsewhere.
    holder.apply(link(LinkState.online, ''));
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 100));
    expect(find.text('connecting…'), findsOneWidget, reason: 'still sliding');
    await tester.pumpAndSettle();
    expect(find.byKey(const Key('link-label')), findsNothing);
  });

  testWidgets('the banner comes and goes at once with reduced motion', (
    tester,
  ) async {
    tester.platformDispatcher.accessibilityFeaturesTestValue =
        const FakeAccessibilityFeatures(disableAnimations: true);
    addTearDown(tester.platformDispatcher.clearAccessibilityFeaturesTestValue);
    await pump(tester);
    holder.apply(link(LinkState.connecting, 'connecting…'));
    await tester.pump();
    expect(find.text('connecting…'), findsOneWidget);
    holder.apply(link(LinkState.online, ''));
    await tester.pump();
    expect(find.byKey(const Key('link-label')), findsNothing);
  });

  testWidgets('Retry now and Ctrl+R send RetryNow', (tester) async {
    await pump(tester);
    holder.apply(
      link(LinkState.reconnecting, 'offline · retry in 07s', canRetryNow: true),
    );
    await tester.pumpAndSettle();
    await tester.tap(find.text('Retry now'));
    await tester.sendKeyDownEvent(LogicalKeyboardKey.controlLeft);
    await tester.sendKeyEvent(LogicalKeyboardKey.keyR);
    await tester.sendKeyUpEvent(LogicalKeyboardKey.controlLeft);
    expect(sent, [
      const GuiIntentAct(action: ActionIntentRetryNow()),
      const GuiIntentAct(action: ActionIntentRetryNow()),
    ]);
  });

  testWidgets('Reset local data asks first, then sends ResetReplica', (
    tester,
  ) async {
    await pump(tester);
    holder.apply(link(LinkState.reconnecting, 'offline', canRetryNow: true));
    await tester.pumpAndSettle();
    expect(find.text('Reset local data'), findsNothing);

    holder.apply(link(LinkState.historyLost, 'server lost history'));
    await tester.pumpAndSettle();
    expect(find.text('Retry now'), findsNothing);
    await tester.tap(find.text('Reset local data'));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Cancel'));
    await tester.pumpAndSettle();
    expect(sent, isEmpty);

    await tester.tap(find.text('Reset local data'));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Reset'));
    await tester.pumpAndSettle();
    expect(sent, [const GuiIntentAct(action: ActionIntentResetReplica())]);
  });

  testWidgets('approvals elsewhere and Ctrl+G open the next one', (
    tester,
  ) async {
    await pump(tester);
    holder.apply(
      link(
        LinkState.online,
        'online',
        elsewhere: 2,
        elsewhereLabel: '2 approvals elsewhere',
      ),
    );
    await tester.pumpAndSettle();
    await tester.tap(find.text('2 approvals elsewhere'));
    await tester.sendKeyDownEvent(LogicalKeyboardKey.controlLeft);
    await tester.sendKeyEvent(LogicalKeyboardKey.keyG);
    await tester.sendKeyUpEvent(LogicalKeyboardKey.controlLeft);
    expect(sent, [
      const GuiIntentAct(action: ActionIntentOpenNextApproval()),
      const GuiIntentAct(action: ActionIntentOpenNextApproval()),
    ]);
  });

  testWidgets('both buttons fit a phone-width banner', (tester) async {
    tester.view.physicalSize = const Size(320, 640);
    tester.view.devicePixelRatio = 1;
    addTearDown(tester.view.reset);
    await pump(tester);
    holder.apply(
      link(
        LinkState.reconnecting,
        'offline · 4 queued\nattempt 3 · retry in 07s',
        canRetryNow: true,
        elsewhere: 12,
        elsewhereLabel: '12 approvals elsewhere',
      ),
    );
    await tester.pump();
    expect(tester.takeException(), isNull);
    expect(find.text('Retry now'), findsOneWidget);
    expect(find.text('12 approvals elsewhere'), findsOneWidget);
  });
}
