import 'dart:math' show max;

import 'package:material_ui/material_ui.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// One store's kinds as a stacked bar, each kind's share of the bytes, with
/// a legend under it: a colour, the label and the size. Rust sends the bytes;
/// Dart only sizes the segments. Nothing shows when there are no bytes.
class StorageKindBar extends StatelessWidget {
  const StorageKindBar({required this.kinds, super.key});

  final List<StorageKindView> kinds;

  @override
  Widget build(BuildContext context) {
    final total = kinds.fold(0, (sum, kind) => sum + kind.bytes);
    if (total == 0) return const SizedBox.shrink();
    final scheme = Theme.of(context).colorScheme;
    final palette = [
      scheme.primary,
      scheme.tertiary,
      scheme.secondary,
      scheme.error,
      scheme.outline,
    ];
    return Padding(
      padding: const EdgeInsets.fromLTRB(16, 12, 16, 4),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          ClipRRect(
            borderRadius: BorderRadius.circular(4),
            child: SizedBox(
              height: 8,
              child: Row(
                children: [
                  for (final (i, kind) in kinds.indexed)
                    if (kind.bytes > 0)
                      Expanded(
                        flex: max(1, (kind.bytes * 1000 / total).round()),
                        child: ColoredBox(color: palette[i % palette.length]),
                      ),
                ],
              ),
            ),
          ),
          const SizedBox(height: 8),
          Wrap(
            spacing: 16,
            runSpacing: 4,
            children: [
              for (final (i, kind) in kinds.indexed)
                Row(
                  mainAxisSize: MainAxisSize.min,
                  children: [
                    ColoredBox(
                      color: palette[i % palette.length],
                      child: const SizedBox.square(dimension: 10),
                    ),
                    const SizedBox(width: 6),
                    Text('${kind.label} ${kind.size}'),
                  ],
                ),
            ],
          ),
        ],
      ),
    );
  }
}
