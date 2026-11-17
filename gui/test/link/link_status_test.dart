import 'package:material_ui/material_ui.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/link/link_banner.dart';
import 'package:skywave_gui/link/link_holder.dart';
import 'package:skywave_gui/link/link_status.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

import 'link_banner_test.dart' show link;

void main() {
  late LinkHolder holder;
  late List<GuiIntent> sent;

  /// A page whose app bar shows the link, as every page's does.
  Future<void> pump(WidgetTester tester) async {
    holder = LinkHolder();
    sent = [];
    await tester.pumpWidget(
      MaterialApp(
        builder: (context, child) => LinkScope(holder: holder, child: child!),
        home: LinkShortcuts(
          send: sent.add,
          child: Builder(
            builder: (context) => Scaffold(
              appBar: AppBar(
                toolbarHeight: LinkTitle.toolbarHeight(context),
                flexibleSpace: const LinkProgress(),
                title: LinkTitle(
                  title: const Text('work'),
                  subtitle: const Text('~/src/work'),
                  send: sent.add,
                ),
                actions: [ApprovalsButton(send: sent.add)],
              ),
              body: const SizedBox.expand(key: Key('body')),
            ),
          ),
        ),
      ),
    );
  }

  double bodyTop(WidgetTester tester) =>
      tester.getTopLeft(find.byKey(const Key('body'))).dy;

  testWidgets('the status line takes the subtitle\'s place, moving nothing', (
    tester,
  ) async {
    await pump(tester);
    holder.apply(link(LinkState.online, ''));
    await tester.pumpAndSettle();
    expect(find.text('~/src/work'), findsOneWidget);
    final top = bodyTop(tester);

    for (final (state, status) in [
      (LinkState.connecting, 'Connecting…'),
      (LinkState.catchingUp, 'Syncing · 4/9 · 2 queued'),
      (LinkState.reconnecting, 'Offline · retry in 07s · server unreachable'),
    ]) {
      holder.apply(link(state, '', statusLabel: status));
      // The progress line never settles while connecting.
      await tester.pump();
      expect(find.text(status), findsOneWidget, reason: '$state');
      expect(find.text('~/src/work'), findsNothing, reason: '$state');
      expect(bodyTop(tester), top, reason: '$state');
    }

    holder.apply(link(LinkState.online, ''));
    await tester.pumpAndSettle();
    expect(find.byKey(const Key('link-status')), findsNothing);
    expect(find.text('~/src/work'), findsOneWidget);
  });

  testWidgets('the progress line shows while connecting or syncing only', (
    tester,
  ) async {
    await pump(tester);
    for (final (state, busy) in [
      (LinkState.connecting, true),
      (LinkState.catchingUp, true),
      (LinkState.reconnecting, false),
      (LinkState.online, false),
    ]) {
      holder.apply(link(state, '', statusLabel: busy ? 'busy' : null));
      await tester.pump();
      expect(
        find.byKey(const Key('link-progress')),
        busy ? findsOneWidget : findsNothing,
        reason: '$state',
      );
    }
  });

  testWidgets('a tap on the offline line says it in full, with Retry now', (
    tester,
  ) async {
    const offline = 'Offline · retry in 07s · server unreachable · 4 queued';
    await pump(tester);
    holder.apply(link(LinkState.connecting, '', statusLabel: 'Connecting…'));
    await tester.pump();
    await tester.tap(find.text('Connecting…'));
    await tester.pump();
    expect(find.text('Retry now'), findsNothing, reason: 'nothing to retry');

    holder.apply(
      link(LinkState.reconnecting, '', statusLabel: offline, canRetryNow: true),
    );
    await tester.pumpAndSettle();
    await tester.tap(find.byKey(const Key('link-status')));
    await tester.pumpAndSettle();
    expect(find.byKey(const Key('link-status-full')), findsOneWidget);
    await tester.tap(find.text('Retry now'));
    await tester.pumpAndSettle();
    expect(sent, [const GuiIntentAct(action: ActionIntentRetryNow())]);
    expect(find.text('Retry now'), findsNothing, reason: 'the sheet closed');
  });

  testWidgets(
    'approvals elsewhere: a badged bell opens them all, Ctrl+G the next',
    (tester) async {
      await pump(tester);
      holder.apply(link(LinkState.online, ''));
      await tester.pumpAndSettle();
      expect(find.byKey(const Key('approvals-elsewhere')), findsNothing);
      final top = bodyTop(tester);

      holder.apply(
        link(
          LinkState.online,
          '',
          elsewhere: 2,
          elsewhereLabel: '2 approvals elsewhere',
        ),
      );
      await tester.pumpAndSettle();
      expect(bodyTop(tester), top);
      expect(
        find.descendant(
          of: find.byKey(const Key('approvals-elsewhere')),
          matching: find.text('2'),
        ),
        findsOneWidget,
      );
      await tester.tap(find.byKey(const Key('approvals-elsewhere')));
      await tester.sendKeyDownEvent(LogicalKeyboardKey.controlLeft);
      await tester.sendKeyEvent(LogicalKeyboardKey.keyG);
      await tester.sendKeyUpEvent(LogicalKeyboardKey.controlLeft);
      expect(sent, [
        const GuiIntentAct(
          action: ActionIntentNavigate(route: RouteView.approvals),
        ),
        const GuiIntentAct(action: ActionIntentOpenNextApproval()),
      ]);
    },
  );

  testWidgets('a phone app bar fits the offline line and the bell', (
    tester,
  ) async {
    tester.view.physicalSize = const Size(320, 640);
    tester.view.devicePixelRatio = 1;
    addTearDown(tester.view.reset);
    await pump(tester);
    holder.apply(
      link(
        LinkState.reconnecting,
        '',
        statusLabel: 'Offline · retry in 07s · server unreachable · 4 queued',
        elsewhere: 12,
        elsewhereLabel: '12 approvals elsewhere',
      ),
    );
    await tester.pumpAndSettle();
    expect(tester.takeException(), isNull);
    expect(find.byKey(const Key('link-status')), findsOneWidget);
    expect(find.byKey(const Key('approvals-elsewhere')), findsOneWidget);
  });
}
