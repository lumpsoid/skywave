import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/session/search_holder.dart';
import 'package:skywave_gui/session/session_page.dart';
import 'package:skywave_gui/session/shell_holder.dart';
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
  List<MarkdownBlock> markdown = const [],
  String? timeLabel,
  String? mark,
  bool turnStart = false,
  bool unreadRule = false,
}) => TimelineRow(
  key: key,
  kind: kind,
  author: author,
  own: own,
  text: text,
  markdown: markdown,
  mark: mark,
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
  String? label,
  LineStyle style = LineStyle.plain,
}) => SectionView(
  format: SectionFormat.text,
  numbered: false,
  firstLine: 1,
  input: false,
  label: label,
  lines: [for (final t in texts) SectionLine(text: t, style: style)],
  markdown: const [],
  note: note,
  blob: blob,
  cutShort: cutShort,
);

const composerAt1 = ComposerView(
  text: '',
  cursorUtf16: 0,
  rev: 1,
  mode: ComposerModeView.agent,
  shellEnabled: false,
  canStop: false,
  images: [],
  preparing: 0,
);

ModeOption _mode(
  PermissionModeView mode,
  String label, {
  bool current = false,
  bool confirm = false,
}) => ModeOption(
  mode: mode,
  label: label,
  detail: '$label detail',
  needsConfirmation: confirm,
  current: current,
  pending: false,
);

/// An agent in ask mode running Sonnet at medium effort, with every mode
/// and two models to switch to, and three efforts.
final askAgent = AgentSettingsView(
  modeLabel: 'ask',
  modelLabel: 'Sonnet',
  effortLabel: 'effort medium',
  modelName: 'model-s',
  modes: [
    _mode(PermissionModeView.ask, 'ask', current: true),
    _mode(PermissionModeView.acceptEdits, 'accept edits'),
    _mode(PermissionModeView.plan, 'plan'),
    _mode(PermissionModeView.auto, 'auto'),
    _mode(PermissionModeView.dontAsk, "don't ask"),
    _mode(PermissionModeView.bypass, 'bypass', confirm: true),
  ],
  models: const [
    ModelOption(
      id: 'sonnet',
      label: 'Sonnet',
      detail: 'fast',
      current: true,
      pending: false,
    ),
    ModelOption(id: 'opus', label: 'Opus', current: false, pending: false),
  ],
  efforts: const [
    EffortOption(
      effort: EffortView.low,
      label: 'low',
      current: false,
      pending: false,
    ),
    EffortOption(
      effort: EffortView.medium,
      label: 'medium',
      current: true,
      pending: false,
    ),
    EffortOption(
      effort: EffortView.high,
      label: 'high',
      current: false,
      pending: false,
    ),
  ],
);

const _badges = {
  AgentState.idle: 'idle',
  AgentState.working: 'working',
  AgentState.awaitingApproval: 'approval',
  AgentState.errored: 'error',
  AgentState.stopped: 'stopped',
};

/// A session screen, idle unless told otherwise: a working agent's bar
/// runs, so a test showing one pumps rather than settles.
SessionScreen screenWith({
  ComposerView composer = composerAt1,
  List<SnippetView> snippets = const [],
  ApprovalPanel? approval,
  QuestionPanel? question,
  List<String> usage = const [],
  RenamePrompt? renamePrompt,
  String? archivedLabel,
  AgentSettingsView? agent,
  AgentState state = AgentState.idle,
  bool canInterrupt = true,
  bool interruptBusy = false,
  String? interruptRefusal,
  bool canClear = false,
  List<ContextRow> contexts = const [
    ContextRow(contextId: 'c1', label: 'start', current: true, canMove: false),
  ],
  String contextId = 'c1',
  String? viewingId,
  String? viewingHint,
  String? clearLabel,
  String? switchLabel,
  String? workLabel,
  String? stopWarning,
  List<TaskRow> tasks = const [],
  List<ScheduleRow> schedules = const [],
  SubagentHeaderView? subagent,
  bool conversationOnly = false,
}) => SessionScreen(
  sessionId: 's1',
  title: 'work',
  workdir: '/home/me/skywave',
  workdirName: 'skywave',
  state: state,
  badgeLabel: _badges[state]!,
  agentLabel: 'agent ${_badges[state]}',
  canInterrupt: canInterrupt,
  interruptBusy: interruptBusy,
  interruptRefusal: interruptRefusal,
  canClear: canClear,
  workLabel: workLabel,
  stopWarning: stopWarning,
  stopHeadings: const StopHeadings(
    stopAgent: 'Stop the agent?',
    newContext: 'Start a new context?',
    switchContext: 'Switch context?',
    moveContext: 'Move this context to a new session?',
    stopTask: 'Stop this subagent?',
  ),
  tasks: tasks,
  schedules: schedules,
  loopsNote: schedules.isEmpty
      ? null
      : 'Loops as of the last turn; ask the agent to end one.',
  workEmptyLabel: 'Nothing is running in the background.',
  contexts: contexts,
  contextId: contextId,
  viewingId: viewingId,
  viewingHint: viewingHint,
  clearLabel: clearLabel,
  switchLabel: switchLabel,
  rules: const [],
  agent: agent ?? askAgent,
  usage: usage,
  composer: composer,
  snippets: snippets,
  approval: approval,
  question: question,
  subagent: subagent,
  renamePrompt: renamePrompt,
  archivedLabel: archivedLabel,
  conversationOnly: conversationOnly,
);

/// The session page over [timeline], [shell] and [search], recording what
/// it sends.
Future<void> pumpSession(
  WidgetTester tester, {
  required SessionScreen screen,
  required TimelineHolder timeline,
  required List<GuiIntent> sent,
  SearchHolder? search,
  ShellHolder? shell,
}) async {
  await tester.pumpWidget(
    MaterialApp(
      home: SessionPage(
        screen: screen,
        timeline: timeline,
        shell: shell,
        search: search,
        send: sent.add,
      ),
    ),
  );
  await tester.pumpAndSettle();
}
