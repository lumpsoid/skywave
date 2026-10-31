import 'dart:math';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/session/approval_panel.dart';
import 'package:skywave_gui/session/composer.dart';
import 'package:skywave_gui/session/look.dart';
import 'package:skywave_gui/sessions/rename_dialog.dart';
import 'package:skywave_gui/session/search_bar.dart';
import 'package:skywave_gui/session/search_holder.dart';
import 'package:skywave_gui/session/status_line.dart';
import 'package:skywave_gui/session/timeline_holder.dart';
import 'package:skywave_gui/session/timeline_view.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// The open session: its header and status line, a bar under them while
/// the agent works, the search bar while a search is open, the timeline,
/// the approval it waits on, and the composer; the rename prompt over it
/// while open.
///
/// Desktop keys, as in the TUI: Ctrl+X interrupts (unless text is
/// selected: then it cuts), Ctrl+F searches, PgUp/PgDn scroll, Esc closes
/// the search, leaves the composer, or goes back.
class SessionPage extends StatefulWidget {
  const SessionPage({
    required this.screen,
    required this.timeline,
    required this.send,
    this.search,
    super.key,
  });

  final SessionScreen screen;
  final TimelineHolder timeline;
  final SendIntent send;

  /// The app's search holder; without one the page has no search.
  final SearchHolder? search;

  @override
  State<SessionPage> createState() => _SessionPageState();
}

class _SessionPageState extends State<SessionPage> {
  final _timelineView = GlobalKey<TimelineViewState>();
  final _composerFocus = FocusNode(debugLabel: 'composer');
  final _searchFocus = FocusNode(debugLabel: 'search');
  SearchHolder? _ownSearch;

  SearchHolder get _search => widget.search ?? (_ownSearch ??= SearchHolder());

  SessionScreen get _screen => widget.screen;

  @override
  void didUpdateWidget(SessionPage oldWidget) {
    super.didUpdateWidget(oldWidget);
    _announceRefusals(oldWidget.screen);
  }

  /// What the agent or server refused, in words, for each thing this page
  /// asks of it: an interrupt, a mode, a model.
  static List<String?> _refusalsOf(SessionScreen screen) => [
    if (screen.interruptRefusal case final refusal?)
      'Interrupt failed: $refusal'
    else
      null,
    screen.agent.modeRefusal,
    screen.agent.modelRefusal,
  ];

