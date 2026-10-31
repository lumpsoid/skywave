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

/// The open session: its header and status line, the search bar while a
/// search is open, the timeline, the approval it waits on, and the
/// composer; the rename prompt over it while open.
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

    return Stack(
      children: [
        Focus(
          onKeyEvent: _onKey,
          child: Scaffold(
            appBar: AppBar(
              leading: BackButton(
                onPressed: () => widget.send.act(const ActionIntentBack()),
              ),
              titleSpacing: 0,
              title: _Title(screen: screen),
              actions: [
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
                      value: () => widget.send.act(
                        ActionIntentStopSession(sessionId: screen.sessionId),
                      ),
                      child: ListTile(
                        leading: Icon(
                          Icons.power_settings_new,
                          color: theme.colorScheme.error,
                        ),
                        title: const Text('Stop agent'),
                        subtitle: screen.stopLabel == null
                            ? null
                            : Text(screen.stopLabel!),
                      ),
                    ),
                  ],
                ),
              ],
              bottom: PreferredSize(
                preferredSize: const Size.fromHeight(26),
                child: StatusLine(screen: screen, send: widget.send),
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

/// The timeline's column width, for what sits under it.
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
/// then the workdir and the agent's state.
class _Title extends StatelessWidget {
  const _Title({required this.screen});

  final SessionScreen screen;

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
        Row(
          children: [
            Icon(Icons.circle, size: 8, color: colors.agent(screen.state)),
            const SizedBox(width: 6),
            Flexible(
              child: Text(
                [?screen.workdir, screen.badgeLabel].join(' · '),
                maxLines: 1,
                overflow: TextOverflow.ellipsis,
                style: sub,
              ),
            ),
          ],
        ),
      ],
    );
  }
}
