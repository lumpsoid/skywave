import 'package:material_ui/material_ui.dart';
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

  group('a blob row', () {
    const question =
        "Delete this output from the server and from every device? This "
        "can't be undone.";
    const target = BlobTargetView(
      blobId: 'b1',
      canCrop: true,
      deleteQuestion: question,
      sessionId: 's1',
      holds: BlobUseView.toolOutput,
      title: 'fix the build · tool output',
    );

    Future<List<GuiIntent>> pump(
      WidgetTester tester,
      BlobTargetView target,
    ) async {
      final sent = <GuiIntent>[];
      await tester.pumpWidget(
        MaterialApp(
          home: StoragePage(
            screen: ScreenViewStorage(
              grouping: StorageGroupingView.blobs,
              totals: const [],
              rows: [
                StorageRowView(
                  label: 'fix the build',
                  detail: 'tool output · event 7',
                  size: '4.0 KiB',
                  target: target,
                ),
              ],
            ),
            send: sent.add,
          ),
        ),
      );
      return sent;
    }

    Future<void> choose(WidgetTester tester, String item) async {
      await tester.tap(find.byKey(const Key('blob-menu-b1')));
      await tester.pumpAndSettle();
      await tester.tap(find.byKey(Key(item)));
      await tester.pumpAndSettle();
    }

    testWidgets('deletes only once the question is confirmed', (tester) async {
      final sent = await pump(tester, target);
      await choose(tester, 'blob-delete');
      expect(find.text(question), findsOneWidget);
      expect(sent, isEmpty, reason: 'nothing before the answer');
      await tester.tap(find.byKey(const Key('confirm-cancel')));
      await tester.pumpAndSettle();
      expect(find.text(question), findsNothing);
      expect(sent, isEmpty, reason: 'cancel sends none');

      await choose(tester, 'blob-delete');
      await tester.tap(find.byKey(const Key('confirm-ok')));
      await tester.pumpAndSettle();
      expect(sent, [
        const GuiIntentAct(action: ActionIntentDeleteBlob(blobId: 'b1')),
      ]);
    });

    testWidgets('crop opens the crop page with the target', (tester) async {
      final sent = await pump(tester, target);
      await choose(tester, 'blob-crop');
      expect(find.byType(AlertDialog), findsNothing);
      expect(sent, [
        const GuiIntentAct(
          action: ActionIntentOpenCrop(
            blobId: 'b1',
            sessionId: 's1',
            holds: BlobUseView.toolOutput,
            title: 'fix the build · tool output',
          ),
        ),
      ]);
    });

    testWidgets('a pending delete shows instead of the menu, and discards', (
      tester,
    ) async {
      final sent = await pump(
        tester,
        const BlobTargetView(
          blobId: 'b1',
          canCrop: false,
          pendingNote: 'Delete queued',
          discardId: 'c1',
          sessionId: 's1',
          holds: BlobUseView.toolOutput,
          title: 'fix the build · tool output',
        ),
      );
      expect(find.byKey(const Key('blob-menu-b1')), findsNothing);
      expect(find.text('Delete queued'), findsOneWidget);
      await tester.tap(find.byKey(const Key('blob-discard-b1')));
      expect(sent, [
        const GuiIntentAct(action: ActionIntentDiscard(commandId: 'c1')),
      ]);
    });
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
