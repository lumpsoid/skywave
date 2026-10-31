import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/pairing/pairing_page.dart';
import 'package:skywave_gui/screen/screen_holder.dart';
import 'package:skywave_gui/screen/revoked_page.dart';
import 'package:skywave_gui/screen/screen_navigator.dart';
import 'package:skywave_gui/session/session_page.dart';
import 'package:skywave_gui/sessions/session_list_page.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

import '../fakes.dart';

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
    expect(find.byType(CircularProgressIndicator), findsOneWidget);

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
    expect(find.text('agent: hello'), findsOneWidget);
    expect(sent, isEmpty);
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

    await tester.binding.handlePopRoute();
    await tester.pump();
    expect(sent, [const GuiIntentAct(action: ActionIntentBack())]);
  });

  testWidgets('the revoked notice covers the screen', (tester) async {
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
        revokedFullScreen: true,
      ),
    );
    await tester.pumpAndSettle();
    expect(find.byType(RevokedPage), findsOneWidget);
    await tester.tap(find.text('Browse local data'));
    expect(sent, [const GuiIntentRevokedDismiss()]);
  });
}
