import 'package:material_ui/material_ui.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/link/link_status.dart';
import 'package:skywave_gui/screen/readable_width.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// Usage: cost and tokens across sessions over a span of days, by day,
/// session, directory or model. Rust totals them from the replica, so the
/// page works offline; picking a span or grouping sends `UsageShow`.
class UsagePage extends StatelessWidget {
  const UsagePage({
    required this.screen,
    required this.send,
    this.canGoBack = false,
    super.key,
  });

  final ScreenViewUsage screen;
  final bool canGoBack;
  final SendIntent send;

  static const _spans = {
    UsageSpanView.today: 'Today',
    UsageSpanView.week: '7 days',
    UsageSpanView.month: '30 days',
    UsageSpanView.all: 'All',
  };

  static const _groupings = {
    UsageGroupingView.day: 'By day',
    UsageGroupingView.session: 'By session',
    UsageGroupingView.workdir: 'By directory',
    UsageGroupingView.model: 'By model',
  };

  void _show({UsageSpanView? span, UsageGroupingView? grouping}) => send(
    GuiIntentUsageShow(
      span: span ?? screen.span,
      grouping: grouping ?? screen.grouping,
    ),
  );

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final muted = theme.textTheme.bodySmall?.copyWith(
      color: theme.colorScheme.onSurfaceVariant,
    );
    final emptyLabel = screen.emptyLabel;
    return Scaffold(
      appBar: AppBar(
        leading: backButton(
          canGoBack: canGoBack,
          onBack: () => send.act(const ActionIntentBack()),
        ),
        toolbarHeight: LinkTitle.toolbarHeight(context),
        flexibleSpace: const LinkProgress(),
        title: LinkTitle(title: const Text('Usage'), send: send),
        actions: [
          ApprovalsButton(send: send),
          IconButton(
            tooltip: 'Usage limits',
            icon: const Icon(Icons.speed),
            onPressed: () => send.act(
              const ActionIntentNavigate(route: RouteView.usageLimits),
            ),
          ),
        ],
      ),
      body: ReadableWidth(
        child: ListView(
          children: [
            Padding(
              padding: const EdgeInsets.fromLTRB(16, 12, 16, 0),
              child: Wrap(
                spacing: 8,
                runSpacing: 8,
                children: [
                  for (final MapEntry(key: span, value: label)
                      in _spans.entries)
                    ChoiceChip(
                      key: Key('usage-span-${span.name}'),
                      label: Text(label),
                      selected: screen.span == span,
                      onSelected: (_) => _show(span: span),
                    ),
                ],
              ),
            ),
            Padding(
              padding: const EdgeInsets.fromLTRB(16, 8, 16, 0),
              child: Wrap(
                spacing: 8,
                runSpacing: 8,
                children: [
                  for (final MapEntry(key: grouping, value: label)
                      in _groupings.entries)
                    ChoiceChip(
                      key: Key('usage-grouping-${grouping.name}'),
                      label: Text(label),
                      selected: screen.grouping == grouping,
                      onSelected: (_) => _show(grouping: grouping),
                    ),
                ],
              ),
            ),
            Card(
              margin: const EdgeInsets.all(16),
              child: _RowTile(row: screen.total, bold: true),
            ),
            if (emptyLabel != null)
              Padding(
                padding: const EdgeInsets.all(16),
                child: Text(emptyLabel, style: muted),
              ),
            for (final row in screen.rows) _RowTile(row: row),
            Padding(
              padding: const EdgeInsets.all(16),
              child: Text(screen.note, style: muted),
            ),
          ],
        ),
      ),
    );
  }
}

/// One day, session, directory or model: cost on the right, tokens and
/// turns beneath the label.
class _RowTile extends StatelessWidget {
  const _RowTile({required this.row, this.bold = false});

  final UsageRowView row;
  final bool bold;

  @override
  Widget build(BuildContext context) {
    final weight = bold ? FontWeight.bold : null;
    return ListTile(
      title: Text.rich(
        TextSpan(
          text: row.label,
          children: [
            if (row.archived)
              TextSpan(
                text: '  archived',
                style: TextStyle(
                  color: Theme.of(context).colorScheme.onSurfaceVariant,
                  fontWeight: FontWeight.normal,
                ),
              ),
          ],
        ),
        style: TextStyle(fontWeight: weight),
        maxLines: 1,
        overflow: TextOverflow.ellipsis,
      ),
      subtitle: Text('${row.tokensLabel} · ${row.turnsLabel}'),
      trailing: Text(row.costLabel, style: TextStyle(fontWeight: weight)),
    );
  }
}
