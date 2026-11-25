import 'package:material_ui/material_ui.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/link/link_holder.dart';
import 'package:skywave_gui/screen/repair_page.dart';
import 'package:skywave_gui/settings/settings_page.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// The page's one list, which the sections below the fold scroll.
Finder pageScrollable() => find
    .descendant(of: find.byType(ListView), matching: find.byType(Scrollable))
    .first;

void _ignore() {}

void main() {
  testWidgets('settings shows the device and the toggle sets the policy', (
    tester,
  ) async {
    // Tall enough that the fingerprint and agent login, under the blob
    // wording, are built.
    tester.view
      ..physicalSize = const Size(800, 900)
      ..devicePixelRatio = 1;
    addTearDown(tester.view.reset);
    final sent = <GuiIntent>[];
    await tester.pumpWidget(
      MaterialApp(
        home: SettingsPage(
          screen: const ScreenViewSettings(
            blobPolicy: BlobPolicyView.onDemand,
            blobPolicyLabel: "A tool's blob downloads when you expand it.",
            server: 'srv:7443',
            serverAddresses: null,
            device: 'dev-1',
            fingerprint: 'AB:CD',
            agentLogin: 'expires in 12 days · 2026-11-21',
            stayConnected: null,
            notifications: NotificationsView(
              all: true,
              approvals: true,
              idle: true,
              shell: true,
            ),
            safetyRules: [],
            version: '0.1.0 (abc1234)',
          ),
          link: LinkHolder(),
          canGoBack: true,
          send: sent.add,
          onNotifications: _ignore,
        ),
      ),
    );
    for (final text in [
      "A tool's blob downloads when you expand it.",
      'srv:7443',
      'dev-1',
      'AB:CD',
      'expires in 12 days · 2026-11-21',
    ]) {
      expect(find.text(text), findsOneWidget, reason: text);
    }
    expect(
      find.textContaining('workdir diffs and files always download'),
      findsOneWidget,
    );
    await tester.tap(find.text('Eager'));
    await tester.drag(find.byType(ListView), const Offset(0, -300));
    await tester.pump();
    await tester.tap(find.text('Re-pair'));
    await tester.pumpAndSettle();
    await tester.tap(find.byKey(const Key('confirm-cancel')));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Re-pair'));
    await tester.pumpAndSettle();
    await tester.tap(find.byKey(const Key('confirm-ok')));
    await tester.pumpAndSettle();
    await tester.tap(find.byType(BackButton));
    expect(sent, [
      const GuiIntentAct(
        action: ActionIntentSetBlobPolicy(policy: BlobPolicyChoice.eager),
      ),
      const GuiIntentAct(
        action: ActionIntentNavigate(route: RouteView.pairing),
      ),
      const GuiIntentAct(action: ActionIntentBack()),
    ]);
  });

  Widget settings(
    List<GuiIntent> sent, {
    String? serverAddresses,
    bool? stayConnected,
    List<SafetyRuleRow> safetyRules = const [],
    NotificationsView notifications = const NotificationsView(
      all: true,
      approvals: true,
      idle: true,
      shell: true,
    ),
    VoidCallback onNotifications = _ignore,
  }) => MaterialApp(
    home: SettingsPage(
      screen: ScreenViewSettings(
        blobPolicy: BlobPolicyView.onDemand,
        blobPolicyLabel: "A tool's blob downloads when you expand it.",
        server: serverAddresses == null ? 'not paired' : 'srv:7443',
        serverAddresses: serverAddresses,
        device: 'dev-1',
        fingerprint: null,
        agentLogin: null,
        stayConnected: stayConnected,
        notifications: notifications,
        safetyRules: safetyRules,
        version: '0.1.0 (abc1234)',
      ),
      link: LinkHolder(),
      send: sent.add,
      onNotifications: onNotifications,
    ),
  );

  testWidgets('the version shows under About', (tester) async {
    await tester.pumpWidget(settings([]));
    await tester.scrollUntilVisible(
      find.byKey(const Key('version')),
      200,
      scrollable: find.byType(Scrollable).first,
    );
    expect(find.text('0.1.0 (abc1234)'), findsOneWidget);

    await tester.scrollUntilVisible(
      find.byKey(const Key('licenses')),
      200,
      scrollable: find.byType(Scrollable).first,
    );
    await tester.ensureVisible(find.byKey(const Key('licenses')));
    await tester.pumpAndSettle();
    await tester.tap(find.byKey(const Key('licenses')));
    await tester.pumpAndSettle();
    expect(find.byType(LicensePage), findsOneWidget);
  });

  testWidgets('stay connected shows only where offered, and toggles', (
    tester,
  ) async {
    final toggle = find.byKey(const Key('stay-connected'));
    await tester.pumpWidget(settings([]));
    expect(toggle, findsNothing);
    expect(find.text('Background'), findsNothing);

    final sent = <GuiIntent>[];
    await tester.pumpWidget(settings(sent, stayConnected: false));
    expect(toggle, findsOneWidget);
    expect(find.text('Stay connected'), findsOneWidget);
    expect(tester.widget<SwitchListTile>(toggle).value, isFalse);
    await tester.tap(toggle);
    expect(sent, [const GuiIntentSetStayConnected(on: true)]);

    sent.clear();
    await tester.pumpWidget(settings(sent, stayConnected: true));
    expect(tester.widget<SwitchListTile>(toggle).value, isTrue);
    await tester.tap(toggle);
    expect(sent, [const GuiIntentSetStayConnected(on: false)]);
  });

  testWidgets(
    'the notifications tile says off, some off or on, and opens its page',
    (tester) async {
      Finder label(String text) => find.descendant(
        of: find.byKey(const Key('notifications')),
        matching: find.text(text),
      );
      await tester.pumpWidget(settings([]));
      expect(label('On'), findsOneWidget);

      await tester.pumpWidget(
        settings(
          [],
          notifications: const NotificationsView(
            all: true,
            approvals: true,
            idle: false,
            shell: true,
          ),
        ),
      );
      expect(label('Some off'), findsOneWidget);

      await tester.pumpWidget(
        settings(
          [],
          notifications: const NotificationsView(
            all: false,
            approvals: true,
            idle: true,
            shell: true,
          ),
        ),
      );
      expect(label('Off'), findsOneWidget);

      var opened = 0;
      await tester.pumpWidget(
        settings(
          [],
          onNotifications: () {
            opened++;
          },
        ),
      );
      await tester.tap(find.byKey(const Key('notifications')));
      expect(opened, 1);
    },
  );

  testWidgets('a paired server tile opens its addresses', (tester) async {
    final sent = <GuiIntent>[];
    await tester.pumpWidget(
      settings(sent, serverAddresses: '2 other addresses'),
    );
    expect(find.text('srv:7443\n2 other addresses'), findsOneWidget);
    await tester.tap(find.byKey(const Key('server-addresses')));
    expect(sent, [
      const GuiIntentAct(
        action: ActionIntentNavigate(route: RouteView.serverAddresses),
      ),
    ]);
  });

  testWidgets('the usage and storage tiles open their pages', (tester) async {
    final sent = <GuiIntent>[];
    await tester.pumpWidget(settings(sent));
    for (final key in ['usage', 'storage']) {
      await tester.scrollUntilVisible(
        find.byKey(Key(key)),
        200,
        scrollable: pageScrollable(),
      );
      await tester.ensureVisible(find.byKey(Key(key)));
      await tester.pumpAndSettle();
      await tester.tap(find.byKey(Key(key)));
    }
    expect(sent, [
      const GuiIntentAct(action: ActionIntentNavigate(route: RouteView.usage)),
      const GuiIntentAct(
        action: ActionIntentNavigate(route: RouteView.storage),
      ),
    ]);
  });

  testWidgets('an unpaired device has no address to change', (tester) async {
    await tester.pumpWidget(settings([]));
    expect(find.byKey(const Key('server-addresses')), findsNothing);
  });

  testWidgets('safety rules list with a remove button', (tester) async {
    final sent = <GuiIntent>[];
    await tester.pumpWidget(
      settings(
        sent,
        safetyRules: const [
          SafetyRuleRow(
            sessionId: 's1',
            words: ['git', 'commit'],
            label: 'git commit',
            on: true,
          ),
          SafetyRuleRow(
            sessionId: 's2',
            words: ['cargo', 'test'],
            label: 'cargo test',
            on: false,
            pending: 'adding',
          ),
        ],
      ),
    );
    await tester.scrollUntilVisible(
      find.byKey(const Key('remove-safety-rule-1')),
      200,
      scrollable: pageScrollable(),
    );
    expect(find.text('Safety rules'), findsOneWidget);
    expect(find.text('git commit'), findsOneWidget);
    expect(find.text('cargo test'), findsOneWidget);
    expect(find.text('adding'), findsOneWidget);

    await tester.tap(find.byKey(const Key('remove-safety-rule-0')));
    expect(sent, [
      const GuiIntentAct(
        action: ActionIntentSetSafetyRule(
          sessionId: 's1',
          words: ['git', 'commit'],
          on: false,
        ),
      ),
    ]);

    // A pending add has nothing logged to remove yet: its button is disabled.
    await tester.tap(find.byKey(const Key('remove-safety-rule-1')));
    expect(sent, hasLength(1));
  });

  testWidgets('a failed safety rule change can be dismissed', (tester) async {
    final sent = <GuiIntent>[];
    await tester.pumpWidget(
      settings(
        sent,
        safetyRules: const [
          SafetyRuleRow(
            sessionId: 's1',
            words: ['ls'],
            label: 'ls',
            on: false,
            pending: 'could not add: refused',
            discard: 'c9',
          ),
        ],
      ),
    );
    await tester.scrollUntilVisible(
      find.byKey(const Key('dismiss-safety-rule-0')),
      200,
      scrollable: pageScrollable(),
    );
    await tester.ensureVisible(find.byKey(const Key('dismiss-safety-rule-0')));
    await tester.pumpAndSettle();
    expect(find.byKey(const Key('remove-safety-rule-0')), findsNothing);

    await tester.tap(find.byKey(const Key('dismiss-safety-rule-0')));
    expect(sent, [
      const GuiIntentAct(action: ActionIntentDiscard(commandId: 'c9')),
    ]);
  });

  testWidgets('safety rules with the same label are both listed', (
    tester,
  ) async {
    final sent = <GuiIntent>[];
    await tester.pumpWidget(
      settings(
        sent,
        safetyRules: const [
          SafetyRuleRow(
            sessionId: 's1',
            words: ['git', 'commit', '-m', 'fix it'],
            label: 'git commit -m fix it',
            on: true,
          ),
          SafetyRuleRow(
            sessionId: 's1',
            words: ['git', 'commit', '-m', 'fix', 'it'],
            label: 'git commit -m fix it',
            on: true,
          ),
        ],
      ),
    );
    // Below the fold: the lazy list builds the rows only once scrolled to.
    await tester.scrollUntilVisible(
      find.byKey(const Key('remove-safety-rule-1')),
      200,
      scrollable: pageScrollable(),
    );
    expect(tester.takeException(), isNull);
    expect(find.text('git commit -m fix it'), findsNWidgets(2));

    await tester.tap(find.byKey(const Key('remove-safety-rule-1')));
    expect(sent, [
      const GuiIntentAct(
        action: ActionIntentSetSafetyRule(
          sessionId: 's1',
          words: ['git', 'commit', '-m', 'fix', 'it'],
          on: false,
        ),
      ),
    ]);
  });

  testWidgets('no safety rules shows how to add one', (tester) async {
    const hint =
        'Commands you trust run without asking. Add one from a shell row\'s '
        'menu.';
    await tester.pumpWidget(settings([]));
    // Below the fold, so the lazy list has not built it yet.
    await tester.scrollUntilVisible(
      find.text(hint),
      200,
      scrollable: pageScrollable(),
    );
    expect(find.text('Safety rules'), findsOneWidget);
    expect(find.text(hint), findsOneWidget);
  });

  testWidgets('the connection tile follows the link and its latency', (
    tester,
  ) async {
    final holder = LinkHolder();
    addTearDown(holder.dispose);
    LinkUpdate link(LinkState state, String connection) => LinkUpdate(
      state: state,
      label: '',
      connectionLabel: connection,
      canRetryNow: false,
      rejected: false,
      canResetReplica: false,
      queued: 0,
      approvalsElsewhere: 0,
    );
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(body: ConnectionTile(holder: holder)),
      ),
    );
    String shown() =>
        tester.widget<Text>(find.byKey(const Key('connection-label'))).data!;
    expect(shown(), 'Not known yet');

    holder.apply(link(LinkState.online, 'Connected · 12 ms'));
    await tester.pump();
    expect(shown(), 'Connected · 12 ms');
    expect(find.byIcon(Icons.link), findsOneWidget);

    holder.apply(link(LinkState.online, 'Connected · 30 ms'));
    await tester.pump();
    expect(shown(), 'Connected · 30 ms');

    holder.apply(link(LinkState.reconnecting, 'Offline · retry in 07s'));
    await tester.pump();
    expect(shown(), 'Offline · retry in 07s');
    expect(find.byIcon(Icons.link_off), findsOneWidget);
  });

  testWidgets('the re-pair notice re-pairs or browses local data', (
    tester,
  ) async {
    final sent = <GuiIntent>[];
    await tester.pumpWidget(
      MaterialApp(
        home: RepairPage(
          notice: const RepairNotice(
            title: 'This device was revoked.',
            body: 'The server no longer accepts this device.',
            canRetry: false,
          ),
          send: sent.add,
        ),
      ),
    );
    expect(find.text('Try again'), findsNothing);
    await tester.tap(find.text('Re-pair'));
    await tester.tap(find.text('Browse local data'));
    expect(sent, [
      const GuiIntentAct(
        action: ActionIntentNavigate(route: RouteView.pairing),
      ),
      const GuiIntentRepairDismiss(),
    ]);
  });

  testWidgets('a re-pair notice a server fix may clear offers a retry', (
    tester,
  ) async {
    final sent = <GuiIntent>[];
    await tester.pumpWidget(
      MaterialApp(
        home: RepairPage(
          notice: const RepairNotice(
            title: "The server's identity changed.",
            body: 'Re-pair with what `skywaved pair` prints now.',
            canRetry: true,
          ),
          send: sent.add,
        ),
      ),
    );
    expect(find.text("The server's identity changed."), findsOneWidget);
    await tester.tap(find.text('Try again'));
    expect(sent, [const GuiIntentAct(action: ActionIntentRetryNow())]);
  });
}
