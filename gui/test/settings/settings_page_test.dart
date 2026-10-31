import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/link/link_holder.dart';
import 'package:skywave_gui/screen/revoked_page.dart';
import 'package:skywave_gui/settings/settings_page.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

void main() {
  testWidgets('settings shows the device and the toggle sets the policy', (
    tester,
  ) async {
    final sent = <GuiIntent>[];
    await tester.pumpWidget(
      MaterialApp(
        home: SettingsPage(
          screen: const ScreenViewSettings(
            blobPolicy: BlobPolicyView.onDemand,
            blobPolicyLabel: 'on demand (download when opened)',
            server: 'srv:7443',
            device: 'dev-1',
            fingerprint: 'AB:CD',
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

  testWidgets('the revoked notice re-pairs or browses local data', (
    tester,
  ) async {
    final sent = <GuiIntent>[];
    await tester.pumpWidget(MaterialApp(home: RevokedPage(send: sent.add)));
    await tester.tap(find.text('Re-pair'));
    await tester.tap(find.text('Browse local data'));
    expect(sent, [
      const GuiIntentAct(
        action: ActionIntentNavigate(route: RouteView.pairing),
      ),
      const GuiIntentRevokedDismiss(),
    ]);
  });
}
