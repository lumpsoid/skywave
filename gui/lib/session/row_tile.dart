import 'package:flutter/material.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/session/highlight.dart';
import 'package:skywave_gui/session/look.dart';
import 'package:skywave_gui/session/section_block.dart';
import 'package:skywave_gui/session/snippets.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// One timeline row as Rust sent it: the turn separator or "new" rule
/// above it, its time, then the item by kind, with its content sections.
/// A tap selects it and expands or collapses it; a long press or right
/// click opens its menu (copy, save as snippet, download, edit or
/// discard). A message's
/// text selects, with its own menu.
class RowTile extends StatelessWidget {
  const RowTile({
    required this.row,
    required this.send,
    required this.onSelect,
    this.selected = false,
    this.hit,
    this.current = false,
    this.currentLine,
    this.currentLineKey,
    super.key,
  });

  final TimelineRow row;
  final SendIntent send;

  /// Selects this row.
  final VoidCallback onSelect;
  final bool selected;

  /// Where the search matches in this row.
  final SearchHit? hit;

  /// The current search match is in this row.
  final bool current;

  /// The blob line of the current match, and the key that marks it.
  final int? currentLine;
  final Key? currentLineKey;

  void _tap() {
    if (row.selectable) onSelect();
    if (!row.expandable) return;
    send(
      row.expanded
          ? GuiIntentCollapseItem(key: row.key)
          : GuiIntentExpandItem(key: row.key),
    );
  }

  Future<void> _menu(BuildContext context, Offset at) async {
    final items = _menuItems();
    if (items.isEmpty) return;
    if (row.selectable) onSelect();
    final overlay =
        Overlay.of(context).context.findRenderObject()! as RenderBox;
    final chosen = await showMenu<GuiIntent>(
      context: context,
      position: RelativeRect.fromRect(
        at & const Size(1, 1),
        Offset.zero & overlay.size,
      ),
      items: [
        for (final (label, icon, intent) in items)
          PopupMenuItem(
            value: intent,
            child: ListTile(
              dense: true,
              contentPadding: EdgeInsets.zero,
              leading: Icon(icon, size: 20),
              title: Text(label),
            ),
          ),
      ],
    );
    if (chosen is GuiIntentSaveSnippet) {
      // The name comes first; the menu only says which message.
      if (!context.mounted) return;
      final name = await askSnippetName(context);
      if (name != null) send(GuiIntentSaveSnippet(key: row.key, name: name));
    } else if (chosen != null) {
      send(chosen);
    }
  }

  List<(String, IconData, GuiIntent)> _menuItems() => [
    if (row.selectable)
      (
        row.tool == null ? 'Copy' : 'Copy output',
        Icons.copy,
        GuiIntentCopy(key: row.key, what: CopyWhat.main),
      ),
    if (row.kind == RowKind.userMessage || row.kind == RowKind.pendingMessage)
      (
        'Save as snippet',
        Icons.bookmark_add_outlined,
        GuiIntentSaveSnippet(key: row.key, name: ''),
      ),
    if (row.tool != null)
      (
        'Copy call',
        Icons.data_object,
        GuiIntentCopy(key: row.key, what: CopyWhat.call),
      ),
    if (row.expandable && !row.expanded) ...[
      ('Expand', Icons.unfold_more, GuiIntentExpandItem(key: row.key)),
      ('Download', Icons.download, GuiIntentDownloadItem(key: row.key)),
    ],
    if (row.expandable && row.expanded)
      ('Collapse', Icons.unfold_less, GuiIntentCollapseItem(key: row.key)),
    if (row.discardable && row.commandId != null) ...[
      (
        'Edit queued',
        Icons.edit,
        GuiIntentAct(action: ActionIntentEditQueued(commandId: row.commandId!)),
      ),
      (
        'Discard',
        Icons.delete_outline,
        GuiIntentAct(action: ActionIntentDiscard(commandId: row.commandId!)),
      ),
    ],
  ];

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final colors = SkyColors.of(context);
    final small = theme.textTheme.labelSmall?.copyWith(color: colors.muted);
    final marks = MatchStyle.of(context);
    final mark = current ? marks.current : marks.match;

