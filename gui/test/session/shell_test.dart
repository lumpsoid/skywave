import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/session/shell_holder.dart';
import 'package:skywave_gui/session/timeline_holder.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

import '../fakes.dart';
import 'rows.dart';

ComposerView shellComposer({
  String text = '',
  int rev = 1,
  String? whenLabel,
  bool canStop = false,
  ComposerModeView mode = ComposerModeView.shell,
  bool shellEnabled = true,
}) => ComposerView(
  text: text,
  cursorUtf16: text.length,
  rev: rev,
  mode: mode,
  shellEnabled: shellEnabled,
  promptLabel: mode == ComposerModeView.shell ? r'work $' : null,
  whenLabel: whenLabel,
  canStop: canStop,
);

ShellRunRow shellRun(
  String key,
  String command, {
  ShellRunKind state = ShellRunKind.running,
  String stateLabel = 'running 3s',
  bool canStop = true,
  bool discardable = false,
  String tail = '',
}) => ShellRunRow(
  key: key,
  command: command,
  state: state,
  stateLabel: stateLabel,
  tail: tail,
  lines: tail.isEmpty ? 0 : tail.split('\n').length,
  truncated: false,
  stopping: false,
  discardable: discardable,
  canStop: canStop,
);

TimelineHolder timelineOf(String text) => TimelineHolder()
  ..apply(TimelineUpdateSnapshot(sessionId: 's1', items: [row('s:1', text)]));

IconButton iconButton(WidgetTester tester, String key) =>
    tester.widget<IconButton>(find.byKey(Key(key)));

