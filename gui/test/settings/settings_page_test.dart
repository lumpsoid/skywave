import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/link/link_holder.dart';
import 'package:skywave_gui/screen/repair_page.dart';
import 'package:skywave_gui/settings/settings_page.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

void main() {
  testWidgets('settings shows the device and the toggle sets the policy', (
    tester,
  ) async {
    // Tall enough that the fingerprint, under the blob wording, is built.
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
            blobPolicyLabel: 'on demand (download when opened)',
            server: 'srv:7443',
            editableAddress: 'srv:7443',
            device: 'dev-1',
            fingerprint: 'AB:CD',
            stayConnected: null,
          ),
          link: LinkHolder(),
          canGoBack: true,
          send: sent.add,
        ),
      ),
    );
    for (final text in [
      'on demand (download when opened)',
      'srv:7443',
      'dev-1',
      'AB:CD',
    ]) {
      expect(find.text(text), findsOneWidget, reason: text);
    }
    expect(
      find.textContaining('Workdir diffs and files download only when opened'),
      findsOneWidget,
    );
    await tester.tap(find.text('Eager'));
    await tester.drag(find.byType(ListView), const Offset(0, -300));
    await tester.pump();
    await tester.tap(find.text('Re-pair'));
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
    String? editableAddress,
    bool? stayConnected,
  }) => MaterialApp(
    home: SettingsPage(
      screen: ScreenViewSettings(
        blobPolicy: BlobPolicyView.onDemand,
        blobPolicyLabel: 'on demand (download when opened)',
        server: editableAddress ?? 'not paired',
        editableAddress: editableAddress,
        device: 'dev-1',
        fingerprint: null,
        stayConnected: stayConnected,
      ),
      link: LinkHolder(),
      send: sent.add,
    ),
  );

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

  testWidgets('a paired server address is changed in place', (tester) async {
    final sent = <GuiIntent>[];
    await tester.pumpWidget(settings(sent, editableAddress: 'srv:7443'));
    final edit = find.byKey(const Key('edit-server-address'));

    await tester.tap(edit);
    await tester.pumpAndSettle();
    await tester.tap(find.text('Cancel'));
    await tester.pumpAndSettle();
    expect(sent, isEmpty);

    await tester.tap(edit);
    await tester.pumpAndSettle();
    final field = find.byKey(const Key('server-address-field'));
    expect(tester.widget<TextField>(field).controller!.text, 'srv:7443');
    await tester.enterText(field, '10.0.0.7:7443');
    await tester.tap(find.text('Save'));
    await tester.pumpAndSettle();
    expect(sent, [
      const GuiIntentAct(
        action: ActionIntentSetServerAddress(address: '10.0.0.7:7443'),
      ),
    ]);
  });

  testWidgets('an unpaired device has no address to change', (tester) async {
    await tester.pumpWidget(settings([]));
    expect(find.byKey(const Key('edit-server-address')), findsNothing);
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
