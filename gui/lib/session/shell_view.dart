import 'package:flutter/material.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/session/look.dart';
import 'package:skywave_gui/session/shell_holder.dart';
import 'package:skywave_gui/session/shell_output.dart';
import 'package:skywave_gui/session/timeline_view.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// Most lines of a run's tail shown in its card.
const shellTailLines = 8;

/// The session's shell runs, oldest first and the newest at the bottom, as
/// a terminal reads; shown in place of the timeline in shell mode. Each
/// waiting or running run has its own stop, each run this device can take
/// back its Edit and Discard.
class ShellView extends StatelessWidget {
  const ShellView({
    required this.sessionId,
    required this.shell,
    required this.send,
    super.key,
  });

  final String sessionId;
  final ShellHolder shell;
  final SendIntent send;

  @override
  Widget build(BuildContext context) {
    final colors = SkyColors.of(context);
    return ListenableBuilder(
      listenable: shell,
      builder: (context, _) {
        final runs = shell.runs;
        if (runs.isEmpty) {
          return Center(
            child: Padding(
              padding: const EdgeInsets.all(24),
              child: Text(
                'No shell commands yet. Each runs on its own in the '
                "session's workdir, in turn with the messages.",
                textAlign: TextAlign.center,
                style: TextStyle(color: colors.muted),
              ),
            ),
          );
        }
        return Align(
          alignment: Alignment.topCenter,
          child: ConstrainedBox(
            constraints: const BoxConstraints(maxWidth: timelineMaxWidth),
            child: ListView.builder(
              // Newest at the bottom, where the list starts.
              reverse: true,
              padding: const EdgeInsets.symmetric(vertical: 8),
              itemCount: runs.length,
              findChildIndexCallback: (key) {
                final at = runs.indexWhere((run) => ValueKey(run.key) == key);
                return at < 0 ? null : runs.length - 1 - at;
              },
              itemBuilder: (context, index) {
                final run = runs[runs.length - 1 - index];
                return ShellRunTile(
                  key: ValueKey(run.key),
                  sessionId: sessionId,
                  run: run,
                  shell: shell,
                  send: send,
                );
              },
            ),
          ),
        );
      },
    );
  }
}

/// One run: its command and condition, how it stands, its output's tail,
/// and what can be done with it. "Show all" opens its whole output in the
/// app ([showShellOutput]).
class ShellRunTile extends StatelessWidget {
  const ShellRunTile({
    required this.sessionId,
    required this.run,
    required this.shell,
    required this.send,
    super.key,
  });

  final String sessionId;
  final ShellRunRow run;

  /// Where the open output view reads the run from.
  final ShellHolder shell;
  final SendIntent send;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final colors = SkyColors.of(context);
    final mono = theme.textTheme.bodyMedium?.copyWith(
      fontFamily: monoFamily,
      fontFamilyFallback: monoFallback,
    );
    final state = colors.shellRun(run.state);
    final tail = run.tail.isEmpty ? <String>[] : run.tail.split('\n');
    final shown = tail.length > shellTailLines
        ? tail.sublist(tail.length - shellTailLines)
        : tail;
    final hidden =
        (run.lines > tail.length ? run.lines : tail.length) - shown.length;
    final output = run.output;
    return Card(
      margin: const EdgeInsets.symmetric(horizontal: 12, vertical: 4),
      color: theme.colorScheme.surfaceContainerLow,
      child: Padding(
        padding: const EdgeInsets.fromLTRB(12, 8, 4, 8),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Row(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Expanded(
                  child: SelectableText(
                    run.command,
                    style: mono?.copyWith(fontWeight: FontWeight.bold),
                  ),
                ),
                if (run.whenLabel case final condition?) ...[
                  const SizedBox(width: 8),
                  StateBadge(label: condition, color: colors.warning),
                ],
                if (run.canStop)
                  IconButton(
                    tooltip: 'Stop this run',
                    visualDensity: VisualDensity.compact,
                    onPressed: () => send(
                      GuiIntentStopShellRun(
                        sessionId: sessionId,
                        runId: run.key,
                      ),
                    ),
                    icon: Icon(
                      Icons.stop_circle_outlined,
                      color: theme.colorScheme.error,
                    ),
                  ),
              ],
            ),
            const SizedBox(height: 4),
            Wrap(
              spacing: 8,
              crossAxisAlignment: WrapCrossAlignment.center,
              children: [
                StateBadge(label: run.stateLabel, color: state),
                if (run.byLabel case final by?)
                  Text(
                    by,
                    style: theme.textTheme.labelSmall?.copyWith(
                      color: colors.muted,
                    ),
                  ),
              ],
            ),
            if (hidden > 0)
              Padding(
                padding: const EdgeInsets.only(top: 6),
                child: Text(
                  '… $hidden earlier lines',
                  style: theme.textTheme.labelSmall?.copyWith(
                    color: colors.muted,
                  ),
                ),
              ),
            if (shown.isNotEmpty)
              Padding(
                padding: const EdgeInsets.only(top: 4, right: 8),
                child: SelectableText(shown.join('\n'), style: mono),
              ),
            if (output != null || tail.isNotEmpty)
              Padding(
                padding: const EdgeInsets.only(top: 4),
                child: Row(
                  children: [
                    Expanded(
                      child: output == null
                          ? const SizedBox.shrink()
                          : Text(
                              'whole output · ${output.label}',
                              style: theme.textTheme.labelSmall?.copyWith(
                                color: colors.muted,
                              ),
                            ),
                    ),
                    if (output != null && output.canDownload)
                      TextButton(
                        onPressed: () => send.act(
                          ActionIntentDownloadBlob(blobId: output.blobId),
                        ),
                        child: const Text('Download'),
                      ),
                    TextButton(
                      key: Key('shell-show-${run.key}'),
                      onPressed: () => showShellOutput(
                        context,
                        sessionId: sessionId,
                        runKey: run.key,
                        shell: shell,
                        send: send,
                      ),
                      child: const Text('Show all'),
                    ),
                  ],
                ),
              ),
            if (run.discardable)
              Row(
                mainAxisAlignment: MainAxisAlignment.end,
                children: [
                  TextButton(
                    onPressed: () =>
                        send.act(ActionIntentEditQueued(commandId: run.key)),
                    child: const Text('Edit'),
                  ),
                  TextButton(
                    onPressed: () =>
                        send.act(ActionIntentDiscard(commandId: run.key)),
                    child: const Text('Discard'),
                  ),
                ],
              ),
          ],
        ),
      ),
    );
  }
}
