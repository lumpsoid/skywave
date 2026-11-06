import 'package:flutter/foundation.dart';
import 'package:flutter/gestures.dart';
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/session/approval_panel.dart';
import 'package:skywave_gui/session/composer.dart';
import 'package:skywave_gui/session/look.dart';
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
        deliveryLabel: 'queued 14:02:11',
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
      anyRow(
        's:15',
        RowKind.contextDivider,
        'New context · velvet walrus · by laptop',
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
      'queued 14:02:11',
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
      'New context · velvet walrus · by laptop',
    ]) {
      expect(find.text(text, findRichText: true), findsWidgets, reason: text);
    }
    // A context divider reads as a branch, not a warning.
    expect(find.byIcon(Icons.alt_route), findsOneWidget);

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

  testWidgets('imported history is marked beside its time and dimmed', (
    tester,
  ) async {
    tall(tester);
    final timeline = holding([
      anyRow(
        's:1',
        RowKind.importedUserMessage,
        'fix the build',
        author: 'you',
        mark: 'imported',
        timeLabel: '09:15',
      ),
      anyRow(
        's:2',
        RowKind.importedTool,
        '',
        tool: shellFacts('cargo test'),
        mark: 'imported',
      ),
      anyRow(
        's:3',
        RowKind.importedAgentMessage,
        'fixed',
        author: 'agent',
        mark: 'imported',
      ),
      anyRow('s:4', RowKind.contextDivider, 'Continued in Skywave from here'),
      anyRow('s:5', RowKind.agentMessage, 'live again', author: 'agent'),
    ]);
    await pumpSession(
      tester,
      screen: screenWith(),
      timeline: timeline,
      sent: <GuiIntent>[],
    );
    expect(find.text('09:15 · imported', findRichText: true), findsOneWidget);
    expect(find.text('imported', findRichText: true), findsNWidgets(2));
    expect(find.text('Bash  cargo test', findRichText: true), findsOneWidget);
    expect(
      find.text('Continued in Skywave from here', findRichText: true),
      findsOneWidget,
    );
    double opacityOf(String text) => tester
        .widget<Opacity>(
          find
              .ancestor(
                of: find.text(text, findRichText: true),
                matching: find.byType(Opacity),
              )
              .first,
        )
        .opacity;
    expect(opacityOf('fix the build'), lessThan(1));
    expect(opacityOf('fixed'), lessThan(1));
    expect(opacityOf('Bash  cargo test'), lessThan(1));
    // The session's own rows are not.
    expect(opacityOf('live again'), 1);
  });

  testWidgets('a logged message replacing its pending one keeps the scroll', (
    tester,
  ) async {
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
      200,
      scrollable: listScrollable(),
    );
    await tester.pumpAndSettle();
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

  testWidgets('a message\'s text selects; its menu copies it or the whole '
      'message; a tap and the row menu still work', (tester) async {
    debugDefaultTargetPlatformOverride = TargetPlatform.linux;
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
    try {
      final timeline = holding([
        anyRow(
          's:1',
          RowKind.agentMessage,
          'hello brave new world',
          author: 'agent',
        ),
      ]);
      final sent = <GuiIntent>[];
      await pumpSession(
        tester,
        screen: screenWith(),
        timeline: timeline,
        sent: sent,
      );
      final text = find.text('hello brave new world', findRichText: true);
      expect(
        find.ancestor(of: text, matching: find.byType(SelectionArea)),
        findsOneWidget,
      );

      // A mouse drag selects part of it; Copy takes just that.
      final box = tester.getRect(text);
      final drag = await tester.startGesture(
        box.centerLeft + const Offset(1, 0),
        kind: PointerDeviceKind.mouse,
      );
      await tester.pump();
      await drag.moveTo(box.center);
      await tester.pump();
      await drag.up();
      await tester.pumpAndSettle();
      await tester.tapAt(
        box.centerLeft + const Offset(4, 0),
        buttons: kSecondaryButton,
        kind: PointerDeviceKind.mouse,
      );
      await tester.pumpAndSettle();
      expect(find.text('Copy message'), findsOneWidget);
      await tester.tap(find.text('Copy'));
      await tester.pumpAndSettle();
      expect(copied, hasLength(1));
      expect('hello brave new world', startsWith(copied.single));
      expect(copied.single, isNot('hello brave new world'));

      // Copy message is the whole message, through the hub.
      await tester.tapAt(
        box.center,
        buttons: kSecondaryButton,
        kind: PointerDeviceKind.mouse,
      );
      await tester.pumpAndSettle();
      sent.clear();
      await tester.tap(find.text('Copy message'));
      await tester.pumpAndSettle();
      expect(sent, [const GuiIntentCopy(key: 's:1', what: CopyWhat.main)]);
      expect(find.text('Copy message'), findsNothing);

      // Off the text, the row's menu.
      await tester.tapAt(
        tester.getCenter(find.text('agent')),
        buttons: kSecondaryButton,
        kind: PointerDeviceKind.mouse,
      );
      await tester.pumpAndSettle();
      expect(find.text('Copy message'), findsNothing);
      await tester.tap(find.text('Copy'));
      await tester.pumpAndSettle();
      expect(sent.last, const GuiIntentCopy(key: 's:1', what: CopyWhat.main));
    } finally {
      debugDefaultTargetPlatformOverride = null;
    }
  });

  testWidgets('on touch a long press on the text selects; off it, the row '
      'menu', (tester) async {
    final timeline = holding([
      anyRow('s:1', RowKind.agentMessage, 'hello world', author: 'agent'),
    ]);
    final sent = <GuiIntent>[];
    await pumpSession(
      tester,
      screen: screenWith(),
      timeline: timeline,
      sent: sent,
    );
    await tester.longPress(find.text('hello world', findRichText: true));
    await tester.pumpAndSettle();
    expect(find.text('Copy message'), findsOneWidget);
    expect(find.text('Edit queued'), findsNothing);

    // A tap clears the text selection.
    await tester.tap(find.text('hello world', findRichText: true));
    await tester.pumpAndSettle();
    expect(find.text('Copy message'), findsNothing);

    await tester.longPress(find.text('agent'));
    await tester.pumpAndSettle();
    expect(find.text('Copy message'), findsNothing);
    await tester.tap(find.text('Copy'));
    await tester.pumpAndSettle();
    expect(sent.last, const GuiIntentCopy(key: 's:1', what: CopyWhat.main));
  });

  testWidgets('a message of the user saves as a snippet, named or not', (
    tester,
  ) async {
    final sent = <GuiIntent>[];
    await pumpSession(
      tester,
      screen: screenWith(),
      timeline: holding([
        anyRow('s:1', RowKind.userMessage, 'run the tests', author: 'you'),
        anyRow('s:2', RowKind.agentMessage, 'done', author: 'agent'),
      ]),
      sent: sent,
    );
    await tester.longPress(find.text('agent'));
    await tester.pumpAndSettle();
    expect(find.text('Save as snippet'), findsNothing);
    await tester.tapAt(Offset.zero);
    await tester.pumpAndSettle();

    await tester.longPress(find.text('you'));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Save as snippet'));
    await tester.pumpAndSettle();
    await tester.enterText(
      find.byKey(const Key('snippet-name-field')),
      'tests',
    );
    await tester.tap(find.text('Save'));
    await tester.pumpAndSettle();
    expect(sent.last, const GuiIntentSaveSnippet(key: 's:1', name: 'tests'));
  });

  testWidgets('the snippet button inserts, edits, deletes and saves', (
    tester,
  ) async {
    const tests = SnippetView(
      snippetId: 'a',
      label: 'tests',
      detail: 'run the tests',
      name: 'tests',
      text: 'run the tests',
    );
    const commit = SnippetView(
      snippetId: 'b',
      label: 'commit this',
      text: 'commit this',
    );
    final sent = <GuiIntent>[];
    final timeline = holding([]);
    Future<void> show(List<SnippetView> snippets) => pumpSession(
      tester,
      screen: screenWith(snippets: snippets),
      timeline: timeline,
      sent: sent,
    );
    await show([tests, commit]);
    await tester.tap(find.byTooltip('Snippets'));
    await tester.pumpAndSettle();
    expect(find.text('run the tests'), findsOneWidget);
    expect(find.text('Save the message as a snippet'), findsNothing);

    await tester.tap(find.byTooltip('Edit snippet').first);
    await tester.pumpAndSettle();
    await tester.enterText(find.byKey(const Key('snippet-edit-name')), '');
    await tester.enterText(find.byKey(const Key('snippet-edit-text')), ' ');
    await tester.pump();
    expect(
      tester
          .widget<FilledButton>(find.widgetWithText(FilledButton, 'Save'))
          .onPressed,
      isNull,
      reason: 'blank text is not saved',
    );
    await tester.enterText(
      find.byKey(const Key('snippet-edit-text')),
      'run every test',
    );
    await tester.pump();
    await tester.tap(find.text('Save'));
    await tester.pumpAndSettle();
    expect(
      sent.whereType<GuiIntentAct>().last,
      const GuiIntentAct(
        action: ActionIntentEditSnippet(
          snippetId: 'a',
          name: '',
          text: 'run every test',
        ),
      ),
    );

    await tester.tap(find.byTooltip('Delete snippet').last);
    await tester.pumpAndSettle();
    expect(
      sent.whereType<GuiIntentAct>().last,
      const GuiIntentAct(action: ActionIntentDeleteSnippet(snippetId: 'b')),
    );
    // The open sheet follows what the hub sends next.
    await show([
      const SnippetView(
        snippetId: 'a',
        label: 'run every test',
        text: 'run every test',
      ),
    ]);
    expect(find.text('commit this'), findsNothing);
    await tester.tap(find.text('run every test'));
    await tester.pumpAndSettle();
    expect(
      sent.whereType<GuiIntentInsertSnippet>().last,
      const GuiIntentInsertSnippet(sessionId: 's1', snippetId: 'a'),
    );

    await tester.enterText(find.byType(TextField), 'ship it');
    await tester.tap(find.byTooltip('Snippets'));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Save the message as a snippet'));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Save'));
    await tester.pumpAndSettle();
    expect(
      sent.whereType<GuiIntentSaveComposerSnippet>().last,
      const GuiIntentSaveComposerSnippet(sessionId: 's1', name: ''),
    );
  });

  testWidgets('an empty timeline says how to start, once it is loaded', (
    tester,
  ) async {
    const hint = 'Send a message to start the agent.';
    // No rows sent yet: not known to be empty.
    final timeline = TimelineHolder();
    await pumpSession(
      tester,
      screen: screenWith(),
      timeline: timeline,
      sent: [],
    );
    expect(find.text(hint), findsNothing);
    // Another session's rows, still shown while this one's are on the way.
    timeline.apply(const TimelineUpdateSnapshot(sessionId: 's0', items: []));
    await tester.pump();
    expect(find.text(hint), findsNothing);

    timeline.apply(const TimelineUpdateSnapshot(sessionId: 's1', items: []));
    await tester.pump();
    expect(find.text(hint), findsOneWidget);
    final context = tester.element(find.text(hint));
    expect(
      tester.widget<Text>(find.text(hint)).style?.color,
      SkyColors.of(context).muted,
    );

    timeline.apply(
      TimelineUpdateUpserted(
        key: 'p:c1',
        item: anyRow('p:c1', RowKind.pendingMessage, 'hi', own: true),
        index: 0,
      ),
    );
    await tester.pump();
    expect(find.text(hint), findsNothing);
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

    testWidgets('a phone with the keyboard up: one button row, no overflow', (
      tester,
    ) async {
      tester.view.physicalSize = const Size(360, 780);
      tester.view.devicePixelRatio = 1;
      tester.view.viewInsets = const FakeViewPadding(bottom: 330);
      addTearDown(tester.view.reset);
      await pumpSession(
        tester,
        screen: screenWith(
          approval: ApprovalPanel(
            approvalId: 'a1',
            kind: ToolKindView.shell,
            tool: 'Bash',
            title: r'$ rm -rf build',
            detail: List.filled(12, 'a long line of what it does').join('\n'),
            canAnswer: true,
            suggestions: const [
              RuleOption(index: 0, label: 'Bash(rm:*) (project)'),
              RuleOption(index: 1, label: 'Bash(*) (session)'),
            ],
            alwaysPicks: true,
          ),
        ),
        timeline: holding([]),
        sent: [],
      );
      expect(tester.takeException(), isNull);
      final row = tester.getCenter(find.text('Approve')).dy;
      expect(tester.getCenter(find.text('Deny')).dy, row);
      expect(tester.getCenter(find.text('Always allow…')).dy, row);
    });

    ApprovalPanel withPreview(int count) => ApprovalPanel(
      approvalId: 'a1',
      kind: ToolKindView.shell,
      tool: 'Bash',
      title: r'$ ./deploy.sh',
      preview: lines([for (var i = 1; i <= count; i++) 'line $i']),
      canAnswer: true,
      suggestions: const [],
      alwaysPicks: false,
    );

    testWidgets('expanded on a phone with the keyboard up: the title and the '
        'buttons stay on screen, the preview scrolls between them', (
      tester,
    ) async {
      tester.view.physicalSize = const Size(360, 780);
      tester.view.devicePixelRatio = 1;
      tester.view.viewInsets = const FakeViewPadding(bottom: 330);
      addTearDown(tester.view.reset);
      await pumpSession(
        tester,
        screen: screenWith(approval: withPreview(60)),
        timeline: holding([]),
        sent: [],
      );
      await tester.ensureVisible(find.text('Show 52 more lines'));
      await tester.tap(find.text('Show 52 more lines'));
      await tester.pump();
      expect(tester.takeException(), isNull);
      final card = tester.getRect(find.byType(Card).last);
      for (final fixed in ['Approval needed', 'Approve', 'Deny']) {
        final at = tester.getRect(find.text(fixed));
        expect(
          card.contains(at.topLeft) && card.contains(at.bottomRight),
          isTrue,
        );
      }
      expect(find.text('line 60'), findsOneWidget);
      // Below what the panel shows until scrolled to.
      expect(
        tester.getTopLeft(find.text('line 60')).dy,
        greaterThan(card.bottom),
      );
    });

    testWidgets('scrolling the panel does not tint the app bar', (
      tester,
    ) async {
      tester.view.physicalSize = const Size(360, 780);
      tester.view.devicePixelRatio = 1;
      tester.view.viewInsets = const FakeViewPadding(bottom: 330);
      addTearDown(tester.view.reset);
      await pumpSession(
        tester,
        screen: screenWith(approval: withPreview(60)),
        timeline: holding([]),
        sent: [],
      );
      // The app bar tints on what the scaffold's observer hears.
      final heard = <ScrollNotification>[];
      ScrollNotificationObserver.of(
        tester.element(find.text('Approval needed')),
      ).addListener(heard.add);
      await tester.drag(find.textContaining('deploy.sh'), const Offset(0, -80));
      await tester.pump();
      expect(heard, isEmpty);
    });

    testWidgets('a long preview shows its first lines until expanded', (
      tester,
    ) async {
      await pumpSession(
        tester,
        screen: screenWith(approval: withPreview(20)),
        timeline: holding([]),
        sent: [],
      );
      expect(find.text('line 8'), findsOneWidget);
      expect(find.text('line 9'), findsNothing);
      await tester.ensureVisible(find.text('Show 12 more lines'));
      await tester.tap(find.text('Show 12 more lines'));
      await tester.pump();
      expect(find.text('line 20'), findsOneWidget);
      await tester.ensureVisible(find.text('Show less'));
      await tester.tap(find.text('Show less'));
      await tester.pump();
      expect(find.text('line 9'), findsNothing);

      // Control: a short preview has nothing to expand.
      await pumpSession(
        tester,
        screen: screenWith(approval: withPreview(approvalPreviewLines)),
        timeline: holding([]),
        sent: [],
      );
      expect(find.text('line 8'), findsOneWidget);
      expect(find.textContaining('more line'), findsNothing);
    });

    testWidgets('a phone docks the panel to the composer, a wide screen not', (
      tester,
    ) async {
      tester.view.physicalSize = const Size(360, 780);
      tester.view.devicePixelRatio = 1;
      addTearDown(tester.view.reset);
      await pumpSession(
        tester,
        screen: screenWith(approval: withPreview(1)),
        timeline: holding([]),
        sent: [],
      );
      Card panel() => tester.widget<Card>(
        find.ancestor(
          of: find.text('Approval needed'),
          matching: find.byType(Card),
        ),
      );
      expect(panel().margin, EdgeInsets.zero);
      expect(panel().shape, isA<Border>());

      tester.view.physicalSize = const Size(1200, 800);
      await tester.pump();
      expect(panel().margin, isNot(EdgeInsets.zero));
      expect(panel().shape, isA<RoundedRectangleBorder>());
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
            answerLabel: 'you approved · queued 14:02:11',
          ),
        ),
        timeline: holding([]),
        sent: [],
      );
      expect(find.text('you approved · queued 14:02:11'), findsOneWidget);
      expect(find.text('Approve'), findsNothing);
      expect(find.text('Show call'), findsNothing);
    });

    testWidgets('an input the server lost is said up front', (tester) async {
      const lost =
          'the server could not store the full input: you are seeing a summary';
      ApprovalPanel withLost(String? said) => ApprovalPanel(
        approvalId: 'a1',
        kind: ToolKindView.other,
        tool: 'write',
        title: 'a.txt',
        lost: said,
        canAnswer: true,
        suggestions: const [],
        alwaysPicks: false,
      );
      await pumpSession(
        tester,
        screen: screenWith(approval: withLost(lost)),
        timeline: holding([]),
        sent: [],
      );
      expect(find.text(lost), findsOneWidget);
      expect(find.byIcon(Icons.warning_amber_rounded), findsOneWidget);

      // Control: nothing lost, nothing said.
      await pumpSession(
        tester,
        screen: screenWith(approval: withLost(null)),
        timeline: holding([]),
        sent: [],
      );
      expect(find.text(lost), findsNothing);
      expect(find.byIcon(Icons.warning_amber_rounded), findsNothing);
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
          composer: const ComposerView(
            text: '',
            cursorUtf16: 0,
            rev: 2,
            mode: ComposerModeView.agent,
            shellEnabled: false,
            canStop: false,
          ),
        ),
        timeline: timeline,
        sent: sent,
      );
      expect(send().onPressed, isNull);
      // A queued message taken back to edit fills the field: on.
      await pumpSession(
        tester,
        screen: screenWith(
          composer: const ComposerView(
            text: 'queued',
            cursorUtf16: 6,
            rev: 3,
            mode: ComposerModeView.agent,
            shellEnabled: false,
            canStop: false,
          ),
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
          composer: const ComposerView(
            text: 'old',
            cursorUtf16: 0,
            rev: 1,
            mode: ComposerModeView.agent,
            shellEnabled: false,
            canStop: false,
          ),
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
            mode: ComposerModeView.agent,
            shellEnabled: false,
            canStop: false,
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

    // New rows do not move a view scrolled back: the rows in view stay put.
    final newest = tester
        .state<TimelineViewState>(find.byType(TimelineView))
        .newestVisibleKey()!;
    final shown = find.text('row ${newest.substring(2)}');
    final where = tester.getTopLeft(shown);
    timeline.apply(
      TimelineUpdateUpserted(
        key: 's:60',
        item: row('s:60', 'row 60'),
        index: 60,
      ),
    );
    await tester.pumpAndSettle();
    expect(tester.getTopLeft(shown), where);

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

  testWidgets('the keyboard opening keeps the newest row in view', (
    tester,
  ) async {
    tester.view
      ..physicalSize = const Size(360, 780)
      ..devicePixelRatio = 1;
    addTearDown(tester.view.reset);
    await pumpSession(
      tester,
      screen: screenWith(),
      timeline: holding([for (var i = 0; i < 60; i++) row('s:$i', 'row $i')]),
      sent: [],
    );
    final list = find.byType(ListView);
    bool inView(String text) {
      final box = tester.getRect(list);
      final rect = tester.getRect(find.text(text));
      return rect.top >= box.top && rect.bottom <= box.bottom;
    }

    expect(inView('row 59'), isTrue);

    tester.view.viewInsets = const FakeViewPadding(bottom: 330);
    await tester.pumpAndSettle();
    expect(inView('row 59'), isTrue);
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
    // Still, not a spinner: offline it can wait as long as the link is down.
    expect(find.byIcon(Icons.hourglass_top), findsOneWidget);
    expect(find.byType(CircularProgressIndicator), findsNothing);
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
          effortLabel: 'effort medium',
          modeRefusal: mode,
          modelRefusal: model,
          modes: askAgent.modes,
          models: askAgent.models,
          efforts: askAgent.efforts,
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

  testWidgets('no progress bar runs while the agent works', (tester) async {
    await pumpSession(
      tester,
      screen: screenWith(state: AgentState.working),
      timeline: holding([row('s:1', 'first row')]),
      sent: <GuiIntent>[],
    );
    expect(find.byType(LinearProgressIndicator), findsNothing);
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

  group('contexts', () {
    const two = [
      ContextRow(
        contextId: 'c1',
        label: 'start · fix the test',
        current: false,
        canMove: false,
      ),
      ContextRow(
        contextId: 'c2',
        label: 'velvet walrus',
        current: true,
        canMove: false,
      ),
    ];

    Future<void> openMore(
      WidgetTester tester,
      SessionScreen screen,
      List<GuiIntent> sent,
    ) async {
      await pumpSession(
        tester,
        screen: screen,
        timeline: holding([]),
        sent: sent,
      );
      await tester.tap(find.byTooltip('More'));
      await tester.pumpAndSettle();
    }

    testWidgets('New context is disabled in an empty context, sends once '
        'there is something to clear', (tester) async {
      final sent = <GuiIntent>[];
      await openMore(tester, screenWith(), sent);
      expect(find.text('This context is empty'), findsOneWidget);
      await tester.tap(find.text('New context'));
      await tester.pumpAndSettle();
      expect(sent.whereType<GuiIntentAct>(), isEmpty);
      await tester.tapAt(Offset.zero);
      await tester.pumpAndSettle();

      await openMore(tester, screenWith(canClear: true), sent);
      await tester.tap(find.text('New context'));
      await tester.pumpAndSettle();
      expect(sent.whereType<GuiIntentAct>(), [
        const GuiIntentAct(action: ActionIntentClearContext(sessionId: 's1')),
      ]);
    });

    testWidgets('the picker lists contexts; switching sends the intent, '
        'the current one offers no switch', (tester) async {
      final sent = <GuiIntent>[];
      await openMore(tester, screenWith(contexts: two, contextId: 'c2'), sent);
      await tester.tap(find.text('Contexts…'));
      await tester.pumpAndSettle();
      expect(find.text('start · fix the test'), findsOneWidget);
      expect(find.text('velvet walrus'), findsOneWidget);
      expect(find.text('Switch'), findsOneWidget, reason: 'not on current');
      await tester.tap(find.text('Switch'));
      await tester.pumpAndSettle();
      expect(sent.whereType<GuiIntentAct>(), [
        const GuiIntentAct(
          action: ActionIntentSwitchContext(sessionId: 's1', contextId: 'c1'),
        ),
      ]);
    });

    const movable = [
      ContextRow(
        contextId: 'c1',
        label: 'start · fix the test',
        current: false,
        canMove: true,
      ),
      ContextRow(
        contextId: 'c2',
        label: 'velvet walrus',
        current: true,
        canMove: false,
        moveRefusal: 'can\'t move yet: it needs an answered turn',
      ),
    ];

    testWidgets('moving asks first; cancelling sends nothing, confirming '
        'sends the intent; a row that cannot move has no enabled action', (
      tester,
    ) async {
      final sent = <GuiIntent>[];
      await openMore(
        tester,
        screenWith(contexts: movable, contextId: 'c2'),
        sent,
      );
      await tester.tap(find.text('Contexts…'));
      await tester.pumpAndSettle();
      final buttons = find.widgetWithText(TextButton, 'Move to new session');
      expect(buttons, findsNWidgets(2));
      expect(tester.widget<TextButton>(buttons.last).onPressed, isNull);
      expect(tester.widget<TextButton>(buttons.first).onPressed, isNotNull);

      await tester.tap(buttons.first);
      await tester.pumpAndSettle();
      expect(
        find.text('Move start · fix the test to a new session?'),
        findsOneWidget,
      );
      await tester.tap(find.text('Cancel'));
      await tester.pumpAndSettle();
      expect(sent.whereType<GuiIntentAct>(), isEmpty);

      await tester.tap(
        find.widgetWithText(TextButton, 'Move to new session').first,
      );
      await tester.pumpAndSettle();
      await tester.tap(find.widgetWithText(TextButton, 'Move'));
      await tester.pumpAndSettle();
      expect(sent.whereType<GuiIntentAct>(), [
        const GuiIntentAct(
          action: ActionIntentMoveContext(sessionId: 's1', contextId: 'c1'),
        ),
      ]);
    });

    testWidgets('a moved row names where it went and Open opens that '
        'session; Switch is not offered', (tester) async {
      final sent = <GuiIntent>[];
      await openMore(
        tester,
        screenWith(
          contexts: const [
            ContextRow(
              contextId: 'c1',
              label: 'start',
              current: false,
              canMove: false,
              movedToSessionId: 's2',
              movedLabel: 'moved → “forked”',
            ),
            ContextRow(
              contextId: 'c2',
              label: 'velvet walrus',
              current: true,
              canMove: false,
            ),
          ],
          contextId: 'c2',
        ),
        sent,
      );
      await tester.tap(find.text('Contexts…'));
      await tester.pumpAndSettle();
      expect(find.text('moved → “forked”'), findsOneWidget);
      expect(find.text('Switch'), findsNothing);
      expect(find.text('Move to new session'), findsOneWidget, reason: 'c2');
      await tester.tap(find.text('Open'));
      await tester.pumpAndSettle();
      expect(sent.whereType<GuiIntentAct>(), [
        const GuiIntentAct(action: ActionIntentOpenSession(sessionId: 's2')),
      ]);
    });

    testWidgets('Discard move sends Discard with the command, only on a row '
        'that has one', (tester) async {
      final sent = <GuiIntent>[];
      await openMore(
        tester,
        screenWith(
          contexts: const [
            ContextRow(
              contextId: 'c1',
              label: 'start',
              current: true,
              canMove: false,
              discardMove: 'cmd-1',
            ),
          ],
        ),
        sent,
      );
      await tester.tap(find.text('Contexts…'));
      await tester.pumpAndSettle();
      expect(find.text('Discard move'), findsOneWidget);
      await tester.tap(find.text('Discard move'));
      await tester.pumpAndSettle();
      expect(sent.whereType<GuiIntentAct>(), [
        const GuiIntentAct(action: ActionIntentDiscard(commandId: 'cmd-1')),
      ]);
    });

    testWidgets('a row without a queued move offers no Discard move', (
      tester,
    ) async {
      await openMore(tester, screenWith(contexts: two, contextId: 'c2'), []);
      await tester.tap(find.text('Contexts…'));
      await tester.pumpAndSettle();
      expect(find.text('Discard move'), findsNothing);
    });

    testWidgets('viewing another context sends it, the current one shows '
        'all', (tester) async {
      final sent = <GuiIntent>[];
      final screen = screenWith(contexts: two, contextId: 'c2');
      await openMore(tester, screen, sent);
      await tester.tap(find.text('Contexts…'));
      await tester.pumpAndSettle();
      await tester.tap(find.text('View').first);
      await tester.pumpAndSettle();
      expect(sent.whereType<GuiIntentAct>(), [
        const GuiIntentAct(
          action: ActionIntentViewContext(sessionId: 's1', contextId: 'c1'),
        ),
      ]);

      sent.clear();
      await tester.tap(find.byTooltip('More'));
      await tester.pumpAndSettle();
      await tester.tap(find.text('Contexts…'));
      await tester.pumpAndSettle();
      await tester.tap(find.text('View').last);
      await tester.pumpAndSettle();
      expect(sent.whereType<GuiIntentAct>(), [
        const GuiIntentAct(action: ActionIntentViewContext(sessionId: 's1')),
      ]);
    });

    testWidgets('viewing shows the hub\'s hint with a way back', (
      tester,
    ) async {
      final sent = <GuiIntent>[];
      await pumpSession(
        tester,
        screen: screenWith(),
        timeline: holding([]),
        sent: sent,
      );
      expect(find.text('Show all'), findsNothing, reason: 'no banner');

      await pumpSession(
        tester,
        screen: screenWith(
          contexts: two,
          contextId: 'c2',
          viewingId: 'c1',
          viewingHint: 'Viewing start — messages go to velvet walrus',
        ),
        timeline: holding([]),
        sent: sent,
      );
      expect(
        find.text('Viewing start — messages go to velvet walrus'),
        findsOneWidget,
      );
      await tester.tap(find.text('Show all'));
      expect(sent.whereType<GuiIntentAct>(), [
        const GuiIntentAct(action: ActionIntentViewContext(sessionId: 's1')),
      ]);
    });
  });

  testWidgets('conversation only switches from a header button that shows it', (
    tester,
  ) async {
    const off = 'Conversation only: hide tool calls';
    const on = 'Conversation only is on: show tool calls';
    IconButton button(String tooltip) => tester.widget<IconButton>(
      find.ancestor(
        of: find.byTooltip(tooltip),
        matching: find.byType(IconButton),
      ),
    );
    final sent = <GuiIntent>[];
    await pumpSession(
      tester,
      screen: screenWith(),
      timeline: holding([]),
      sent: sent,
    );
    expect(button(off).isSelected, isFalse);
    expect(button(off).style, isNull);
    await tester.tap(find.byTooltip(off));
    expect(sent.last, const GuiIntentToggleConversationOnly());

    await pumpSession(
      tester,
      screen: screenWith(conversationOnly: true),
      timeline: holding([]),
      sent: sent,
    );
    expect(button(on).isSelected, isTrue);
    expect(button(on).style, isNotNull, reason: 'filled while on');
    expect(find.byIcon(Icons.forum), findsOneWidget);
    await tester.tap(find.byTooltip(on));
    expect(sent.last, const GuiIntentToggleConversationOnly());
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
    await tester.tap(find.byTooltip('Workdir'));
    expect(
      sent.last,
      const GuiIntentAct(action: ActionIntentOpenWorkdir(sessionId: 's1')),
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

    testWidgets('an effort is picked from the list', (tester) async {
      final sent = <GuiIntent>[];
      await pumpSession(
        tester,
        screen: screenWith(),
        timeline: holding([]),
        sent: sent,
      );
      expect(find.text('effort medium'), findsOneWidget);
      await tester.tap(find.byTooltip('Effort'));
      await tester.pumpAndSettle();
      expect(find.byType(AlertDialog), findsOneWidget);
      await tester.tap(find.text('high'));
      await tester.pumpAndSettle();
      // No confirmation: the pick is sent at once.
      expect(find.byType(AlertDialog), findsNothing);
      expect(
        sent.last,
        const GuiIntentAct(
          action: ActionIntentSetEffort(
            sessionId: 's1',
            effort: EffortView.high,
          ),
        ),
      );

      // The level it runs at sends nothing.
      sent.clear();
      await tester.tap(find.byTooltip('Effort'));
      await tester.pumpAndSettle();
      await tester.tap(find.text('medium'));
      await tester.pumpAndSettle();
      expect(sent, isEmpty);
    });

    testWidgets('a model that takes no effort says so, with nothing to pick', (
      tester,
    ) async {
      final sent = <GuiIntent>[];
      await pumpSession(
        tester,
        screen: screenWith(
          agent: AgentSettingsView(
            modeLabel: 'ask',
            modelLabel: 'Haiku',
            effortLabel: 'no effort',
            modes: askAgent.modes,
            models: askAgent.models,
            efforts: const [],
          ),
        ),
        timeline: holding([]),
        sent: sent,
      );
      expect(find.text('no effort'), findsOneWidget);
      await tester.tap(find.byTooltip('Effort'));
      await tester.pumpAndSettle();
      expect(find.text('This model takes no effort level.'), findsOneWidget);
      expect(
        find.descendant(
          of: find.byType(AlertDialog),
          matching: find.byType(ListTile),
        ),
        findsNothing,
      );
      expect(sent.whereType<GuiIntentAct>(), isEmpty);
    });

    testWidgets('an effort refusal shows on its chip and in its picker', (
      tester,
    ) async {
      await pumpSession(
        tester,
        screen: screenWith(
          agent: AgentSettingsView(
            modeLabel: 'ask',
            modelLabel: 'Sonnet',
            effortLabel: 'effort medium',
            effortRefusal: 'effort max refused: not for this model',
            modes: askAgent.modes,
            models: askAgent.models,
            efforts: askAgent.efforts,
          ),
        ),
        timeline: holding([]),
        sent: [],
      );
      expect(
        find.byTooltip('effort max refused: not for this model'),
        findsOne,
      );
      await tester.tap(find.text('effort medium'));
      await tester.pumpAndSettle();
      expect(
        find.text('effort max refused: not for this model'),
        findsOneWidget,
      );
    });

    testWidgets('a narrow line folds the effort refusal onto the chip', (
      tester,
    ) async {
      tester.view
        ..physicalSize = const Size(360, 700)
        ..devicePixelRatio = 1;
      addTearDown(tester.view.reset);
      await pumpSession(
        tester,
        screen: screenWith(
          agent: AgentSettingsView(
            modeLabel: "accept edits → don't ask",
            modelLabel: 'claude-sonnet-5-5-20261001 → claude-opus-5-5',
            effortLabel: 'effort medium',
            effortRefusal: 'effort max refused: not for this model',
            modes: askAgent.modes,
            models: const [],
            efforts: askAgent.efforts,
          ),
        ),
        timeline: holding([]),
        sent: [],
      );
      expect(find.byTooltip('Effort'), findsNothing, reason: 'folded');
      expect(
        find.byTooltip('effort max refused: not for this model'),
        findsOne,
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
            modelLabel: 'model unknown',
            effortLabel: 'effort medium',
            modeRefusal: 'bypass refused: not launched for it',
            modes: askAgent.modes,
            models: const [],
            efforts: askAgent.efforts,
          ),
        ),
        timeline: holding([]),
        sent: sent,
      );
      // No model named yet: the chip still opens the name field.
      expect(find.text('model unknown'), findsOneWidget);
      expect(find.bySemanticsLabel('Model: model unknown'), findsOneWidget);
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
            effortLabel: 'effort medium',
            modes: askAgent.modes,
            models: const [],
            efforts: askAgent.efforts,
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

    testWidgets('an unknown mode and model say so, folded too', (tester) async {
      tester.view
        ..physicalSize = const Size(360, 700)
        ..devicePixelRatio = 1;
      addTearDown(tester.view.reset);
      await pumpSession(
        tester,
        screen: screenWith(
          agent: AgentSettingsView(
            modeLabel: 'mode unknown',
            modelLabel: 'unknown → claude-opus-5-5',
            effortLabel: 'effort unknown',
            modes: askAgent.modes,
            models: const [],
            efforts: askAgent.efforts,
          ),
        ),
        timeline: holding([]),
        sent: [],
      );
      expect(
        find.bySemanticsLabel(
          'Agent settings: mode unknown · unknown → claude-opus-5-5 · '
          'effort unknown',
        ),
        findsOneWidget,
      );
      await tester.tap(find.byTooltip('Agent settings'));
      await tester.pumpAndSettle();
      expect(find.text('mode unknown'), findsOneWidget);
      expect(find.text('unknown → claude-opus-5-5'), findsOneWidget);
      expect(find.text('effort unknown'), findsOneWidget);
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
      for (final tooltip in ['Permission mode', 'Model', 'Effort']) {
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
  group('semantics', () {
    testWidgets('the status-line chips read as named buttons', (tester) async {
      final semantics = tester.ensureSemantics();
      await pumpSession(
        tester,
        screen: screenWith(
          agent: AgentSettingsView(
            modeLabel: 'ask',
            modelLabel: 'Sonnet',
            effortLabel: 'effort medium',
            modelRefusal: 'no such model',
            modes: askAgent.modes,
            models: askAgent.models,
            efforts: askAgent.efforts,
          ),
        ),
        timeline: holding([]),
        sent: [],
      );
      expect(
        tester.getSemantics(find.bySemanticsLabel('Permission mode: ask')),
        matchesSemantics(
          label: 'Permission mode: ask',
          isButton: true,
          isFocusable: true,
          hasTapAction: true,
          hasFocusAction: true,
        ),
      );
      expect(
        find.bySemanticsLabel('Model: Sonnet, refused: no such model'),
        findsOneWidget,
      );
      expect(find.bySemanticsLabel('Effort: effort medium'), findsOneWidget);
      semantics.dispose();
    });

    testWidgets('the icon buttons have names', (tester) async {
      final semantics = tester.ensureSemantics();
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
      for (final (tooltip, menu) in [
        ('Search (Ctrl+F)', false),
        ('Interrupt (Ctrl+X)', false),
        ('More', true),
      ]) {
        expect(
          tester.getSemantics(find.byTooltip(tooltip)),
          matchesSemantics(
            tooltip: tooltip,
            isButton: true,
            isFocusable: true,
            hasEnabledState: true,
            isEnabled: true,
            hasTapAction: true,
            hasFocusAction: true,
            hasExpandedState: menu,
          ),
          reason: tooltip,
        );
      }
      semantics.dispose();
    });

    testWidgets('the unread rule says new messages follow', (tester) async {
      final semantics = tester.ensureSemantics();
      await pumpSession(
        tester,
        screen: screenWith(),
        timeline: holding([
          anyRow('a', RowKind.agentMessage, 'hi', unreadRule: true),
        ]),
        sent: [],
      );
      expect(find.bySemanticsLabel('new messages below'), findsOneWidget);
      semantics.dispose();
    });
  });
  group('background work', () {
    const warning = 'Stopping ends 1 background task.';
    const task = TaskRow(
      taskId: 't1',
      depth: 0,
      kindLabel: 'shell',
      title: 'npm run dev',
    );
    const child = TaskRow(
      taskId: 't2',
      depth: 1,
      kindLabel: 'subagent',
      title: 'review the diff',
      stopLabel: 'queued',
    );
    const loop = ScheduleRow(
      scheduleId: 's1',
      when: '*/5 * * * *',
      repeatLabel: 'repeats',
      prompt: 'check the build',
      savedLabel: 'saved in this folder',
    );

    Future<List<GuiIntent>> openMore(
      WidgetTester tester,
      SessionScreen screen,
    ) async {
      final sent = <GuiIntent>[];
      await pumpSession(
        tester,
        screen: screen,
        timeline: holding([]),
        sent: sent,
      );
      await tester.tap(find.byTooltip('More'));
      await tester.pumpAndSettle();
      return sent;
    }

    testWidgets('Stop agent and New context ask first while there is work', (
      tester,
    ) async {
      final sent = await openMore(
        tester,
        screenWith(
          canClear: true,
          workLabel: '1 background task',
          stopWarning: warning,
          tasks: [task],
        ),
      );
      await tester.tap(find.text('Stop agent'));
      await tester.pumpAndSettle();
      expect(find.text('Stop the agent?'), findsOneWidget);
      expect(find.text(warning), findsOneWidget);
      expect(sent.whereType<GuiIntentAct>(), isEmpty);
      await tester.tap(find.text('Cancel'));
      await tester.pumpAndSettle();
      expect(sent.whereType<GuiIntentAct>(), isEmpty);

      await tester.tap(find.byTooltip('More'));
      await tester.pumpAndSettle();
      await tester.tap(find.text('Stop agent'));
      await tester.pumpAndSettle();
      await tester.tap(find.widgetWithText(FilledButton, 'Stop agent'));
      await tester.pumpAndSettle();
      expect(sent.whereType<GuiIntentAct>(), [
        const GuiIntentAct(action: ActionIntentStopSession(sessionId: 's1')),
      ]);

      sent.clear();
      await tester.tap(find.byTooltip('More'));
      await tester.pumpAndSettle();
      await tester.tap(find.text('New context'));
      await tester.pumpAndSettle();
      expect(find.text('Start a new context?'), findsOneWidget);
      expect(sent.whereType<GuiIntentAct>(), isEmpty);
      await tester.tap(find.widgetWithText(FilledButton, 'New context'));
      await tester.pumpAndSettle();
      expect(sent.whereType<GuiIntentAct>(), [
        const GuiIntentAct(action: ActionIntentClearContext(sessionId: 's1')),
      ]);
    });

    testWidgets('without work they send at once', (tester) async {
      final sent = await openMore(tester, screenWith(canClear: true));
      expect(find.text('nothing running'), findsOneWidget);
      await tester.tap(find.text('Stop agent'));
      await tester.pumpAndSettle();
      expect(find.byType(AlertDialog), findsNothing);
      expect(sent.whereType<GuiIntentAct>(), [
        const GuiIntentAct(action: ActionIntentStopSession(sessionId: 's1')),
      ]);
    });

    testWidgets('the sheet lists tasks and loops and stops a task after a '
        'confirm; a loop has no stop', (tester) async {
      final sent = await openMore(
        tester,
        screenWith(
          workLabel: '2 background tasks · 1 loop',
          stopWarning: warning,
          tasks: [task, child],
          schedules: [loop],
        ),
      );
      expect(find.text('2 background tasks · 1 loop'), findsOneWidget);
      await tester.tap(find.text('Background work'));
      await tester.pumpAndSettle();
      expect(find.text('npm run dev'), findsOneWidget);
      expect(find.text('shell'), findsOneWidget);
      expect(find.text('subagent · stop queued'), findsOneWidget);
      expect(find.text('check the build'), findsOneWidget);
      expect(
        find.text('loop */5 * * * * · repeats · saved in this folder'),
        findsOneWidget,
      );
      expect(
        find.text('Loops as of the last turn; ask the agent to end one.'),
        findsOneWidget,
      );
      // The child sits deeper than its parent; only tasks have a Stop.
      expect(
        tester.getTopLeft(find.text('review the diff')).dx,
        greaterThan(tester.getTopLeft(find.text('npm run dev')).dx),
      );
      expect(find.widgetWithText(TextButton, 'Stop'), findsNWidgets(2));

      await tester.tap(find.widgetWithText(TextButton, 'Stop').first);
      await tester.pumpAndSettle();
      expect(find.text('Stop npm run dev?'), findsOneWidget);
      expect(sent.whereType<GuiIntentAct>(), isEmpty);
      await tester.tap(find.widgetWithText(FilledButton, 'Stop'));
      await tester.pumpAndSettle();
      expect(sent.whereType<GuiIntentAct>(), [
        const GuiIntentAct(
          action: ActionIntentStopTask(sessionId: 's1', taskId: 't1'),
        ),
      ]);
    });

    testWidgets('an empty sheet says nothing runs', (tester) async {
      await openMore(tester, screenWith());
      await tester.tap(find.text('Background work'));
      await tester.pumpAndSettle();
      expect(
        find.text('Nothing is running in the background.'),
        findsOneWidget,
      );
    });

    testWidgets('switching and moving the current context warn', (
      tester,
    ) async {
      const contexts = [
        ContextRow(
          contextId: 'c1',
          label: 'start',
          current: false,
          canMove: false,
        ),
        ContextRow(
          contextId: 'c2',
          label: 'walrus',
          current: true,
          canMove: true,
        ),
      ];
      final sent = await openMore(
        tester,
        screenWith(
          contexts: contexts,
          contextId: 'c2',
          stopWarning: warning,
          tasks: [task],
        ),
      );
      await tester.tap(find.text('Contexts…'));
      await tester.pumpAndSettle();

      await tester.tap(find.text('Switch'));
      await tester.pumpAndSettle();
      expect(find.text('Switch context?'), findsOneWidget);
      expect(sent.whereType<GuiIntentAct>(), isEmpty);
      await tester.tap(find.widgetWithText(FilledButton, 'Switch'));
      await tester.pumpAndSettle();
      expect(sent.whereType<GuiIntentAct>(), [
        const GuiIntentAct(
          action: ActionIntentSwitchContext(sessionId: 's1', contextId: 'c1'),
        ),
      ]);

      sent.clear();
      await tester.tap(find.byTooltip('More'));
      await tester.pumpAndSettle();
      await tester.tap(find.text('Contexts…'));
      await tester.pumpAndSettle();
      await tester.tap(
        find.widgetWithText(TextButton, 'Move to new session').last,
      );
      await tester.pumpAndSettle();
      // One dialog, the warning in its body.
      expect(find.byType(AlertDialog), findsNWidgets(2));
      expect(find.textContaining(warning), findsOneWidget);
    });
  });

  group('subagent', () {
    ToolFacts agentFacts(String? opens) => ToolFacts(
      kind: ToolKindView.subagent,
      name: 'Agent',
      title: 'Explore sync',
      state: ToolState.running,
      stateLabel: 'Async agent launched · 0.1s',
      opens: opens,
      status: opens == null ? null : 'running · 3 steps · Write src/sync.rs',
    );

    testWidgets('a subagent row shows how it stands; Open sends '
        'ViewSubagent; another call has no Open', (tester) async {
      final sent = <GuiIntent>[];
      await pumpSession(
        tester,
        screen: screenWith(),
        timeline: holding([
          anyRow('s:1', RowKind.tool, '', tool: agentFacts('call-1')),
          anyRow('s:2', RowKind.tool, '', tool: shellFacts('ls')),
        ]),
        sent: sent,
      );
      expect(
        find.text('running · 3 steps · Write src/sync.rs'),
        findsOneWidget,
      );
      // The status stands in for the launch's own label.
      expect(find.text('Async agent launched · 0.1s'), findsNothing);
      expect(find.text('exit 0 · 0.4s'), findsOneWidget);
      expect(find.widgetWithText(TextButton, 'Open'), findsOneWidget);
      sent.clear();
      await tester.tap(find.widgetWithText(TextButton, 'Open'));
      await tester.pump();
      expect(sent, [
        const GuiIntentAct(
          action: ActionIntentViewSubagent(sessionId: 's1', call: 'call-1'),
        ),
      ]);
    });

    testWidgets('its approval says which subagent asks', (tester) async {
      await pumpSession(
        tester,
        screen: screenWith(
          approval: const ApprovalPanel(
            approvalId: 'a1',
            from: 'from subagent: Explore sync · open it to see the call',
            kind: ToolKindView.fileWrite,
            tool: 'Write',
            title: 'src/sync.rs',
            canAnswer: true,
            suggestions: [],
            alwaysPicks: false,
          ),
        ),
        timeline: holding([]),
        sent: [],
      );
      expect(find.text('Approval needed'), findsOneWidget);
      expect(
        find.text('from subagent: Explore sync · open it to see the call'),
        findsOneWidget,
      );
    });

    testWidgets('the main agent\'s approval names no subagent', (tester) async {
      await pumpSession(
        tester,
        screen: screenWith(
          approval: const ApprovalPanel(
            approvalId: 'a1',
            kind: ToolKindView.shell,
            tool: 'Bash',
            title: 'ls',
            canAnswer: true,
            suggestions: [],
            alwaysPicks: false,
          ),
        ),
        timeline: holding([]),
        sent: [],
      );
      expect(find.text('Approval needed'), findsOneWidget);
      expect(find.textContaining('from subagent'), findsNothing);
    });

    testWidgets('a subagent\'s task opens from the background sheet', (
      tester,
    ) async {
      final sent = <GuiIntent>[];
      await pumpSession(
        tester,
        screen: screenWith(
          workLabel: '2 background tasks',
          tasks: const [
            TaskRow(
              taskId: 't0',
              depth: 0,
              kindLabel: 'shell',
              title: 'npm test',
            ),
            TaskRow(
              taskId: 't1',
              depth: 0,
              kindLabel: 'subagent',
              title: 'Explore sync',
              call: 'call-1',
            ),
          ],
        ),
        timeline: holding([]),
        sent: sent,
      );
      await tester.tap(find.byTooltip('More'));
      await tester.pumpAndSettle();
      await tester.tap(find.text('Background work'));
      await tester.pumpAndSettle();
      expect(find.widgetWithText(TextButton, 'Stop'), findsNWidgets(2));
      expect(find.widgetWithText(TextButton, 'Open'), findsOneWidget);
      sent.clear();
      await tester.tap(find.widgetWithText(TextButton, 'Open'));
      await tester.pumpAndSettle();
      expect(sent, [
        const GuiIntentAct(
          action: ActionIntentViewSubagent(sessionId: 's1', call: 'call-1'),
        ),
      ]);
      expect(find.text('npm test'), findsNothing, reason: 'the sheet closed');
    });

    testWidgets('its view is headed by its title, where it is and how it '
        'stands; Stop asks, then stops its task', (tester) async {
      final sent = <GuiIntent>[];
      await pumpSession(
        tester,
        screen: screenWith(
          subagent: subagentHeader('call-1', taskId: 't1'),
          composer: const ComposerView(
            text: '',
            cursorUtf16: 0,
            rev: 1,
            mode: ComposerModeView.agent,
            shellEnabled: false,
            canStop: false,
            toLabel: 'to the main agent',
          ),
        ),
        timeline: holding([]),
        sent: sent,
      );
      expect(find.text('Explore sync'), findsOneWidget);
      expect(find.text('work'), findsOneWidget);
      expect(find.text('/home/me/work'), findsNothing);
      expect(
        find.text('running · 3 steps · Write src/sync.rs'),
        findsOneWidget,
      );
      expect(find.text('to the main agent'), findsOneWidget);

      sent.clear();
      await tester.tap(find.byTooltip('Stop this subagent'));
      await tester.pumpAndSettle();
      expect(find.text('Stop this subagent?'), findsOneWidget);
      expect(sent, isEmpty);
      await tester.tap(find.widgetWithText(FilledButton, 'Stop subagent'));
      await tester.pumpAndSettle();
      expect(sent, [
        const GuiIntentAct(
          action: ActionIntentStopTask(sessionId: 's1', taskId: 't1'),
        ),
      ]);
    });

    testWidgets('a subagent with no task to stop has no Stop', (tester) async {
      await pumpSession(
        tester,
        screen: screenWith(subagent: subagentHeader('call-1')),
        timeline: holding([]),
        sent: [],
      );
      expect(find.text('Explore sync'), findsOneWidget);
      expect(find.byTooltip('Stop this subagent'), findsNothing);
    });
  });
}