  /// Says each refusal that arrived since [before] once, in a snack bar;
  /// its control keeps it afterwards. Those already there when the page
  /// opened, or another session was shown, were not news here.
  void _announceRefusals(SessionScreen before) {
    if (before.sessionId != _screen.sessionId) return;
    final was = _refusalsOf(before);
    final now = _refusalsOf(_screen);
    final news = [
      for (var i = 0; i < now.length; i++)
        if (now[i] case final refusal? when refusal != was[i]) refusal,
    ];
    if (news.isEmpty) return;
    // Not mid-build: the messenger above rebuilds to show it.
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (!mounted) return;
      final messenger = ScaffoldMessenger.maybeOf(context);
      for (final refusal in news) {
        messenger?.showSnackBar(
          SnackBar(content: Text(refusal), showCloseIcon: true),
        );
      }
    });
  }

  @override
  void dispose() {
    _composerFocus.dispose();
    _searchFocus.dispose();
    _ownSearch?.dispose();
    super.dispose();
  }

  /// Interrupts the agent's turn, if it has one and no interrupt from
  /// here is already on its way.
  void _interrupt() {
    if (!_screen.canInterrupt || _screen.interruptBusy) return;
    widget.send.act(ActionIntentInterrupt(sessionId: _screen.sessionId));
  }

  void _openSearch() {
    final search = _search.search;
    if (search != null && search.open) {
      _searchFocus.requestFocus();
    } else {
      widget.send(GuiIntentSearchTyped(query: search?.query ?? ''));
    }
  }

  /// Reveals the call an approval gates, expanded.
  void _showCall(String key) {
    _timelineView.currentState?.reveal(key);
    widget.timeline.select(key);
    final index = widget.timeline.indexOf(key);
    if (index < 0) return;
    final row = widget.timeline.rows[index];
    if (row.expandable && !row.expanded) {
      widget.send(GuiIntentExpandItem(key: key));
    }
  }

  /// Text is selected in the focused field: Ctrl+X cuts it there.
  bool get _textSelected {
    final focused = FocusManager.instance.primaryFocus?.context;
    final editable = focused?.findAncestorStateOfType<EditableTextState>();
    final selection = editable?.textEditingValue.selection;
    return selection != null && selection.isValid && !selection.isCollapsed;
  }

  KeyEventResult _onKey(FocusNode node, KeyEvent event) {
    if (event is! KeyDownEvent && event is! KeyRepeatEvent) {
      return KeyEventResult.ignored;
    }
    final keyboard = HardwareKeyboard.instance;
    final ctrl = keyboard.isControlPressed;
    final key = event.logicalKey;
    final timeline = _timelineView.currentState;
    if (ctrl && key == LogicalKeyboardKey.keyX) {
      if (_textSelected) return KeyEventResult.ignored;
      _interrupt();
      return KeyEventResult.handled;
    }
    if (ctrl && key == LogicalKeyboardKey.keyF) {
      _openSearch();
      return KeyEventResult.handled;
    }
    if (key == LogicalKeyboardKey.pageUp ||
        key == LogicalKeyboardKey.pageDown) {
      timeline?.page(older: key == LogicalKeyboardKey.pageUp);
      return KeyEventResult.handled;
    }
    if (key == LogicalKeyboardKey.escape) {
      final search = _search.search;
      if (search != null && search.open) {
        widget.send(const GuiIntentSearchClose());
      } else if (_composerFocus.hasFocus) {
        timeline?.focusNode.requestFocus();
      } else {
        widget.send.act(const ActionIntentBack());
      }
      return KeyEventResult.handled;
    }
    return KeyEventResult.ignored;
  }

  @override
  Widget build(BuildContext context) {
    final screen = _screen;
    final theme = Theme.of(context);
    final rename = screen.renamePrompt;
    // Only a stopped agent surely has no process: an idle one stays warm a
    // while, an errored one may have failed a turn and still run.
    final stopped = screen.state == AgentState.stopped;

    return Stack(
      children: [
        Focus(
          onKeyEvent: _onKey,
          child: Scaffold(
            appBar: AppBar(
              // The header's content keeps to the timeline's column.
              automaticallyImplyLeading: false,
              titleSpacing: 0,
              toolbarHeight: _Title.heightOf(context),
              title: _Column(
                child: Row(
                  children: [
                    const SizedBox(width: 4),
                    BackButton(
                      onPressed: () =>
                          widget.send.act(const ActionIntentBack()),
                    ),
                    const SizedBox(width: 4),
                    Expanded(child: _Title(screen: screen)),
                    IconButton(
                      tooltip: 'Search (Ctrl+F)',
                      onPressed: _openSearch,
                      icon: const Icon(Icons.search),
                    ),
                    _InterruptButton(screen: screen, onPressed: _interrupt),
                    PopupMenuButton<VoidCallback>(
                      tooltip: 'More',
                      onSelected: (action) => action(),
                      itemBuilder: (context) => [
                        PopupMenuItem(
                          value: () => widget.send(
                            GuiIntentRenameOpen(sessionId: screen.sessionId),
                          ),
                          child: const ListTile(
                            leading: Icon(Icons.drive_file_rename_outline),
                            title: Text('Rename'),
                          ),
                        ),
                        PopupMenuItem(
                          enabled: !stopped,
                          value: () => widget.send.act(
                            ActionIntentStopSession(
                              sessionId: screen.sessionId,
                            ),
                          ),
                          child: ListTile(
                            enabled: !stopped,
                            leading: Icon(
                              Icons.power_settings_new,
                              color: stopped ? null : theme.colorScheme.error,
                            ),
                            title: const Text('Stop agent'),
                            subtitle: stopped
                                ? const Text(
                                    'Already stopped · a message restarts it',
                                  )
                                : switch (screen.stopLabel) {
                                    final label? => Text(label),
                                    null => null,
                                  },
                          ),
                        ),
                      ],
                    ),
                    const SizedBox(width: 4),
                  ],
                ),
              ),
              bottom: PreferredSize(
                preferredSize: Size.fromHeight(StatusLine.heightOf(context)),
                child: _Column(
                  child: StatusLine(screen: screen, send: widget.send),
                ),
              ),
            ),
            body: Column(
              children: [
                _WorkingBar(working: screen.state == AgentState.working),
                if (screen.archivedLabel case final archived?)
                  Material(
                    color: theme.colorScheme.secondaryContainer,
                    child: Padding(
                      padding: const EdgeInsets.symmetric(
                        horizontal: 16,
                        vertical: 6,
                      ),
                      child: Row(
                        children: [
                          Icon(
                            Icons.inventory_2_outlined,
                            size: 16,
                            color: theme.colorScheme.onSecondaryContainer,
                          ),
                          const SizedBox(width: 8),
                          Expanded(
                            child: Text(
                              archived,
                              style: theme.textTheme.labelMedium?.copyWith(
                                color: theme.colorScheme.onSecondaryContainer,
                              ),
                            ),
                          ),
                        ],
                      ),
                    ),
                  ),
                ListenableBuilder(
                  listenable: _search,
                  builder: (context, _) {
                    final search = _search.search;
                    if (search == null || !search.open) {
                      return const SizedBox.shrink();
                    }
                    return SessionSearchBar(
                      search: search,
                      send: widget.send,
                      focusNode: _searchFocus,
                      newestVisibleKey: () =>
                          _timelineView.currentState?.newestVisibleKey(),
                      onStepped: () =>
                          _timelineView.currentState?.focusNode.requestFocus(),
                    );
                  },
                ),
                Expanded(
                  child: TimelineView(
                    key: _timelineView,
                    timeline: widget.timeline,
                    search: _search,
                    send: widget.send,
                  ),
                ),
                if (screen.approval case final approval?)
                  _Column(
                    child: ApprovalPanelView(
                      approval: approval,
                      send: widget.send,
                      onShowCall: approval.gatedKey == null
                          ? null
                          : () => _showCall(approval.gatedKey!),
                    ),
                  ),
                ColoredBox(
                  color: theme.colorScheme.surfaceContainer,
                  child: _Column(
                    child: Composer(
                      sessionId: screen.sessionId,
                      composer: screen.composer,
                      send: widget.send,
                      focusNode: _composerFocus,
                    ),
                  ),
                ),
              ],
            ),
            backgroundColor: theme.colorScheme.surface,
          ),
        ),
        if (rename != null)
          Positioned.fill(
            child: RenameDialog(
              key: ValueKey(rename.sessionId),
              prompt: rename,
              send: widget.send,
            ),
          ),
      ],
    );
  }
}

