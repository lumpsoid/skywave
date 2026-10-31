import 'package:flutter/material.dart';
import 'package:skywave_gui/session/highlight.dart';
import 'package:skywave_gui/session/look.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// One section of an item's content: its lines in monospace with their
/// colours (and line numbers for file content), then its note, the blob
/// behind it with a Download button, and "Open full file" when the shown
/// part was cut short.
class SectionBlock extends StatelessWidget {
  const SectionBlock({
    required this.section,
    this.hits = const {},
    this.mark,
    this.currentLine,
    this.currentLineKey,
    this.onDownload,
    this.onOpenFull,
    super.key,
  });

  final SectionView section;

  /// Search matches by line index.
  final Map<int, List<Utf16Range>> hits;

  /// How [hits] are marked.
  final TextStyle? mark;

  /// The line of the current match, keyed [currentLineKey] so the
  /// timeline can scroll to it.
  final int? currentLine;
  final Key? currentLineKey;

  /// `DownloadBlob` for this section's blob.
  final VoidCallback? onDownload;

  /// `OpenFull` for this section's blob.
  final VoidCallback? onOpenFull;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final colors = SkyColors.of(context);
    final base = theme.textTheme.bodySmall!.copyWith(
      fontFamily: monoFamily,
      fontFamilyFallback: monoFallback,
      height: 1.35,
      color: theme.colorScheme.onSurface,
    );
    final numbers = base.copyWith(color: colors.muted);
    final lines = section.lines;
    final width = section.numbered
        ? '${section.firstLine + lines.length}'.length
        : 0;
    final small = theme.textTheme.labelSmall?.copyWith(color: colors.muted);
    final blob = section.blob;

    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        if (lines.isNotEmpty)
          Container(
            decoration: BoxDecoration(
              color: theme.colorScheme.surfaceContainerHighest.withValues(
                alpha: 0.6,
              ),
              borderRadius: BorderRadius.circular(8),
            ),
            padding: const EdgeInsets.symmetric(horizontal: 10, vertical: 8),
            child: SingleChildScrollView(
              scrollDirection: Axis.horizontal,
              primary: false,
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  for (final (i, line) in lines.indexed)
                    Text.rich(
                      key: i == currentLine ? currentLineKey : null,
                      TextSpan(
                        children: [
                          if (section.numbered)
                            TextSpan(
                              text:
                                  '${'${section.firstLine + i}'.padLeft(width)}  ',
                              style: numbers,
                            ),
                          TextSpan(
                            style: colors.line(line.style, base),
                            children: highlighted(
                              line.text,
                              hits[i] ?? const [],
                              mark ?? base,
                            ),
                          ),
                        ],
                      ),
                      softWrap: false,
                    ),
                ],
              ),
            ),
          ),
        if (section.note != null || blob != null || section.cutShort)
          Padding(
            padding: const EdgeInsets.only(top: 4),
            child: Wrap(
              spacing: 8,
              runSpacing: 4,
              crossAxisAlignment: WrapCrossAlignment.center,
              children: [
                if (section.note case final note?)
                  Text(
                    note,
                    style: small?.copyWith(fontStyle: FontStyle.italic),
                  ),
                if (blob != null) Text(blob.label, style: small),
                if (blob != null && blob.canDownload)
                  _SmallButton(
                    icon: Icons.download,
                    label: 'Download',
                    onPressed: onDownload,
                  ),
                if (blob != null && section.cutShort)
                  _SmallButton(
                    icon: Icons.open_in_new,
                    label: 'Open full file',
                    onPressed: onOpenFull,
                  ),
              ],
            ),
          ),
      ],
    );
  }
}

class _SmallButton extends StatelessWidget {
  const _SmallButton({
    required this.icon,
    required this.label,
    required this.onPressed,
  });

  final IconData icon;
  final String label;
  final VoidCallback? onPressed;

  @override
  Widget build(BuildContext context) {
    return TextButton.icon(
      style: TextButton.styleFrom(
        visualDensity: VisualDensity.compact,
        padding: const EdgeInsets.symmetric(horizontal: 8),
        tapTargetSize: MaterialTapTargetSize.shrinkWrap,
      ),
      onPressed: onPressed,
      icon: Icon(icon, size: 16),
      label: Text(label),
    );
  }
}
