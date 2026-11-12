import 'dart:ui' show PointerDeviceKind;

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';
import 'package:skywave_gui/workdir/workdir_page.dart';

void main() {
  WorkdirScreen screen({
    WorkdirPaneKind pane = WorkdirPaneKind.changes,
    String path = '',
    String header = 'main @ 1a2b3c4 · taken 12:00:00',
    String? fetchLabel,
    bool fetchFailed = false,
    String? wording,
    List<String> notes = const [],
    List<WorkdirChangeRow> changes = const [],
    bool canGoUp = false,
    List<WorkdirEntryRow> entries = const [],
    WorkdirContent? content,
  }) => WorkdirScreen(
    sessionId: 's-1',
    title: 'fix the build',
    workdir: '/srv/work',
    pane: pane,
    path: path,
    lastDir: 'src',
    header: header,
    fetchLabel: fetchLabel,
    fetchFailed: fetchFailed,
    wording: wording,
    notes: notes,
    changes: changes,
    canGoUp: canGoUp,
    entries: entries,
    content: content,
  );

  Future<List<GuiIntent>> pump(WidgetTester tester, WorkdirScreen s) async {
    final sent = <GuiIntent>[];
    await tester.pumpWidget(
      MaterialApp(
        home: WorkdirPage(screen: s, canGoBack: true, send: sent.add),
      ),
    );
    return sent;
  }

  testWidgets('the changes list their files and open their diffs', (
    tester,
  ) async {
    final sent = await pump(
      tester,
      screen(
        fetchLabel: 'Loading…',
        notes: ['Showing 2 of 3 changed files.'],
        changes: const [
          WorkdirChangeRow(
            change: WorkdirChangeKind.modified,
            mark: 'M',
            changeLabel: 'modified',
            pathLabel: 'src/a.rs',
            path: 'src/a.rs',
            countsLabel: '+1 −1',
            untracked: false,
          ),
          WorkdirChangeRow(
            change: WorkdirChangeKind.untracked,
            mark: '?',
            changeLabel: 'untracked',
            pathLabel: 'new.txt',
            path: 'new.txt',
            untracked: true,
          ),
        ],
      ),
    );
    for (final text in [
      'fix the build',
      '/srv/work',
      'M',
      'src/a.rs',
      '+1 −1',
      '?',
      'new.txt',
      'Showing 2 of 3 changed files.',
    ]) {
      expect(find.text(text), findsOneWidget, reason: text);
    }
    expect(
      find.textContaining(
        'main @ 1a2b3c4 · taken 12:00:00 · Loading…',
        findRichText: true,
      ),
      findsOneWidget,
    );

    await tester.tap(find.text('new.txt'));
    await tester.tap(find.byKey(const Key('workdir-tab-files')));
    // Already on the changes: nothing to show again.
    await tester.tap(find.byKey(const Key('workdir-tab-changes')));
    await tester.tap(find.byTooltip('Refresh'));
    await tester.tap(find.byType(BackButton));
    expect(sent, [
      const GuiIntentAct(
        action: ActionIntentShowWorkdir(
          pane: WorkdirPaneKind.diff,
          path: 'new.txt',
          untracked: true,
        ),
      ),
      const GuiIntentAct(
        action: ActionIntentShowWorkdir(
          pane: WorkdirPaneKind.dir,
          path: 'src',
          untracked: false,
        ),
      ),
      const GuiIntentAct(action: ActionIntentRefreshWorkdir()),
      const GuiIntentAct(action: ActionIntentBack()),
    ]);
  });

  testWidgets('a workdir outside git says so', (tester) async {
    await pump(
      tester,
      screen(
        header: 'Changes',
        wording: 'Not a git repository. Browse the files under Files.',
        fetchLabel: 'Refresh failed: offline',
        fetchFailed: true,
      ),
    );
    expect(
      find.text('Not a git repository. Browse the files under Files.'),
      findsOneWidget,
    );
    expect(
      find.textContaining('Refresh failed: offline', findRichText: true),
      findsOneWidget,
    );
    expect(find.byType(ListTile), findsNothing);
  });

  testWidgets('a diff shows its lines, with Open once downloaded', (
    tester,
  ) async {
    final sent = await pump(
      tester,
      screen(
        pane: WorkdirPaneKind.diff,
        path: 'src/a.rs',
        header: 'M src/a.rs  +1 −1',
        content: const WorkdirContent(
          blob: BlobFacts(
            blobId: 'b-1',
            state: BlobStateKind.available,
            label: 'downloaded',
            canDownload: false,
          ),
          lines: [
            WorkdirLine(kind: WorkdirLineKind.hunk, text: '@@ -1 +1 @@'),
            WorkdirLine(kind: WorkdirLineKind.removed, text: '---x'),
            WorkdirLine(kind: WorkdirLineKind.added, text: '+new'),
          ],
          cut: 'cut short: 1.0 MiB of 3.0 MiB shown',
        ),
      ),
    );
    for (final text in [
      '@@ -1 +1 @@',
      '---x',
      '+new',
      'downloaded',
      'cut short: 1.0 MiB of 3.0 MiB shown',
    ]) {
      expect(find.text(text), findsOneWidget, reason: text);
    }
    // A diff has no line numbers.
    expect(find.textContaining('1  '), findsNothing);
    expect(find.text('Download'), findsNothing);
    final removed = tester.widget<Text>(find.text('---x'));
    final added = tester.widget<Text>(find.text('+new'));
    expect(removed.style?.color, isNot(added.style?.color));

    await tester.tap(find.text('Open'));
    expect(sent, [const GuiIntentOpenFull(blobId: 'b-1')]);
  });

  testWidgets('a failed download offers Download', (tester) async {
    final sent = await pump(
      tester,
      screen(
        pane: WorkdirPaneKind.file,
        path: 'src/a.rs',
        header: 'src/a.rs  812 B',
        content: const WorkdirContent(
          blob: BlobFacts(
            blobId: 'b-2',
            state: BlobStateKind.failed,
            label: 'download failed: offline · 812 B',
            canDownload: true,
          ),
          lines: [],
        ),
      ),
    );
    expect(find.text('Open'), findsNothing);
    await tester.tap(find.text('Download'));
    expect(sent, [
      const GuiIntentAct(action: ActionIntentDownloadBlob(blobId: 'b-2')),
    ]);
  });

  testWidgets('a file shows numbered lines', (tester) async {
    await pump(
      tester,
      screen(
        pane: WorkdirPaneKind.file,
        path: 'src/main.rs',
        header: 'src/main.rs  25 B',
        content: const WorkdirContent(
          blob: BlobFacts(
            blobId: 'b-3',
            state: BlobStateKind.available,
            label: 'downloaded',
            canDownload: false,
          ),
          lines: [
            WorkdirLine(kind: WorkdirLineKind.plain, text: 'fn main() {}'),
            WorkdirLine(kind: WorkdirLineKind.plain, text: '-not a diff'),
          ],
        ),
      ),
    );
    expect(find.text('fn main() {}'), findsOneWidget);
    expect(find.text('1  '), findsOneWidget);
    expect(find.text('2  '), findsOneWidget);
  });

  // A file of two lines with a blank one between, and the clipboard writes.
  Future<List<String>> pumpThreeLines(WidgetTester tester) async {
    final copied = <String>[];
    tester.binding.defaultBinaryMessenger.setMockMethodCallHandler(
      SystemChannels.platform,
      (call) async {
        if (call.method == 'Clipboard.setData') {
          copied.add((call.arguments as Map)['text'] as String);
        }
        return null;
      },
    );
    addTearDown(
      () => tester.binding.defaultBinaryMessenger.setMockMethodCallHandler(
        SystemChannels.platform,
        null,
      ),
    );
    await pump(
      tester,
      screen(
        pane: WorkdirPaneKind.file,
        path: 'src/main.rs',
        header: 'src/main.rs  25 B',
        content: const WorkdirContent(
          blob: BlobFacts(
            blobId: 'b-3',
            state: BlobStateKind.available,
            label: 'downloaded',
            canDownload: false,
          ),
          lines: [
            WorkdirLine(kind: WorkdirLineKind.plain, text: 'fn main() {}'),
            WorkdirLine(kind: WorkdirLineKind.plain, text: ''),
            WorkdirLine(kind: WorkdirLineKind.plain, text: '-not a diff'),
          ],
        ),
      ),
    );
    return copied;
  }

  Future<void> copyShortcut(WidgetTester tester, {bool all = false}) async {
    await tester.sendKeyDownEvent(LogicalKeyboardKey.controlLeft);
    if (all) {
      await tester.sendKeyEvent(LogicalKeyboardKey.keyA);
    }
    await tester.sendKeyEvent(LogicalKeyboardKey.keyC);
    await tester.sendKeyUpEvent(LogicalKeyboardKey.controlLeft);
    await tester.pumpAndSettle();
  }

  testWidgets(
    'a file\'s lines are copied one per line, without their numbers',
    (tester) async {
      final copied = await pumpThreeLines(tester);
      await tester.tap(find.text('fn main() {}'));
      await tester.pump();
      await copyShortcut(tester, all: true);
      expect(copied, ['fn main() {}\n\n-not a diff\n']);
      expect(copied.single, isNot(contains('1  ')));
      expect(copied.single, isNot(contains('2  ')));
      expect(copied.single, isNot(contains('3  ')));
      // The lines sit as far apart as plain Text lines do: three lines of the
      // same style, the blank one included, and no extra line per line.
      final first = find.text('fn main() {}');
      final style = tester.widget<Text>(first).style;
      final line = tester.getSize(first).height;
      expect(
        tester.getTopLeft(find.text('-not a diff')).dy -
            tester.getTopLeft(first).dy,
        2 * line,
      );
      await tester.pumpWidget(
        MaterialApp(
          home: Column(
            children: [
              Text('a', style: style),
              Text('b', style: style),
            ],
          ),
        ),
      );
      expect(tester.getSize(find.text('a')).height, line);
      expect(
        tester.getTopLeft(find.text('b')).dy -
            tester.getTopLeft(find.text('a')).dy,
        line,
      );
    },
  );

  testWidgets('part of one line copies only that part', (tester) async {
    final copied = await pumpThreeLines(tester);
    final line = find.text('fn main() {}');
    final from = tester.getTopLeft(line) + const Offset(1, 4);
    // The Text spans the page, so aim a few characters in, not at its centre.
    final to = from + const Offset(30, 0);
    final gesture = await tester.startGesture(
      from,
      kind: PointerDeviceKind.mouse,
    );
    await tester.pump();
    await gesture.moveTo(to);
    await tester.pump();
    await gesture.up();
    await tester.pumpAndSettle();
    await copyShortcut(tester);
    expect(copied, hasLength(1));
    expect(copied.single, isNotEmpty);
    expect(copied.single, isNot(contains('\n')));
    expect('fn main() {}'.startsWith(copied.single), isTrue);
    expect(copied.single.length, lessThan('fn main() {}'.length));
  });

  testWidgets('a file listing opens entries and goes up', (tester) async {
    final sent = await pump(
      tester,
      screen(
        pane: WorkdirPaneKind.dir,
        path: 'src',
        header: './src · taken 12:00:00',
        canGoUp: true,
        notes: ['Showing the first 1,000 entries.'],
        entries: const [
          WorkdirEntryRow(
            name: 'a/',
            opens: WorkdirPaneKind.dir,
            path: 'src/a',
          ),
          WorkdirEntryRow(
            name: 'b.rs',
            detail: '812 B',
            opens: WorkdirPaneKind.file,
            path: 'src/b.rs',
          ),
        ],
      ),
    );
    for (final text in [
      '../',
      'a/',
      'b.rs',
      '812 B',
      'Showing the first 1,000 entries.',
    ]) {
      expect(find.text(text), findsOneWidget, reason: text);
    }
    await tester.tap(find.text('a/'));
    await tester.tap(find.text('b.rs'));
    await tester.tap(find.text('../'));
    // The listing shown is the Files tab's: nothing to show again.
    await tester.tap(find.byKey(const Key('workdir-tab-files')));
    await tester.tap(find.byKey(const Key('workdir-tab-changes')));
    expect(sent, [
      const GuiIntentAct(
        action: ActionIntentShowWorkdir(
          pane: WorkdirPaneKind.dir,
          path: 'src/a',
          untracked: false,
        ),
      ),
      const GuiIntentAct(
        action: ActionIntentShowWorkdir(
          pane: WorkdirPaneKind.file,
          path: 'src/b.rs',
          untracked: false,
        ),
      ),
      const GuiIntentAct(action: ActionIntentBack()),
      const GuiIntentAct(
        action: ActionIntentShowWorkdir(
          pane: WorkdirPaneKind.changes,
          path: '',
          untracked: false,
        ),
      ),
    ]);
  });
}
