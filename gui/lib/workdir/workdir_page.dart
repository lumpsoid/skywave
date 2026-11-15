import 'package:material_ui/material_ui.dart';
import 'package:flutter/rendering.dart';
import 'package:skywave_gui/crop/blob_menu.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/link/link_status.dart';
import 'package:skywave_gui/screen/readable_width.dart';
import 'package:skywave_gui/session/look.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';
import 'package:skywave_gui/workdir/pushing.dart';
import 'package:skywave_gui/workdir/saving.dart';
import 'package:skywave_gui/workdir/workdir_text_holder.dart';

/// A session's workdir, read-only: its git changes and a changed file's
/// diff (Changes), its newest commits (Commits), or its files and one
/// file's content (Files). Rust reads each pane on request and keeps the
/// answer, so what was read shows offline; this lays out what it worded.
/// The diff or file shown is capped by the hub; "Open" opens the whole
/// downloaded file.
class WorkdirPage extends StatelessWidget {
  const WorkdirPage({
    required this.screen,
    required this.text,
    required this.send,
    this.canGoBack = false,
    super.key,
  });

  final WorkdirScreen screen;

  /// The lines of the diff or file shown, sent apart from the screen.
  final WorkdirTextHolder text;
  final bool canGoBack;
  final SendIntent send;

  bool get _onChanges =>
      screen.pane == WorkdirPaneKind.changes ||
      screen.pane == WorkdirPaneKind.diff;

  bool get _onCommits => screen.pane == WorkdirPaneKind.commits;

  void _show(WorkdirPaneKind pane, String path, {bool untracked = false}) =>
      send.act(
        ActionIntentShowWorkdir(pane: pane, path: path, untracked: untracked),
      );

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final colors = SkyColors.of(context);
    final muted = theme.textTheme.bodySmall?.copyWith(color: colors.muted);
    final workdir = screen.workdir;
    final fetch = screen.fetchLabel;
    return Scaffold(
      appBar: AppBar(
        leading: backButton(
          canGoBack: canGoBack,
          onBack: () => send.act(const ActionIntentBack()),
        ),
        toolbarHeight: LinkTitle.toolbarHeight(context),
        flexibleSpace: const LinkProgress(),
        title: LinkTitle(
          title: Text(
            screen.title,
            maxLines: 1,
            overflow: TextOverflow.ellipsis,
          ),
          subtitle: workdir == null
              ? null
              : Text(
                  workdir,
                  style: muted,
                  maxLines: 1,
                  overflow: TextOverflow.ellipsis,
                ),
          send: send,
        ),
        actions: [
          ApprovalsButton(send: send),
          if (screen.pane == WorkdirPaneKind.file)
            IconButton(
              tooltip: 'Download file',
              icon: const Icon(Icons.download_outlined),
              onPressed: () => send.act(
                ActionIntentDownloadWorkdirFile(
                  sessionId: screen.sessionId,
                  path: screen.path,
                ),
              ),
            ),
          if (screen.pane == WorkdirPaneKind.dir)
            IconButton(
              key: const ValueKey('workdir-push'),
              tooltip: 'Push a file here',
              icon: const Icon(Icons.upload_file_outlined),
              onPressed: () => _push(context, screen, send),
            ),
          IconButton(
            tooltip: 'Refresh',
            icon: const Icon(Icons.refresh),
            onPressed: () => send.act(const ActionIntentRefreshWorkdir()),
          ),
        ],
      ),
      body: ReadableWidth(
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Padding(
              padding: const EdgeInsets.fromLTRB(16, 12, 16, 0),
              child: Wrap(
                spacing: 8,
                children: [
                  ChoiceChip(
                    key: const Key('workdir-tab-changes'),
                    label: const Text('Changes'),
                    selected: _onChanges,
                    onSelected: (_) {
                      if (screen.pane != WorkdirPaneKind.changes) {
                        _show(WorkdirPaneKind.changes, '');
                      }
                    },
                  ),
                  ChoiceChip(
                    key: const Key('workdir-tab-commits'),
                    label: const Text('Commits'),
                    selected: _onCommits,
                    onSelected: (_) {
                      if (screen.pane != WorkdirPaneKind.commits) {
                        _show(WorkdirPaneKind.commits, '');
                      }
                    },
                  ),
                  ChoiceChip(
                    key: const Key('workdir-tab-files'),
                    label: const Text('Files'),
                    selected: !_onChanges && !_onCommits,
                    onSelected: (_) {
                      if (screen.pane != WorkdirPaneKind.dir ||
                          screen.path != screen.lastDir) {
                        _show(WorkdirPaneKind.dir, screen.lastDir);
                      }
                    },
                  ),
                ],
              ),
            ),
            Padding(
              padding: const EdgeInsets.fromLTRB(16, 8, 16, 4),
              child: Text.rich(
                TextSpan(
                  text: screen.header,
                  style: theme.textTheme.bodyMedium?.copyWith(
                    fontWeight: FontWeight.w600,
                  ),
                  children: [
                    if (fetch != null)
                      TextSpan(
                        text: ' · $fetch',
                        style: TextStyle(
                          fontWeight: FontWeight.normal,
                          color: screen.fetchFailed
                              ? theme.colorScheme.error
                              : colors.warning,
                        ),
                      ),
                  ],
                ),
              ),
            ),
            const Divider(height: 1),
            Expanded(
              child: switch (screen.pane) {
                WorkdirPaneKind.changes ||
                WorkdirPaneKind.commits ||
                WorkdirPaneKind.dir => _ListPane(screen: screen, send: send),
                WorkdirPaneKind.diff || WorkdirPaneKind.file => _TextPane(
                  screen: screen,
                  text: text,
                  send: send,
                ),
              },
            ),
            if (screen.downloads.isNotEmpty) ...[
              const Divider(height: 1),
              _DownloadsPanel(
                sessionId: screen.sessionId,
                downloads: screen.downloads,
                send: send,
              ),
            ],
            if (screen.pushes.isNotEmpty) ...[
              const Divider(height: 1),
              _PushesPanel(
                sessionId: screen.sessionId,
                pushes: screen.pushes,
                send: send,
              ),
            ],
          ],
        ),
      ),
    );
  }
}

