import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/session/look.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// Lists the agent's background work in a sheet: the tasks, each under the
/// one that started it, with a Stop that asks first (and, for a subagent's
/// task, an Open that closes the sheet on its steps), then the loops, which
/// only inform (Skywave cannot end a loop; the agent can, when asked). The
/// sheet follows [screen], so a stop's progress shows in its row.
Future<void> showBackgroundWork(
  BuildContext context,
  ValueListenable<SessionScreen> screen,
  SendIntent send,
) {
  return showModalBottomSheet<void>(
    context: context,
    isScrollControlled: true,
    showDragHandle: true,
    builder: (context) => ValueListenableBuilder<SessionScreen>(
      valueListenable: screen,
      builder: (context, screen, _) =>
          BackgroundSheet(screen: screen, send: send),
    ),
  );
}

/// The sheet's content.
class BackgroundSheet extends StatelessWidget {
  const BackgroundSheet({required this.screen, required this.send, super.key});

  final SessionScreen screen;
  final SendIntent send;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final empty = screen.tasks.isEmpty && screen.schedules.isEmpty;
    return SafeArea(
      child: ConstrainedBox(
        constraints: BoxConstraints(
          maxHeight: MediaQuery.sizeOf(context).height * 0.7,
        ),
        child: ListView(
          shrinkWrap: true,
          padding: const EdgeInsets.only(bottom: 16),
          children: [
            Padding(
              padding: const EdgeInsets.symmetric(horizontal: 24),
              child: Text(
                'Background work'
                '${screen.workLabel == null ? '' : ' · ${screen.workLabel}'}',
                style: theme.textTheme.titleMedium,
              ),
            ),
            if (empty)
              Padding(
                padding: const EdgeInsets.all(24),
                child: Text(screen.workEmptyLabel),
              ),
            for (final task in screen.tasks)
              _TaskTile(
                task: task,
                onOpen: switch (task.call) {
                  final call? => () {
                    Navigator.of(context).pop();
                    send.act(
                      ActionIntentViewSubagent(
                        sessionId: screen.sessionId,
                        call: call,
                      ),
                    );
                  },
                  null => null,
                },
                onStop: () async {
                  final ok = await _confirmTaskStop(context, task);
                  if (!ok || !context.mounted) return;
                  send.act(
                    ActionIntentStopTask(
                      sessionId: screen.sessionId,
                      taskId: task.taskId,
                    ),
                  );
                },
              ),
            for (final schedule in screen.schedules)
              _ScheduleTile(schedule: schedule),
            if (screen.loopsNote case final note?)
              Padding(
                padding: const EdgeInsets.fromLTRB(24, 8, 24, 0),
                child: Text(note, style: theme.textTheme.bodySmall),
              ),
          ],
        ),
      ),
    );
  }
}

class _TaskTile extends StatelessWidget {
  const _TaskTile({required this.task, required this.onStop, this.onOpen});

  final TaskRow task;
  final VoidCallback onStop;

  /// Shows the steps of the subagent the task runs; null for any other.
  final VoidCallback? onOpen;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final colors = SkyColors.of(context);
    return ListTile(
      contentPadding: EdgeInsets.only(left: 24 + 24.0 * task.depth, right: 24),
      leading: const Icon(Icons.terminal),
      title: Text(task.title, maxLines: 2, overflow: TextOverflow.ellipsis),
      subtitle: Text(
        [
          task.kindLabel,
          ?task.ambientLabel,
          if (task.stopLabel case final label?) 'stop $label',
        ].join(' · '),
        style: task.stopLabel == null
            ? null
            : theme.textTheme.bodySmall?.copyWith(color: colors.warning),
      ),
      trailing: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          if (onOpen case final open?)
            TextButton(onPressed: open, child: const Text('Open')),
          TextButton(onPressed: onStop, child: const Text('Stop')),
        ],
      ),
    );
  }
}

class _ScheduleTile extends StatelessWidget {
  const _ScheduleTile({required this.schedule});

  final ScheduleRow schedule;

  @override
  Widget build(BuildContext context) {
    return ListTile(
      contentPadding: const EdgeInsets.symmetric(horizontal: 24),
      leading: const Icon(Icons.schedule),
      title: Text(
        schedule.prompt,
        maxLines: 2,
        overflow: TextOverflow.ellipsis,
      ),
      subtitle: Text(
        [
          'loop ${schedule.when}',
          schedule.repeatLabel,
          ?schedule.savedLabel,
        ].join(' · '),
      ),
    );
  }
}

Future<bool> _confirmTaskStop(BuildContext context, TaskRow task) async {
  final confirmed = await showDialog<bool>(
    context: context,
    builder: (context) => AlertDialog(
      title: Text('Stop ${task.title}?'),
      actions: [
        TextButton(
          onPressed: () => Navigator.of(context).pop(false),
          child: const Text('Cancel'),
        ),
        FilledButton(
          onPressed: () => Navigator.of(context).pop(true),
          child: const Text('Stop'),
        ),
      ],
    ),
  );
  return confirmed ?? false;
}

/// Asks before an action that stops the agent, naming what it ends or
/// pauses. Call only while [SessionScreen.stopWarning] is set; without it
/// the action goes ahead unasked.
Future<bool> confirmStop(
  BuildContext context, {
  required String heading,
  required String warning,
  required String action,
}) async {
  final confirmed = await showDialog<bool>(
    context: context,
    builder: (context) {
      final scheme = Theme.of(context).colorScheme;
      return AlertDialog(
        icon: Icon(Icons.warning_amber_rounded, color: scheme.error),
        title: Text(heading),
        content: Text(warning),
        actions: [
          TextButton(
            onPressed: () => Navigator.of(context).pop(false),
            child: const Text('Cancel'),
          ),
          FilledButton(
            style: FilledButton.styleFrom(
              backgroundColor: scheme.error,
              foregroundColor: scheme.onError,
            ),
            onPressed: () => Navigator.of(context).pop(true),
            child: Text(action),
          ),
        ],
      );
    },
  );
  return confirmed ?? false;
}
