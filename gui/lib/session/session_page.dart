import 'package:material_ui/material_ui.dart';
import 'package:flutter/services.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/link/link_status.dart';
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

  /// The More sheet of a narrow window: the session's name and workdir
  /// in full, the header's view actions, then [menu]. Each tap closes it
  /// first.
  void _showActions(
    SessionScreen screen,
    List<_MenuItem> menu, {
    required VoidCallback? openWorkdir,
    required VoidCallback rename,
    required VoidCallback toggleConversationOnly,
  }) {
    showModalBottomSheet<void>(
      context: context,
      showDragHandle: true,
      isScrollControlled: true,
      builder: (sheet) {
        final theme = Theme.of(sheet);
        VoidCallback? closing(VoidCallback? action) => action == null
            ? null
            : () {
                Navigator.of(sheet).pop();
                action();
              };
        return SafeArea(
          child: SingleChildScrollView(
            child: Column(
              mainAxisSize: MainAxisSize.min,
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: [
                Padding(
                  padding: const EdgeInsets.symmetric(horizontal: 24),
                  child: Column(
                    crossAxisAlignment: CrossAxisAlignment.start,
                    children: [
                      SelectableText(
                        screen.title,
                        style: theme.textTheme.titleMedium,
                      ),
                      if (screen.workdir case final workdir?)
                        SelectableText(
                          workdir,
                          style: theme.textTheme.bodySmall?.copyWith(
                            color: SkyColors.of(sheet).muted,
                          ),
                        ),
                    ],
                  ),
                ),
                const SizedBox(height: 16),
                Padding(
                  padding: const EdgeInsets.symmetric(horizontal: 8),
                  child: Row(
                    crossAxisAlignment: CrossAxisAlignment.start,
                    children: [
                      _QuickAction(
                        icon: Icons.search,
                        label: 'Search',
                        onPressed: closing(_openSearch),
                      ),
                      _QuickAction(
                        icon: Icons.folder_open,
                        label: 'Workdir',
                        onPressed: closing(openWorkdir),
                      ),
                      _QuickAction(
                        icon: Icons.drive_file_rename_outline,
                        label: 'Rename',
                        onPressed: closing(rename),
                      ),
                      _QuickAction(
                        icon: screen.conversationOnly
                            ? Icons.forum
                            : Icons.forum_outlined,
                        label: 'Conversation only',
                        selected: screen.conversationOnly,
                        onPressed: closing(toggleConversationOnly),
                      ),
                    ],
                  ),
                ),
                const Divider(height: 24),
                for (final item in menu) item.tile(onTap: closing(item.onTap)),
              ],
            ),
          ),
        );
      },
    );
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
    final renamePrompt = screen.renamePrompt;
    // Only a stopped agent surely has no process: an idle one stays warm a
    // while, an errored one may have failed a turn and still run.
    final stopped = screen.state == AgentState.stopped;
    final isArchived = screen.archivedLabel != null;
    final compact = isCompact(context);
    final VoidCallback? openWorkdir = screen.workdir == null
        ? null
        : () => widget.send.act(
            ActionIntentOpenWorkdir(sessionId: screen.sessionId),
          );
    void rename() =>
        widget.send(GuiIntentRenameOpen(sessionId: screen.sessionId));
    void toggleConversationOnly() =>
        widget.send(const GuiIntentToggleConversationOnly());
    // The More menu on a wide window, the sheet's list on a narrow one.
    final menu = [
      _MenuItem(
        icon: Icons.add,
        title: 'New session here',
        subtitle: 'In the same workdir',
        enabled: screen.workdir != null,
        onTap: () => widget.send.act(
          ActionIntentCreateSessionLike(sessionId: screen.sessionId),
        ),
      ),
      _MenuItem(
        icon: Icons.alt_route,
        title: 'New context',
        subtitle:
            screen.clearLabel ??
            (screen.canClear
                ? "Clear the agent's context"
                : 'This context is empty'),
        enabled: screen.canClear,
        onTap: () => _stopping(
          (h) => h.newContext,
          'New context',
          () => widget.send.act(
            ActionIntentClearContext(sessionId: screen.sessionId),
          ),
        ),
      ),
      _MenuItem(
        icon: Icons.account_tree_outlined,
        title: 'Contexts…',
        subtitle: switch (screen.switchLabel) {
          final label? => 'switch $label',
          null => null,
        },
        onTap: () => pickContext(context, screen, widget.send),
      ),
      _MenuItem(
        icon: Icons.info_outline,
        title: 'Session info',
        subtitle: 'Usage, limits, agent and subagents',
        onTap: () => widget.send.act(
          ActionIntentOpenSessionInfo(sessionId: screen.sessionId),
        ),
      ),
      _MenuItem(
        icon: Icons.pending_actions,
        title: 'Background work',
        subtitle: screen.workLabel ?? 'nothing running',
        onTap: () => showBackgroundWork(context, _work, widget.send),
      ),
      _MenuItem(
        icon: isArchived ? Icons.unarchive : Icons.archive,
        title: isArchived ? 'Unarchive' : 'Archive',
        subtitle: isArchived
            ? 'Back to the active list'
            : 'Move to the archived list',
        onTap: () => widget.send.act(
          ActionIntentSetSessionArchived(
            sessionId: screen.sessionId,
            archived: !isArchived,
          ),
        ),
      ),
      _MenuItem(
        icon: Icons.power_settings_new,
        color: stopped ? null : theme.colorScheme.error,
        title: 'Stop agent',
        subtitle: stopped
            ? 'Already stopped · a message restarts it'
            : screen.stopLabel,
        enabled: !stopped,
        onTap: () => _stopping(
          (h) => h.stopAgent,
          'Stop agent',
          () => widget.send.act(
            ActionIntentStopSession(sessionId: screen.sessionId),
          ),
        ),
      ),
    ];

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
                    Expanded(
                      child: _Title(
                        screen: screen,
                        compact: compact,
                        send: widget.send,
                      ),
                    ),
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
                    if (!compact) ...[
                      IconButton(
                        tooltip: 'Workdir',
                        onPressed: openWorkdir,
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
                        onPressed: toggleConversationOnly,
                        icon: const Icon(Icons.forum_outlined),
                        selectedIcon: const Icon(Icons.forum),
                      ),
                      IconButton(
                        tooltip: 'Search (Ctrl+F)',
                        onPressed: _openSearch,
                        icon: const Icon(Icons.search),
                      ),
                    ],
                    ApprovalsButton(send: widget.send),
                    _InterruptButton(screen: screen, onPressed: _interrupt),
                    if (compact)
                      IconButton(
                        tooltip: 'More',
                        onPressed: () => _showActions(
                          screen,
                          menu,
                          openWorkdir: openWorkdir,
                          rename: rename,
                          toggleConversationOnly: toggleConversationOnly,
                        ),
                        icon: const Icon(Icons.more_vert),
                      )
                    else
                      PopupMenuButton<VoidCallback>(
                        tooltip: 'More',
                        onSelected: (action) => action(),
                        itemBuilder: (context) => [
                          for (final item in [
                            _MenuItem(
                              icon: Icons.drive_file_rename_outline,
                              title: 'Rename',
                              onTap: rename,
                            ),
                            ...menu,
                          ])
                            PopupMenuItem(
                              enabled: item.enabled,
                              value: item.onTap,
                              child: item.tile(),
                            ),
                        ],
                      ),
                    const SizedBox(width: 4),
                  ],
                ),
              ),
              flexibleSpace: const LinkProgress(),
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
                  child: _TimelineAndPanels(
                    timeline: _shell
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
                    panels: [
                      if (screen.question case final question?)
                        QuestionPanelView(
                          question: question,
                          send: widget.send,
                          onShowCall: question.callKey == null
                              ? null
                              : () => _showCall(question.callKey!),
                        ),
                      if (screen.approval case final approval?)
                        ApprovalPanelView(
                          approval: approval,
                          send: widget.send,
                          onShowCall: approval.gatedKey == null
                              ? null
                              : () => _showCall(approval.gatedKey!),
                        ),
                    ],
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
        if (renamePrompt != null)
          Positioned.fill(
            child: RenameDialog(
              key: ValueKey(renamePrompt.sessionId),
              prompt: renamePrompt,
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

/// A session action: an item of the More menu, or a row of the More
/// sheet.
class _MenuItem {
  const _MenuItem({
    required this.icon,
    required this.title,
    required this.onTap,
    this.subtitle,
    this.color,
    this.enabled = true,
  });

  final IconData icon;

  /// The icon's colour: the error one for a destructive action.
  final Color? color;
  final String title;
  final String? subtitle;
  final bool enabled;
  final VoidCallback onTap;

  /// Its row; a menu item takes the tap itself, so passes no [onTap].
  Widget tile({VoidCallback? onTap}) => ListTile(
    enabled: enabled,
    leading: Icon(icon, color: color),
    title: Text(title),
    subtitle: switch (subtitle) {
      final subtitle? => Text(subtitle),
      null => null,
    },
    onTap: onTap,
  );
}

/// A labelled round button in the More sheet's top row: filled in the
/// accent colour while [selected], as the header's toggle is.
class _QuickAction extends StatelessWidget {
  const _QuickAction({
    required this.icon,
    required this.label,
    required this.onPressed,
    this.selected = false,
  });

  final IconData icon;
  final String label;
  final VoidCallback? onPressed;
  final bool selected;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Expanded(
      child: Column(
        children: [
          IconButton(
            tooltip: label,
            isSelected: selected,
            onPressed: onPressed,
            style: IconButton.styleFrom(
              backgroundColor: selected
                  ? theme.colorScheme.primary
                  : theme.colorScheme.secondaryContainer,
              foregroundColor: selected
                  ? theme.colorScheme.onPrimary
                  : theme.colorScheme.onSecondaryContainer,
              disabledBackgroundColor: theme.colorScheme.onSurface.withValues(
                alpha: 0.12,
              ),
            ),
            icon: Icon(icon),
          ),
          const SizedBox(height: 4),
          ExcludeSemantics(
            child: Text(
              label,
              textAlign: TextAlign.center,
              maxLines: 2,
              style: theme.textTheme.labelMedium,
            ),
          ),
        ],
      ),
    );
  }
}

/// The timeline over the panels the agent is blocked on. Each panel is as
/// tall as it needs, up to an equal share with the timeline: with the
/// keyboard up it shrinks and scrolls instead of overflowing. The timeline
/// takes the rest. A `Flexible` panel would not give back what it leaves
/// of its share, and that height would gape under the composer.
class _TimelineAndPanels extends StatelessWidget {
  const _TimelineAndPanels({required this.timeline, required this.panels});

  final Widget timeline;
  final List<Widget> panels;

  @override
  Widget build(BuildContext context) {
    return LayoutBuilder(
      builder: (context, constraints) {
        final share = constraints.maxHeight / (panels.length + 1);
        return Column(
          children: [
            Expanded(child: timeline),
            for (final panel in panels)
              ConstrainedBox(
                constraints: BoxConstraints(maxHeight: share),
                child: _Column(child: panel),
              ),
          ],
        );
      },
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
/// then the workdir, or the link's status line while the link is on its
/// way back; in a subagent's view, the subagent's title with how it
/// stands, then where it is (`work › outer`). The agent's state is on the
/// status line.
class _Title extends StatelessWidget {
  const _Title({
    required this.screen,
    required this.compact,
    required this.send,
  });

  final SessionScreen screen;

  /// Under the title the workdir's last component, not its path: the
  /// More sheet shows the path whole.
  final bool compact;

  /// For the link's status line.
  final SendIntent send;

  /// The toolbar's height: the standard one, or taller when the text
  /// scale makes the title's two lines and a margin (8) need more.
  static double heightOf(BuildContext context) =>
      LinkTitle.heightFor(context, Theme.of(context).textTheme.titleMedium!);

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final colors = SkyColors.of(context);
    final rename = screen.rename;
    final sub = theme.textTheme.labelMedium?.copyWith(color: colors.muted);
    final subagent = screen.subagent;
    final under =
        subagent?.trail ?? (compact ? screen.workdirName : screen.workdir);
    return LinkTitle(
      send: send,
      subtitle: under == null
          ? null
          : Text(
              under,
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
              style: sub,
            ),
      title: Row(
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
    );
  }
}