/// The changed files, the commits, or a directory's entries; the pane's
/// wording and notes beneath them.
class _ListPane extends StatelessWidget {
  const _ListPane({required this.screen, required this.send});

  final WorkdirScreen screen;
  final SendIntent send;

  @override
  Widget build(BuildContext context) {
    final colors = SkyColors.of(context);
    final muted = Theme.of(context).textTheme.bodySmall
        ?.copyWith(color: colors.muted);
    final wording = screen.wording;
    return ListView(
      children: [
        for (final row in screen.changes) _ChangeTile(row: row, send: send),
        for (final row in screen.commits) _CommitTile(row: row),
        if (screen.canGoUp)
          ListTile(
            dense: true,
            leading: const Icon(Icons.drive_folder_upload_outlined),
            title: const Text('../'),
            // The core goes up a directory.
            onTap: () => send.act(const ActionIntentBack()),
          ),
        for (final entry in screen.entries)
          ListTile(
            dense: true,
            leading: Icon(
              entry.opens == WorkdirPaneKind.dir
                  ? Icons.folder_outlined
                  : Icons.description_outlined,
            ),
            title: Text(entry.name),
            trailing: Row(
              mainAxisSize: MainAxisSize.min,
              children: [
                if (entry.detail != null) Text(entry.detail!, style: muted),
                if (entry.opens == WorkdirPaneKind.file)
                  IconButton(
                    key: ValueKey('workdir-download-${entry.path}'),
                    tooltip: 'Download',
                    icon: const Icon(Icons.download_outlined),
                    onPressed: () => send.act(
                      ActionIntentDownloadWorkdirFile(
                        sessionId: screen.sessionId,
                        path: entry.path,
                      ),
                    ),
                  ),
              ],
            ),
            onTap: () => send.act(
              ActionIntentShowWorkdir(
                pane: entry.opens,
                path: entry.path,
                untracked: false,
              ),
            ),
          ),
        if (wording != null)
          Padding(
            padding: const EdgeInsets.all(16),
            child: Text(wording, style: muted),
          ),
        for (final note in screen.notes)
          Padding(
            padding: const EdgeInsets.fromLTRB(16, 8, 16, 8),
            child: Text(note, style: muted),
          ),
      ],
    );
  }
}

/// The downloads this session started, one row each in the core's order: the
/// path, its one line of progress or outcome, then Save… once the file is on
/// this device, or Retry when the download failed.
class _DownloadsPanel extends StatelessWidget {
  const _DownloadsPanel({
    required this.sessionId,
    required this.downloads,
    required this.send,
  });

  final String sessionId;
  final List<WorkdirDownloadRow> downloads;
  final SendIntent send;

