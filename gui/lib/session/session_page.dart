import 'dart:math';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/session/approval_panel.dart';
import 'package:skywave_gui/session/question_panel.dart';
import 'package:skywave_gui/session/background_sheet.dart';
import 'package:skywave_gui/session/composer.dart';
import 'package:skywave_gui/session/context_picker.dart';
import 'package:skywave_gui/session/look.dart';
import 'package:skywave_gui/sessions/rename_dialog.dart';
import 'package:skywave_gui/session/search_bar.dart';
import 'package:skywave_gui/session/search_holder.dart';
import 'package:skywave_gui/session/shell_holder.dart';
import 'package:skywave_gui/session/shell_view.dart';
import 'package:skywave_gui/session/status_line.dart';
import 'package:skywave_gui/session/timeline_holder.dart';
import 'package:skywave_gui/session/timeline_view.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// The open session: its header and status line, a bar under them while
/// the agent works, the search bar while a search is open, the timeline,
/// the approval it waits on, and the composer; the rename prompt over it
/// while open. In shell mode the shell runs take the timeline's place.
///
/// Desktop keys, as in the TUI: Ctrl+X interrupts (unless text is
/// selected: then it cuts; in shell mode it stops the shell), Ctrl+F
/// searches, PgUp/PgDn scroll, Esc closes the search, leaves the
/// composer, or goes back.
class SessionPage extends StatefulWidget {
  const SessionPage({
    required this.screen,
    required this.timeline,
    required this.send,
    this.shell,
    this.search,
    super.key,
  });

  final SessionScreen screen;
  final TimelineHolder timeline;
  final SendIntent send;

  /// The app's shell holder; without one the page keeps its own.
  final ShellHolder? shell;

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
  ShellHolder? _ownShell;

  SearchHolder get _search => widget.search ?? (_ownSearch ??= SearchHolder());

  ShellHolder get _shellRuns => widget.shell ?? (_ownShell ??= ShellHolder());

  SessionScreen get _screen => widget.screen;

  bool get _shell => _screen.composer.mode == ComposerModeView.shell;

  /// The screen for the background work sheet, which stays open across
  /// updates.
  late final _work = ValueNotifier<SessionScreen>(widget.screen);