    final body = switch (row.kind) {
      RowKind.userMessage ||
      RowKind.pendingMessage ||
      RowKind.agentMessage ||
      RowKind.agentError => _Message(
        row: row,
        hit: hit,
        mark: mark,
        send: send,
        onTap: row.selectable || row.expandable ? _tap : null,
      ),
      RowKind.tool ||
      RowKind.toolCall ||
      RowKind.toolResult ||
      RowKind.approval => _ToolCard(row: row, hit: hit, mark: mark),
      RowKind.interrupted || RowKind.stopped => _Notice(
        row: row,
        icon: row.kind == RowKind.stopped
            ? Icons.power_settings_new
            : Icons.pan_tool_outlined,
        color: colors.warning,
        hit: hit,
        mark: mark,
      ),
      RowKind.renamed => _Notice(
        row: row,
        icon: Icons.drive_file_rename_outline,
        color: colors.muted,
        hit: hit,
        mark: mark,
      ),
      RowKind.agentSetting => _Notice(
        row: row,
        icon: Icons.tune,
        color: colors.muted,
        hit: hit,
        mark: mark,
      ),
      RowKind.settingRefused => _Notice(
        row: row,
        icon: Icons.error_outline,
        color: colors.warning,
        hit: hit,
        mark: mark,
      ),
      RowKind.archived => _Notice(
        row: row,
        icon: Icons.inventory_2_outlined,
        color: colors.muted,
        hit: hit,
        mark: mark,
      ),
      RowKind.contextDivider => _Notice(
        row: row,
        icon: Icons.alt_route,
        color: colors.muted,
        hit: hit,
        mark: mark,
      ),
      RowKind.turnUsage => _Notice(
        row: row,
        icon: Icons.timer_outlined,
        color: colors.muted,
        hit: hit,
        mark: mark,
        trailing: true,
      ),
    };

    final lineHits = <int, Map<int, List<Utf16Range>>>{};
    for (final line in hit?.lines ?? const <LineHit>[]) {
      (lineHits[line.section] ??= {})[line.line] = line.ranges;
    }
    // The current match's line is in the section with a blob: its lines
    // are the blob's from line 0.
    final blobSection = row.sections.indexWhere((s) => s.blob != null);

    final sections = [
      for (final (i, section) in row.sections.indexed)
        Padding(
          padding: const EdgeInsets.only(top: 6),
          child: SectionBlock(
            section: section,
            hits: lineHits[i] ?? const {},
            mark: mark,
            currentLine: i == blobSection ? currentLine : null,
            currentLineKey: currentLineKey,
            onDownload: section.blob == null
                ? null
                : () => send.act(
                    ActionIntentDownloadBlob(blobId: section.blob!.blobId),
                  ),
            onOpenFull: section.blob == null
                ? null
                : () => send(GuiIntentOpenFull(blobId: section.blob!.blobId)),
          ),
        ),
    ];

    final interactive = row.selectable || row.expandable;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        if (row.turnStart)
          Padding(
            padding: const EdgeInsets.symmetric(vertical: 6),
            child: Divider(height: 1, color: theme.colorScheme.outlineVariant),
          ),
        if (row.unreadRule) _UnreadRule(color: theme.colorScheme.error),
        if (row.timeLabel case final time?)
          Padding(
            padding: const EdgeInsets.fromLTRB(16, 4, 16, 0),
            child: Text(time, style: small),
          ),
        Material(
          type: MaterialType.transparency,
          child: InkWell(
            onTap: interactive ? _tap : null,
            onLongPress: interactive
                ? () {
                    final box = context.findRenderObject()! as RenderBox;
                    _menu(
                      context,
                      box.localToGlobal(box.size.center(Offset.zero)),
                    );
                  }
                : null,
            onSecondaryTapUp: interactive
                ? (details) => _menu(context, details.globalPosition)
                : null,
            child: AnimatedContainer(
              duration: const Duration(milliseconds: 150),
              decoration: BoxDecoration(
                color: selected
                    ? theme.colorScheme.secondaryContainer.withValues(
                        alpha: 0.45,
                      )
                    : null,
                border: Border(
                  left: BorderSide(
                    width: 3,
                    color: selected
                        ? theme.colorScheme.secondary
                        : Colors.transparent,
                  ),
                ),
              ),
              padding: const EdgeInsets.fromLTRB(13, 4, 16, 4),
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.stretch,
                children: [
                  body,
                  if (sections.isNotEmpty)
                    _Indented(own: row.own, children: sections),
                ],
              ),
            ),
          ),
        ),
      ],
    );
  }
}

