import 'package:material_ui/material_ui.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';
import 'package:skywave_gui/storage/storage_page.dart';

/// Whether the button keyed [key] can be pressed.
bool enabled(WidgetTester tester, String key) =>
    tester.widget<ButtonStyleButton>(find.byKey(Key(key))).onPressed != null;

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
            serverKinds: [],
            serverReclaiming: false,
            deviceTotals: [],
            deviceKinds: [],
            deviceReclaiming: false,
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
              serverKinds: const [],
              serverReclaiming: false,
              deviceTotals: const [],
              deviceKinds: const [],
              deviceReclaiming: false,
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
            serverKinds: [],
            serverReclaiming: false,
            deviceTotals: [],
            deviceKinds: [],
            deviceReclaiming: false,
          ),
          send: (_) {},
        ),
      ),
    );
    expect(find.text('No shell runs.'), findsOneWidget);
  });

  testWidgets('the server and this device each show their bar and reclaim', (
    tester,
  ) async {
    final sent = <GuiIntent>[];
    await tester.pumpWidget(
      MaterialApp(
        home: StoragePage(
          screen: const ScreenViewStorage(
            grouping: StorageGroupingView.sessions,
            totals: [StorageTotalView(label: 'Free', value: '18.6 GiB')],
            rows: [],
            serverKinds: [
              StorageKindView(label: 'Database', bytes: 3000, size: '2.9 KiB'),
              StorageKindView(
                label: 'Event blobs',
                bytes: 1000,
                size: '1000 B',
              ),
            ],
            serverReclaimable: '4.0 KiB',
            serverReclaiming: false,
            deviceTotals: [StorageTotalView(label: 'Used', value: '17.0 KiB')],
            deviceKinds: [
              StorageKindView(
                label: 'Database',
                bytes: 16384,
                size: '16.0 KiB',
              ),
            ],
            deviceReclaimable: '2.0 KiB',
            deviceReclaiming: false,
          ),
          send: sent.add,
        ),
      ),
    );
    for (final text in [
      'Server',
      'This device',
      'Reclaim 4.0 KiB',
      'Reclaim 2.0 KiB',
      'Database 2.9 KiB',
      'Database 16.0 KiB',
    ]) {
      expect(find.text(text), findsOneWidget, reason: text);
    }

    await tester.tap(find.byKey(const Key('storage-reclaim-server')));
    await tester.tap(find.byKey(const Key('storage-reclaim-device')));
    expect(sent, [
      const GuiIntentAct(action: ActionIntentReclaimServerSpace()),
      const GuiIntentAct(action: ActionIntentReclaimDeviceSpace()),
    ]);
  });

  testWidgets('a reclaim in progress shows instead of its button', (
    tester,
  ) async {
    await tester.pumpWidget(
      MaterialApp(
        home: StoragePage(
          screen: const ScreenViewStorage(
            grouping: StorageGroupingView.sessions,
            totals: [StorageTotalView(label: 'Free', value: '18.6 GiB')],
            rows: [],
            serverKinds: [],
            serverReclaiming: true,
            deviceTotals: [],
            deviceKinds: [],
            deviceReclaiming: true,
          ),
          send: (_) {},
        ),
      ),
    );
    // A spinner never settles, so no pumpAndSettle here.
    await tester.pump();
    expect(find.byKey(const Key('storage-reclaiming-server')), findsOneWidget);
    expect(find.byKey(const Key('storage-reclaiming-device')), findsOneWidget);
    expect(find.byKey(const Key('storage-reclaim-server')), findsNothing);
    expect(find.byKey(const Key('storage-reclaim-device')), findsNothing);
  });

  testWidgets('selected sessions get only the bulk actions their rows allow', (
    tester,
  ) async {
    final sent = <GuiIntent>[];
    const a = StorageSessionView(
      sessionId: 'a',
      here: true,
      archived: false,
      deletable: false,
      deletableEverywhere: false,
    );
    const b = StorageSessionView(
      sessionId: 'b',
      here: true,
      archived: true,
      deletable: true,
      deletableEverywhere: true,
    );
    const c = StorageSessionView(
      sessionId: 'c',
      here: false,
      archived: false,
      deletable: false,
      deletableEverywhere: false,
    );
    const archiveA = GuiIntentAct(
      action: ActionIntentSetSessionArchived(sessionId: 'a', archived: true),
    );
    const deleteB = GuiIntentAct(
      action: ActionIntentDeleteSessionEverywhere(sessionId: 'b'),
    );
    const removeB = GuiIntentAct(
      action: ActionIntentDeleteSessionLocally(sessionId: 'b'),
    );
    await tester.pumpWidget(
      MaterialApp(
        home: StoragePage(
          screen: const ScreenViewStorage(
            grouping: StorageGroupingView.sessions,
            totals: [],
            rows: [
              StorageRowView(
                label: 'fix the build',
                detail: '',
                size: '4.0 KiB',
                session: a,
              ),
              StorageRowView(
                label: 'write docs',
                detail: '',
                size: '2.0 KiB',
                session: b,
              ),
              StorageRowView(
                label: 'refactor the parser',
                detail: '',
                size: '1.0 KiB',
                session: c,
              ),
            ],
            serverKinds: [],
            serverReclaiming: false,
            deviceTotals: [],
            deviceKinds: [],
            deviceReclaiming: false,
          ),
          send: sent.add,
        ),
      ),
    );
    expect(find.byKey(const Key('storage-bulk-archive')), findsNothing);

    await tester.tap(find.byKey(const Key('storage-select-a')));
    await tester.tap(find.byKey(const Key('storage-select-c')));
    await tester.pump();
    expect(find.text('2 selected'), findsOneWidget);
    expect(enabled(tester, 'storage-bulk-archive'), isTrue);
    expect(enabled(tester, 'storage-bulk-remove'), isFalse);
    expect(enabled(tester, 'storage-bulk-delete'), isFalse);

    await tester.tap(find.byKey(const Key('storage-bulk-archive')));
    await tester.pump();
    expect(sent, [archiveA]);
    expect(find.byKey(const Key('storage-bulk-archive')), findsNothing);

    await tester.tap(find.byKey(const Key('storage-select-b')));
    await tester.pump();
    await tester.tap(find.byKey(const Key('storage-bulk-delete')));
    await tester.pumpAndSettle();
    expect(
      find.descendant(
        of: find.byType(AlertDialog),
        matching: find.textContaining('write docs'),
      ),
      findsOneWidget,
    );
    await tester.tap(find.byKey(const Key('confirm-cancel')));
    await tester.pumpAndSettle();
    expect(sent, [archiveA], reason: 'cancel sends none');

    await tester.tap(find.byKey(const Key('storage-bulk-delete')));
    await tester.pumpAndSettle();
    await tester.tap(find.byKey(const Key('confirm-ok')));
    await tester.pumpAndSettle();
    expect(sent, [archiveA, deleteB]);

    await tester.tap(find.byKey(const Key('storage-select-b')));
    await tester.pump();
    await tester.tap(find.byKey(const Key('storage-bulk-remove')));
    await tester.pumpAndSettle();
    await tester.tap(find.byKey(const Key('confirm-ok')));
    await tester.pumpAndSettle();
    expect(sent, [archiveA, deleteB, removeB]);
  });

  testWidgets('only session rows can be selected', (tester) async {
    await tester.pumpWidget(
      MaterialApp(
        home: StoragePage(
          screen: const ScreenViewStorage(
            grouping: StorageGroupingView.sessions,
            totals: [],
            rows: [
              StorageRowView(
                label: 'fix the build',
                detail: 'tool output · event 7',
                size: '4.0 KiB',
                target: BlobTargetView(
                  blobId: 'b1',
                  canCrop: true,
                  sessionId: 's1',
                  holds: BlobUseView.toolOutput,
                  title: 'fix the build · tool output',
                ),
              ),
              StorageRowView(
                label: 'write docs',
                detail: '',
                size: '2.0 KiB',
                session: StorageSessionView(
                  sessionId: 's2',
                  here: true,
                  archived: false,
                  deletable: false,
                  deletableEverywhere: false,
                ),
              ),
            ],
            serverKinds: [],
            serverReclaiming: false,
            deviceTotals: [],
            deviceKinds: [],
            deviceReclaiming: false,
          ),
          send: (_) {},
        ),
      ),
    );
    // The session row's tick is the only one: the blob row has none.
    expect(find.byType(Checkbox), findsOneWidget);
    expect(find.byKey(const Key('storage-select-s2')), findsOneWidget);
  });
}
