import 'dart:ui' show PointerDeviceKind;

import 'package:material_ui/material_ui.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';
import 'package:skywave_gui/workdir/pushing.dart';
import 'package:skywave_gui/workdir/saving.dart';
import 'package:skywave_gui/workdir/workdir_page.dart';
import 'package:skywave_gui/workdir/workdir_text_holder.dart';

void main() {
  final originalSaver = saveWorkdirFile;
  final originalPicker = pickPushFile;
  tearDown(() {
    saveWorkdirFile = originalSaver;
    pickPushFile = originalPicker;
  });

  WorkdirScreen screen({
    WorkdirPaneKind pane = WorkdirPaneKind.changes,
    String path = '',
    String header = 'main @ 1a2b3c4 · taken 12:00:00',
    String? fetchLabel,
    bool fetchFailed = false,
    String? wording,
    List<String> notes = const [],
    List<WorkdirChangeRow> changes = const [],
    List<WorkdirCommitRow> commits = const [],
    bool canGoUp = false,
    List<WorkdirEntryRow> entries = const [],
    WorkdirContent? content,
    List<WorkdirDownloadRow> downloads = const [],
    List<WorkdirPushRow> pushes = const [],
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
    commits: commits,
    canGoUp: canGoUp,
    entries: entries,
    content: content,
    downloads: downloads,
    pushes: pushes,
  );

  Future<List<GuiIntent>> pump(
    WidgetTester tester,
    WorkdirScreen s, {
    WorkdirTextHolder? text,
  }) async {
    final sent = <GuiIntent>[];
    await tester.pumpWidget(
      MaterialApp(
        home: WorkdirPage(
          screen: s,
          text: text ?? WorkdirTextHolder(),
          canGoBack: true,
          send: sent.add,
        ),
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
    await tester.tap(find.byKey(const Key('workdir-tab-commits')));
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
      const GuiIntentAct(
        action: ActionIntentShowWorkdir(
          pane: WorkdirPaneKind.commits,
          path: '',
          untracked: false,
        ),
      ),
      const GuiIntentAct(action: ActionIntentRefreshWorkdir()),
      const GuiIntentAct(action: ActionIntentBack()),
    ]);
  });

  testWidgets('the commits list their commits', (tester) async {
    final sent = await pump(
      tester,
      screen(
        pane: WorkdirPaneKind.commits,
        header: 'main · taken 12:00:00',
        notes: ['Showing the newest 2 commits.'],
        commits: const [
          WorkdirCommitRow(
            short: 'a1b2c3d',
            subject: 'Add the log',
            author: 'Ada',
            timeLabel: '3 h ago',
          ),
          WorkdirCommitRow(short: 'e4f5a6b', subject: 'First', author: 'Zoë'),
        ],
      ),
    );
    for (final text in [
      'a1b2c3d',
      'Add the log',
      'Ada · 3 h ago',
      'e4f5a6b',
      'First',
      'Zoë',
      'Showing the newest 2 commits.',
    ]) {
      expect(find.text(text), findsOneWidget, reason: text);
    }
    bool selected(String key) =>
        tester.widget<ChoiceChip>(find.byKey(Key(key))).selected;
    expect(selected('workdir-tab-commits'), isTrue);
    expect(selected('workdir-tab-files'), isFalse);
    expect(selected('workdir-tab-changes'), isFalse);

    // A commit opens nothing; the chips move the pane as the others do.
    await tester.tap(find.text('Add the log'));
    await tester.tap(find.byKey(const Key('workdir-tab-commits')));
    await tester.tap(find.byKey(const Key('workdir-tab-changes')));
    await tester.tap(find.byKey(const Key('workdir-tab-files')));
    expect(sent, [
      const GuiIntentAct(
        action: ActionIntentShowWorkdir(
          pane: WorkdirPaneKind.changes,
          path: '',
          untracked: false,
        ),
      ),
      const GuiIntentAct(
        action: ActionIntentShowWorkdir(
          pane: WorkdirPaneKind.dir,
          path: 'src',
          untracked: false,
        ),
      ),
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
          cut: 'cut short: 1.0 MiB of 3.0 MiB shown',
        ),
      ),
      text: WorkdirTextHolder()
        ..apply(
          const WorkdirTextUpdate(
            blobId: 'b-1',
            lines: [
              WorkdirLine(kind: WorkdirLineKind.hunk, text: '@@ -1 +1 @@'),
              WorkdirLine(kind: WorkdirLineKind.removed, text: '---x'),
              WorkdirLine(kind: WorkdirLineKind.added, text: '+new'),
            ],
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
        ),
      ),
      text: WorkdirTextHolder()
        ..apply(
          const WorkdirTextUpdate(
            blobId: 'b-3',
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

  testWidgets('a text for another blob shows no lines', (tester) async {
    final text = WorkdirTextHolder()
      ..apply(
        const WorkdirTextUpdate(
          blobId: 'b-2',
          lines: [WorkdirLine(kind: WorkdirLineKind.plain, text: 'nope')],
        ),
      );
    await pump(
      tester,
      screen(
        pane: WorkdirPaneKind.file,
        path: 'src/main.rs',
        header: 'src/main.rs  4 B',
        content: const WorkdirContent(
          blob: BlobFacts(
            blobId: 'b-1',
            state: BlobStateKind.available,
            label: 'downloaded',
            canDownload: false,
          ),
        ),
      ),
      text: text,
    );
    expect(find.text('nope'), findsNothing);

    // The same page shows the lines once their blob's text arrives.
    text.apply(
      const WorkdirTextUpdate(
        blobId: 'b-1',
        lines: [WorkdirLine(kind: WorkdirLineKind.plain, text: 'yes')],
      ),
    );
    await tester.pump();
    expect(find.text('yes'), findsOneWidget);
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
        ),
      ),
      text: WorkdirTextHolder()
        ..apply(
          const WorkdirTextUpdate(
            blobId: 'b-3',
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

  testWidgets('a file entry offers Download, a directory does not', (
    tester,
  ) async {
    final sent = await pump(
      tester,
      screen(
        pane: WorkdirPaneKind.dir,
        path: 'src',
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
    expect(find.byKey(const ValueKey('workdir-download-src/a')), findsNothing);
    await tester.tap(find.byKey(const ValueKey('workdir-download-src/b.rs')));
    expect(sent, [
      const GuiIntentAct(
        action: ActionIntentDownloadWorkdirFile(
          sessionId: 's-1',
          path: 'src/b.rs',
        ),
      ),
    ]);
  });

  testWidgets('the file pane downloads the file it shows', (tester) async {
    await pump(tester, screen());
    expect(find.byTooltip('Download file'), findsNothing);

    final sent = await pump(
      tester,
      screen(pane: WorkdirPaneKind.file, path: 'bin/app', header: 'bin/app'),
    );
    await tester.tap(find.byTooltip('Download file'));
    expect(sent, [
      const GuiIntentAct(
        action: ActionIntentDownloadWorkdirFile(
          sessionId: 's-1',
          path: 'bin/app',
        ),
      ),
    ]);
  });

  testWidgets('the downloads panel shows each download, retries and saves', (
    tester,
  ) async {
    final saves = <(String, String)>[];
    saveWorkdirFile = ({required String source, required String name}) async {
      saves.add((source, name));
      return '/home/u/c.zip';
    };
    final sent = await pump(
      tester,
      screen(
        downloads: const [
          WorkdirDownloadRow(
            path: 'a.bin',
            label: 'Asking the server…',
            failed: false,
            canCancel: false,
          ),
          WorkdirDownloadRow(
            path: 'b.bin',
            label: 'Download failed: timed out',
            failed: true,
            canCancel: false,
          ),
          WorkdirDownloadRow(
            path: 'out/c.zip',
            label: 'downloaded',
            failed: false,
            canCancel: false,
            localPath: '/data/blobs/c',
          ),
        ],
      ),
    );
    for (final text in [
      'Asking the server…',
      'Download failed: timed out',
      'downloaded',
    ]) {
      expect(find.text(text), findsOneWidget, reason: text);
    }
    expect(find.text('Retry'), findsOneWidget);
    expect(find.text('Save…'), findsOneWidget);

    await tester.tap(find.text('Retry'));
    expect(sent, [
      const GuiIntentAct(
        action: ActionIntentDownloadWorkdirFile(
          sessionId: 's-1',
          path: 'b.bin',
        ),
      ),
    ]);

    // The panel is bounded, so its last row scrolls into view first.
    await tester.ensureVisible(find.text('Save…'));
    await tester.pump();
    await tester.tap(find.text('Save…'));
    await tester.pumpAndSettle();
    expect(saves, [('/data/blobs/c', 'c.zip')]);
    expect(find.text('Saved out/c.zip'), findsOneWidget);
  });

  testWidgets('a failed save says why', (tester) async {
    saveWorkdirFile = ({required String source, required String name}) async {
      throw Exception('disk full');
    };
    await pump(
      tester,
      screen(
        downloads: const [
          WorkdirDownloadRow(
            path: 'out/c.zip',
            label: 'downloaded',
            failed: false,
            canCancel: false,
            localPath: '/data/blobs/c',
          ),
        ],
      ),
    );
    await tester.tap(find.text('Save…'));
    await tester.pumpAndSettle();
    expect(
      find.textContaining('Could not save out/c.zip', findRichText: true),
      findsOneWidget,
    );
    expect(find.textContaining('Saved out/c.zip'), findsNothing);
  });

  testWidgets('the dir pane pushes a picked file under the shown directory', (
    tester,
  ) async {
    pickPushFile = () async => '/home/u/notes.txt';
    final sent = await pump(
      tester,
      screen(pane: WorkdirPaneKind.dir, path: 'src'),
    );
    await tester.tap(find.byKey(const ValueKey('workdir-push')));
    await tester.pumpAndSettle();
    expect(
      tester.widget<EditableText>(find.byType(EditableText)).controller.text,
      'src/notes.txt',
    );
    await tester.tap(find.byKey(const Key('push-target-ok')));
    await tester.pumpAndSettle();
    expect(sent, [
      const GuiIntentAct(
        action: ActionIntentPushWorkdirFile(
          sessionId: 's-1',
          source: '/home/u/notes.txt',
          path: 'src/notes.txt',
        ),
      ),
    ]);

    await tester.tap(find.byKey(const ValueKey('workdir-push')));
    await tester.pumpAndSettle();
    await tester.enterText(
      find.byKey(const Key('push-target-field')),
      'docs/n.txt',
    );
    await tester.tap(find.byKey(const Key('push-target-ok')));
    await tester.pumpAndSettle();
    expect(sent, hasLength(2));
    expect(
      sent.last,
      const GuiIntentAct(
        action: ActionIntentPushWorkdirFile(
          sessionId: 's-1',
          source: '/home/u/notes.txt',
          path: 'docs/n.txt',
        ),
      ),
    );
  });

  testWidgets('a cancelled pick or dialog pushes nothing; only the dir pane '
      'offers Push', (tester) async {
    pickPushFile = () async => null;
    final sent = await pump(
      tester,
      screen(pane: WorkdirPaneKind.dir, path: 'src'),
    );
    expect(find.byKey(const ValueKey('workdir-push')), findsOneWidget);
    await tester.tap(find.byKey(const ValueKey('workdir-push')));
    await tester.pumpAndSettle();
    expect(find.byKey(const Key('push-target-field')), findsNothing);
    expect(sent, isEmpty);

    pickPushFile = () async => '/home/u/notes.txt';
    await tester.tap(find.byKey(const ValueKey('workdir-push')));
    await tester.pumpAndSettle();
    expect(find.byKey(const Key('push-target-field')), findsOneWidget);
    await tester.tap(find.byKey(const Key('push-target-cancel')));
    await tester.pumpAndSettle();
    expect(sent, isEmpty);

    await pump(
      tester,
      screen(pane: WorkdirPaneKind.file, path: 'bin/app', header: 'bin/app'),
    );
    expect(find.byKey(const ValueKey('workdir-push')), findsNothing);
  });

  testWidgets('the pushes panel replaces after asking, and retries', (
    tester,
  ) async {
    final sha = List.filled(32, 'ab').join();
    final sent = await pump(
      tester,
      screen(
        pushes: [
          WorkdirPushRow(
            path: 'a.bin',
            source: '/home/u/a.bin',
            label: 'Sending…',
            pushing: true,
            done: false,
            failed: false,
            canCancel: false,
          ),
          WorkdirPushRow(
            path: 'out/c.zip',
            source: '/home/u/c.zip',
            label: 'A file is already there (3 B)',
            pushing: false,
            done: false,
            existingSha256: sha,
            failed: false,
            canCancel: false,
          ),
          WorkdirPushRow(
            path: 'b.bin',
            source: '/home/u/b.bin',
            label: 'Push failed: timed out',
            pushing: false,
            done: false,
            failed: true,
            canCancel: false,
          ),
        ],
      ),
    );
    for (final text in [
      'Sending…',
      'A file is already there (3 B)',
      'Push failed: timed out',
    ]) {
      expect(find.text(text), findsOneWidget, reason: text);
    }

    await tester.ensureVisible(find.text('Replace…'));
    await tester.pump();
    await tester.tap(find.text('Replace…'));
    await tester.pumpAndSettle();
    expect(
      find.text('Replace out/c.zip in the workdir with c.zip?'),
      findsOneWidget,
    );
    await tester.tap(find.byKey(const Key('confirm-cancel')));
    await tester.pumpAndSettle();
    expect(sent, isEmpty);

    await tester.tap(find.text('Replace…'));
    await tester.pumpAndSettle();
    await tester.tap(find.byKey(const Key('confirm-ok')));
    await tester.pumpAndSettle();
    expect(sent, [
      GuiIntentAct(
        action: ActionIntentPushWorkdirFile(
          sessionId: 's-1',
          source: '/home/u/c.zip',
          path: 'out/c.zip',
          replace: sha,
        ),
      ),
    ]);

    await tester.ensureVisible(find.text('Retry'));
    await tester.pump();
    await tester.tap(find.text('Retry'));
    expect(sent, hasLength(2));
    expect(
      sent.last,
      const GuiIntentAct(
        action: ActionIntentPushWorkdirFile(
          sessionId: 's-1',
          source: '/home/u/b.bin',
          path: 'b.bin',
        ),
      ),
    );
  });

  testWidgets(
    'a download or push under way cancels; a stopped download resumes',
    (tester) async {
      final sent = await pump(
        tester,
        screen(
          downloads: const [
            WorkdirDownloadRow(
              path: 'a.bin',
              label: 'Asking the server…',
              failed: false,
              canCancel: true,
            ),
            WorkdirDownloadRow(
              path: 'b.bin',
              label: 'not downloaded',
              failed: false,
              canCancel: false,
              blob: BlobFacts(
                blobId: 'b-1',
                state: BlobStateKind.notDownloaded,
                label: 'not downloaded',
                canDownload: true,
              ),
            ),
          ],
          pushes: const [
            WorkdirPushRow(
              path: 'c.bin',
              source: '/home/u/c.bin',
              label: 'Sending…',
              pushing: true,
              done: false,
              failed: false,
              canCancel: true,
            ),
          ],
        ),
      );
      await tester.ensureVisible(
        find.byKey(const ValueKey('download-cancel-a.bin')),
      );
      await tester.pump();
      await tester.tap(find.byKey(const ValueKey('download-cancel-a.bin')));
      await tester.ensureVisible(
        find.byKey(const ValueKey('download-resume-b.bin')),
      );
      await tester.pump();
      await tester.tap(find.byKey(const ValueKey('download-resume-b.bin')));
      await tester.ensureVisible(
        find.byKey(const ValueKey('push-cancel-c.bin')),
      );
      await tester.pump();
      await tester.tap(find.byKey(const ValueKey('push-cancel-c.bin')));
      expect(sent, [
        const GuiIntentAct(
          action: ActionIntentCancelWorkdirDownload(
            sessionId: 's-1',
            path: 'a.bin',
          ),
        ),
        const GuiIntentAct(action: ActionIntentDownloadBlob(blobId: 'b-1')),
        const GuiIntentAct(
          action: ActionIntentCancelWorkdirPush(
            sessionId: 's-1',
            path: 'c.bin',
          ),
        ),
      ]);
    },
  );

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