/// Content under an own message lines up with its bubble.
class _Indented extends StatelessWidget {
  const _Indented({required this.own, required this.children});

  final bool own;
  final List<Widget> children;

  @override
  Widget build(BuildContext context) {
    return Padding(
      padding: EdgeInsets.only(left: own ? 48 : 0),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: children,
      ),
    );
  }
}

class _UnreadRule extends StatelessWidget {
  const _UnreadRule({required this.color});

  final Color color;

  @override
  Widget build(BuildContext context) {
    final line = Expanded(child: Divider(height: 1, color: color));
    return Padding(
      padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 6),
      child: Row(
        children: [
          line,
          Padding(
            padding: const EdgeInsets.symmetric(horizontal: 8),
            child: Text(
              'new',
              semanticsLabel: 'new messages below',
              style: Theme.of(context).textTheme.labelSmall
                  ?.copyWith(color: color, fontWeight: FontWeight.bold),
            ),
          ),
          line,
        ],
      ),
    );
  }
}

/// A message: the author, the text, and for this device's own, how it
/// went out (with Edit and Discard while it can be taken back).
class _Message extends StatelessWidget {
  const _Message({
    required this.row,
    required this.hit,
    required this.mark,
    required this.send,
    required this.onTap,
  });

  final TimelineRow row;
  final SearchHit? hit;
  final TextStyle mark;
  final SendIntent send;

  /// The row's tap, for a tap on the text.
  final VoidCallback? onTap;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final scheme = theme.colorScheme;
    final colors = SkyColors.of(context);
    final error = row.kind == RowKind.agentError;
    final agent = row.kind == RowKind.agentMessage;
    final (background, foreground, authorColor) = switch (row) {
      _ when error => (
        scheme.errorContainer,
        scheme.onErrorContainer,
        scheme.error,
      ),
      _ when row.own => (
        scheme.primaryContainer,
        scheme.onPrimaryContainer,
        scheme.primary,
      ),
      _ when agent => (
        scheme.surfaceContainerHigh,
        scheme.onSurface,
        colors.success,
      ),
      _ => (
        scheme.tertiaryContainer,
        scheme.onTertiaryContainer,
        colors.denied,
      ),
    };
    final author = error ? 'agent error' : row.author;
    final outgoing = row.outgoing;
    final small = theme.textTheme.labelSmall?.copyWith(color: colors.muted);
    final pending = row.kind == RowKind.pendingMessage;

    final bubble = Container(
      padding: const EdgeInsets.fromLTRB(12, 8, 12, 10),
      decoration: BoxDecoration(
        color: pending ? background.withValues(alpha: 0.6) : background,
        borderRadius: BorderRadius.only(
          topLeft: const Radius.circular(16),
          topRight: const Radius.circular(16),
          bottomLeft: Radius.circular(row.own ? 16 : 4),
          bottomRight: Radius.circular(row.own ? 4 : 16),
        ),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        mainAxisSize: MainAxisSize.min,
        children: [
          if (author != null)
            Text(
              author,
              style: theme.textTheme.labelMedium?.copyWith(
                color: authorColor,
                fontWeight: FontWeight.bold,
              ),
            ),
          if (row.text.isNotEmpty)
            _SelectableBody(
              onTap: onTap,
              copyMessage: row.selectable
                  ? () => send(GuiIntentCopy(key: row.key, what: CopyWhat.main))
                  : null,
              child: Text.rich(
                TextSpan(
                  style: theme.textTheme.bodyMedium?.copyWith(
                    color: foreground,
                  ),
                  children: highlighted(row.text, hit?.text ?? const [], mark),
                ),
              ),
            ),
        ],
      ),
    );

