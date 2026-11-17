import 'package:material_ui/material_ui.dart';
import 'package:skywave_gui/crop/blob_menu.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/link/link_status.dart';
import 'package:skywave_gui/screen/readable_width.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';
import 'package:skywave_gui/storage/storage_kind_bar.dart';

/// A ticked session row: the row and its session as Rust sent them.
typedef _Pick = (StorageRowView, StorageSessionView);

/// The storage hub: what the server and this device keep, each as a bar by
/// kind with its totals and a reclaim button, then the list (sessions,
/// largest blobs or shell runs, the biggest first). Rust asks the server
/// when the page opens; refresh asks again, and picking a list sends
/// `StorageShow`. A blob or shell run row has the blob's delete and crop
/// menu. On the sessions list each row can be ticked: the bulk actions act
/// on the ticked rows that Rust's flags allow, and the two deletes ask first.
/// Which rows are ticked is this page's own state; what may be done to a
/// session is Rust's.
class StoragePage extends StatefulWidget {
  const StoragePage({
    required this.screen,
    required this.send,
    this.canGoBack = false,
    super.key,
  });

  final ScreenViewStorage screen;
  final bool canGoBack;
  final SendIntent send;

  @override
  State<StoragePage> createState() => _StoragePageState();
}

class _StoragePageState extends State<StoragePage> {
  static const _groupings = {
    StorageGroupingView.sessions: 'By session',
    StorageGroupingView.blobs: 'Largest blobs',
    StorageGroupingView.shellRuns: 'Shell runs',
  };