  @override
  Widget build(BuildContext context) {
    final colors = SkyColors.of(context);
    final muted = Theme.of(context).textTheme.bodySmall
        ?.copyWith(color: colors.muted);
    return ConstrainedBox(
      constraints: const BoxConstraints(maxHeight: 160),
      child: ListView(
        shrinkWrap: true,
        children: [
          for (final row in downloads)
            ListTile(
              dense: true,
              title: Text(row.path),
              subtitle: Text(row.label, style: muted),
              trailing: _action(context, row),
            ),
        ],
      ),
    );
  }

  Widget? _action(BuildContext context, WorkdirDownloadRow row) {
    final localPath = row.localPath;
    if (localPath != null) {
      return TextButton(
        onPressed: () => _save(context, localPath, row.path),
        child: const Text('Save…'),
      );
    }
    if (row.failed) {
      return TextButton(
        onPressed: () => send.act(
          ActionIntentDownloadWorkdirFile(sessionId: sessionId, path: row.path),
        ),
        child: const Text('Retry'),
      );
    }
    return null;
  }

  Future<void> _save(
    BuildContext context,
    String localPath,
    String path,
  ) async {
    final name = path.split('/').last;
    try {
      final saved = await saveWorkdirFile(source: localPath, name: name);
      if (saved != null && context.mounted) {
        _say(context, 'Saved $path');
      }
    } catch (error) {
      if (context.mounted) {
        _say(context, 'Could not save $path: $error');
      }
    }
  }

  void _say(BuildContext context, String text) {
    ScaffoldMessenger.of(context).showSnackBar(SnackBar(content: Text(text)));
  }
}

/// Asks for the file to push and the path it lands at, then sends the push.
/// The path proposed is the shown directory and the file's name; nothing is
/// sent when the picker is cancelled or the path is left empty.
Future<void> _push(
  BuildContext context,
  WorkdirScreen screen,
  SendIntent send,
) async {
  final source = await pickPushFile();
  if (source == null || !context.mounted) {
    return;
  }
  final name = _fileName(source);
  final dir = screen.lastDir;
  final proposal = dir.isEmpty || dir == '.' ? name : '$dir/$name';
  final target = await showDialog<String>(
    context: context,
    builder: (context) => _TargetDialog(proposal: proposal),
  );
  if (target == null || target.isEmpty) {
    return;
  }
  send.act(
    ActionIntentPushWorkdirFile(
      sessionId: screen.sessionId,
      source: source,
      path: target,
    ),
  );
}

/// The last segment of a path, split on `/` or `\` so a Windows path works.
String _fileName(String path) => path.split(RegExp(r'[/\\]')).last;

/// Edits the path a file is pushed to: pops the trimmed text, or null when
/// cancelled.
class _TargetDialog extends StatefulWidget {
  const _TargetDialog({required this.proposal});

  final String proposal;

  @override
  State<_TargetDialog> createState() => _TargetDialogState();
}

class _TargetDialogState extends State<_TargetDialog> {
  late final _target = TextEditingController(text: widget.proposal);

  @override
  void dispose() {
    _target.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return AlertDialog(
      title: const Text('Push into the workdir'),
      content: SizedBox(
        width: 420,
        child: TextField(
          key: const Key('push-target-field'),
          controller: _target,
          autofocus: true,
          decoration: const InputDecoration(labelText: 'Path in the workdir'),
        ),
      ),
      actions: [
        TextButton(
          key: const Key('push-target-cancel'),
          onPressed: () => Navigator.of(context).pop(),
          child: const Text('Cancel'),
        ),
        FilledButton(
          key: const Key('push-target-ok'),
          onPressed: () => Navigator.of(context).pop(_target.text.trim()),
          child: const Text('Push'),
        ),
      ],
    );
  }
}

/// The pushes this session started, one row each: the path, its one line of
/// progress or outcome, then Replace… when a different file is already at the
/// path (asked first), or Retry when the push failed.
class _PushesPanel extends StatelessWidget {
  const _PushesPanel({
    required this.sessionId,
    required this.pushes,
    required this.send,
  });

  final String sessionId;
  final List<WorkdirPushRow> pushes;
  final SendIntent send;

  @override
  Widget build(BuildContext context) {
    final colors = SkyColors.of(context);
    final muted = Theme.of(context).textTheme.bodySmall
        ?.copyWith(color: colors.muted);
    return ConstrainedBox(
      constraints: const BoxConstraints(maxHeight: 160),
      child: ListView(
        shrinkWrap: true,
        children: [
          for (final row in pushes)
            ListTile(
              dense: true,
              title: Text(row.path),
              subtitle: Text(row.label, style: muted),
              trailing: _action(context, row),
            ),
        ],
      ),
    );
  }

