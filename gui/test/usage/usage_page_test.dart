import 'package:material_ui/material_ui.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';
import 'package:skywave_gui/usage/usage_page.dart';

void main() {
  UsageRowView row(String label, String cost, {bool archived = false}) =>
      UsageRowView(
        label: label,
        archived: archived,
        costLabel: cost,
        tokensLabel: '3.1k in · 812 out',
        turnsLabel: '2 turns',
      );

  testWidgets('usage shows the totals and picks span and grouping', (
    tester,
  ) async {
    final sent = <GuiIntent>[];
    await tester.pumpWidget(
      MaterialApp(
        home: UsagePage(
          screen: ScreenViewUsage(
            span: UsageSpanView.week,
            grouping: UsageGroupingView.session,
            total: row('Total', r'$1.27 + 1 unpriced'),
            rows: [
              row('fix the build', r'$1.27'),
              row('old work', 'cost unknown', archived: true),
            ],
            note: 'sessions deleted from this device do not.',
          ),
          canGoBack: true,
          send: sent.add,
        ),
      ),
    );
    for (final text in [
      'Total',
      r'$1.27 + 1 unpriced',
      'fix the build',
      'cost unknown',
      'sessions deleted from this device do not.',
    ]) {
      expect(find.text(text), findsOneWidget, reason: text);
    }
    expect(find.textContaining('archived', findRichText: true), findsOneWidget);

    await tester.tap(find.byKey(const Key('usage-span-all')));
    await tester.tap(find.byKey(const Key('usage-grouping-model')));
    await tester.tap(find.byType(BackButton));
    expect(sent, [
      const GuiIntentUsageShow(
        span: UsageSpanView.all,
        grouping: UsageGroupingView.session,
      ),
      const GuiIntentUsageShow(
        span: UsageSpanView.week,
        grouping: UsageGroupingView.model,
      ),
      const GuiIntentAct(action: ActionIntentBack()),
    ]);
  });

  testWidgets('an empty span says so', (tester) async {
    await tester.pumpWidget(
      MaterialApp(
        home: UsagePage(
          screen: ScreenViewUsage(
            span: UsageSpanView.today,
            grouping: UsageGroupingView.day,
            total: row('Total', r'$0'),
            rows: const [],
            emptyLabel: 'No turn reported usage in this span.',
            note: '',
          ),
          send: (_) {},
        ),
      ),
    );
    expect(find.text('No turn reported usage in this span.'), findsOneWidget);
  });
}
