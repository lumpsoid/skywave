import 'package:material_ui/material_ui.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/link/link_holder.dart';
import 'package:skywave_gui/settings/server_addresses_page.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

void main() {
  Widget page(
    List<GuiIntent> sent, {
    String current = 'srv:7443',
    List<String> others = const [],
    LinkHolder? link,
  }) => MaterialApp(
    home: ServerAddressesPage(
      screen: ScreenViewServerAddresses(current: current, others: others),
      link: link ?? LinkHolder(),
      canGoBack: true,
      send: sent.add,
    ),
  );

  testWidgets('another address is a tap to switch to, or forgotten', (
    tester,
  ) async {
    final sent = <GuiIntent>[];
    await tester.pumpWidget(page(sent, others: ['10.0.0.7:7443', 'vpn:7443']));
    expect(find.text('srv:7443'), findsOneWidget);
    expect(find.textContaining('kept here'), findsNothing);
    await tester.tap(find.text('vpn:7443'));
    await tester.tap(find.byKey(const Key('forget-address-10.0.0.7:7443')));
    await tester.tap(find.byType(BackButton));
    expect(sent, [
      const GuiIntentAct(
        action: ActionIntentSetServerAddress(address: 'vpn:7443'),
      ),
      const GuiIntentAct(
        action: ActionIntentForgetServerAddress(address: '10.0.0.7:7443'),
      ),
      const GuiIntentAct(action: ActionIntentBack()),
    ]);
  });

  testWidgets('the current address shows how the link stands', (tester) async {
    final link = LinkHolder();
    addTearDown(link.dispose);
    await tester.pumpWidget(page([], link: link));
    expect(find.text('Not known yet'), findsOneWidget);
    expect(find.textContaining('kept here'), findsOneWidget);
    link.apply(
      const LinkUpdate(
        state: LinkState.online,
        label: '',
        connectionLabel: 'Connected · 12 ms',
        canRetryNow: false,
        rejected: false,
        canResetReplica: false,
        queued: 0,
        approvalsElsewhere: 0,
      ),
    );
    await tester.pump();
    expect(find.text('Connected · 12 ms'), findsOneWidget);
  });

  testWidgets('a new address is sent, and leaves the field once in use', (
    tester,
  ) async {
    final sent = <GuiIntent>[];
    await tester.pumpWidget(page(sent));
    final field = find.byKey(const Key('server-address-field'));
    String typed() => tester.widget<TextField>(field).controller!.text;

    await tester.tap(find.byKey(const Key('connect-address')));
    expect(sent, isEmpty, reason: 'nothing typed');

    await tester.enterText(field, ' 10.0.0.7:7443 ');
    await tester.tap(find.byKey(const Key('connect-address')));
    expect(sent, [
      const GuiIntentAct(
        action: ActionIntentSetServerAddress(address: '10.0.0.7:7443'),
      ),
    ]);
    // Refused (no new screen): kept, to correct.
    await tester.pump();
    expect(typed(), ' 10.0.0.7:7443 ');

    await tester.pumpWidget(
      page(sent, current: '10.0.0.7:7443', others: ['srv:7443']),
    );
    expect(typed(), isEmpty);
  });
}