  Widget? _action(BuildContext context, WorkdirPushRow row) {
    if (row.existingSha256 != null) {
      return TextButton(
        onPressed: () => _replace(context, row),
        child: const Text('Replace…'),
      );
    }
    if (row.failed) {
      return TextButton(
        onPressed: () => send.act(
          ActionIntentPushWorkdirFile(
            sessionId: sessionId,
            source: row.source,
            path: row.path,
          ),
        ),
        child: const Text('Retry'),
      );
    }
    return null;
  }

  /// Replaces the file at the row's path only after the user confirms.
  Future<void> _replace(BuildContext context, WorkdirPushRow row) async {
    final name = _fileName(row.source);
    final yes = await confirmIrreversible(
      context,
      question: 'Replace ${row.path} in the workdir with $name?',
      action: 'Replace',
    );
    if (yes) {
      send.act(
        ActionIntentPushWorkdirFile(
          sessionId: sessionId,
          source: row.source,
          path: row.path,
          replace: row.existingSha256,
        ),
      );
    }
  }
}

/// One changed file: its mark coloured by how it changed, its path, its
/// line counts.
class _ChangeTile extends StatelessWidget {
  const _ChangeTile({required this.row, required this.send});

  final WorkdirChangeRow row;
  final SendIntent send;

  @override
  Widget build(BuildContext context) {
    final colors = SkyColors.of(context);
    final scheme = Theme.of(context).colorScheme;
    final color = switch (row.change) {
      WorkdirChangeKind.added || WorkdirChangeKind.untracked => colors.success,
      WorkdirChangeKind.deleted || WorkdirChangeKind.conflicted => scheme.error,
      WorkdirChangeKind.modified ||
      WorkdirChangeKind.typeChanged => colors.warning,
      WorkdirChangeKind.renamed => scheme.primary,
    };
    final from = row.from;
    final counts = row.countsLabel;
    return ListTile(
      dense: true,
      leading: Tooltip(
        message: row.changeLabel,
        child: Text(
          row.mark,
          style: TextStyle(
            fontFamily: monoFamily,
            fontFamilyFallback: monoFallback,
            fontWeight: FontWeight.bold,
            color: color,
          ),
        ),
      ),
      title: Text(row.pathLabel),
      subtitle: from == null ? null : Text('from $from'),
      trailing: counts == null
          ? null
          : Text(counts, style: TextStyle(color: colors.muted)),
      onTap: () => send.act(
        ActionIntentShowWorkdir(
          pane: WorkdirPaneKind.diff,
          path: row.path,
          untracked: row.untracked,
        ),
      ),
    );
  }
}

/// One commit: its short sha, subject, author and age.
class _CommitTile extends StatelessWidget {
  const _CommitTile({required this.row});

  final WorkdirCommitRow row;

  @override
  Widget build(BuildContext context) {
    final colors = SkyColors.of(context);
    final time = row.timeLabel;
    return ListTile(
      dense: true,
      leading: Text(
        row.short,
        style: TextStyle(
          fontFamily: monoFamily,
          fontFamilyFallback: monoFallback,
          color: colors.muted,
        ),
      ),
      title: Text(row.subject),
      subtitle: Text(time == null ? row.author : '${row.author} · $time'),
    );
  }
}

/// A diff coloured by line kind, or a file with line numbers, from its
/// downloaded blob; its download and an Open button above it.
class _TextPane extends StatelessWidget {
  const _TextPane({
    required this.screen,
    required this.text,
    required this.send,
  });

