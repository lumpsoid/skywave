import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/link/link_banner.dart';
import 'package:skywave_gui/link/link_holder.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

LinkUpdate link(
  LinkState state,
  String label, {
  String? statusLabel,
  bool canRetryNow = false,
  int elsewhere = 0,
  String? elsewhereLabel,
  String? loginLabel,
  String? loggedOutLabel,
}) => LinkUpdate(
  state: state,
  label: label,
  statusLabel: statusLabel,
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
  loggedOutLabel: loggedOutLabel,
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
    const warning =
        'agent login expires in 2 days · log in again on the server';
    await pump(tester);
    holder.apply(link(LinkState.online, ''));
    await tester.pump();
    expect(find.byKey(const Key('login-label')), findsNothing);

    holder.apply(link(LinkState.online, '', loginLabel: warning));
    await tester.pumpAndSettle();
    expect(find.text(warning), findsOneWidget);
    expect(find.byKey(const Key('link-label')), findsNothing);

    holder.apply(
      link(
        LinkState.repairNeeded,
        'device revoked · re-pair to reconnect',
        loginLabel: warning,
      ),
    );
    await tester.pumpAndSettle();
    expect(find.text('device revoked · re-pair to reconnect'), findsOneWidget);
    expect(find.text(warning), findsOneWidget);
  });

  testWidgets('a dismissed expiry warning stays away until it changes', (
    tester,
  ) async {
    const twoDays =
        'agent login expires in 2 days · log in again on the server';
    const oneDay = 'agent login expires in 1 day · log in again on the server';
    await pump(tester);
    holder.apply(link(LinkState.online, '', loginLabel: twoDays));
    await tester.pumpAndSettle();
    await tester.tap(find.byKey(const Key('login-dismiss')));
    await tester.pumpAndSettle();
    expect(find.byKey(const Key('link-banner')), findsNothing);
    expect(holder.bannerShown, isFalse);

    // Rust sends the same warning again on every link change.
    holder.apply(link(LinkState.connecting, '', loginLabel: twoDays));
    await tester.pumpAndSettle();
    expect(find.byKey(const Key('link-banner')), findsNothing);

    holder.apply(link(LinkState.online, '', loginLabel: oneDay));
    await tester.pumpAndSettle();
    expect(find.text(oneDay), findsOneWidget);
  });

  testWidgets('the logged-out warning shows while online, ahead of expiry', (
    tester,
  ) async {
    const loggedOut = 'agent not logged in · log in from settings';
    const expiry = 'agent login expires in 2 days · log in again from settings';
    await pump(tester);
    holder.apply(link(LinkState.online, ''));
    await tester.pumpAndSettle();
    expect(find.byKey(const Key('link-banner')), findsNothing);

    holder.apply(link(LinkState.online, '', loggedOutLabel: loggedOut));
    await tester.pumpAndSettle();
    expect(find.byKey(const Key('link-banner')), findsOneWidget);
    expect(find.text(loggedOut), findsOneWidget);
    expect(find.byKey(const Key('link-label')), findsNothing);
    expect(
      find.byKey(const Key('login-dismiss')),
      findsNothing,
      reason: 'nothing works until the agent logs in again',
    );

    // Rust never sends both, but the order holds if it did.
    holder.apply(
      link(LinkState.online, '', loggedOutLabel: loggedOut, loginLabel: expiry),
    );
    await tester.pumpAndSettle();
    expect(
      tester.getTopLeft(find.text(loggedOut)).dy,
      lessThan(tester.getTopLeft(find.text(expiry)).dy),
    );
  });

  testWidgets('the banner shows only a link that stopped', (tester) async {
    await pump(tester);
    for (final (state, status) in [
      (LinkState.unpaired, null),
      (LinkState.connecting, 'Connecting…'),
      (LinkState.catchingUp, 'Syncing · 4/9'),
      (LinkState.reconnecting, 'Offline · retry in 07s · server unreachable'),
    ]) {
      holder.apply(
        link(
          state,
          '',
          statusLabel: status,
          canRetryNow: state == LinkState.reconnecting,
          elsewhere: 2,
          elsewhereLabel: '2 approvals elsewhere',
        ),
      );
      await tester.pumpAndSettle();
      expect(
        find.byKey(const Key('link-banner')),
        findsNothing,
        reason: '$state',
      );
    }

    for (final (state, label) in [
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
      await tester.pumpAndSettle();
      expect(find.text(label), findsOneWidget, reason: '$state');
    }
    expect(find.text('Retry now'), findsNothing);
  });

  testWidgets('the banner slides away once the link is back', (tester) async {
    await pump(tester);
    const revoked = 'device revoked · re-pair to reconnect';
    holder.apply(link(LinkState.repairNeeded, revoked));
    await tester.pumpAndSettle();
    expect(find.text(revoked), findsOneWidget);

    holder.apply(link(LinkState.online, ''));
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 100));
    expect(find.text(revoked), findsOneWidget, reason: 'still sliding');
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
    const revoked = 'device revoked · re-pair to reconnect';
    holder.apply(link(LinkState.repairNeeded, revoked));
    await tester.pump();
    expect(find.text(revoked), findsOneWidget);
    holder.apply(link(LinkState.online, ''));
    await tester.pump();
    expect(find.byKey(const Key('link-label')), findsNothing);
  });

  testWidgets('Retry now and Ctrl+R send RetryNow', (tester) async {
    await pump(tester);
    holder.apply(
      link(
        LinkState.protocolMismatch,
        'protocol mismatch · client v2, server v3 · update, then retry',
        canRetryNow: true,
      ),
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
}
