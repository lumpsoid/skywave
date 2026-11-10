import 'package:flutter/material.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/link/link_status.dart';
import 'package:skywave_gui/screen/readable_width.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';
import 'package:skywave_gui/usage/limits_page.dart';

/// A session's info page: the session (name, full workdir), the agent
/// account's usage limits, what the session's turns took, its agent's
/// model, effort and mode, its models, and every subagent it ran, each as
/// its timeline row reads. Rust words it all from the replica; only the
/// limits are asked for, and refresh asks again.
class InfoPage extends StatelessWidget {
  const InfoPage({
    required this.screen,
    required this.send,
    this.canGoBack = false,
    super.key,
  });

  final ScreenViewSessionInfo screen;
  final bool canGoBack;
  final SendIntent send;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final muted = theme.textTheme.bodySmall?.copyWith(
      color: theme.colorScheme.onSurfaceVariant,
    );
    final [session, ...rest] = screen.sections;
    return Scaffold(
      appBar: AppBar(
        leading: backButton(
          canGoBack: canGoBack,
          onBack: () => send.act(const ActionIntentBack()),
        ),
        toolbarHeight: LinkTitle.toolbarHeight(context),
        flexibleSpace: const LinkProgress(),
        title: LinkTitle(title: const Text('Session info'), send: send),
        actions: [
          ApprovalsButton(send: send),
          IconButton(
            tooltip: 'Refresh usage limits',
            icon: const Icon(Icons.refresh),
            onPressed: () => send.act(const ActionIntentRefreshUsageLimits()),
          ),
        ],
      ),
      body: ReadableWidth(
        child: ListView(
          padding: const EdgeInsets.only(bottom: 16),
          children: [
            _Section(section: session),
            const _Heading('Usage limits'),
            ...limitsChildren(context, screen.limits),
            for (final section in rest) _Section(section: section),
            _Heading('Subagents (${screen.subagents.length})'),
            if (screen.subagentsEmptyLabel case final empty?)
              Padding(
                padding: const EdgeInsets.symmetric(horizontal: 16),
                child: Text(empty, style: muted),
              ),
            for (final row in screen.subagents) _SubagentTile(row: row),
          ],
        ),
      ),
    );
  }
}

class _Heading extends StatelessWidget {
  const _Heading(this.title);

  final String title;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Padding(
      padding: const EdgeInsets.fromLTRB(16, 20, 16, 8),
      child: Text(
        title,
        style: theme.textTheme.titleSmall?.copyWith(
          color: theme.colorScheme.primary,
        ),
      ),
    );
  }
}

/// A section's heading, then a row per field: its label, and its value
/// whole and selectable (a workdir is copied from here). Labels share a
/// column; one too wide for it (a long model name) is never cut: it takes
/// its own line, its value beneath.
class _Section extends StatelessWidget {
  const _Section({required this.section});

  /// The label column's width, at the text scale of 1.
  static const labelWidth = 112.0;

  final InfoSectionView section;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final label = theme.textTheme.bodyMedium?.copyWith(
      color: theme.colorScheme.onSurfaceVariant,
    );
    final value = theme.textTheme.bodyMedium;
    final scaler = MediaQuery.textScalerOf(context);
    final column = scaler.scale(labelWidth);
    bool fits(String text) {
      final painter = TextPainter(
        text: TextSpan(text: text, style: label),
        textDirection: Directionality.of(context),
        textScaler: scaler,
        maxLines: 1,
      )..layout();
      final width = painter.width;
      painter.dispose();
      return width <= column;
    }

    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        _Heading(section.title),
        for (final field in section.fields)
          Padding(
            padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 4),
            child: fits(field.label)
                ? Row(
                    crossAxisAlignment: CrossAxisAlignment.start,
                    children: [
                      SizedBox(
                        width: column,
                        child: Text(field.label, style: label),
                      ),
                      const SizedBox(width: 8),
                      Expanded(
                        child: SelectableText(field.value, style: value),
                      ),
                    ],
                  )
                : Column(
                    crossAxisAlignment: CrossAxisAlignment.start,
                    children: [
                      SelectableText(field.label, style: label),
                      Padding(
                        padding: EdgeInsets.only(left: column + 8),
                        child: SelectableText(field.value, style: value),
                      ),
                    ],
                  ),
          ),
      ],
    );
  }
}

/// A subagent: where it ran and what it was, then how it stands.
class _SubagentTile extends StatelessWidget {
  const _SubagentTile({required this.row});

  final SubagentRowView row;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final title = [if (row.trail.isNotEmpty) row.trail, row.title].join(' › ');
    return ListTile(
      leading: Icon(
        Icons.smart_toy_outlined,
        color: row.live ? scheme.primary : scheme.onSurfaceVariant,
      ),
      title: Text(title, maxLines: 2, overflow: TextOverflow.ellipsis),
      subtitle: Text(
        [?row.detail, row.statusLabel].join(' · '),
        maxLines: 2,
        overflow: TextOverflow.ellipsis,
      ),
    );
  }
}