  final WorkdirScreen screen;
  final WorkdirTextHolder text;
  final SendIntent send;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final colors = SkyColors.of(context);
    final muted = theme.textTheme.bodySmall?.copyWith(color: colors.muted);
    final wording = screen.wording;
    final content = screen.content;
    if (content == null) {
      return ListView(
        children: [
          if (wording != null)
            Padding(
              padding: const EdgeInsets.all(16),
              child: Text(wording, style: muted),
            ),
        ],
      );
    }
    final blob = content.blob;
    final error = content.error;
    final cut = content.cut;
    final base = theme.textTheme.bodySmall!.copyWith(
      fontFamily: monoFamily,
      fontFamilyFallback: monoFallback,
      height: 1.35,
      color: theme.colorScheme.onSurface,
    );
    final numbered = screen.pane == WorkdirPaneKind.file;
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
              Text(blob.label, style: muted),
              if (blob.canDownload)
                TextButton.icon(
                  icon: const Icon(Icons.download, size: 18),
                  label: const Text('Download'),
                  onPressed: () =>
                      send.act(ActionIntentDownloadBlob(blobId: blob.blobId)),
                ),
              if (blob.state == BlobStateKind.available)
                TextButton.icon(
                  icon: const Icon(Icons.open_in_new, size: 18),
                  label: const Text('Open'),
                  onPressed: () => send(GuiIntentOpenFull(blobId: blob.blobId)),
                ),
              if (cut != null)
                Text(cut, style: muted?.copyWith(fontStyle: FontStyle.italic)),
            ],
          ),
        ),
        if (error != null)
          Padding(
            padding: const EdgeInsets.fromLTRB(16, 4, 16, 4),
            child: Text(
              error,
              style: TextStyle(color: theme.colorScheme.error),
            ),
          ),
        Expanded(
          child: SelectionArea(
            // Only the lines rebuild when they arrive; the rest of the pane
            // stays as it is.
            child: ListenableBuilder(
              listenable: text,
              builder: (context, _) {
                final lines = text.lines(blob.blobId);
                final width = '${lines.length}'.length;
                return ListView.builder(
                  padding: const EdgeInsets.fromLTRB(16, 4, 16, 16),
                  itemCount: lines.length,
                  itemBuilder: (context, i) {
                    final line = lines[i];
                    final selectable = _SelectableLine(
                      text: line.text,
                      style: _style(line.kind, base, colors),
                    );
                    if (!numbered) {
                      return selectable;
                    }
                    return Row(
                      crossAxisAlignment: CrossAxisAlignment.start,
                      children: [
                        SelectionContainer.disabled(
                          child: Text(
                            '${'${i + 1}'.padLeft(width)}  ',
                            style: base.copyWith(color: colors.muted),
                          ),
                        ),
                        Expanded(child: selectable),
                      ],
                    );
                  },
                );
              },
            ),
          ),
        ),
      ],
    );
  }

  /// A line's style by its kind, as the timeline colours a diff.
  static TextStyle _style(
    WorkdirLineKind kind,
    TextStyle base,
    SkyColors colors,
  ) => switch (kind) {
    WorkdirLineKind.fileHeader => colors.line(LineStyle.fileHeader, base),
    WorkdirLineKind.hunk => colors.line(LineStyle.hunk, base),
    WorkdirLineKind.added => colors.line(LineStyle.added, base),
    WorkdirLineKind.removed => colors.line(LineStyle.removed, base),
    WorkdirLineKind.meta => base.copyWith(color: colors.muted),
    WorkdirLineKind.context || WorkdirLineKind.plain => base,
  };
}

/// One line of a file or diff as its own selection container, so a copy ends
/// the line with a newline. A scrollable joins its children's selected text
/// with nothing, which would run the copied lines together.
class _SelectableLine extends StatefulWidget {
  const _SelectableLine({required this.text, required this.style});

  final String text;
  final TextStyle style;

  /// What a blank line shows: an empty `Text` has nothing to select, so a
  /// copy would skip the line. A zero-width space is selectable and takes
  /// the same height; the delegate drops it from the copy.
  static const blank = '\u{200B}';

  @override
  State<_SelectableLine> createState() => _SelectableLineState();
}

class _SelectableLineState extends State<_SelectableLine> {
  final _delegate = _LineSelectionDelegate();

  @override
  void dispose() {
    _delegate.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    _delegate.isBlank = widget.text.isEmpty;
    return SelectionContainer(
      delegate: _delegate,
      child: Text(
        widget.text.isEmpty ? _SelectableLine.blank : widget.text,
        style: widget.style,
      ),
    );
  }
}

/// Selects its one line and ends the copied text with `\n` when the
/// selection reaches the end of the line, not when it stops inside it.
class _LineSelectionDelegate extends MultiSelectableSelectionContainerDelegate {
  bool isBlank = false;

  @override
  SelectedContent? getSelectedContent() {
    final range = getSelection();
    if (range == null) {
      return null;
    }
    final copied = super.getSelectedContent()?.plainText ?? '';
    final text = isBlank
        ? copied.replaceAll(_SelectableLine.blank, '')
        : copied;
    final reachesEnd =
        range.startOffset >= contentLength || range.endOffset >= contentLength;
    return SelectedContent(plainText: reachesEnd ? '$text\n' : text);
  }

  @override
  void ensureChildUpdated(Selectable selectable) {}
}
