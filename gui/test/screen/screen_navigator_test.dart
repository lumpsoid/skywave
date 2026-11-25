import 'package:material_ui/material_ui.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/approvals/approvals_page.dart';
import 'package:skywave_gui/boot/startup_page.dart';
import 'package:skywave_gui/link/link_banner.dart';
import 'package:skywave_gui/pairing/pairing_page.dart';
import 'package:skywave_gui/screen/screen_holder.dart';
import 'package:skywave_gui/screen/repair_page.dart';
import 'package:skywave_gui/screen/screen_navigator.dart';
import 'package:skywave_gui/session/session_page.dart';
import 'package:skywave_gui/sessions/session_list_page.dart';
import 'package:skywave_gui/settings/notifications_page.dart';
import 'package:skywave_gui/settings/settings_page.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

import '../fakes.dart';

/// Settings with every notification kind on.
ScreenUpdate settingsScreen({bool canGoBack = true}) => ScreenUpdate(
  screen: const ScreenViewSettings(
    blobPolicy: BlobPolicyView.onDemand,
    blobPolicyLabel: '',
    server: 'srv:7443',
    device: 'dev-1',
    notifications: NotificationsView(
      all: true,
      approvals: true,
      idle: true,
      shell: true,
    ),
    safetyRules: [],
    defaultModes: [],
    defaultModeLabel: "the agent's own",
    defaultSafetyCheck: false,
    version: '0.1.0 (abc1234)',
  ),
  canGoBack: canGoBack,
  repair: null,
);

