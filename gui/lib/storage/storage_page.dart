import 'package:flutter/material.dart';
import 'package:skywave_gui/crop/blob_menu.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/link/link_status.dart';
import 'package:skywave_gui/screen/readable_width.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// Storage: what the server's data takes on its disk, with the free space,
/// and the sessions, largest blobs or shell runs taking the most. Rust asks
/// the server when the page opens; refresh asks again, and picking a list
/// sends `StorageShow`. A blob or shell run row has the blob's delete and
/// crop menu.
class StoragePage extends StatelessWidget {
  const StoragePage({
    required this.screen,
    required this.send,
    this.canGoBack = false,
    super.key,
  });

  final ScreenViewStorage screen;
  final bool canGoBack;
  final SendIntent send;

  static const _groupings = {
    StorageGroupingView.sessions: 'By session',
    StorageGroupingView.blobs: 'Largest blobs',
    StorageGroupingView.shellRuns: 'Shell runs',
  };

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final muted = theme.textTheme.bodySmall?.copyWith(
      color: theme.colorScheme.onSurfaceVariant,
    );
    final statusLabel = screen.statusLabel;
    final emptyLabel = screen.emptyLabel;
    return Scaffold(
      appBar: AppBar(
        leading: backButton(
          canGoBack: canGoBack,
          onBack: () => send.act(const ActionIntentBack()),
        ),
        toolbarHeight: LinkTitle.toolbarHeight(context),
        flexibleSpace: const LinkProgress(),
        title: LinkTitle(title: const Text('Server storage'), send: send),
        actions: [
          ApprovalsButton(send: send),
          IconButton(
            tooltip: 'Refresh',
            icon: const Icon(Icons.refresh),
            onPressed: () => send.act(const ActionIntentRefreshStorage()),
          ),
        ],
      ),
      body: ReadableWidth(
        child: ListView(
          children: [
            if (statusLabel != null)
              Padding(
                padding: const EdgeInsets.fromLTRB(16, 12, 16, 0),
                child: Text(statusLabel, style: muted),
              ),
            if (screen.totals.isNotEmpty)
              Card(
                margin: const EdgeInsets.all(16),
                child: Column(
                  children: [
                    for (final (i, total) in screen.totals.indexed)
                      ListTile(
                        dense: true,
                        title: Text(
                          total.label,
                          style: TextStyle(
                            fontWeight: i == 0 ? FontWeight.bold : null,
                          ),
                        ),
                        trailing: Text(
                          total.value,
                          style: TextStyle(
                            fontWeight: i == 0 ? FontWeight.bold : null,
                          ),
                        ),
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
}
