import 'package:flutter/material.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// The monospace family for code, output and file content.
const monoFamily = 'monospace';
const monoFallback = ['DejaVu Sans Mono', 'Menlo', 'Consolas', 'Roboto Mono'];

/// The height of one line of [style] at the context's text scale.
double lineHeightOf(BuildContext context, TextStyle style) =>
    MediaQuery.textScalerOf(context).scale(style.fontSize ?? 14) *
    (style.height ?? 1.2);

/// A tool kind's icon.
IconData toolIcon(ToolKindView kind) => switch (kind) {
  ToolKindView.fileWrite => Icons.edit_document,
  ToolKindView.fileEdit => Icons.difference_outlined,
  ToolKindView.fileRead => Icons.description_outlined,
  ToolKindView.shell => Icons.terminal,
  ToolKindView.search => Icons.manage_search,
  ToolKindView.fetch => Icons.cloud_download_outlined,
  ToolKindView.subagent => Icons.account_tree_outlined,
  ToolKindView.other => Icons.build_outlined,
};

/// Colours that are not in the Material scheme: success, the TUI's
/// magenta for a denial, the agent working, the link banner's, picked
/// for the theme's brightness. Each text colour meets WCAG AA (4.5:1) on
/// the theme's surfaces, and each banner's text on its background.
@immutable
class SkyColors {
  const SkyColors._(this.scheme, this.dark);

  factory SkyColors.of(BuildContext context) {
    final theme = Theme.of(context);
    return SkyColors._(theme.colorScheme, theme.brightness == Brightness.dark);
  }

  final ColorScheme scheme;
  final bool dark;

  Color get success => dark ? const Color(0xFF81C995) : const Color(0xFF1E7B34);
  Color get warning => dark ? const Color(0xFFFDD663) : const Color(0xFF8A5A00);
  Color get denied => dark ? const Color(0xFFE3A1F0) : const Color(0xFF8E24AA);
  Color get running => scheme.primary;

  /// The agent at work: its state's label and the bar under the header.
  Color get working => dark ? const Color(0xFF81D4FA) : const Color(0xFF01579B);
  Color get muted => scheme.onSurfaceVariant;

  /// A tool state's colour (the TUI's: yellow, cyan, green, red, magenta,
  /// grey).
  Color tool(ToolState state) => switch (state) {
    ToolState.needsApproval => warning,
    ToolState.running => running,
    ToolState.done => success,
    ToolState.failed => scheme.error,
    ToolState.denied => denied,
    ToolState.cancelled || ToolState.noResult || ToolState.plain => muted,
  };

  /// An outgoing badge's colour.
  Color outgoing(OutgoingState state) => switch (state) {
    OutgoingState.queued => warning,
    OutgoingState.sending => running,
    OutgoingState.accepted => success,
    OutgoingState.failed => scheme.error,
  };

  /// A shell run's state (the TUI's: grey waiting, cyan running, green
  /// exit 0, red otherwise).
  Color shellRun(ShellRunKind state) => switch (state) {
    ShellRunKind.pending || ShellRunKind.queued => muted,
    ShellRunKind.running => running,
    ShellRunKind.succeeded => success,
    ShellRunKind.refused ||
    ShellRunKind.failed ||
    ShellRunKind.stopped => scheme.error,
    ShellRunKind.skipped => muted,
  };

  /// The agent's state, for the header's dot.
  Color agent(AgentState state) => switch (state) {
    AgentState.idle => muted,
    AgentState.working => working,
    AgentState.awaitingApproval => warning,
    AgentState.errored => scheme.error,
    AgentState.stopped => muted,
  };

  /// The link banner's background and text for [state]: online green,
  /// syncing blue, offline amber, refused red, unpaired plain. Dark mode
  /// keeps them deep so the banner does not glare.
  ({Color background, Color foreground}) link(LinkState state) =>
      switch (state) {
        LinkState.online =>
          dark
              ? (
                  background: const Color(0xFF1B5E20),
                  foreground: const Color(0xFFE8F5E9),
                )
              : (background: const Color(0xFF2E7D32), foreground: Colors.white),
        LinkState.connecting || LinkState.catchingUp =>
          dark
              ? (
                  background: const Color(0xFF01579B),
                  foreground: const Color(0xFFE1F5FE),
                )
              : (background: const Color(0xFF0277BD), foreground: Colors.white),
        LinkState.reconnecting =>
          dark
              ? (
                  background: const Color(0xFF4A3800),
                  foreground: const Color(0xFFFFE082),
                )
              : (background: const Color(0xFFFFB300), foreground: Colors.black),
        LinkState.repairNeeded ||
        LinkState.protocolMismatch ||
        LinkState.historyLost => (
          background: scheme.error,
          foreground: scheme.onError,
        ),
        LinkState.unpaired => (
          background: scheme.surfaceContainerHighest,
          foreground: scheme.onSurface,
        ),
      };

  /// A content line's text style over [base].
  TextStyle line(LineStyle style, TextStyle base) => switch (style) {
    LineStyle.plain => base,
    LineStyle.added => base.copyWith(
      color: success,
      backgroundColor: success.withValues(alpha: 0.10),
    ),
    LineStyle.removed => base.copyWith(
      color: scheme.error,
      backgroundColor: scheme.error.withValues(alpha: 0.10),
    ),
    LineStyle.fileHeader => base.copyWith(fontWeight: FontWeight.bold),
    LineStyle.hunk => base.copyWith(color: scheme.primary),
    LineStyle.error => base.copyWith(color: scheme.error),
  };
}

/// A small coloured label: a tool state, a delivery state.
class StateBadge extends StatelessWidget {
  const StateBadge({required this.label, required this.color, super.key});

  final String label;
  final Color color;

  @override
  Widget build(BuildContext context) {
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 2),
      decoration: BoxDecoration(
        color: color.withValues(alpha: 0.12),
        borderRadius: BorderRadius.circular(8),
      ),
      child: Text(
        label,
        style: Theme.of(context).textTheme.labelSmall
            ?.copyWith(color: color, fontWeight: FontWeight.w600),
        overflow: TextOverflow.ellipsis,
      ),
    );
  }
}