void main() {
  test('the holder names the page of the screen it holds', () {
    final holder = ScreenHolder();
    expect(holder.kind, isNull);
    holder.apply(pairingScreen());
    expect(holder.kind, ScreenKind.pairing);
    holder.apply(sessionScreen());
    expect(holder.kind, ScreenKind.session);
  });

  testWidgets('the navigator follows ScreenUpdate', (tester) async {
    final holders = fakeHolders();
    final sent = <GuiIntent>[];
    await tester.pumpWidget(
      MaterialApp(
        home: ScreenNavigator(holders: holders, send: sent.add),
      ),
    );
    expect(find.byType(StartupPage), findsOneWidget);
    expect(find.byType(CircularProgressIndicator), findsNothing);

    holders.screen.apply(pairingScreen());
    await tester.pumpAndSettle();
    expect(find.byType(PairingPage), findsOneWidget);

    holders.screen.apply(listScreen());
    await tester.pumpAndSettle();
    expect(find.byType(PairingPage), findsNothing);
    expect(find.byType(SessionListPage), findsOneWidget);
    expect(find.text('No sessions yet. Start one.'), findsOneWidget);

    holders.screen.apply(sessionScreen());
    holders.timeline.apply(
      TimelineUpdateSnapshot(sessionId: 's1', items: [row('s:1', 'hello')]),
    );
    await tester.pumpAndSettle();
    expect(find.byType(SessionPage), findsOneWidget);
    expect(find.text('hello'), findsOneWidget);
    // The session view reports that it shows the newest row.
    expect(sent, [const GuiIntentFollowingChanged(following: true)]);
  });

  testWidgets('the approvals screen shows its page, not the startup page', (
    tester,
  ) async {
    final holders = fakeHolders();
    await tester.pumpWidget(
      MaterialApp(
        home: ScreenNavigator(holders: holders, send: (_) {}),
      ),
    );
    holders.screen.apply(
      const ScreenUpdate(
        screen: ScreenViewApprovals(items: []),
        canGoBack: false,
        repair: null,
      ),
    );
    await tester.pumpAndSettle();
    expect(find.byType(ApprovalsPage), findsOneWidget);
    expect(find.byType(StartupPage), findsNothing);
  });

  testWidgets('the screen leaves the top inset to the banner while it shows', (
    tester,
  ) async {
    final holders = fakeHolders();
    await tester.pumpWidget(
      MaterialApp(
        home: MediaQuery(
          data: const MediaQueryData(padding: EdgeInsets.only(top: 24)),
          child: ScreenNavigator(holders: holders, send: (_) {}),
        ),
      ),
    );
    holders.screen.apply(listScreen());
    LinkUpdate online({String? loggedOut}) => LinkUpdate(
      state: LinkState.online,
      label: '',
      connectionLabel: '',
      canRetryNow: false,
      rejected: false,
      canResetReplica: false,
      queued: 0,
      approvalsElsewhere: 0,
      loggedOutLabel: loggedOut,
    );
    double screenTop() =>
        MediaQuery.paddingOf(tester.element(find.byType(SessionListPage))).top;

    holders.link.apply(online());
    await tester.pumpAndSettle();
    expect(find.byKey(const Key('link-banner')), findsNothing);
    expect(screenTop(), 24, reason: 'no banner: the screen takes the inset');

    holders.link.apply(
      online(loggedOut: 'agent not logged in · log in from settings'),
    );
    await tester.pumpAndSettle();
    expect(find.byKey(const Key('link-banner')), findsOneWidget);
    expect(screenTop(), 0, reason: 'the banner took it');
  });

  testWidgets('system back is Back only where Rust says it goes somewhere', (
    tester,
  ) async {
    final holders = fakeHolders();
    final sent = <GuiIntent>[];
    await tester.pumpWidget(
      MaterialApp(
        home: ScreenNavigator(holders: holders, send: sent.add),
      ),
    );
    holders.screen.apply(sessionScreen());
    await tester.pumpAndSettle();

    sent.clear();
    await tester.binding.handlePopRoute();
    await tester.pump();
    expect(sent, [const GuiIntentAct(action: ActionIntentBack())]);
  });

  testWidgets('the re-pair notice covers the screen', (tester) async {
    final holders = fakeHolders();
    final sent = <GuiIntent>[];
    await tester.pumpWidget(
      MaterialApp(
        home: ScreenNavigator(holders: holders, send: sent.add),
      ),
    );
    final list = listScreen();
    holders.screen.apply(
      ScreenUpdate(
        screen: list.screen,
        canGoBack: false,
        repair: const RepairNotice(
          title: 'This device was revoked.',
          body: 'The server no longer accepts this device.',
          canRetry: false,
        ),
      ),
    );
    await tester.pumpAndSettle();
    expect(find.byType(RepairPage), findsOneWidget);
    expect(find.text('This device was revoked.'), findsOneWidget);
    await tester.tap(find.text('Browse local data'));
    expect(sent, [const GuiIntentRepairDismiss()]);
  });

  testWidgets('a subagent\'s view is a page over its session\'s, which '
      'keeps the text typed in its composer', (tester) async {
    final holders = fakeHolders();
    final sent = <GuiIntent>[];
    await tester.pumpWidget(
      MaterialApp(
        home: ScreenNavigator(holders: holders, send: sent.add),
      ),
    );
    holders.screen.apply(sessionScreen());
    await tester.pumpAndSettle();
    final main = tester.state(find.byType(SessionPage));
    await tester.enterText(find.byType(TextField), 'half a thought');
    expect(sent.whereType<GuiIntentDraftEdited>().last.text, 'half a thought');

    // Nothing comes back from a hub: every update carries the composer the
    // session opened with, so only a page that stayed still has the text.
    holders.screen.apply(sessionScreen(subagent: subagentHeader('call-1')));
    await tester.pumpAndSettle();
    expect(find.text('Explore sync'), findsOneWidget);
    final top = tester.state(find.byType(SessionPage));
    expect(top, isNot(same(main)), reason: 'a page of its own');
    expect(
      find.byType(SessionPage, skipOffstage: false),
      findsNWidgets(2),
      reason: 'the session page stays under it',
    );

    // Another subagent is another page.
    holders.screen.apply(sessionScreen(subagent: subagentHeader('call-2')));
    await tester.pumpAndSettle();
    expect(tester.state(find.byType(SessionPage)), isNot(same(top)));

    sent.clear();
    holders.screen.apply(sessionScreen());
    await tester.pumpAndSettle();
    expect(tester.state(find.byType(SessionPage)), same(main));
    expect(find.byType(SessionPage, skipOffstage: false), findsOneWidget);
    expect(find.text('half a thought'), findsOneWidget);
    // The page under sent nothing while it was under; shown again, it says
    // whether it follows.
    expect(sent, [const GuiIntentFollowingChanged(following: true)]);
  });

  testWidgets('notifications open over settings; back closes them, not Back', (
    tester,
  ) async {
    final holders = fakeHolders();
    final sent = <GuiIntent>[];
    await tester.pumpWidget(
      MaterialApp(
        home: ScreenNavigator(holders: holders, send: sent.add),
      ),
    );
    holders.screen.apply(settingsScreen());
    await tester.pumpAndSettle();
    expect(find.byType(NotificationsPage), findsNothing);

    await tester.tap(find.byKey(const Key('notifications')));
    await tester.pumpAndSettle();
    expect(find.byType(NotificationsPage), findsOneWidget);
    expect(
      find.byType(SettingsPage, skipOffstage: false),
      findsOneWidget,
      reason: 'settings stays under it',
    );

    // System back closes the page: Settings stays, and Back is not sent.
    await tester.binding.handlePopRoute();
    await tester.pumpAndSettle();
    expect(find.byType(NotificationsPage), findsNothing);
    expect(find.byType(SettingsPage), findsOneWidget);
    expect(sent, isEmpty);

    // With the page closed, system back is Back again.
    await tester.binding.handlePopRoute();
    await tester.pump();
    expect(sent, [const GuiIntentAct(action: ActionIntentBack())]);
  });

  testWidgets('the back button of the notifications page closes it', (
    tester,
  ) async {
    final holders = fakeHolders();
    final sent = <GuiIntent>[];
    await tester.pumpWidget(
      MaterialApp(
        home: ScreenNavigator(holders: holders, send: sent.add),
      ),
    );
    holders.screen.apply(settingsScreen());
    await tester.pumpAndSettle();
    await tester.tap(find.byKey(const Key('notifications')));
    await tester.pumpAndSettle();

    await tester.tap(find.byType(BackButton));
    await tester.pumpAndSettle();
    expect(find.byType(NotificationsPage), findsNothing);
    expect(find.byType(SettingsPage), findsOneWidget);
    expect(sent, isEmpty);
  });

  testWidgets(
    'system back closes the notifications page where Settings cannot go back',
    (tester) async {
      final holders = fakeHolders();
      final sent = <GuiIntent>[];
      await tester.pumpWidget(
        MaterialApp(
          home: ScreenNavigator(holders: holders, send: sent.add),
        ),
      );
      holders.screen.apply(settingsScreen(canGoBack: false));
      await tester.pumpAndSettle();
      await tester.tap(find.byKey(const Key('notifications')));
      await tester.pumpAndSettle();
      expect(find.byType(NotificationsPage), findsOneWidget);

      await tester.binding.handlePopRoute();
      await tester.pumpAndSettle();
      expect(find.byType(NotificationsPage), findsNothing);
      expect(find.byType(SettingsPage), findsOneWidget);
      expect(sent, isEmpty);
    },
  );

  testWidgets('leaving settings closes the notifications page', (tester) async {
    final holders = fakeHolders();
    await tester.pumpWidget(
      MaterialApp(
        home: ScreenNavigator(holders: holders, send: (_) {}),
      ),
    );
    holders.screen.apply(settingsScreen());
    await tester.pumpAndSettle();
    await tester.tap(find.byKey(const Key('notifications')));
    await tester.pumpAndSettle();
    expect(find.byType(NotificationsPage), findsOneWidget);

    // Settings sent again, as after a switch changes, keeps the page open.
    holders.screen.apply(settingsScreen());
    await tester.pumpAndSettle();
    expect(find.byType(NotificationsPage), findsOneWidget);

    holders.screen.apply(listScreen());
    await tester.pumpAndSettle();
    expect(find.byType(SessionListPage), findsOneWidget);

    // Reopening Settings shows Settings itself.
    holders.screen.apply(settingsScreen());
    await tester.pumpAndSettle();
    expect(find.byType(NotificationsPage), findsNothing);
    expect(find.byType(SettingsPage), findsOneWidget);
  });

  testWidgets('Escape is Back where Rust says it goes somewhere', (
    tester,
  ) async {
    final holders = fakeHolders();
    final sent = <GuiIntent>[];
    await tester.pumpWidget(
      MaterialApp(
        home: LinkShortcuts(
          send: sent.add,
          child: ScreenNavigator(holders: holders, send: sent.add),
        ),
      ),
    );
    holders.screen.apply(settingsScreen(canGoBack: true));
    await tester.pumpAndSettle();

    sent.clear();
    await tester.sendKeyEvent(LogicalKeyboardKey.escape);
    await tester.pump();
    expect(sent, [const GuiIntentAct(action: ActionIntentBack())]);
  });

  testWidgets('a held Escape sends one Back', (tester) async {
    final holders = fakeHolders();
    final sent = <GuiIntent>[];
    await tester.pumpWidget(
      MaterialApp(
        home: LinkShortcuts(
          send: sent.add,
          child: ScreenNavigator(holders: holders, send: sent.add),
        ),
      ),
    );
    holders.screen.apply(settingsScreen(canGoBack: true));
    await tester.pumpAndSettle();

    sent.clear();
    await tester.sendKeyDownEvent(LogicalKeyboardKey.escape);
    await tester.sendKeyRepeatEvent(LogicalKeyboardKey.escape);
    await tester.sendKeyRepeatEvent(LogicalKeyboardKey.escape);
    await tester.sendKeyUpEvent(LogicalKeyboardKey.escape);
    await tester.pump();
    expect(sent, [const GuiIntentAct(action: ActionIntentBack())]);
  });

  testWidgets('Escape does nothing where back would leave the app', (
    tester,
  ) async {
    final holders = fakeHolders();
    final sent = <GuiIntent>[];
    await tester.pumpWidget(
      MaterialApp(
        home: LinkShortcuts(
          send: sent.add,
          child: ScreenNavigator(holders: holders, send: sent.add),
        ),
      ),
    );
    holders.screen.apply(settingsScreen(canGoBack: false));
    await tester.pumpAndSettle();

    await tester.sendKeyEvent(LogicalKeyboardKey.escape);
    await tester.pumpAndSettle();
    expect(sent, isEmpty);
    expect(find.byType(SettingsPage), findsOneWidget);
  });

  testWidgets('Escape closes the notifications page and sends nothing', (
    tester,
  ) async {
    final holders = fakeHolders();
    final sent = <GuiIntent>[];
    await tester.pumpWidget(
      MaterialApp(
        home: LinkShortcuts(
          send: sent.add,
          child: ScreenNavigator(holders: holders, send: sent.add),
        ),
      ),
    );
    holders.screen.apply(settingsScreen());
    await tester.pumpAndSettle();
    await tester.tap(find.byKey(const Key('notifications')));
    await tester.pumpAndSettle();
    expect(find.byType(NotificationsPage), findsOneWidget);

    sent.clear();
    await tester.sendKeyEvent(LogicalKeyboardKey.escape);
    await tester.pumpAndSettle();
    expect(holders.notificationsOpen.value, isFalse);
    expect(find.byType(NotificationsPage), findsNothing);
    expect(find.byType(SettingsPage), findsOneWidget);
    expect(sent, isEmpty);
  });
}