/// Interrupts the agent's turn: off while there is none, a spinner while
/// the interrupt is on its way, the error colour with the reason as its
/// tooltip when it failed and the agent still works.
class _InterruptButton extends StatelessWidget {
  const _InterruptButton({required this.screen, required this.onPressed});

  final SessionScreen screen;
  final VoidCallback onPressed;

  @override
  Widget build(BuildContext context) {
    if (screen.interruptBusy) {
      return const IconButton(
        tooltip: 'Interrupting…',
        onPressed: null,
        icon: SizedBox.square(
          dimension: 18,
          child: CircularProgressIndicator(strokeWidth: 2),
        ),
      );
    }
    final refusal = screen.interruptRefusal;
    return IconButton(
      tooltip: refusal == null
          ? 'Interrupt (Ctrl+X)'
          : 'Interrupt failed: $refusal',
      onPressed: screen.canInterrupt ? onPressed : null,
      icon: refusal == null
          ? const Icon(Icons.pan_tool_outlined)
          : Icon(Icons.pan_tool, color: Theme.of(context).colorScheme.error),
    );
  }
}

/// A thin bar under the header while the agent works, next to the state
/// on the status line: it runs, or holds still where animations are
/// off. Its room is kept when it is gone, so the timeline never shifts.
class _WorkingBar extends StatelessWidget {
  const _WorkingBar({required this.working});

  /// The bar's thickness.
  static const height = 2.0;

  final bool working;

  @override
  Widget build(BuildContext context) {
    if (!working) return const SizedBox(height: height);
    final still = MediaQuery.disableAnimationsOf(context);
    return LinearProgressIndicator(
      minHeight: height,
      value: still ? 1 : null,
      semanticsLabel: 'Agent working',
      color: SkyColors.of(context).agent(AgentState.working),
      backgroundColor: Colors.transparent,
    );
  }
}

/// The timeline's column width, for what sits over and under it.
class _Column extends StatelessWidget {
  const _Column({required this.child});

  final Widget child;

  @override
  Widget build(BuildContext context) {
    return Center(
      child: ConstrainedBox(
        constraints: const BoxConstraints(maxWidth: timelineMaxWidth),
        child: child,
      ),
    );
  }
}

/// The header's title: the session's name with its rename on the way,
/// then the workdir. The agent's state is on the status line.
class _Title extends StatelessWidget {
  const _Title({required this.screen});

  final SessionScreen screen;

  /// The toolbar's height: the standard one, or taller when the text
  /// scale makes the title's two lines and a margin (8) need more.
  static double heightOf(BuildContext context) {
    final text = Theme.of(context).textTheme;
    final lines =
        lineHeightOf(context, text.titleMedium!) +
        lineHeightOf(context, text.labelMedium!);
    return max(kToolbarHeight, (lines + 8).ceilToDouble());
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final colors = SkyColors.of(context);
    final rename = screen.rename;
    final sub = theme.textTheme.labelMedium?.copyWith(color: colors.muted);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      mainAxisSize: MainAxisSize.min,
      children: [
        Row(
          children: [
            Flexible(
              child: Text(
                screen.title,
                maxLines: 1,
                overflow: TextOverflow.ellipsis,
                style: theme.textTheme.titleMedium,
              ),
            ),
            if (screen.renameLabel case final label?) ...[
              const SizedBox(width: 8),
              StateBadge(
                label: label,
                color: rename == null
                    ? colors.muted
                    : colors.outgoing(rename.state),
              ),
            ],
          ],
        ),
        if (screen.workdir case final workdir?)
          Text(
            workdir,
            maxLines: 1,
            overflow: TextOverflow.ellipsis,
            style: sub,
          ),
      ],
    );
  }
}