void main() {
  test('a snapshot, then diffs, fold into the runs Rust holds', () {
    final holder = ShellHolder();
    var notified = 0;
    holder
      ..addListener(() => notified++)
      ..apply(
        ShellUpdateSnapshot(
          sessionId: 's1',
          runs: [shellRun('a', 'make'), shellRun('b', 'ls')],
        ),
      )
      ..apply(
        ShellUpdateUpserted(
          key: 'a',
          run: shellRun('a', 'make', stateLabel: 'exit 0'),
          index: 0,
        ),
      )
      ..apply(
        ShellUpdateUpserted(key: 'c', run: shellRun('c', 'pwd'), index: 2),
      )
      ..apply(const ShellUpdateRemoved(key: 'b'));
    expect(holder.sessionId, 's1');
    expect(
      [for (final run in holder.runs) '${run.key}=${run.stateLabel}'],
      ['a=exit 0', 'c=running 3s'],
    );
    expect(notified, 4);
  });

  testWidgets('the shell toggle is hidden while the server runs no shell '
      'commands, and switches modes when shown', (tester) async {
    final sent = <GuiIntent>[];
    await pumpSession(
      tester,
      screen: screenWith(
        composer: shellComposer(
          mode: ComposerModeView.agent,
          shellEnabled: false,
        ),
      ),
      timeline: timelineOf('hello'),
      sent: sent,
    );
    expect(find.byKey(const Key('shell-toggle')), findsNothing);
    // Positive control: the same composer with the server's shell on.
    await pumpSession(
      tester,
      screen: screenWith(composer: shellComposer(mode: ComposerModeView.agent)),
      timeline: timelineOf('hello'),
      sent: sent,
    );
    await tester.tap(find.byKey(const Key('shell-toggle')));
    expect(sent.whereType<GuiIntentToggleShellMode>().single.sessionId, 's1');
    // In shell mode it stays, to switch back, even with the shell off.
    await pumpSession(
      tester,
      screen: screenWith(composer: shellComposer(shellEnabled: false)),
      timeline: timelineOf('hello'),
      sent: sent,
    );
    expect(find.byKey(const Key('shell-toggle')), findsOneWidget);
  });

  testWidgets('shell mode swaps the timeline for the runs and shows the '
      'cwd prompt', (tester) async {
    final sent = <GuiIntent>[];
    final shell = ShellHolder()
      ..apply(
        ShellUpdateSnapshot(
          sessionId: 's1',
          runs: [
            shellRun('r1', 'cargo test', tail: 'running 3 tests'),
            shellRun(
              'r2',
              'make',
              state: ShellRunKind.pending,
              stateLabel: 'queued 2s',
              discardable: true,
            ),
          ],
        ),
      );
    await pumpSession(
      tester,
      screen: screenWith(composer: shellComposer()),
      timeline: timelineOf('from the agent'),
      shell: shell,
      sent: sent,
    );
    expect(find.text('from the agent'), findsNothing);
    expect(find.text('cargo test'), findsOneWidget);
    expect(find.text('running 3 tests'), findsOneWidget);
    expect(find.text('queued 2s'), findsOneWidget);
    expect(find.byKey(const Key('shell-prompt')), findsOneWidget);
    expect(find.text(r'work $'), findsOneWidget);

    // Each run's own stop names it.
    await tester.tap(find.byTooltip('Stop this run').last);
    expect(sent.whereType<GuiIntentStopShellRun>().single.runId, 'r1');
    await tester.tap(find.text('Discard'));
    final discard = sent.whereType<GuiIntentAct>().single.action;
    expect((discard as ActionIntentDiscard).commandId, 'r2');

    // Positive control: agent mode shows the timeline.
    await pumpSession(
      tester,
      screen: screenWith(composer: shellComposer(mode: ComposerModeView.agent)),
      timeline: timelineOf('from the agent'),
      shell: shell,
      sent: sent,
    );
    expect(find.text('from the agent'), findsOneWidget);
    expect(find.text('cargo test'), findsNothing);
  });

  testWidgets('the condition chip steps, and resets once a run is sent', (
    tester,
  ) async {
    final sent = <GuiIntent>[];
    final timeline = timelineOf('hello');
    await pumpSession(
      tester,
      screen: screenWith(composer: shellComposer(text: 'make')),
      timeline: timeline,
      sent: sent,
    );
    final chip = find.byKey(const Key('shell-condition'));
    expect(
      find.descendant(of: chip, matching: find.text('always')),
      findsOneWidget,
    );
    await tester.tap(chip);
    expect(sent.whereType<GuiIntentCycleCondition>(), hasLength(1));
    // The hub says the condition stepped.
    await pumpSession(
      tester,
      screen: screenWith(
        composer: shellComposer(text: 'make', whenLabel: 'if ok ✓'),
      ),
      timeline: timeline,
      sent: sent,
    );
    expect(
      find.descendant(of: chip, matching: find.text('if ok ✓')),
      findsOneWidget,
    );
    await tester.tap(find.byTooltip('Run'));
    expect(sent.whereType<GuiIntentComposerSubmit>(), hasLength(1));
    // The hub took the run: cleared, condition back to always.
    await pumpSession(
      tester,
      screen: screenWith(composer: shellComposer(rev: 2)),
      timeline: timeline,
      sent: sent,
    );
    expect(
      find.descendant(of: chip, matching: find.text('always')),
      findsOneWidget,
    );
    expect(find.text('if ok ✓'), findsNothing);
  });

  testWidgets('the stop button is off while there is nothing to stop', (
    tester,
  ) async {
    final sent = <GuiIntent>[];
    final timeline = timelineOf('hello');
    await pumpSession(
      tester,
      screen: screenWith(composer: shellComposer()),
      timeline: timeline,
      sent: sent,
    );
    expect(iconButton(tester, 'shell-stop').onPressed, isNull);
    // Positive control: something to stop.
    await pumpSession(
      tester,
      screen: screenWith(composer: shellComposer(canStop: true)),
      timeline: timeline,
      sent: sent,
    );
    expect(iconButton(tester, 'shell-stop').onPressed, isNotNull);
    await tester.tap(find.byKey(const Key('shell-stop')));
    expect(sent.whereType<GuiIntentStopShell>().single.sessionId, 's1');
  });

  testWidgets('a saved command is put in with a tap and run with a long '
      'press', (tester) async {
    final sent = <GuiIntent>[];
    const build = SnippetView(
      snippetId: 'n1',
      label: 'build',
      detail: 'cargo build',
      name: 'build',
      text: 'cargo build',
    );
    await pumpSession(
      tester,
      screen: screenWith(composer: shellComposer(), snippets: const [build]),
      timeline: timelineOf('hello'),
      sent: sent,
    );
    await tester.tap(find.byTooltip('Saved commands'));
    await tester.pumpAndSettle();
    expect(find.text('Saved commands'), findsOneWidget);
    await tester.longPress(find.text('build'));
    await tester.pumpAndSettle();
    expect(sent.whereType<GuiIntentRunSnippet>().single.snippetId, 'n1');
    expect(sent.whereType<GuiIntentInsertSnippet>(), isEmpty);

    await tester.tap(find.byTooltip('Saved commands'));
    await tester.pumpAndSettle();
    await tester.tap(find.text('build'));
    await tester.pumpAndSettle();
    expect(sent.whereType<GuiIntentInsertSnippet>().single.snippetId, 'n1');
  });
}
