import 'package:flutter/material.dart';
import 'package:skywave_gui/crop/blob_menu.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/link/link_status.dart';
import 'package:skywave_gui/session/look.dart';
import 'package:skywave_gui/session/shell_holder.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// Opens a run's whole output over the session: Rust sends it in the run's
/// row while it is expanded (`ExpandShellRun`, which downloads the output
/// if needed), and stops once the view closes (`CollapseShellRun`).
Future<void> showShellOutput(
  BuildContext context, {
  required String sessionId,
  required String runKey,
  required ShellHolder shell,
  required SendIntent send,
}) async {
  send(GuiIntentExpandShellRun(sessionId: sessionId, runId: runKey));
  await showDialog<void>(
    context: context,
    builder: (_) => Dialog.fullscreen(
      child: ShellOutputView(
        sessionId: sessionId,
        runKey: runKey,
        shell: shell,
        send: send,
      ),
    ),
  );
  send(GuiIntentCollapseShellRun(sessionId: sessionId, runId: runKey));
}

/// One run's whole output, ANSI stripped, as its row carries it: line by
/// line, selectable, with a copy of all of it and its delete and crop
/// menu. While the output is still downloading, where that stands and its
/// Download button.
class ShellOutputView extends StatelessWidget {
  const ShellOutputView({
    required this.sessionId,
    required this.runKey,
    required this.shell,
    required this.send,
    super.key,
  });

  final String sessionId;
  final String runKey;
  final ShellHolder shell;
  final SendIntent send;

  @override
  Widget build(BuildContext context) {
    return ListenableBuilder(
      listenable: shell,
      builder: (context, _) {
        final run = shell.runs.where((run) => run.key == runKey).firstOrNull;
        return Scaffold(
          appBar: AppBar(
            leading: const CloseButton(),
            toolbarHeight: LinkTitle.toolbarHeight(context),
            flexibleSpace: const LinkProgress(),
            title: LinkTitle(
              title: Text(
                run?.command.split('\n').first ?? 'Shell output',
                style: const TextStyle(
                  fontFamily: monoFamily,
                  fontFamilyFallback: monoFallback,
                ),
                maxLines: 1,
                overflow: TextOverflow.ellipsis,
              ),
              send: send,
            ),
            actions: [
              IconButton(
                key: const Key('shell-output-copy'),
                tooltip: 'Copy output',
                onPressed: run == null
                    ? null
                    : () => send(
                        GuiIntentCopyShellOutput(
                          sessionId: sessionId,
                          runId: runKey,
                        ),
                      ),
                icon: const Icon(Icons.copy),
              ),
              if (run?.target case final target?)
                BlobMenu(
                  target: target,
                  send: send,
                  // This view is a dialog over the session: it would
                  // cover the crop page.
                  beforeCrop: () => Navigator.of(context).pop(),
                ),
            ],
          ),
          body: run == null
              ? const Center(child: Text('This run is gone.'))
              : _Body(run: run, send: send),
        );
      },
    );
  }
}

class _Body extends StatelessWidget {
  const _Body({required this.run, required this.send});

  final ShellRunRow run;
  final SendIntent send;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final colors = SkyColors.of(context);
    final small = theme.textTheme.labelSmall?.copyWith(color: colors.muted);
    final mono = theme.textTheme.bodySmall?.copyWith(
      fontFamily: monoFamily,
      fontFamilyFallback: monoFallback,
      height: 1.35,
    );
    final whole = run.whole;
    final output = run.output;
    // A final line break ends the last line; it does not start one.
    final lines = whole == null || whole.isEmpty
        ? const <String>[]
        : (whole.endsWith('\n') ? whole.substring(0, whole.length - 1) : whole)
              .split('\n');
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Padding(
          padding: const EdgeInsets.fromLTRB(16, 8, 16, 4),
          child: Wrap(
            spacing: 8,
            runSpacing: 4,
            crossAxisAlignment: WrapCrossAlignment.center,
            children: [
              StateBadge(
                label: run.stateLabel,
                color: colors.shellRun(run.state),
              ),
              if (run.wholeNote case final note?) Text(note, style: small),
            ],
          ),
        ),
        Expanded(
          child: whole == null
              ? Center(
                  child: Column(
                    mainAxisSize: MainAxisSize.min,
                    children: [
                      if (output != null) Text(output.label, style: small),
                      if (output != null && output.canDownload)
                        TextButton(
                          onPressed: () => send.act(
                            ActionIntentDownloadBlob(blobId: output.blobId),
                          ),
                          child: const Text('Download'),
                        ),
                    ],
                  ),
                )
              : lines.isEmpty
              ? Center(child: Text('No output.', style: small))
              : SelectionArea(
                  child: ListView.builder(
                    key: const Key('shell-output-lines'),
                    padding: const EdgeInsets.fromLTRB(16, 4, 16, 16),
                    itemCount: lines.length,
                    itemBuilder: (context, i) => Text(lines[i], style: mono),
                  ),
                ),
        ),
      ],
    );
  }
}
