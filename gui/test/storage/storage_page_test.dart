import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';
import 'package:skywave_gui/storage/storage_page.dart';

void main() {
  testWidgets('storage shows the totals and rows, refreshes and picks a list', (
    tester,
  ) async {
    final sent = <GuiIntent>[];
    await tester.pumpWidget(
      MaterialApp(
        home: StoragePage(
          screen: const ScreenViewStorage(
            grouping: StorageGroupingView.sessions,
            statusLabel: 'Refreshing…',
            totals: [
              StorageTotalView(label: 'Free', value: '18.6 GiB'),
              StorageTotalView(label: 'Event blobs', value: '2.0 MiB'),
            ],
            rows: [
              StorageRowView(
                label: 'fix the build',
                detail: '12 events · 4.0 KiB in the log · 1 blob 2.0 MiB',
                size: '2.0 MiB',
              ),
            ],
          ),
          canGoBack: true,
          send: sent.add,
        ),
      ),
    );
    for (final text in [
      'Refreshing…',
      'Free',
      '18.6 GiB',
      'fix the build',
      '12 events · 4.0 KiB in the log · 1 blob 2.0 MiB',
    ]) {
      expect(find.text(text), findsOneWidget, reason: text);
    }

    await tester.tap(find.byTooltip('Refresh'));
    await tester.tap(find.byKey(const Key('storage-grouping-shellRuns')));
    await tester.tap(find.byType(BackButton));
    expect(sent, [
      const GuiIntentAct(action: ActionIntentRefreshStorage()),
      const GuiIntentStorageShow(grouping: StorageGroupingView.shellRuns),
      const GuiIntentAct(action: ActionIntentBack()),
    ]);
  });

  testWidgets('an empty list says so', (tester) async {
    await tester.pumpWidget(
      MaterialApp(
        home: StoragePage(
          screen: const ScreenViewStorage(
            grouping: StorageGroupingView.shellRuns,
            totals: [],
            rows: [],
            emptyLabel: 'No shell runs.',
          ),
          send: (_) {},
        ),
      ),
    );
    expect(find.text('No shell runs.'), findsOneWidget);
  });
}
