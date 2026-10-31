import 'dart:math';

import 'package:flutter/material.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/session/agent_pickers.dart';
import 'package:skywave_gui/session/look.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// The session's status under its header: the agent's state, an
/// interrupt or stop on its way, the agent's mode and model (a tap
/// switches them; a refusal turns them red), the rules granted here (a
/// tap lists them), and the usage summary at the end, its fields dropped
/// from the front when the line is short of room.
class StatusLine extends StatelessWidget {
  const StatusLine({required this.screen, required this.send, super.key});

  final SessionScreen screen;
  final SendIntent send;

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
    final settings = screen.agent;

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

        final chips = [
          _Chip(
            icon: Icons.shield_outlined,
            label: settings.modeLabel,
            refusal: settings.modeRefusal,
            tooltip: 'Permission mode',
            onTap: () => pickMode(context, screen, send),
          ),
          _Chip(
            icon: Icons.memory,
            label: settings.modelLabel ?? 'model',
            refusal: settings.modelRefusal,
            tooltip: 'Model',
            onTap: () => pickModel(context, screen, send),
          ),
          if (rules != null)
            _Chip(
              icon: Icons.verified_outlined,
              label: rules,
              tooltip: 'Rules',
              onTap: () => _showRules(context),
            ),
        ];
        // A chip takes its label plus the gap, its padding and icon (44);
        // a label longer than its share of the line is cut short.
        final labelRoom = (constraints.maxWidth - 32) / chips.length - 44;
        final chipsWidth = chips.fold(
          0.0,
          (sum, chip) =>
              sum +
              min(width(TextSpan(text: chip.label, style: style)), labelRoom) +
              44,
        );
        final leftWidth = width(TextSpan(children: left)) + chipsWidth;
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
              for (final chip in chips) ...[
                const SizedBox(width: 8),
                ConstrainedBox(
                  constraints: BoxConstraints(maxWidth: max(0, labelRoom) + 36),
                  child: chip,
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

/// A tappable label on the status line: primary, or the error colour
/// with the refusal as its tooltip.
class _Chip extends StatelessWidget {
  const _Chip({
    required this.icon,
    required this.label,
    required this.tooltip,
    required this.onTap,
    this.refusal,
  });

  final IconData icon;
  final String label;
  final String tooltip;
  final VoidCallback onTap;

  /// Why the agent refused the last switch.
  final String? refusal;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final color = refusal == null
        ? theme.colorScheme.primary
        : theme.colorScheme.error;
    return Tooltip(
      message: refusal ?? tooltip,
      child: InkWell(
        borderRadius: BorderRadius.circular(8),
        onTap: onTap,
        child: Padding(
          padding: const EdgeInsets.symmetric(horizontal: 6, vertical: 2),
          child: Row(
            mainAxisSize: MainAxisSize.min,
            children: [
              Icon(
                refusal == null ? icon : Icons.error_outline,
                size: 14,
                color: color,
              ),
              const SizedBox(width: 4),
              Flexible(
                child: Text(
                  label,
                  style: theme.textTheme.labelMedium!.copyWith(color: color),
                  maxLines: 1,
                  softWrap: false,
                  overflow: TextOverflow.ellipsis,
                ),
              ),
            ],
          ),
        ),
      ),
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
