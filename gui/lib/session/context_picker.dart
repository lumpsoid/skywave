import 'package:flutter/material.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/session/look.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// Lists the session's contexts. Switch makes one the context messages go
/// to (not offered on the current one); View filters the timeline to it,
/// and on the current one shows the whole session again.
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
                onSwitch: () {
                  Navigator.of(context).pop();
                  send.act(
                    ActionIntentSwitchContext(
                      sessionId: screen.sessionId,
                      contextId: c.contextId,
                    ),
                  );
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

class _ContextTile extends StatelessWidget {
  const _ContextTile({
    required this.context,
    required this.viewing,
    required this.onSwitch,
    required this.onView,
  });

  final ContextRow context;
  final bool viewing;
  final VoidCallback onSwitch;
  final VoidCallback onView;

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
      subtitle: switch (c.outgoingLabel) {
        final label? => Text(
          label,
          style: theme.textTheme.labelSmall?.copyWith(color: colors.warning),
        ),
        null => c.current ? const Text('current') : null,
      },
      trailing: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          if (!c.current)
            TextButton(onPressed: onSwitch, child: const Text('Switch')),
          TextButton(
            onPressed: viewing ? null : onView,
            child: const Text('View'),
          ),
        ],
      ),
    );
  }
}
