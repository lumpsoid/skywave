import 'package:flutter/material.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/session/background_sheet.dart';
import 'package:skywave_gui/session/look.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// Lists the session's contexts. Switch makes one the context messages go
/// to (not offered on the current one); View filters the timeline to it,
/// and on the current one shows the whole session again. Move continues one
/// as a session of its own, after a confirm; a moved one offers Open on that
/// session instead of Switch, and a move of this device's that is refused or
/// untried offers Discard move.
Future<void> pickContext(
  BuildContext context,
  SessionScreen screen,
  SendIntent send,
) {
  return showDialog<void>(
    context: context,
    builder: (context) => AlertDialog(
      title: const Text('Contexts'),
      contentPadding: const EdgeInsets.fromLTRB(0, 16, 0, 0),
      content: SingleChildScrollView(
        child: Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            for (final c in screen.contexts)
              _ContextTile(
                context: c,
                viewing: c.contextId == screen.viewingId,
                onSwitch: () async {
                  if (screen.stopWarning case final warning?) {
                    final ok = await confirmStop(
                      context,
                      heading: screen.stopHeadings.switchContext,
                      warning: warning,
                      action: 'Switch',
                    );
                    if (!ok || !context.mounted) return;
                  }
                  Navigator.of(context).pop();
                  send.act(
                    ActionIntentSwitchContext(
                      sessionId: screen.sessionId,
                      contextId: c.contextId,
                    ),
                  );
                },
                onOpen: () {
                  Navigator.of(context).pop();
                  send.act(
                    ActionIntentOpenSession(sessionId: c.movedToSessionId!),
                  );
                },
                onMove: () async {
                  final ok = await _confirmMove(context, c, screen);
                  if (ok != true || !context.mounted) return;
                  Navigator.of(context).pop();
                  send.act(
                    ActionIntentMoveContext(
                      sessionId: screen.sessionId,
                      contextId: c.contextId,
                    ),
                  );
                },
                onDiscardMove: () {
                  Navigator.of(context).pop();
                  send.act(ActionIntentDiscard(commandId: c.discardMove!));
                },
                onView: () {
                  Navigator.of(context).pop();
                  send.act(
                    ActionIntentViewContext(
                      sessionId: screen.sessionId,
                      contextId: c.current ? null : c.contextId,
                    ),
                  );
                },
              ),
          ],
        ),
      ),
      actions: [
        TextButton(
          onPressed: () => Navigator.of(context).pop(),
          child: const Text('Close'),
        ),
      ],
    ),
  );
}

/// The move confirm. Moving the current context stops the agent, so its
/// warning joins the confirm rather than following it with a second one.
Future<bool?> _confirmMove(
  BuildContext context,
  ContextRow c,
  SessionScreen screen,
) {
  return showDialog<bool>(
    context: context,
    builder: (context) => AlertDialog(
      title: Text('Move ${c.label} to a new session?'),
      content: Text(
        [
          'It goes on there; here it stays as it was.',
          if (c.current) ?screen.stopWarning,
        ].join('\n\n'),
      ),
      actions: [
        TextButton(
          onPressed: () => Navigator.of(context).pop(false),
          child: const Text('Cancel'),
        ),
        TextButton(
          onPressed: () => Navigator.of(context).pop(true),
          child: const Text('Move'),
        ),
      ],
    ),
  );
}

class _ContextTile extends StatelessWidget {
  const _ContextTile({
    required this.context,
    required this.viewing,
    required this.onSwitch,
    required this.onView,
    required this.onOpen,
    required this.onMove,
    required this.onDiscardMove,
  });

  final ContextRow context;
  final bool viewing;
  final VoidCallback onSwitch;
  final VoidCallback onView;
  final VoidCallback onOpen;
  final VoidCallback onMove;
  final VoidCallback onDiscardMove;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final colors = SkyColors.of(context);
    final c = this.context;
    return ListTile(
      contentPadding: const EdgeInsets.symmetric(horizontal: 24),
      selected: c.current,
      leading: Icon(c.current ? Icons.check_circle : Icons.circle_outlined),
      title: Text(c.label, maxLines: 2, overflow: TextOverflow.ellipsis),
      subtitle: _subtitle(theme, colors),
      trailing: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          if (c.movedToSessionId != null)
            TextButton(onPressed: onOpen, child: const Text('Open'))
          else if (!c.current)
            TextButton(onPressed: onSwitch, child: const Text('Switch')),
          TextButton(
            onPressed: viewing ? null : onView,
            child: const Text('View'),
          ),
        ],
      ),
    );
  }

  Widget? _subtitle(ThemeData theme, SkyColors colors) {
    final c = context;
    final warn = theme.textTheme.labelSmall?.copyWith(color: colors.warning);
    final lines = <Widget>[
      if (c.outgoingLabel case final label?) Text(label, style: warn),
      if (c.movedLabel case final label?) Text(label),
      if (c.outgoingLabel == null && c.movedLabel == null && c.current)
        const Text('current'),
    ];
    final actions = <Widget>[
      if (c.movedToSessionId == null)
        Tooltip(
          message: c.moveRefusal ?? '',
          child: TextButton(
            onPressed: c.canMove ? onMove : null,
            child: const Text('Move to new session'),
          ),
        ),
      if (c.discardMove != null)
        TextButton(onPressed: onDiscardMove, child: const Text('Discard move')),
    ];
    if (lines.isEmpty && actions.isEmpty) return null;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        ...lines,
        if (actions.isNotEmpty) Wrap(children: actions),
      ],
    );
  }
}
