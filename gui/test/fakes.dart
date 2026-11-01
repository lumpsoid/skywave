import 'package:skywave_gui/effects/effects.dart';
import 'package:skywave_gui/holders.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// Records every effect it is asked to carry out, as `kind: text`.
class FakeEffectSink implements EffectSink {
  final done = <String>[];

  @override
  void notify({
    required String title,
    required String body,
    String? sessionId,
  }) => done.add('notify: $body ($sessionId)');

  @override
  void copy(String text) => done.add('copy: $text');

  @override
  void openFile(String path) => done.add('open: $path');

  @override
  void notice(String text) => done.add('notice: $text');

  @override
  void error(String text) => done.add('error: $text');
}

/// Holders that are never connected: tests feed them with `apply`.
Holders fakeHolders([EffectSink? effects]) =>
    Holders(effects: effects ?? FakeEffectSink());

/// A plain agent message row keyed [key].
TimelineRow row(String key, String text) => TimelineRow(
  key: key,
  kind: RowKind.agentMessage,
  author: 'agent',
  own: false,
  text: text,
  turnStart: false,
  unreadRule: false,
  selectable: true,
  expandable: false,
  expanded: false,
  discardable: false,
  sections: const [],
);

/// An own message, pending or logged.
TimelineRow message(String key, String text, {required bool pending}) =>
    TimelineRow(
      key: key,
      kind: pending ? RowKind.pendingMessage : RowKind.userMessage,
      author: 'you',
      own: true,
      text: text,
      turnStart: true,
      unreadRule: false,
      selectable: true,
      expandable: false,
      expanded: false,
      discardable: pending,
      sections: const [],
    );

const _composer = ComposerView(text: '', cursorUtf16: 0, rev: 1);

/// An agent that has not said its mode, model or effort yet.
const unknownAgent = AgentSettingsView(
  modeLabel: 'mode unknown',
  modelLabel: 'model unknown',
  effortLabel: 'effort unknown',
  modes: [],
  models: [],
  efforts: [],
);

ScreenUpdate pairingScreen({bool canGoBack = false}) => ScreenUpdate(
  screen: const ScreenViewPairing(
    state: PairingState.idle,
    form: PairForm(address: '', code: '', fingerprint: '', rev: 0),
  ),
  canGoBack: canGoBack,
  revokedFullScreen: false,
);

ScreenUpdate listScreen() => const ScreenUpdate(
  screen: ScreenViewSessionList(
    kind: ListKindView.active,
    title: 'Sessions',
    emptyLabel: 'No sessions yet. Start one.',
    rows: [],
    otherCount: 0,
    creating: [],
  ),
  canGoBack: false,
  revokedFullScreen: false,
);

ScreenUpdate sessionScreen() => const ScreenUpdate(
  screen: ScreenViewSession(
    session: SessionScreen(
      sessionId: 's1',
      title: 'work',
      state: AgentState.idle,
      badgeLabel: 'idle',
      agentLabel: 'agent idle',
      canInterrupt: false,
      interruptBusy: false,
      canClear: false,
      contexts: [ContextRow(contextId: 'c1', label: 'start', current: true)],
      contextId: 'c1',
      rules: [],
      agent: unknownAgent,
      usage: [],
      composer: _composer,
      snippets: [],
    ),
  ),
  canGoBack: true,
  revokedFullScreen: false,
);