  @override
  void didUpdateWidget(SessionPage oldWidget) {
    super.didUpdateWidget(oldWidget);
    // After the frame: the sheet rebuilds from this, not during our build.
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (mounted) _work.value = widget.screen;
    });
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
    // A page kept under a subagent's is the same session's: the top one
    // says it.
    if (!(ModalRoute.isCurrentOf(context) ?? true)) return;
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
    _ownShell?.dispose();
    _work.dispose();
    super.dispose();
  }

  /// Runs [go], after a confirmation naming what it ends when the agent has
  /// background work an action that stops it would end or pause.
  Future<void> _stopping(
    String Function(StopHeadings) heading,
    String action,
    VoidCallback go,
  ) async {
    if (_screen.stopWarning case final warning?) {
      final ok = await confirmStop(
        context,
        heading: heading(_screen.stopHeadings),
        warning: warning,
        action: action,
      );
      if (!ok || !mounted) return;
    }
    go();
  }

  /// Stops the task of the subagent shown, once confirmed.
  Future<void> _stopSubagent(SubagentHeaderView subagent, String task) async {
    final ok = await confirmStop(
      context,
      heading: _screen.stopHeadings.stopTask,
      warning: subagent.title,
      action: 'Stop subagent',
    );
    if (!ok || !mounted) return;
    widget.send.act(
      ActionIntentStopTask(sessionId: _screen.sessionId, taskId: task),
    );
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
      if (_shell) {
        widget.send(GuiIntentStopShell(sessionId: _screen.sessionId));
      } else {
        _interrupt();
      }
      return KeyEventResult.handled;
    }
    // The timeline's: the run list has neither search nor pages.
    if (_shell &&
        ((ctrl && key == LogicalKeyboardKey.keyF) ||
            key == LogicalKeyboardKey.pageUp ||
            key == LogicalKeyboardKey.pageDown)) {
      return KeyEventResult.ignored;
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
                    if (screen.subagent case final subagent?)
                      if (subagent.taskId case final task?)
                        IconButton(
                          tooltip: 'Stop this subagent',
                          onPressed: () => _stopSubagent(subagent, task),
                          icon: Icon(
                            Icons.stop_circle_outlined,
                            color: theme.colorScheme.error,
                          ),
                        ),
                    IconButton(
                      tooltip: 'Workdir',
                      onPressed: screen.workdir == null
                          ? null
                          : () => widget.send.act(
                              ActionIntentOpenWorkdir(
                                sessionId: screen.sessionId,
                              ),
                            ),
                      icon: const Icon(Icons.folder_open),
                    ),
                    // The button itself says the mode is on: filled, in
                    // the accent colour, as a pressed toggle.
                    IconButton(
                      tooltip: screen.conversationOnly
                          ? 'Conversation only is on: show tool calls'
                          : 'Conversation only: hide tool calls',
                      isSelected: screen.conversationOnly,
                      style: screen.conversationOnly
                          ? IconButton.styleFrom(
                              backgroundColor: theme.colorScheme.primary,
                              foregroundColor: theme.colorScheme.onPrimary,
                            )
                          : null,
                      onPressed: () =>
                          widget.send(const GuiIntentToggleConversationOnly()),
                      icon: const Icon(Icons.forum_outlined),
                      selectedIcon: const Icon(Icons.forum),
                    ),
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
                          enabled: screen.workdir != null,
                          value: () => widget.send.act(
                            ActionIntentCreateSessionLike(
                              sessionId: screen.sessionId,
                            ),
                          ),
                          child: ListTile(
                            enabled: screen.workdir != null,
                            leading: const Icon(Icons.add),
                            title: const Text('New session here'),
                            subtitle: const Text('In the same workdir'),
                          ),
                        ),
                        PopupMenuItem(
                          enabled: screen.canClear,
                          value: () => _stopping(
                            (h) => h.newContext,
                            'New context',
                            () => widget.send.act(
                              ActionIntentClearContext(
                                sessionId: screen.sessionId,
                              ),
                            ),
                          ),
                          child: ListTile(
                            enabled: screen.canClear,
                            leading: const Icon(Icons.alt_route),
                            title: const Text('New context'),
                            subtitle: Text(
                              screen.clearLabel ??
                                  (screen.canClear
                                      ? "Clear the agent's context"
                                      : 'This context is empty'),
                            ),
                          ),
                        ),
                        PopupMenuItem(
                          value: () =>
                              pickContext(this.context, screen, widget.send),
                          child: ListTile(
                            leading: const Icon(Icons.account_tree_outlined),
                            title: const Text('Contexts…'),
                            subtitle: switch (screen.switchLabel) {
                              final label? => Text('switch $label'),
                              null => null,
                            },
                          ),
                        ),
                        PopupMenuItem(
                          value: () => showBackgroundWork(
                            this.context,
                            _work,
                            widget.send,
                          ),
                          child: ListTile(
                            leading: const Icon(Icons.pending_actions),
                            title: const Text('Background work'),
                            subtitle: Text(
                              screen.workLabel ?? 'nothing running',
                            ),
                          ),
                        ),
                        PopupMenuItem(
                          enabled: !stopped,
                          value: () => _stopping(
                            (h) => h.stopAgent,
                            'Stop agent',
                            () => widget.send.act(
                              ActionIntentStopSession(
                                sessionId: screen.sessionId,
                              ),
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
                if (screen.viewingHint case final hint?)
                  Material(
                    color: theme.colorScheme.tertiaryContainer,
                    child: Padding(
                      padding: const EdgeInsets.symmetric(
                        horizontal: 16,
                        vertical: 2,
                      ),
                      child: Row(
                        children: [
                          Icon(
                            Icons.visibility_outlined,
                            size: 16,
                            color: theme.colorScheme.onTertiaryContainer,
                          ),
                          const SizedBox(width: 8),
                          Expanded(
                            child: Text(
                              hint,
                              style: theme.textTheme.labelMedium?.copyWith(
                                color: theme.colorScheme.onTertiaryContainer,
                              ),
                            ),
                          ),
                          TextButton(
                            onPressed: () => widget.send.act(
                              ActionIntentViewContext(
                                sessionId: screen.sessionId,
                              ),
                            ),
                            child: const Text('Show all'),
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
                  child: _shell
                      ? ShellView(
                          sessionId: screen.sessionId,
                          shell: _shellRuns,
                          send: widget.send,
                        )
                      : TimelineView(
                          key: _timelineView,
                          sessionId: screen.sessionId,
                          timeline: widget.timeline,
                          search: _search,
                          send: widget.send,
                        ),
                ),
                // Flexible: with the keyboard up the panel shrinks to what
                // the timeline leaves and scrolls, instead of overflowing.
                if (screen.question case final question?)
                  Flexible(
                    child: _Column(
                      child: QuestionPanelView(
                        question: question,
                        send: widget.send,
                        onShowCall: question.callKey == null
                            ? null
                            : () => _showCall(question.callKey!),
                      ),
                    ),
                  ),
                if (screen.approval case final approval?)
                  Flexible(
                    child: _Column(
                      child: ApprovalPanelView(
                        approval: approval,
                        send: widget.send,
                        onShowCall: approval.gatedKey == null
                            ? null
                            : () => _showCall(approval.gatedKey!),
                      ),
                    ),
                  ),
                ColoredBox(
                  color: theme.colorScheme.surfaceContainer,
                  child: _Column(
                    child: Composer(
                      sessionId: screen.sessionId,
                      composer: screen.composer,
                      send: widget.send,
                      snippets: screen.snippets,
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

/// Interrupts the agent's turn: off while there is none, the hand under
/// an hourglass while the interrupt is on its way (still, not a spinner:
/// offline it can wait for as long as the link is down), the error colour
/// with the reason as its tooltip when it failed and the agent still works.
class _InterruptButton extends StatelessWidget {
  const _InterruptButton({required this.screen, required this.onPressed});

  final SessionScreen screen;
  final VoidCallback onPressed;

  @override
  Widget build(BuildContext context) {
    if (screen.interruptBusy) {
      final warning = SkyColors.of(context).warning;
      return IconButton(
        tooltip: 'Interrupting…',
        onPressed: null,
        icon: Badge(
          backgroundColor: Colors.transparent,
          padding: EdgeInsets.zero,
          offset: const Offset(6, -6),
          label: Icon(Icons.hourglass_top, size: 14, color: warning),
          child: const Icon(Icons.pan_tool_outlined),
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

/// The timeline's column width, for what sits over and under it.
class _Column extends StatelessWidget {
  const _Column({required this.child});

  final Widget child;

  @override
  Widget build(BuildContext context) {
    // heightFactor: as tall as the child, even under a bounded height.
    return Center(
      heightFactor: 1,
      child: ConstrainedBox(
        constraints: const BoxConstraints(maxWidth: timelineMaxWidth),
        child: child,
      ),
    );
  }
}

/// The header's title: the session's name with its rename on the way,
/// then the workdir; in a subagent's view, the subagent's title with how
/// it stands, then where it is (`work › outer`). The agent's state is on
/// the status line.
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
    final subagent = screen.subagent;
    final under = subagent?.trail ?? screen.workdir;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      mainAxisSize: MainAxisSize.min,
      children: [
        Row(
          children: [
            Flexible(
              child: Text(
                subagent?.title ?? screen.title,
                maxLines: 1,
                overflow: TextOverflow.ellipsis,
                style: theme.textTheme.titleMedium,
              ),
            ),
            if (subagent != null) ...[
              const SizedBox(width: 8),
              StateBadge(
                label: subagent.statusLabel,
                color: colors.tool(
                  subagent.live ? ToolState.running : ToolState.done,
                ),
              ),
            ] else if (screen.renameLabel case final label?) ...[
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
        if (under case final under?)
          Text(under, maxLines: 1, overflow: TextOverflow.ellipsis, style: sub),
      ],
    );
  }
}
