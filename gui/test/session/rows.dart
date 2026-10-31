import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/session/search_holder.dart';
import 'package:skywave_gui/session/session_page.dart';
import 'package:skywave_gui/session/timeline_holder.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// A row of any kind, plain unless told otherwise.
TimelineRow anyRow(
  String key,
  RowKind kind,
  String text, {
  String? author,
  bool own = false,
  bool expandable = false,
  bool expanded = false,
  bool discardable = false,
  String? commandId,
  OutgoingView? outgoing,
  String? deliveryLabel,
  ToolFacts? tool,
  List<SectionView> sections = const [],
  String? timeLabel,
  bool turnStart = false,
  bool unreadRule = false,
}) => TimelineRow(
  key: key,
  kind: kind,
  author: author,
  own: own,
  text: text,
  timeLabel: timeLabel,
  turnStart: turnStart,
  unreadRule: unreadRule,
  selectable: true,
  expandable: expandable,
  expanded: expanded,
  outgoing: outgoing,
  deliveryLabel: deliveryLabel,
  discardable: discardable,
  commandId: commandId,
  tool: tool,
  sections: sections,
);

ToolFacts shellFacts(String title, {ToolState state = ToolState.done}) =>
    ToolFacts(
      kind: ToolKindView.shell,
      name: 'Bash',
      title: title,
      state: state,
      stateLabel: 'exit 0 · 0.4s',
    );

SectionView lines(
  List<String> texts, {
  BlobFacts? blob,
  bool cutShort = false,
  String? note,
  LineStyle style = LineStyle.plain,
}) => SectionView(
  format: SectionFormat.text,
  numbered: false,
  firstLine: 1,
  input: false,
  lines: [for (final t in texts) SectionLine(text: t, style: style)],
  note: note,
  blob: blob,
  cutShort: cutShort,
);

const composerAt1 = ComposerView(text: '', cursorUtf16: 0, rev: 1);

SessionScreen screenWith({
  ComposerView composer = composerAt1,
  ApprovalPanel? approval,
  List<String> usage = const [],
  RenamePrompt? renamePrompt,
  String? archivedLabel,
}) => SessionScreen(
  sessionId: 's1',
  title: 'work',
  workdir: '/home/me/work',
  state: AgentState.working,
  badgeLabel: 'working',
  agentLabel: 'agent working',
  rules: const [],
  usage: usage,
  composer: composer,
  approval: approval,
  renamePrompt: renamePrompt,
  archivedLabel: archivedLabel,
);

/// The session page over [timeline] and [search], recording what it sends.
Future<void> pumpSession(
  WidgetTester tester, {
  required SessionScreen screen,
  required TimelineHolder timeline,
  required List<GuiIntent> sent,
  SearchHolder? search,
}) async {
  await tester.pumpWidget(
    MaterialApp(
      home: SessionPage(
        screen: screen,
        timeline: timeline,
        search: search,
        send: sent.add,
      ),
    ),
  );
  await tester.pumpAndSettle();
}
