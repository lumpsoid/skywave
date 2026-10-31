import 'package:flutter/material.dart';
import 'package:skywave_gui/session/look.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// The session's status under its header: the agent's state, an
/// interrupt or stop on its way, the rules granted here (a tap lists
/// them), and the usage summary at the end, its fields dropped from the
/// front when the line is short of room.
class StatusLine extends StatelessWidget {
  const StatusLine({required this.screen, super.key});

  final SessionScreen screen;

  void _showRules(BuildContext context) {
    showDialog<void>(
      context: context,
      builder: (context) => AlertDialog(
        title: Text(screen.rulesLabel ?? 'Rules'),
        content: SingleChildScrollView(
          child: Column(
            mainAxisSize: MainAxisSize.min,
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              for (final rule in screen.rules)
                ListTile(
                  dense: true,
                  contentPadding: EdgeInsets.zero,
                  leading: const Icon(Icons.verified_outlined),
                  title: Text(rule),
                ),
            ],
          ),
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.of(context).pop(),
            child: const Text('Close'),
          ),
        ],
      ),
    );
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final colors = SkyColors.of(context);
    final style = theme.textTheme.labelMedium!;
    final muted = style.copyWith(color: colors.muted);
    final agent = style.copyWith(
      color: colors.agent(screen.state),
      fontWeight: FontWeight.w600,
    );
    final rules = screen.rulesLabel;

    final left = <InlineSpan>[
      TextSpan(text: screen.agentLabel, style: agent),
      if (screen.interruptLabel case final label?)
        TextSpan(text: ' · $label', style: muted),
      if (screen.stopLabel case final label?)
        TextSpan(text: ' · $label', style: muted),
    ];

    return LayoutBuilder(
      builder: (context, constraints) {
        final scaler = MediaQuery.textScalerOf(context);
        final direction = Directionality.of(context);
        double width(InlineSpan span) {
          final painter = TextPainter(
            text: span,
            textDirection: direction,
            textScaler: scaler,
            maxLines: 1,
          )..layout();
          final w = painter.width;
          painter.dispose();
          return w;
        }

        // The rules chip takes its label plus its padding and icon.
        final rulesWidth = rules == null
            ? 0.0
            : width(TextSpan(text: rules, style: style)) + 44;
        final leftWidth = width(TextSpan(children: left)) + rulesWidth;
        final usage = fitTail(
          screen.usage,
          (text) => width(TextSpan(text: text, style: muted)),
          // Less the line's padding (32), the gaps, and a margin.
          constraints.maxWidth - leftWidth - 64,
        );

        return Padding(
          padding: const EdgeInsets.fromLTRB(16, 2, 16, 6),
          child: Row(
            children: [
              Flexible(
                child: Text.rich(
                  TextSpan(children: left),
                  maxLines: 1,
                  overflow: TextOverflow.ellipsis,
                ),
              ),
              if (rules != null) ...[
                const SizedBox(width: 8),
                InkWell(
                  borderRadius: BorderRadius.circular(8),
                  onTap: () => _showRules(context),
                  child: Padding(
                    padding: const EdgeInsets.symmetric(
                      horizontal: 6,
                      vertical: 2,
                    ),
                    child: Row(
                      mainAxisSize: MainAxisSize.min,
                      children: [
                        Icon(
                          Icons.verified_outlined,
                          size: 14,
                          color: theme.colorScheme.primary,
                        ),
                        const SizedBox(width: 4),
                        Text(
                          rules,
                          style: style.copyWith(
                            color: theme.colorScheme.primary,
                          ),
                        ),
                      ],
                    ),
                  ),
                ),
              ],
              const Spacer(),
              if (usage.isNotEmpty)
                Text(usage, style: muted, maxLines: 1, softWrap: false),
            ],
          ),
        );
      },
    );
  }
}

/// The last of [fields], joined with ` · `, that fit in [room]: fields
/// drop from the front. Empty when not even the last fits.
String fitTail(
  List<String> fields,
  double Function(String) width,
  double room,
) {
  for (var from = 0; from < fields.length; from++) {
    final text = fields.sublist(from).join(' · ');
    if (width(text) <= room) return text;
  }
  return '';
}
