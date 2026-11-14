import 'dart:async';

import 'package:flutter/gestures.dart';
import 'package:material_ui/material_ui.dart';
import 'package:flutter/services.dart';
import 'package:skywave_gui/session/look.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';
import 'package:url_launcher/url_launcher.dart';

/// An agent message's markdown blocks as the hub parsed them, in one
/// column: a list item's content indented under its marker, a quote's
/// behind a bar. Text takes [style]; a link opens only after a dialog
/// shows where it goes.
class MarkdownBody extends StatelessWidget {
  const MarkdownBody({required this.blocks, required this.style, super.key});

  final List<MarkdownBlock> blocks;
  final TextStyle style;

  /// Width of a list marker's column, and so of one level of indent.
  static const _indent = 22.0;

  @override
  Widget build(BuildContext context) {
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      mainAxisSize: MainAxisSize.min,
      children: [
        for (final (i, block) in blocks.indexed)
          Padding(
            padding: EdgeInsets.only(
              top: i == 0 ? 0 : _gap(blocks[i - 1], block),
            ),
            child: _framed(context, block, _content(context, block)),
          ),
      ],
    );
  }

  /// Items of one list sit closer than paragraphs do.
  static double _gap(MarkdownBlock above, MarkdownBlock block) =>
      block.marker != null && above.depth > 0 ? 3 : 8;

  /// [child] behind its quote bar and under its list marker.
  Widget _framed(BuildContext context, MarkdownBlock block, Widget child) {
    final colors = SkyColors.of(context);
    var framed = child;
    final depth = block.depth;
    if (block.marker case final marker?) {
      framed = Row(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          SizedBox(
            width: _indent,
            child: Text(marker, style: style.copyWith(color: colors.muted)),
          ),
          Expanded(child: framed),
        ],
      );
    }
    final indent = (depth - (block.marker == null ? 0 : 1)) * _indent;
    if (indent > 0) {
      framed = Padding(
        padding: EdgeInsets.only(left: indent),
        child: framed,
      );
    }
    if (block.quote > 0) {
      framed = Container(
        padding: EdgeInsets.only(left: 10.0 * block.quote),
        decoration: BoxDecoration(
          border: Border(
            left: BorderSide(
              color: Theme.of(context).colorScheme.outlineVariant,
              width: 3,
            ),
          ),
        ),
        child: framed,
      );
    }
    return framed;
  }

  Widget _content(BuildContext context, MarkdownBlock block) {
    final theme = Theme.of(context);
    final muted = block.quote > 0
        ? style.copyWith(color: SkyColors.of(context).muted)
        : style;
    return switch (block.kind) {
      MarkdownKindParagraph(:final spans) => _Spans(spans: spans, style: muted),
      MarkdownKindHeading(:final level, :final spans) => _Spans(
        spans: spans,
        style: muted.copyWith(
          fontSize: switch (level) {
            1 => theme.textTheme.titleLarge?.fontSize,
            2 => theme.textTheme.titleMedium?.fontSize,
            _ => muted.fontSize,
          },
          fontWeight: FontWeight.bold,
        ),
      ),
      MarkdownKindCode(:final lang, :final text) => _CodeBlock(
        lang: lang,
        text: text,
        style: style,
      ),
      MarkdownKindRule() => Divider(
        height: 9,
        color: theme.colorScheme.outlineVariant,
      ),
      MarkdownKindTable(:final align, :final head, :final rows) => _MdTable(
        align: align,
        head: head,
        rows: rows,
        style: muted,
      ),
      _ => const SizedBox.shrink(),
    };
  }
}

/// Spans styled and their links tappable. Stateful only to own the tap
/// recognisers.
class _Spans extends StatefulWidget {
  const _Spans({required this.spans, required this.style});

  final List<MarkdownSpan> spans;
  final TextStyle style;

  @override
  State<_Spans> createState() => _SpansState();
}

class _SpansState extends State<_Spans> {
  final _taps = <TapGestureRecognizer>[];

  void _drop() {
    for (final tap in _taps) {
      tap.dispose();
    }
    _taps.clear();
  }

  @override
  void dispose() {
    _drop();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    _drop();
    return Text.rich(
      TextSpan(
        style: widget.style,
        children: [for (final span in widget.spans) _span(context, span)],
      ),
    );
  }

  TextSpan _span(BuildContext context, MarkdownSpan span) {
    final scheme = Theme.of(context).colorScheme;
    var style = TextStyle(
      fontWeight: span.bold ? FontWeight.bold : null,
      fontStyle: span.italic ? FontStyle.italic : null,
      decoration: span.strike ? TextDecoration.lineThrough : null,
    );
    if (span.code) {
      style = style.copyWith(
        fontFamily: monoFamily,
        fontFamilyFallback: monoFallback,
        backgroundColor: scheme.surfaceContainerHighest,
      );
    }
    final link = span.link;
    if (link == null) return TextSpan(text: span.text, style: style);
    final tap = TapGestureRecognizer()
      ..onTap = () => unawaited(openLink(context, link));
    _taps.add(tap);
    return TextSpan(
      text: span.text,
      style: style.copyWith(
        color: scheme.primary,
        decoration: TextDecoration.underline,
      ),
      recognizer: tap,
      mouseCursor: SystemMouseCursors.click,
    );
  }
}

