import 'package:flutter/material.dart';
import 'package:flutter/rendering.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/link/link_status.dart';
import 'package:skywave_gui/screen/readable_width.dart';
import 'package:skywave_gui/session/look.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// A session's workdir, read-only: its git changes and a changed file's
/// diff (Changes), or its files and one file's content (Files). Rust
/// reads each pane on request and keeps the answer, so what was read
/// shows offline; this lays out what it worded. The diff or file shown is
/// capped by the hub; "Open" opens the whole downloaded file.
class WorkdirPage extends StatelessWidget {
  const WorkdirPage({
    required this.screen,
    required this.send,
    this.canGoBack = false,
    super.key,
  });

  final WorkdirScreen screen;
  final bool canGoBack;
  final SendIntent send;

  bool get _onChanges =>
      screen.pane == WorkdirPaneKind.changes ||
      screen.pane == WorkdirPaneKind.diff;

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
                    key: const Key('workdir-tab-files'),
                    label: const Text('Files'),
                    selected: !_onChanges,
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
                WorkdirPaneKind.dir => _ListPane(screen: screen, send: send),
                WorkdirPaneKind.diff ||
                WorkdirPaneKind.file => _TextPane(screen: screen, send: send),
              },
            ),
          ],
        ),
      ),
    );
  }
}

/// The changed files, or a directory's entries; the pane's wording and
/// notes beneath them.
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
            trailing: entry.detail == null
                ? null
                : Text(entry.detail!, style: muted),
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

/// A diff coloured by line kind, or a file with line numbers, from its
/// downloaded blob; its download and an Open button above it.
class _TextPane extends StatelessWidget {
  const _TextPane({required this.screen, required this.send});

  final WorkdirScreen screen;
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
    final lines = content.lines;
    final width = '${lines.length}'.length;
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
            child: ListView.builder(
              padding: const EdgeInsets.fromLTRB(16, 4, 16, 16),
              itemCount: lines.length,
              itemBuilder: (context, i) {
                final line = lines[i];
                final text = _SelectableLine(
                  text: line.text,
                  style: _style(line.kind, base, colors),
                );
                if (!numbered) {
                  return text;
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
                    Expanded(child: text),
                  ],
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
