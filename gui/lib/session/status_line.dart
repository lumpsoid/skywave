import 'dart:math';

import 'package:flutter/material.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/session/agent_pickers.dart';
import 'package:skywave_gui/session/look.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// The session's status under its header: the agent's state, a stop on
/// its way, the agent's mode and model (a tap switches them; a refusal
/// turns them red), the rules granted here (a tap lists them), and the
/// usage summary at the end, its fields dropped from the front when the
/// line is short of room.
class StatusLine extends StatelessWidget {
  const StatusLine({required this.screen, required this.send, super.key});

  final SessionScreen screen;
  final SendIntent send;

  /// The line's height at the context's text scale: one line of its
  /// labels and a margin (16), at least a chip's hit area.
  static double heightOf(BuildContext context) => max(
    kMinInteractiveDimension,
    (lineHeightOf(context, Theme.of(context).textTheme.labelMedium!) + 16)
        .ceilToDouble(),
  );

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

  /// The chips' settings in full, one per row, for a line too narrow to
  /// show them: a tap opens that setting's picker.
  void _showSettings(BuildContext context, List<_Chip> chips) {
    showModalBottomSheet<void>(
      context: context,
      showDragHandle: true,
      builder: (sheet) {
        final error = Theme.of(sheet).colorScheme.error;
        return SafeArea(
          child: Column(
            mainAxisSize: MainAxisSize.min,
            children: [
              for (final chip in chips)
                ListTile(
                  leading: Icon(
                    chip.refusal == null ? chip.icon : Icons.error_outline,
                    color: chip.refusal == null ? null : error,
                  ),
                  title: Text(chip.tooltip),
                  subtitle: Text.rich(
                    TextSpan(
                      children: [
                        TextSpan(text: chip.label),
                        if (chip.refusal case final refusal?)
                          TextSpan(
                            text: '\n$refusal',
                            style: TextStyle(color: error),
                          ),
                      ],
                    ),
                  ),
                  onTap: () {
                    Navigator.of(sheet).pop();
                    chip.onTap();
                  },
                ),
            ],
          ),
        );
      },
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

        double chipWidth(_Chip chip, double room) => max(
          kMinInteractiveDimension,
          min(width(TextSpan(text: chip.label, style: style)), room) +
              _Chip.chrome,
        );

        var chips = [
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
        // Each chip's share of the line, less its gap (8) and chrome; a
        // line too narrow for every label in full folds the chips into
        // one that lists them.
        final line = constraints.maxWidth - 32;
        var labelRoom = line / chips.length - 8 - _Chip.chrome;
        if (chips.any(
          (chip) => width(TextSpan(text: chip.label, style: style)) > labelRoom,
        )) {
          final all = chips;
          chips = [
            _Chip(
              icon: Icons.tune,
              label: all.map((chip) => chip.label).join(' · '),
              refusal: settings.modeRefusal ?? settings.modelRefusal,
              tooltip: 'Agent settings',
              onTap: () => _showSettings(context, all),
            ),
          ];
          labelRoom = line / 2 - 8 - _Chip.chrome;
        }
        final chipsWidth = chips.fold(
          0.0,
          (sum, chip) => sum + chipWidth(chip, labelRoom) + 8,
        );
        final leftWidth = width(TextSpan(children: left)) + chipsWidth;
        final usage = fitTail(
          screen.usage,
          (text) => width(TextSpan(text: text, style: muted)),
          // Less the line's padding (32), the gap, and a margin.
          constraints.maxWidth - leftWidth - 56,
        );

        return Padding(
          padding: const EdgeInsets.symmetric(horizontal: 16),
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
                  constraints: BoxConstraints(
                    maxWidth: max(0, labelRoom) + _Chip.chrome,
                  ),
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
/// with the refusal as its tooltip. It looks compact but takes taps over
/// a full interactive square around it.
class _Chip extends StatelessWidget {
  const _Chip({
    required this.icon,
    required this.label,
    required this.tooltip,
    required this.onTap,
    this.refusal,
  });

  /// Its padding (6 a side), icon (14) and gap (4) around the label.
  static const chrome = 30.0;

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
        child: ConstrainedBox(
          constraints: const BoxConstraints(
            minWidth: kMinInteractiveDimension,
            minHeight: kMinInteractiveDimension,
          ),
          child: Padding(
            padding: const EdgeInsets.symmetric(horizontal: 6),
            child: Row(
              mainAxisSize: MainAxisSize.min,
              mainAxisAlignment: MainAxisAlignment.center,
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