  /// The session ids ticked on the sessions list.
  final _selected = <String>{};

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final scheme = theme.colorScheme;
    final muted = theme.textTheme.bodySmall?.copyWith(
      color: scheme.onSurfaceVariant,
    );
    final screen = widget.screen;
    final send = widget.send;
    final statusLabel = screen.statusLabel;
    final emptyLabel = screen.emptyLabel;
    final sessionIds = {
      for (final row in screen.rows)
        if (row.session case final session?) session.sessionId,
    };
    _selected.removeWhere((id) => !sessionIds.contains(id));
    final archivable = _picked((s) => s.here && !s.archived);
    final removable = _picked((s) => s.deletable);
    final everywhere = _picked((s) => s.deletableEverywhere);
    return Scaffold(
      appBar: AppBar(
        leading: backButton(
          canGoBack: widget.canGoBack,
          onBack: () => send.act(const ActionIntentBack()),
        ),
        toolbarHeight: LinkTitle.toolbarHeight(context),
        flexibleSpace: const LinkProgress(),
        title: LinkTitle(title: const Text('Storage'), send: send),
        actions: [
          ApprovalsButton(send: send),
          IconButton(
            tooltip: 'Refresh',
            icon: const Icon(Icons.refresh),
            onPressed: () => send.act(const ActionIntentRefreshStorage()),
          ),
        ],
      ),
      bottomNavigationBar: _selected.isEmpty
          ? null
          : Material(
              elevation: 3,
              child: SafeArea(
                top: false,
                child: Padding(
                  padding: const EdgeInsets.symmetric(
                    horizontal: 16,
                    vertical: 4,
                  ),
                  child: Wrap(
                    spacing: 8,
                    crossAxisAlignment: WrapCrossAlignment.center,
                    children: [
                      Text('${_selected.length} selected'),
                      TextButton(
                        key: const Key('storage-bulk-archive'),
                        onPressed: archivable.isEmpty
                            ? null
                            : () => _archive(archivable),
                        child: const Text('Archive'),
                      ),
                      TextButton(
                        key: const Key('storage-bulk-remove'),
                        onPressed: removable.isEmpty
                            ? null
                            : () => _removeFromDevice(context, removable),
                        child: const Text('Remove from this device'),
                      ),
                      TextButton(
                        key: const Key('storage-bulk-delete'),
                        onPressed: everywhere.isEmpty
                            ? null
                            : () => _deleteEverywhere(context, everywhere),
                        child: Text(
                          'Delete everywhere',
                          style: TextStyle(color: scheme.error),
                        ),
                      ),
                      TextButton(
                        key: const Key('storage-bulk-clear'),
                        onPressed: () => setState(_selected.clear),
                        child: const Text('Clear'),
                      ),
                    ],
                  ),
                ),
              ),
            ),
      body: ReadableWidth(
        child: ListView(
          children: [
            if (statusLabel != null)
              Padding(
                padding: const EdgeInsets.fromLTRB(16, 12, 16, 0),
                child: Text(statusLabel, style: muted),
              ),
            if (screen.totals.isNotEmpty || screen.serverReclaiming)
              _storeCard(
                context,
                header: 'Server',
                kinds: screen.serverKinds,
                totals: screen.totals,
                scope: 'server',
                reclaiming: screen.serverReclaiming,
                reclaimable: screen.serverReclaimable,
                reclaimAction: const ActionIntentReclaimServerSpace(),
              ),
            if (screen.deviceTotals.isNotEmpty ||
                screen.deviceStatusLabel != null ||
                screen.deviceReclaiming)
              _storeCard(
                context,
                header: 'This device',
                note: screen.deviceStatusLabel,
                kinds: screen.deviceKinds,
                totals: screen.deviceTotals,
                scope: 'device',
                reclaiming: screen.deviceReclaiming,
                reclaimable: screen.deviceReclaimable,
                reclaimAction: const ActionIntentReclaimDeviceSpace(),
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
                      key: Key('storage-grouping-${grouping.name}'),
                      label: Text(label),
                      selected: screen.grouping == grouping,
                      onSelected: (_) =>
                          send(GuiIntentStorageShow(grouping: grouping)),
                    ),
                ],
              ),
            ),
            if (emptyLabel != null)
              Padding(
                padding: const EdgeInsets.all(16),
                child: Text(emptyLabel, style: muted),
              ),
            for (final row in screen.rows)
              ListTile(
                leading: _select(row),
                title: Text(
                  row.label,
                  maxLines: 1,
                  overflow: TextOverflow.ellipsis,
                ),
                subtitle: Text(row.detail),
                trailing: Row(
                  mainAxisSize: MainAxisSize.min,
                  children: [
                    Text(
                      row.size,
                      style: const TextStyle(fontWeight: FontWeight.bold),
                    ),
                    if (row.target case final target?)
                      BlobMenu(target: target, send: send),
                  ],
                ),
              ),
          ],
        ),
      ),
    );
  }

  /// One store's card: its bar by kind, its totals, and its reclaim line
  /// (a spinner while Rust reclaims, else the button when there is room).
  Widget _storeCard(
    BuildContext context, {
    required String header,
    String? note,
    required List<StorageKindView> kinds,
    required List<StorageTotalView> totals,
    required String scope,
    required bool reclaiming,
    required String? reclaimable,
    required ActionIntent reclaimAction,
  }) {
    final theme = Theme.of(context);
    final muted = theme.textTheme.bodySmall?.copyWith(
      color: theme.colorScheme.onSurfaceVariant,
    );
    return Card(
      margin: const EdgeInsets.all(16),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Padding(
            padding: const EdgeInsets.fromLTRB(16, 16, 16, 0),
            child: Text(header, style: theme.textTheme.titleMedium),
          ),
          if (note != null)
            Padding(
              padding: const EdgeInsets.fromLTRB(16, 4, 16, 0),
              child: Text(note, style: muted),
            ),
          StorageKindBar(kinds: kinds),
          for (final (i, total) in totals.indexed)
            ListTile(
              dense: true,
              title: Text(
                total.label,
                style: TextStyle(fontWeight: i == 0 ? FontWeight.bold : null),
              ),
              trailing: Text(
                total.value,
                style: TextStyle(fontWeight: i == 0 ? FontWeight.bold : null),
              ),
            ),
          if (reclaiming)
            Padding(
              padding: const EdgeInsets.fromLTRB(16, 4, 16, 12),
              child: Row(
                key: Key('storage-reclaiming-$scope'),
                children: [
                  const SizedBox.square(
                    dimension: 16,
                    child: CircularProgressIndicator(strokeWidth: 2),
                  ),
                  const SizedBox(width: 8),
                  Text('Reclaiming…', style: muted),
                ],
              ),
            )
          else if (reclaimable case final r?)
            Padding(
              padding: const EdgeInsets.fromLTRB(16, 4, 16, 12),
              child: FilledButton.tonal(
                key: Key('storage-reclaim-$scope'),
                onPressed: () => widget.send.act(reclaimAction),
                child: Text('Reclaim $r'),
              ),
            ),
        ],
      ),
    );
  }

  /// The tick for a session row; a row without a session has none.
  Widget? _select(StorageRowView row) {
    final session = row.session;
    if (session == null) return null;
    final id = session.sessionId;
    return Checkbox(
      key: Key('storage-select-$id'),
      value: _selected.contains(id),
      onChanged: (on) => setState(() {
        if (on ?? false) {
          _selected.add(id);
        } else {
          _selected.remove(id);
        }
      }),
    );
  }

  /// The ticked rows whose session passes [allowed], in row order.
  List<_Pick> _picked(bool Function(StorageSessionView session) allowed) => [
    for (final row in widget.screen.rows)
      if (row.session case final session?
          when _selected.contains(session.sessionId) && allowed(session))
        (row, session),
  ];

  void _archive(List<_Pick> picks) {
    for (final (_, session) in picks) {
      widget.send.act(
        ActionIntentSetSessionArchived(
          sessionId: session.sessionId,
          archived: true,
        ),
      );
    }
    setState(_selected.clear);
  }

  Future<void> _removeFromDevice(
    BuildContext context,
    List<_Pick> picks,
  ) async {
    final left = _selected.length - picks.length;
    final yes = await confirmIrreversible(
      context,
      question: [
        'Remove ${_count(picks.length)} from this device for good? '
            'They stay on the server and other devices.',
        for (final (row, session) in picks)
          '${row.label} · ${session.deviceSize ?? row.size}',
        if (left > 0)
          "$left selected can't be removed yet (not archived, or still busy).",
      ].join('\n'),
      action: 'Remove',
    );
    if (!yes || !mounted) return;
    for (final (_, session) in picks) {
      widget.send.act(
        ActionIntentDeleteSessionLocally(sessionId: session.sessionId),
      );
    }
    setState(_selected.clear);
  }

  Future<void> _deleteEverywhere(
    BuildContext context,
    List<_Pick> picks,
  ) async {
    final left = _selected.length - picks.length;
    final yes = await confirmIrreversible(
      context,
      question: [
        'Delete ${_count(picks.length)} on the server and every device? '
            "This can't be undone.",
        for (final (row, _) in picks) '${row.label} · ${row.size}',
        if (left > 0)
          "$left selected can't be deleted everywhere yet "
              '(not archived, busy, or a worktree still on disk).',
      ].join('\n'),
      action: 'Delete',
    );
    if (!yes || !mounted) return;
    for (final (_, session) in picks) {
      widget.send.act(
        ActionIntentDeleteSessionEverywhere(sessionId: session.sessionId),
      );
    }
    setState(_selected.clear);
  }
}

String _count(int n) => n == 1 ? '1 session' : '$n sessions';