/// Asks before opening [url] outside the app, showing it whole: a
/// message's link text may say anything.
Future<void> openLink(BuildContext context, String url) async {
  final open = await showDialog<bool>(
    context: context,
    builder: (context) => AlertDialog(
      title: const Text('Open link?'),
      content: SelectableText(url),
      actions: [
        TextButton(
          onPressed: () {
            unawaited(Clipboard.setData(ClipboardData(text: url)));
            Navigator.pop(context, false);
          },
          child: const Text('Copy'),
        ),
        TextButton(
          onPressed: () => Navigator.pop(context, false),
          child: const Text('Cancel'),
        ),
        FilledButton(
          onPressed: () => Navigator.pop(context, true),
          child: const Text('Open'),
        ),
      ],
    ),
  );
  if (open != true) return;
  final uri = Uri.tryParse(url);
  final opened =
      uri != null &&
      await launchUrl(
        uri,
        mode: LaunchMode.externalApplication,
      ).catchError((_) => false);
  if (!opened && context.mounted) {
    ScaffoldMessenger.maybeOf(context)
        ?.showSnackBar(SnackBar(content: Text('Could not open $url')));
  }
}

/// A fenced or indented code block: monospace, scrolling sideways, with
/// its language and a copy button above.
class _CodeBlock extends StatelessWidget {
  const _CodeBlock({
    required this.lang,
    required this.text,
    required this.style,
  });

  final String? lang;
  final String text;
  final TextStyle style;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final colors = SkyColors.of(context);
    final mono = theme.textTheme.bodySmall!.copyWith(
      fontFamily: monoFamily,
      fontFamilyFallback: monoFallback,
      height: 1.35,
      color: style.color,
    );
    return Container(
      decoration: BoxDecoration(
        color: theme.colorScheme.surfaceContainerHighest.withValues(alpha: 0.6),
        borderRadius: BorderRadius.circular(8),
      ),
      padding: const EdgeInsets.fromLTRB(10, 2, 2, 8),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Row(
            children: [
              Expanded(
                child: Text(
                  lang ?? '',
                  style: theme.textTheme.labelSmall?.copyWith(
                    color: colors.muted,
                  ),
                ),
              ),
              IconButton(
                tooltip: 'Copy code',
                visualDensity: VisualDensity.compact,
                iconSize: 16,
                icon: const Icon(Icons.copy),
                onPressed: () {
                  unawaited(Clipboard.setData(ClipboardData(text: text)));
                  ScaffoldMessenger.maybeOf(
                    context,
                  )?.showSnackBar(const SnackBar(content: Text('Code copied')));
                },
              ),
            ],
          ),
          SingleChildScrollView(
            scrollDirection: Axis.horizontal,
            primary: false,
            padding: const EdgeInsets.only(right: 8),
            child: Text(text, style: mono, softWrap: false),
          ),
        ],
      ),
    );
  }
}

/// A table, scrolling sideways when wider than the bubble.
class _MdTable extends StatelessWidget {
  const _MdTable({
    required this.align,
    required this.head,
    required this.rows,
    required this.style,
  });

  final List<MarkdownAlign> align;
  final List<MarkdownCell> head;
  final List<MarkdownRow> rows;
  final TextStyle style;

  @override
  Widget build(BuildContext context) {
    final border = Theme.of(context).colorScheme.outlineVariant;
    final columns = align.length;
    TableRow row(List<MarkdownCell> cells, TextStyle style) => TableRow(
      children: [
        for (var i = 0; i < columns; i++)
          Padding(
            padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 4),
            child: Align(
              alignment: switch (align[i]) {
                MarkdownAlign.center => Alignment.center,
                MarkdownAlign.right => Alignment.centerRight,
                MarkdownAlign.left => Alignment.centerLeft,
              },
              widthFactor: 1,
              child: _Spans(
                spans: i < cells.length ? cells[i].spans : const [],
                style: style,
              ),
            ),
          ),
      ],
    );
    return SingleChildScrollView(
      scrollDirection: Axis.horizontal,
      primary: false,
      child: Table(
        defaultColumnWidth: const IntrinsicColumnWidth(),
        border: TableBorder.all(color: border),
        children: [
          row(head, style.copyWith(fontWeight: FontWeight.bold)),
          for (final r in rows) row(r.cells, style),
        ],
      ),
    );
  }
}