    return Align(
      alignment: row.own ? Alignment.centerRight : Alignment.centerLeft,
      child: LayoutBuilder(
        builder: (context, constraints) => ConstrainedBox(
          constraints: BoxConstraints(maxWidth: constraints.maxWidth * 0.88),
          child: Column(
            crossAxisAlignment: row.own
                ? CrossAxisAlignment.end
                : CrossAxisAlignment.start,
            mainAxisSize: MainAxisSize.min,
            children: [
              bubble,
              if (outgoing != null ||
                  row.deliveryLabel != null ||
                  row.discardable)
                Padding(
                  padding: const EdgeInsets.only(top: 4),
                  child: Wrap(
                    alignment: WrapAlignment.end,
                    crossAxisAlignment: WrapCrossAlignment.center,
                    spacing: 6,
                    runSpacing: 2,
                    children: [
                      if (outgoing != null)
                        StateBadge(
                          label: outgoing.label,
                          color: colors.outgoing(outgoing.state),
                        ),
                      if (row.deliveryLabel case final delivery?)
                        Text(delivery, style: small),
                      if (row.discardable && row.commandId != null) ...[
                        _LinkButton(
                          label: 'Edit',
                          onPressed: () => send.act(
                            ActionIntentEditQueued(commandId: row.commandId!),
                          ),
                        ),
                        _LinkButton(
                          label: 'Discard',
                          onPressed: () => send.act(
                            ActionIntentDiscard(commandId: row.commandId!),
                          ),
                        ),
                      ],
                    ],
                  ),
                ),
            ],
          ),
        ),
      ),
    );
  }
}

/// A message's text, selectable: a drag (a long press on touch) selects,
/// and the selection's menu copies it or, with [copyMessage], the whole
/// message. A tap is still the row's [onTap], or clears a selection.
///
/// One area per message, not one over the timeline: the row's tap and
/// menu stay on the rest of the row, an area over the lazy list would
/// lose its selection as rows scroll out and are disposed, and a long
/// press over the whole list would be either the row's menu or a
/// selection, never both.
class _SelectableBody extends StatefulWidget {
  const _SelectableBody({required this.child, this.onTap, this.copyMessage});

  final Widget child;
  final VoidCallback? onTap;
  final VoidCallback? copyMessage;

  @override
  State<_SelectableBody> createState() => _SelectableBodyState();
}

class _SelectableBodyState extends State<_SelectableBody> {
  final _area = GlobalKey<SelectionAreaState>();
  bool _selected = false;

  /// The region takes a plain tap for its own (placing nothing, as text
  /// here is read-only): this one, nearer, wins it for the row.
  void _tap() {
    if (_selected) {
      _area.currentState?.selectableRegion.clearSelection();
      ContextMenuController.removeAny();
      return;
    }
    widget.onTap?.call();
  }

  @override
  Widget build(BuildContext context) {
    return SelectionArea(
      key: _area,
      onSelectionChanged: (content) {
        _selected = content != null && content.plainText.isNotEmpty;
      },
      contextMenuBuilder: (context, area) {
        final copy = widget.copyMessage;
        return AdaptiveTextSelectionToolbar.buttonItems(
          anchors: area.contextMenuAnchors,
          buttonItems: [
            ...area.contextMenuButtonItems,
            if (copy != null)
              ContextMenuButtonItem(
                label: 'Copy message',
                onPressed: () {
                  ContextMenuController.removeAny();
                  copy();
                },
              ),
          ],
        );
      },
      child: widget.onTap == null
          ? widget.child
          : GestureDetector(onTap: _tap, child: widget.child),
    );
  }
}

class _LinkButton extends StatelessWidget {
  const _LinkButton({required this.label, required this.onPressed});

  final String label;
  final VoidCallback onPressed;

