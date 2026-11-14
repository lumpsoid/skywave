import 'package:material_ui/material_ui.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// How search matches are marked in a row's strings.
@immutable
class MatchStyle {
  const MatchStyle({required this.match, required this.current});

  /// A match, theme-coloured.
  factory MatchStyle.of(BuildContext context) {
    final colors = Theme.of(context).colorScheme;
    return MatchStyle(
      match: TextStyle(
        backgroundColor: colors.tertiaryContainer,
        color: colors.onTertiaryContainer,
      ),
      current: TextStyle(
        backgroundColor: colors.tertiary,
        color: colors.onTertiary,
        fontWeight: FontWeight.bold,
      ),
    );
  }

  /// Any match.
  final TextStyle match;

  /// A match in the row of the current one.
  final TextStyle current;
}

/// [text] as spans, the UTF-16 [ranges] (as the hub sent them, ascending)
/// marked. Ranges past the end are clipped; an empty list is one span.
List<InlineSpan> highlighted(
  String text,
  List<Utf16Range> ranges,
  TextStyle mark,
) {
  if (ranges.isEmpty) return [TextSpan(text: text)];
  final spans = <InlineSpan>[];
  var at = 0;
  for (final range in ranges) {
    final start = range.start.clamp(at, text.length);
    final end = range.end.clamp(start, text.length);
    if (start > at) spans.add(TextSpan(text: text.substring(at, start)));
    if (end > start) {
      spans.add(TextSpan(text: text.substring(start, end), style: mark));
    }
    at = end;
  }
  if (at < text.length) spans.add(TextSpan(text: text.substring(at)));
  return spans;
}
