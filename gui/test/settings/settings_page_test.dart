import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
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
