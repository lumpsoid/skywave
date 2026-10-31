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
}) => LinkUpdate(
  state: state,
  label: label,
  canRetryNow: canRetryNow,
  rejected: state == LinkState.revoked || state == LinkState.protocolMismatch,
  queued: 0,
  approvalsElsewhere: elsewhere,
  elsewhereLabel: elsewhereLabel,
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

  testWidgets('the banner shows each state\'s label, and nothing unpaired', (
    tester,
  ) async {
    await pump(tester);
    holder.apply(link(LinkState.unpaired, ''));
    await tester.pump();
    expect(find.byKey(const Key('link-label')), findsNothing);

    for (final (state, label) in [
      (LinkState.connecting, 'connecting…'),
      (LinkState.online, 'online · 12 ms · 2 queued'),
      (LinkState.catchingUp, 'syncing · 4/9'),
      (LinkState.reconnecting, 'offline · retry in 07s (attempt 3)'),
      (LinkState.revoked, 'device revoked · re-pair to reconnect'),
      (
        LinkState.protocolMismatch,
        'protocol mismatch · client v2, server v3 · update to reconnect',
      ),
    ]) {
      holder.apply(link(state, label));
      await tester.pump();
      expect(find.text(label), findsOneWidget, reason: '$state');
    }
    expect(find.text('Retry now'), findsNothing);
  });

  testWidgets('Retry now and Ctrl+R send RetryNow', (tester) async {
    await pump(tester);
    holder.apply(
      link(LinkState.reconnecting, 'offline · retry in 07s', canRetryNow: true),
    );
    await tester.pump();
    await tester.tap(find.text('Retry now'));
    await tester.sendKeyDownEvent(LogicalKeyboardKey.controlLeft);
    await tester.sendKeyEvent(LogicalKeyboardKey.keyR);
    await tester.sendKeyUpEvent(LogicalKeyboardKey.controlLeft);
    expect(sent, [
      const GuiIntentAct(action: ActionIntentRetryNow()),
      const GuiIntentAct(action: ActionIntentRetryNow()),
    ]);
  });

  testWidgets('approvals elsewhere and Ctrl+G open the next one', (
    tester,
  ) async {
    await pump(tester);
    holder.apply(
      link(
        LinkState.online,
        'online · 3 ms',
        elsewhere: 2,
        elsewhereLabel: '2 approvals elsewhere',
      ),
    );
    await tester.pump();
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
        'offline · 4 queued · retry in 07s (attempt 3)',
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
