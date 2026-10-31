import 'package:flutter/foundation.dart';
import 'package:flutter/gestures.dart';
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/session/composer.dart';
import 'package:skywave_gui/session/session_page.dart';
import 'package:skywave_gui/session/search_holder.dart';
import 'package:skywave_gui/session/status_line.dart';
import 'package:skywave_gui/session/timeline_holder.dart';
import 'package:skywave_gui/session/timeline_view.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

import '../fakes.dart';
import 'rows.dart';

void tall(WidgetTester tester) {
  tester.view
    ..physicalSize = const Size(1200, 3000)
    ..devicePixelRatio = 1;
  addTearDown(tester.view.reset);
}

TimelineHolder holding(List<TimelineRow> rows) =>
    TimelineHolder()
      ..apply(TimelineUpdateSnapshot(sessionId: 's1', items: rows));

Finder listScrollable() => find
    .descendant(of: find.byType(ListView), matching: find.byType(Scrollable))
    .first;

double scrolled(WidgetTester tester) =>
    tester.state<ScrollableState>(listScrollable()).position.pixels;

void main() {
  testWidgets('every row kind renders', (tester) async {
    tall(tester);
    final timeline = holding([
      anyRow(
        's:1',
        RowKind.userMessage,
        'from the laptop',
        author: 'laptop',
        timeLabel: '14:02',
      ),
      anyRow(
        'p:c1',
        RowKind.pendingMessage,
        'on its way',
        author: 'you',
        own: true,
        discardable: true,
        commandId: 'c1',
        outgoing: const OutgoingView(
          state: OutgoingState.queued,
          label: 'queued',
        ),
        deliveryLabel: 'queued 3s ago',
      ),
      anyRow('s:2', RowKind.agentMessage, 'agent says', author: 'agent'),
      anyRow(
        's:3',
        RowKind.tool,
        '',
        tool: shellFacts(r'$ ls -la'),
        turnStart: true,
      ),
      anyRow('s:4', RowKind.toolCall, '', tool: shellFacts('legacy call')),
      anyRow('s:5', RowKind.toolResult, '', tool: shellFacts('legacy result')),
      anyRow(
        's:6',
        RowKind.approval,
        '',
        tool: shellFacts('rm -rf', state: ToolState.needsApproval),
      ),
      anyRow(
        's:7',
        RowKind.interrupted,
        'interrupted · the agent stopped its turn',
        unreadRule: true,
      ),
      anyRow('s:8', RowKind.stopped, 'stopped · a message restarts it'),
      anyRow('s:9', RowKind.renamed, 'renamed to “x” by you'),
      anyRow('s:10', RowKind.archived, 'archived by you'),
      anyRow('s:11', RowKind.agentError, 'it broke'),
      anyRow('s:12', RowKind.turnUsage, r'turn · 12.4s · $0.04'),
      anyRow('s:13', RowKind.agentSetting, 'mode → plan by laptop'),
      anyRow(
        's:14',
        RowKind.settingRefused,
        'mode bypass refused: not launched for it',
      ),
    ]);
    final sent = <GuiIntent>[];
    await pumpSession(
      tester,
      screen: screenWith(),
      timeline: timeline,
      sent: sent,
    );

    for (final text in [
      'from the laptop',
      'laptop',
      '14:02',
      'on its way',
      'queued',
      'queued 3s ago',
      'Edit',
      'Discard',
      'agent says',
      r'Bash  $ ls -la',
      'Bash  legacy call',
      'Bash  legacy result',
      'Bash  rm -rf',
      'interrupted · the agent stopped its turn',
      'new',
      'stopped · a message restarts it',
      'renamed to “x” by you',
      'archived by you',
      'agent error',
      'it broke',
      r'turn · 12.4s · $0.04',
      'mode → plan by laptop',
      'mode bypass refused: not launched for it',
    ]) {
      expect(find.text(text, findRichText: true), findsWidgets, reason: text);
    }

    await tester.tap(find.text('Discard'));
    expect(
      sent.last,
      const GuiIntentAct(action: ActionIntentDiscard(commandId: 'c1')),
    );
    await tester.tap(find.text('Edit'));
    expect(
      sent.last,
      const GuiIntentAct(action: ActionIntentEditQueued(commandId: 'c1')),
    );
  });

  testWidgets('a logged message replacing its pending one keeps the selection '
      'and the scroll', (tester) async {
    final timeline = holding([
      for (var i = 0; i < 30; i++) row('s:$i', 'row $i'),
      message('p:c1', 'mine', pending: true),
      for (var i = 30; i < 60; i++) row('s:$i', 'row $i'),
    ]);
    final sent = <GuiIntent>[];
    await pumpSession(
      tester,
      screen: screenWith(),
      timeline: timeline,
      sent: sent,
    );
    // Scroll back until the pending message shows.
    await tester.scrollUntilVisible(
      find.text('mine'),
      -200,
      scrollable: listScrollable(),
    );
    await tester.pumpAndSettle();
    await tester.tap(find.text('mine'));
    await tester.pump();
    expect(timeline.selectedKey, 'p:c1');
    final offset = scrolled(tester);
    final where = tester.getTopLeft(find.text('mine'));

    timeline
      ..apply(const TimelineUpdateRemoved(key: 'p:c1'))
      ..apply(
        TimelineUpdateUpserted(
          key: 's:99',
          item: message('s:99', 'mine', pending: false),
          index: 30,
          replaces: 'p:c1',
        ),
      );
    await tester.pumpAndSettle();

    expect(timeline.selectedKey, 's:99');
    expect(scrolled(tester), offset);
    expect(tester.getTopLeft(find.text('mine')), where);
  });

  testWidgets('a tap expands or collapses an item; Download fetches its blob', (
    tester,
  ) async {
    tall(tester);
    const blob = BlobFacts(
      blobId: 'b1',
      state: BlobStateKind.notDownloaded,
      label: 'not downloaded · 4.0 KiB',
      canDownload: true,
    );
    final timeline = holding([
      anyRow(
        's:1',
        RowKind.tool,
        '',
        tool: shellFacts('cat a'),
        expandable: true,
      ),
      anyRow(
        's:2',
        RowKind.tool,
        '',
        tool: shellFacts('cat b'),
        expandable: true,
        expanded: true,
        sections: [
          lines(['first'], blob: blob, note: '… +12 lines'),
          lines(
            ['big'],
            blob: const BlobFacts(
              blobId: 'b2',
              state: BlobStateKind.available,
              label: 'downloaded',
              canDownload: false,
            ),
            cutShort: true,
          ),
        ],
      ),
    ]);
    final sent = <GuiIntent>[];
    await pumpSession(
      tester,
      screen: screenWith(),
      timeline: timeline,
      sent: sent,
    );
    sent.clear();

    await tester.tap(find.text('Bash  cat a', findRichText: true));
    expect(sent, [const GuiIntentExpandItem(key: 's:1')]);
    expect(timeline.selectedKey, 's:1');
    await tester.tap(find.text('Bash  cat b', findRichText: true));
    expect(sent.last, const GuiIntentCollapseItem(key: 's:2'));

    expect(find.text('not downloaded · 4.0 KiB'), findsOneWidget);
    expect(find.text('… +12 lines'), findsOneWidget);
    await tester.tap(find.text('Download'));
    expect(
      sent.last,
      const GuiIntentAct(action: ActionIntentDownloadBlob(blobId: 'b1')),
    );
    await tester.tap(find.text('Open full file'));
    expect(sent.last, const GuiIntentOpenFull(blobId: 'b2'));

    // The menu downloads a collapsed item's content, and copies.
    await tester.longPress(find.text('Bash  cat a', findRichText: true));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Download').last);
    await tester.pumpAndSettle();
    expect(sent.last, const GuiIntentDownloadItem(key: 's:1'));

    final at = tester.getCenter(find.text('Bash  cat a', findRichText: true));
    await tester.tapAt(at, buttons: kSecondaryButton);
    await tester.pumpAndSettle();
    await tester.tap(find.text('Copy call'));
    await tester.pumpAndSettle();
    expect(sent.last, const GuiIntentCopy(key: 's:1', what: CopyWhat.call));
  });

  group('approval', () {
    ApprovalPanel panel(List<RuleOption> suggestions) => ApprovalPanel(
      approvalId: 'a1',
      gatedKey: 's:1',
      kind: ToolKindView.shell,
      tool: 'Bash',
      title: r'$ rm -rf build',
      canAnswer: true,
      suggestions: suggestions,
      alwaysPicks: suggestions.length > 1,
      alwaysLabel: suggestions.isEmpty ? null : 'always allow · …',
    );

    GuiIntent resolve(DecisionView decision, [int? rule]) => GuiIntentAct(
      action: ActionIntentResolveApproval(
        approvalId: 'a1',
        decision: decision,
        rule: rule,
      ),
    );

    testWidgets('approve, deny, and always allow its one rule', (tester) async {
      final sent = <GuiIntent>[];
      await pumpSession(
        tester,
        screen: screenWith(
          approval: panel([const RuleOption(index: 0, label: 'Bash(rm:*)')]),
        ),
        timeline: holding([]),
        sent: sent,
      );
      expect(find.text('Approval needed'), findsOneWidget);
      await tester.tap(find.text('Approve'));
      expect(sent.last, resolve(DecisionView.approve));
      await tester.tap(find.text('Deny'));
      expect(sent.last, resolve(DecisionView.deny));
      await tester.tap(find.text('Always allow'));
      await tester.pumpAndSettle();
      expect(find.byType(SimpleDialog), findsNothing);
      expect(sent.last, resolve(DecisionView.approve, 0));
    });

    testWidgets('several rules: a picker chooses which', (tester) async {
      final sent = <GuiIntent>[];
      await pumpSession(
        tester,
        screen: screenWith(
          approval: panel(const [
            RuleOption(index: 0, label: 'Bash(rm:*) (project)'),
            RuleOption(index: 1, label: 'Bash(*) (session)'),
          ]),
        ),
        timeline: holding([]),
        sent: sent,
      );
      sent.clear();
      await tester.tap(find.text('Always allow…'));
      await tester.pumpAndSettle();
      expect(find.byType(SimpleDialog), findsOneWidget);
      expect(sent, isEmpty);
      await tester.tap(find.text('Bash(*) (session)'));
      await tester.pumpAndSettle();
      expect(sent, [resolve(DecisionView.approve, 1)]);
    });

    testWidgets('answered: the answer shows and the buttons go', (
      tester,
    ) async {
      await pumpSession(
        tester,
        screen: screenWith(
          approval: const ApprovalPanel(
            approvalId: 'a1',
            kind: ToolKindView.shell,
            tool: 'Bash',
            title: 'ls',
            canAnswer: false,
            suggestions: [],
            alwaysPicks: false,
            answerLabel: 'you approved · queued 3s ago',
          ),
        ),
        timeline: holding([]),
        sent: [],
      );
      expect(find.text('you approved · queued 3s ago'), findsOneWidget);
      expect(find.text('Approve'), findsNothing);
      expect(find.text('Show call'), findsNothing);
    });
  });

  group('composer', () {
    Finder composer() => find.byType(TextField).last;

    testWidgets('each edit is DraftEdited with a UTF-16 cursor', (
      tester,
    ) async {
      final sent = <GuiIntent>[];
      await pumpSession(
        tester,
        screen: screenWith(),
        timeline: holding([]),
        sent: sent,
      );
      sent.clear();
      await tester.enterText(composer(), 'a👋');
      expect(
        sent.whereType<GuiIntentDraftEdited>().last,
        const GuiIntentDraftEdited(
          sessionId: 's1',
          text: 'a👋',
          cursorUtf16: 3,
        ),
      );
      // Moving the cursor is an edit too.
      final field = tester.widget<TextField>(composer()).controller!;
      field.selection = const TextSelection.collapsed(offset: 1);
      expect(
        sent.last,
        const GuiIntentDraftEdited(
          sessionId: 's1',
          text: 'a👋',
          cursorUtf16: 1,
        ),
      );
    });

    testWidgets('on desktop Enter sends text and Shift+Enter does not', (
      tester,
    ) async {
      debugDefaultTargetPlatformOverride = TargetPlatform.linux;
      try {
        final sent = <GuiIntent>[];
        await pumpSession(
          tester,
          screen: screenWith(),
          timeline: holding([]),
          sent: sent,
        );
        // A blank field sends nothing, and Enter adds no line to it.
        await tester.tap(composer());
        await tester.enterText(composer(), '  ');
        sent.clear();
        await tester.sendKeyEvent(LogicalKeyboardKey.enter);
        expect(sent.whereType<GuiIntentComposerSubmit>(), isEmpty);
        expect(tester.widget<TextField>(composer()).controller!.text, '  ');

        await tester.enterText(composer(), 'hi');
        sent.clear();
        await tester.sendKeyDownEvent(LogicalKeyboardKey.shiftLeft);
        await tester.sendKeyEvent(LogicalKeyboardKey.enter);
        await tester.sendKeyUpEvent(LogicalKeyboardKey.shiftLeft);
        expect(sent.whereType<GuiIntentComposerSubmit>(), isEmpty);
        await tester.sendKeyEvent(LogicalKeyboardKey.enter);
        expect(sent.whereType<GuiIntentComposerSubmit>(), [
          const GuiIntentComposerSubmit(sessionId: 's1'),
        ]);
        await tester.sendKeyDownEvent(LogicalKeyboardKey.controlLeft);
        await tester.sendKeyEvent(LogicalKeyboardKey.keyP);
        await tester.sendKeyUpEvent(LogicalKeyboardKey.controlLeft);
        expect(sent.last, const GuiIntentHistoryOlder());
      } finally {
        debugDefaultTargetPlatformOverride = null;
      }
    });

    testWidgets('the send button sends, only once there is text', (
      tester,
    ) async {
      final sent = <GuiIntent>[];
      final timeline = holding([]);
      await pumpSession(
        tester,
        screen: screenWith(),
        timeline: timeline,
        sent: sent,
      );
      IconButton send() => tester.widget<IconButton>(
        find.ancestor(
          of: find.byIcon(Icons.arrow_upward),
          matching: find.byType(IconButton),
        ),
      );
      expect(send().onPressed, isNull);
      await tester.enterText(composer(), ' \n\t ');
      await tester.pump();
      expect(send().onPressed, isNull, reason: 'whitespace is blank');
      await tester.tap(find.byTooltip('Send'));
      expect(sent.whereType<GuiIntentComposerSubmit>(), isEmpty);

      await tester.enterText(composer(), 'hi');
      await tester.pump();
      expect(send().onPressed, isNotNull);
      await tester.tap(find.byTooltip('Send'));
      expect(sent.last, const GuiIntentComposerSubmit(sessionId: 's1'));

      // The hub clears the sent message: off again.
      await pumpSession(
        tester,
        screen: screenWith(
          composer: const ComposerView(text: '', cursorUtf16: 0, rev: 2),
        ),
        timeline: timeline,
        sent: sent,
      );
      expect(send().onPressed, isNull);
      // A queued message taken back to edit fills the field: on.
      await pumpSession(
        tester,
        screen: screenWith(
          composer: const ComposerView(text: 'queued', cursorUtf16: 6, rev: 3),
        ),
        timeline: timeline,
        sent: sent,
      );
      expect(send().onPressed, isNotNull);
    });

    testWidgets('a new rev overwrites the field, the same one does not', (
      tester,
    ) async {
      final sent = <GuiIntent>[];
      final timeline = holding([]);
      await pumpSession(
        tester,
        screen: screenWith(),
        timeline: timeline,
        sent: sent,
      );
      await tester.enterText(composer(), 'mine');
      await pumpSession(
        tester,
        screen: screenWith(
          composer: const ComposerView(text: 'old', cursorUtf16: 0, rev: 1),
        ),
        timeline: timeline,
        sent: sent,
      );
      expect(find.text('mine'), findsOneWidget);
      sent.clear();
      await pumpSession(
        tester,
        screen: screenWith(
          composer: const ComposerView(
            text: 'recalled 👋',
            cursorUtf16: 11,
            rev: 2,
            historyLabel: '2 of 5',
          ),
        ),
        timeline: timeline,
        sent: sent,
      );
      expect(find.text('recalled 👋'), findsOneWidget);
      expect(find.text('2 of 5'), findsOneWidget);
      final field = tester.widget<TextField>(composer()).controller!;
      expect(field.selection.extentOffset, 11);
      // Taking the hub's text is not an edit to report back.
      expect(sent.whereType<GuiIntentDraftEdited>(), isEmpty);
    });
  });

  testWidgets('search marks its UTF-16 ranges, past an emoji', (tester) async {
    final search = SearchHolder();
    final sent = <GuiIntent>[];
    await pumpSession(
      tester,
      screen: screenWith(),
      timeline: holding([row('s:1', 'héllo 👋 wave'), row('s:2', 'no match')]),
      search: search,
      sent: sent,
    );
    search.apply(
      const SearchUpdate(
        open: true,
        query: 'w',
        countLabel: '1 of 1',
        wrapped: true,
        partialLabel: '3 outputs not downloaded',
        currentKey: 's:1',
        hits: [
          SearchHit(
            key: 's:1',
            // '👋 w': 👋 is two UTF-16 units at 6..8.
            text: [Utf16Range(start: 6, end: 10)],
            title: [],
            detail: [],
            lines: [],
          ),
        ],
      ),
    );
    await tester.pumpAndSettle();

    expect(find.text('1 of 1'), findsOneWidget);
    expect(
      find.text('search wrapped · 3 outputs not downloaded'),
      findsOneWidget,
    );
    final marked = <String>[];
    for (final text in tester.widgetList<RichText>(find.byType(RichText))) {
      text.text.visitChildren((span) {
        if (span is TextSpan &&
            span.text != null &&
            span.style?.backgroundColor != null) {
          marked.add(span.text!);
        }
        return true;
      });
    }
    expect(marked, ['👋 w']);

    // Enter steps from the newest row in view; ↑ steps older; Esc closes.
    sent.clear();
    await tester.showKeyboard(find.byType(TextField).first);
    await tester.testTextInput.receiveAction(TextInputAction.search);
    await tester.pump();
    expect(sent, [const GuiIntentSearchStep(fromKey: 's:2')]);
    await tester.sendKeyEvent(LogicalKeyboardKey.keyN);
    expect(sent.last, const GuiIntentSearchStep(older: true));
    await tester.sendKeyEvent(LogicalKeyboardKey.escape);
    expect(sent.last, const GuiIntentSearchClose());
  });

  testWidgets('scrolling away from the newest row stops following, and back '
      'starts again', (tester) async {
    final timeline = holding([
      for (var i = 0; i < 60; i++) row('s:$i', 'row $i'),
    ]);
    final sent = <GuiIntent>[];
    await pumpSession(
      tester,
      screen: screenWith(),
      timeline: timeline,
      sent: sent,
    );
    expect(sent, [const GuiIntentFollowingChanged(following: true)]);
    expect(find.text('row 59'), findsOneWidget);

    await tester.drag(find.byType(ListView), const Offset(0, 400));
    await tester.pumpAndSettle();
    expect(sent.last, const GuiIntentFollowingChanged(following: false));
    expect(find.byTooltip('Jump to newest'), findsOneWidget);

    // New rows do not move a view scrolled back.
    final offset = scrolled(tester);
    timeline.apply(
      TimelineUpdateUpserted(
        key: 's:60',
        item: row('s:60', 'row 60'),
        index: 60,
      ),
    );
    await tester.pumpAndSettle();
    expect(scrolled(tester), offset);

    await tester.drag(find.byType(ListView), const Offset(0, -4000));
    await tester.pumpAndSettle();
    expect(sent.last, const GuiIntentFollowingChanged(following: true));
    expect(
      sent.whereType<GuiIntentFollowingChanged>().length,
      3,
      reason: 'only changes are sent',
    );

    // Following, a new row scrolls into view.
    timeline.apply(
      TimelineUpdateUpserted(
        key: 's:61',
        item: row('s:61', 'row 61'),
        index: 61,
      ),
    );
    await tester.pumpAndSettle();
    expect(find.text('row 61'), findsOneWidget);
  });

  testWidgets('the current match scrolls into view, far from the end', (
    tester,
  ) async {
    final search = SearchHolder();
    final sent = <GuiIntent>[];
    await pumpSession(
      tester,
      screen: screenWith(),
      timeline: holding([for (var i = 0; i < 200; i++) row('s:$i', 'row $i')]),
      search: search,
      sent: sent,
    );
    expect(find.text('row 3'), findsNothing);
    search.apply(
      const SearchUpdate(
        open: true,
        query: 'row 3',
        countLabel: '1 of 11',
        wrapped: false,
        currentKey: 's:3',
        hits: [],
      ),
    );
    await tester.pumpAndSettle();
    expect(
      find.descendant(of: find.byType(ListView), matching: find.text('row 3')),
      findsOneWidget,
    );
    expect(sent.last, const GuiIntentFollowingChanged(following: false));
  });

  testWidgets('Ctrl+F opens a search; Ctrl+X interrupts', (tester) async {
    final sent = <GuiIntent>[];
    await pumpSession(
      tester,
      screen: screenWith(),
      timeline: holding([row('s:1', 'a')]),
      sent: sent,
    );
    await tester.tap(find.text('a'));
    await tester.sendKeyDownEvent(LogicalKeyboardKey.controlLeft);
    await tester.sendKeyEvent(LogicalKeyboardKey.keyF);
    await tester.sendKeyEvent(LogicalKeyboardKey.keyX);
    await tester.sendKeyUpEvent(LogicalKeyboardKey.controlLeft);
    expect(sent.sublist(sent.length - 2), [
      const GuiIntentSearchTyped(query: ''),
      const GuiIntentAct(action: ActionIntentInterrupt(sessionId: 's1')),
    ]);
  });

  testWidgets('interrupt only while there is a turn and none on its way', (
    tester,
  ) async {
    final sent = <GuiIntent>[];
    Future<void> show(SessionScreen screen) async {
      await tester.pumpWidget(
        MaterialApp(
          home: SessionPage(
            screen: screen,
            timeline: holding([row('s:1', 'a')]),
            send: sent.add,
          ),
        ),
      );
      await tester.pump();
    }

    Future<void> ctrlX() async {
      await tester.sendKeyDownEvent(LogicalKeyboardKey.controlLeft);
      await tester.sendKeyEvent(LogicalKeyboardKey.keyX);
      await tester.sendKeyUpEvent(LogicalKeyboardKey.controlLeft);
    }

    await show(screenWith(canInterrupt: false));
    await tester.tap(find.text('a'));
    await tester.tap(find.byTooltip('Interrupt (Ctrl+X)'));
    await ctrlX();
    expect(sent.whereType<GuiIntentAct>(), isEmpty, reason: 'agent idle');

    await show(screenWith(interruptBusy: true));
    expect(find.byType(CircularProgressIndicator), findsOneWidget);
    await tester.tap(find.byTooltip('Interrupting…'));
    await ctrlX();
    expect(sent.whereType<GuiIntentAct>(), isEmpty, reason: 'on its way');

    await show(screenWith(interruptRefusal: 'the server could not take it'));
    expect(
      find.byTooltip('Interrupt failed: the server could not take it'),
      findsOneWidget,
    );
    expect(find.textContaining('interrupt'), findsNothing);
    await ctrlX();
    expect(
      sent.last,
      const GuiIntentAct(action: ActionIntentInterrupt(sessionId: 's1')),
      reason: 'a failed interrupt can be tried again',
    );
  });

  testWidgets('a refusal that arrives is said once in a snack bar', (
    tester,
  ) async {
    Future<void> show(SessionScreen screen) =>
        pumpSession(tester, screen: screen, timeline: holding([]), sent: []);
    AgentSettingsView refusing({String? mode, String? model}) =>
        AgentSettingsView(
          modeLabel: 'ask',
          modelLabel: 'Sonnet',
          modeRefusal: mode,
          modelRefusal: model,
          modes: askAgent.modes,
          models: askAgent.models,
        );
    Finder snack(String text) =>
        find.descendant(of: find.byType(SnackBar), matching: find.text(text));
    Future<void> dismiss() async {
      await tester.tap(find.byIcon(Icons.close));
      await tester.pumpAndSettle();
    }

    // Already refused when the page opened: not news.
    await show(screenWith(interruptRefusal: 'old news'));
    expect(find.byType(SnackBar), findsNothing);

    await show(screenWith(interruptRefusal: 'the server could not take it'));
    expect(
      snack('Interrupt failed: the server could not take it'),
      findsOneWidget,
    );
    await dismiss();
    // The same refusal again, as on any rebuild: said once.
    await show(screenWith(interruptRefusal: 'the server could not take it'));
    expect(find.byType(SnackBar), findsNothing);
    // It stays on the button.
    expect(
      find.byTooltip('Interrupt failed: the server could not take it'),
      findsOneWidget,
    );

    await show(screenWith());
    await show(
      screenWith(agent: refusing(mode: 'bypass refused: not launched for it')),
    );
    expect(snack('bypass refused: not launched for it'), findsOneWidget);
    await dismiss();
    await show(
      screenWith(
        agent: refusing(
          mode: 'bypass refused: not launched for it',
          model: 'bogus refused: unknown model',
        ),
      ),
    );
    expect(snack('bogus refused: unknown model'), findsOneWidget);
    expect(snack('bypass refused: not launched for it'), findsNothing);
    await dismiss();
    expect(find.byTooltip('bogus refused: unknown model'), findsOneWidget);
  });

  testWidgets('a bar runs under the header only while the agent works', (
    tester,
  ) async {
    Future<void> show(AgentState state, {bool still = false}) async {
      await tester.pumpWidget(
        MaterialApp(
          home: MediaQuery(
            data: MediaQueryData(disableAnimations: still),
            child: SessionPage(
              screen: screenWith(state: state),
              timeline: holding([row('s:1', 'first row')]),
              send: (_) {},
            ),
          ),
        ),
      );
      await tester.pump();
    }

    LinearProgressIndicator bar() =>
        tester.widget(find.byType(LinearProgressIndicator));

    await show(AgentState.working);
    expect(bar().value, isNull, reason: 'it runs');
    final at = tester.getRect(find.byType(LinearProgressIndicator));
    expect(at.top, tester.getRect(find.byType(AppBar)).bottom);
    expect(at.height, lessThanOrEqualTo(4));
    final first = tester.getRect(find.text('first row')).top;

    await show(AgentState.working, still: true);
    expect(bar().value, 1, reason: 'reduced motion: it holds still');

    for (final state in [
      AgentState.idle,
      AgentState.awaitingApproval,
      AgentState.errored,
      AgentState.stopped,
    ]) {
      await show(state);
      expect(
        find.byType(LinearProgressIndicator),
        findsNothing,
        reason: '$state',
      );
      expect(
        tester.getRect(find.text('first row')).top,
        first,
        reason: 'the timeline does not shift',
      );
    }
  });

  testWidgets('Stop agent is offered unless the agent is stopped', (
    tester,
  ) async {
    final sent = <GuiIntent>[];
    Future<void> openMore(AgentState state) async {
      await pumpSession(
        tester,
        screen: screenWith(state: state),
        timeline: holding([]),
        sent: sent,
      );
      await tester.tap(find.byTooltip('More'));
      await tester.pumpAndSettle();
    }

    await openMore(AgentState.stopped);
    expect(
      find.text('Already stopped · a message restarts it'),
      findsOneWidget,
    );
    await tester.tap(find.text('Stop agent'));
    await tester.pumpAndSettle();
    expect(sent.whereType<GuiIntentAct>(), isEmpty);
    await tester.tapAt(Offset.zero);
    await tester.pumpAndSettle();

    // An idle agent is kept warm a while; an errored one may still run.
    for (final state in [AgentState.idle, AgentState.errored]) {
      sent.clear();
      await openMore(state);
      expect(find.textContaining('Already stopped'), findsNothing);
      await tester.tap(find.text('Stop agent'));
      await tester.pumpAndSettle();
      expect(sent.whereType<GuiIntentAct>(), [
        const GuiIntentAct(action: ActionIntentStopSession(sessionId: 's1')),
      ], reason: '$state');
    }
  });

  testWidgets('header actions and the rename prompt', (tester) async {
    final sent = <GuiIntent>[];
    await pumpSession(
      tester,
      screen: screenWith(
        usage: ['last turn 12.4s', '3.1k in / 812 out', r'$0.04'],
        archivedLabel: 'archived · a message brings it back',
      ),
      timeline: holding([]),
      sent: sent,
    );
    expect(find.text('archived · a message brings it back'), findsOneWidget);
    expect(find.textContaining(r'$0.04'), findsOneWidget);
    await tester.tap(find.byTooltip('Interrupt (Ctrl+X)'));
    expect(
      sent.last,
      const GuiIntentAct(action: ActionIntentInterrupt(sessionId: 's1')),
    );
    await tester.tap(find.byTooltip('More'));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Rename'));
    await tester.pumpAndSettle();
    expect(sent.last, const GuiIntentRenameOpen(sessionId: 's1'));

    await pumpSession(
      tester,
      screen: screenWith(
        renamePrompt: const RenamePrompt(sessionId: 's1', initial: 'work'),
      ),
      timeline: holding([]),
      sent: sent,
    );
    expect(find.text('Rename session'), findsOneWidget);
    await tester.enterText(find.byType(TextField).last, 'better');
    await tester.tap(find.widgetWithText(FilledButton, 'Rename'));
    expect(sent.last, const GuiIntentRenameSubmit(name: 'better'));
  });

  group('the status line', () {
    testWidgets('a mode is picked; bypass only once confirmed', (tester) async {
      final sent = <GuiIntent>[];
      await pumpSession(
        tester,
        screen: screenWith(),
        timeline: holding([]),
        sent: sent,
      );
      await tester.tap(find.byTooltip('Permission mode'));
      await tester.pumpAndSettle();
      expect(find.text('accept edits detail'), findsOneWidget);
      await tester.tap(find.text('plan'));
      await tester.pumpAndSettle();
      expect(
        sent.last,
        const GuiIntentAct(
          action: ActionIntentSetPermissionMode(
            sessionId: 's1',
            mode: PermissionModeView.plan,
          ),
        ),
      );

      sent.clear();
      await tester.tap(find.byTooltip('Permission mode'));
      await tester.pumpAndSettle();
      await tester.tap(find.text('bypass'));
      await tester.pumpAndSettle();
      expect(find.text('Switch to bypass?'), findsOneWidget);
      await tester.tap(find.text('Cancel'));
      await tester.pumpAndSettle();
      expect(sent, isEmpty);

      await tester.tap(find.byTooltip('Permission mode'));
      await tester.pumpAndSettle();
      await tester.tap(find.text('bypass'));
      await tester.pumpAndSettle();
      await tester.tap(find.text('Switch to bypass'));
      await tester.pumpAndSettle();
      expect(sent, [
        const GuiIntentAct(
          action: ActionIntentSetPermissionMode(
            sessionId: 's1',
            mode: PermissionModeView.bypass,
          ),
        ),
      ]);
    });

    testWidgets('a model is picked from the list or typed in', (tester) async {
      final sent = <GuiIntent>[];
      await pumpSession(
        tester,
        screen: screenWith(),
        timeline: holding([]),
        sent: sent,
      );
      await tester.tap(find.byTooltip('Model'));
      await tester.pumpAndSettle();
      await tester.tap(find.text('Opus'));
      await tester.pumpAndSettle();
      expect(
        sent.last,
        const GuiIntentAct(
          action: ActionIntentSetModel(sessionId: 's1', model: 'opus'),
        ),
      );

      await tester.tap(find.byTooltip('Model'));
      await tester.pumpAndSettle();
      await tester.enterText(
        find.descendant(
          of: find.byType(AlertDialog),
          matching: find.byType(TextField),
        ),
        ' my-model ',
      );
      await tester.pump();
      await tester.tap(find.widgetWithText(FilledButton, 'Switch'));
      await tester.pumpAndSettle();
      expect(
        sent.last,
        const GuiIntentAct(
          action: ActionIntentSetModel(sessionId: 's1', model: 'my-model'),
        ),
      );
    });

    testWidgets('a refusal shows on its chip and in its picker', (
      tester,
    ) async {
      final sent = <GuiIntent>[];
      await pumpSession(
        tester,
        screen: screenWith(
          agent: AgentSettingsView(
            modeLabel: 'ask',
            modeRefusal: 'bypass refused: not launched for it',
            modes: askAgent.modes,
            models: const [],
          ),
        ),
        timeline: holding([]),
        sent: sent,
      );
      // No model named yet: the chip still opens the name field.
      expect(find.text('model'), findsOneWidget);
      expect(find.byTooltip('bypass refused: not launched for it'), findsOne);
      await tester.tap(find.text('ask'));
      await tester.pumpAndSettle();
      expect(find.text('bypass refused: not launched for it'), findsOneWidget);
    });

    testWidgets('the header grows with the text scale; nothing clips', (
      tester,
    ) async {
      tester.view
        ..physicalSize = const Size(360, 760)
        ..devicePixelRatio = 1;
      addTearDown(tester.view.reset);
      await tester.pumpWidget(
        MaterialApp(
          home: Builder(
            builder: (context) => MediaQuery(
              data: MediaQuery.of(context)
                  .copyWith(textScaler: const TextScaler.linear(1.5)),
              child: SessionPage(
                screen: screenWith(usage: [r'$0.04']),
                timeline: holding([row('s:1', 'first row')]),
                send: (_) {},
              ),
            ),
          ),
        ),
      );
      await tester.pumpAndSettle();
      expect(tester.takeException(), isNull);

      final bar = tester.getRect(find.byType(AppBar));
      final title = tester.getRect(find.text('work'));
      final workdir = tester.getRect(
        find.textContaining('/home/me/work', findRichText: true),
      );
      final state = tester.getRect(
        find.textContaining('idle', findRichText: true),
      );
      final first = tester.getRect(find.text('first row'));
      expect(title.top, greaterThanOrEqualTo(bar.top));
      expect(workdir.top, greaterThanOrEqualTo(title.bottom));
      expect(state.top, greaterThanOrEqualTo(workdir.bottom));
      expect(state.bottom, lessThanOrEqualTo(bar.bottom));
      expect(first.top, greaterThanOrEqualTo(bar.bottom));
    });

    testWidgets('long labels are cut short on a phone', (tester) async {
      tester.view
        ..physicalSize = const Size(360, 700)
        ..devicePixelRatio = 1;
      addTearDown(tester.view.reset);
      await pumpSession(
        tester,
        screen: screenWith(
          usage: ['last turn 12.4s', r'$0.04'],
          agent: AgentSettingsView(
            modeLabel: "accept edits → don't ask",
            modelLabel: 'claude-sonnet-5-5-20261001 → claude-opus-5-5',
            modes: askAgent.modes,
            models: const [],
          ),
        ),
        timeline: holding([]),
        sent: [],
      );
      expect(tester.takeException(), isNull);
      // Too narrow for both labels: one chip lists them in full.
      expect(find.byTooltip('Permission mode'), findsNothing);
      await tester.tap(find.byTooltip('Agent settings'));
      await tester.pumpAndSettle();
      expect(find.text("accept edits → don't ask"), findsOneWidget);
      expect(
        find.text('claude-sonnet-5-5-20261001 → claude-opus-5-5'),
        findsOneWidget,
      );
      await tester.tap(find.text('Permission mode'));
      await tester.pumpAndSettle();
      expect(find.text('accept edits detail'), findsOneWidget);
    });

    testWidgets('the agent state shows once, its full label as a tooltip', (
      tester,
    ) async {
      await tester.pumpWidget(
        MaterialApp(
          home: SessionPage(
            screen: screenWith(state: AgentState.working),
            timeline: holding([]),
            send: (_) {},
          ),
        ),
      );
      await tester.pump();
      expect(find.textContaining('working', findRichText: true), findsOne);
      expect(find.byTooltip('agent working'), findsOneWidget);
      expect(find.text('/home/me/work'), findsOneWidget);
    });

    testWidgets('on a wide window the header keeps to the timeline column', (
      tester,
    ) async {
      tester.view
        ..physicalSize = const Size(1600, 900)
        ..devicePixelRatio = 1;
      addTearDown(tester.view.reset);
      await pumpSession(
        tester,
        screen: screenWith(usage: [r'$0.04']),
        timeline: holding([]),
        sent: [],
      );
      const left = (1600 - timelineMaxWidth) / 2;
      const right = left + timelineMaxWidth;
      final back = tester.getRect(find.byType(BackButton));
      final more = tester.getRect(find.byTooltip('More'));
      final status = tester.getRect(find.byType(StatusLine));
      final composer = tester.getRect(find.byType(Composer));
      expect(back.left, greaterThanOrEqualTo(left));
      expect(more.right, lessThanOrEqualTo(right));
      expect(status.left, left);
      expect(status.right, right);
      expect(composer.left, left);
      expect(tester.getRect(find.text(r'$0.04')).right, lessThan(right));
    });

    testWidgets('the chips take taps over 48 dp', (tester) async {
      await pumpSession(
        tester,
        screen: screenWith(),
        timeline: holding([]),
        sent: [],
      );
      for (final tooltip in ['Permission mode', 'Model']) {
        final size = tester.getSize(
          find.descendant(
            of: find.byTooltip(tooltip),
            matching: find.byType(InkWell),
          ),
        );
        expect(size.width, greaterThanOrEqualTo(48), reason: tooltip);
        expect(size.height, greaterThanOrEqualTo(48), reason: tooltip);
      }
    });
  });
}
