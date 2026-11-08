import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';
import 'package:skywave_gui/usage/limits_page.dart';

void main() {
  testWidgets('usage limits show each limit with its bar, and refresh', (
    tester,
  ) async {
    final sent = <GuiIntent>[];
    await tester.pumpWidget(
      MaterialApp(
        home: LimitsPage(
          screen: const ScreenViewUsageLimits(
            limits: LimitsView(
              statusLabel: 'Refreshing…',
              readLabel: 'Read 12 min ago',
              rows: [
                LimitRowView(
                  label: 'Session',
                  percentLabel: '75%',
                  fillPercent: 75,
                  warning: true,
                  resetsLabel: 'resets in 2 h 13 min',
                ),
                LimitRowView(label: 'Weekly · Fable', warning: false),
              ],
            ),
          ),
          canGoBack: true,
          send: sent.add,
        ),
      ),
    );
    for (final text in [
      'Refreshing…',
      'Read 12 min ago',
      'Session',
      '75%',
      'resets in 2 h 13 min',
      'Weekly · Fable',
      'use not reported',
    ]) {
      expect(find.text(text), findsOneWidget, reason: text);
    }
    final bar = tester.widget<LinearProgressIndicator>(
      find.byType(LinearProgressIndicator),
    );
    expect(bar.value, 0.75);

    await tester.tap(find.byTooltip('Refresh'));
    await tester.tap(find.byType(BackButton));
    expect(sent, [
      const GuiIntentAct(action: ActionIntentRefreshUsageLimits()),
      const GuiIntentAct(action: ActionIntentBack()),
    ]);
  });

  testWidgets('with nothing to list the page says why', (tester) async {
    await tester.pumpWidget(
      MaterialApp(
        home: LimitsPage(
          screen: const ScreenViewUsageLimits(
            limits: LimitsView(
              rows: [],
              emptyLabel: 'No usage limits read yet.',
            ),
          ),
          send: (_) {},
        ),
      ),
    );
    expect(find.text('No usage limits read yet.'), findsOneWidget);
    expect(find.byType(LinearProgressIndicator), findsNothing);
  });
}