  @override
  Widget build(BuildContext context) {
    return TextButton(
      style: TextButton.styleFrom(
        visualDensity: VisualDensity.compact,
        padding: const EdgeInsets.symmetric(horizontal: 8),
        minimumSize: const Size(0, 28),
        tapTargetSize: MaterialTapTargetSize.shrinkWrap,
      ),
      onPressed: onPressed,
      child: Text(label),
    );
  }
}

/// A tool item's head: its kind's icon, name and title, its state, and
/// the first line of its detail.
class _ToolCard extends StatelessWidget {
  const _ToolCard({required this.row, required this.hit, required this.mark});

  final TimelineRow row;
  final SearchHit? hit;
  final TextStyle mark;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final colors = SkyColors.of(context);
    final tool = row.tool;
    if (tool == null) {
      return Text.rich(
        TextSpan(children: highlighted(row.text, hit?.text ?? const [], mark)),
      );
    }
    final color = colors.tool(tool.state);
    final mono = theme.textTheme.bodyMedium?.copyWith(
      fontFamily: monoFamily,
      fontFamilyFallback: monoFallback,
    );
    final small = theme.textTheme.bodySmall?.copyWith(color: colors.muted);
    return Row(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Padding(
          padding: const EdgeInsets.only(top: 2),
          child: Icon(toolIcon(tool.kind), size: 18, color: color),
        ),
        const SizedBox(width: 10),
        Expanded(
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Wrap(
                spacing: 8,
                runSpacing: 4,
                crossAxisAlignment: WrapCrossAlignment.center,
                children: [
                  Text.rich(
                    TextSpan(
                      children: [
                        TextSpan(
                          text: tool.name,
                          style: theme.textTheme.bodyMedium?.copyWith(
                            fontWeight: FontWeight.bold,
                          ),
                        ),
                        const TextSpan(text: '  '),
                        TextSpan(
                          style: mono,
                          children: highlighted(
                            tool.title,
                            hit?.title ?? const [],
                            mark,
                          ),
                        ),
                      ],
                    ),
                    maxLines: row.expanded ? null : 3,
                    overflow: row.expanded ? null : TextOverflow.ellipsis,
                  ),
                  if (tool.stateLabel.isNotEmpty)
                    StateBadge(label: tool.stateLabel, color: color),
                ],
              ),
              if (tool.detail case final detail?)
                Padding(
                  padding: const EdgeInsets.only(top: 2),
                  child: Text.rich(
                    TextSpan(
                      style: small,
                      children: highlighted(
                        detail,
                        hit?.detail ?? const [],
                        mark,
                      ),
                    ),
                    maxLines: row.expanded ? null : 2,
                    overflow: row.expanded ? null : TextOverflow.ellipsis,
                  ),
                ),
              if (tool.aside case final aside?)
                Padding(
                  padding: const EdgeInsets.only(top: 2),
                  child: Text(aside, style: small),
                ),
            ],
          ),
        ),
        if (row.expandable)
          Icon(
            row.expanded ? Icons.expand_less : Icons.expand_more,
            size: 20,
            color: colors.muted,
          ),
      ],
    );
  }
}

/// A line that is not a message: interrupted, stopped, renamed, archived,
/// a mode or model switch or its refusal, a turn's usage.
class _Notice extends StatelessWidget {
  const _Notice({
    required this.row,
    required this.icon,
    required this.color,
    required this.hit,
    required this.mark,
    this.trailing = false,
  });

  final TimelineRow row;
  final IconData icon;
  final Color color;
  final SearchHit? hit;
  final TextStyle mark;

  /// Sits at the end, like a footnote to the turn.
  final bool trailing;

  @override
  Widget build(BuildContext context) {
    final style = Theme.of(context).textTheme.bodySmall?.copyWith(color: color);
    return Row(
      mainAxisAlignment: trailing
          ? MainAxisAlignment.end
          : MainAxisAlignment.center,
      children: [
        Icon(icon, size: 14, color: color),
        const SizedBox(width: 6),
        Flexible(
          child: Text.rich(
            TextSpan(
              style: style,
              children: highlighted(row.text, hit?.text ?? const [], mark),
            ),
          ),
        ),
      ],
    );
  }
}
