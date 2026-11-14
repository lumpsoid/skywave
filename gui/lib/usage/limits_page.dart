import 'package:material_ui/material_ui.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/link/link_status.dart';
import 'package:skywave_gui/screen/readable_width.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// Usage limits: the limits of the account the server's agent runs as, one
/// row per limit with its use, a bar and when it resets, under when they
/// were read. Rust keeps the last ones read and shows them offline; it
/// asks on opening only when none were read, and refresh asks again.
class LimitsPage extends StatelessWidget {
  const LimitsPage({
    required this.screen,
    required this.send,
    this.canGoBack = false,
    super.key,
  });

  final ScreenViewUsageLimits screen;
  final bool canGoBack;
  final SendIntent send;

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(
        leading: backButton(
          canGoBack: canGoBack,
          onBack: () => send.act(const ActionIntentBack()),
        ),
        toolbarHeight: LinkTitle.toolbarHeight(context),
        flexibleSpace: const LinkProgress(),
        title: LinkTitle(title: const Text('Usage limits'), send: send),
        actions: [
          ApprovalsButton(send: send),
          IconButton(
            tooltip: 'Refresh',
            icon: const Icon(Icons.refresh),
            onPressed: () => send.act(const ActionIntentRefreshUsageLimits()),
          ),
        ],
      ),
      body: ReadableWidth(
        child: ListView(
          padding: const EdgeInsets.symmetric(vertical: 12),
          children: limitsChildren(context, screen.limits),
        ),
      ),
    );
  }
}

/// The limits as a list's children: where a read stands, when they were
/// read or why there are none, then one tile per limit. Shared by this
/// page and a session's info page.
List<Widget> limitsChildren(BuildContext context, LimitsView limits) {
  final theme = Theme.of(context);
  final muted = theme.textTheme.bodySmall?.copyWith(
    color: theme.colorScheme.onSurfaceVariant,
  );
  return [
    if (limits.statusLabel case final status?)
      Padding(
        padding: const EdgeInsets.fromLTRB(16, 0, 16, 8),
        child: Text(status, style: muted),
      ),
    if (limits.readLabel case final read?)
      Padding(
        padding: const EdgeInsets.fromLTRB(16, 0, 16, 8),
        child: Text(read, style: muted),
      ),
    if (limits.emptyLabel case final empty?)
      Padding(
        padding: const EdgeInsets.all(16),
        child: Text(empty, style: muted),
      ),
    for (final row in limits.rows) _LimitTile(row: row),
  ];
}

/// One limit: its label and use on a line, the bar, then when it resets.
class _LimitTile extends StatelessWidget {
  const _LimitTile({required this.row});

  final LimitRowView row;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final scheme = theme.colorScheme;
    final muted = theme.textTheme.bodySmall?.copyWith(
      color: scheme.onSurfaceVariant,
    );
    final color = row.warning ? scheme.error : scheme.primary;
    final fill = row.fillPercent;
    final resets = row.resetsLabel;
    return Padding(
      padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 10),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Row(
            children: [
              Expanded(
                child: Text(
                  row.label,
                  style: theme.textTheme.titleSmall,
                  maxLines: 1,
                  overflow: TextOverflow.ellipsis,
                ),
              ),
              if (row.percentLabel case final percent?)
                Text(
                  percent,
                  style: theme.textTheme.titleSmall?.copyWith(
                    color: row.warning ? scheme.error : null,
                  ),
                ),
            ],
          ),
          const SizedBox(height: 6),
          if (fill != null)
            ClipRRect(
              borderRadius: BorderRadius.circular(4),
              child: LinearProgressIndicator(
                value: fill / 100,
                minHeight: 8,
                color: color,
                backgroundColor: scheme.surfaceContainerHighest,
                semanticsLabel: row.label,
                semanticsValue: row.percentLabel,
              ),
            )
          else
            Text('use not reported', style: muted),
          if (resets != null) ...[
            const SizedBox(height: 4),
            Text(resets, style: muted),
          ],
        ],
      ),
    );
  }
}
