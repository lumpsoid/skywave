import 'package:flutter/material.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/session/look.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// Asks what to call a snippet about to be saved: the name, blank for
/// none, or `null` when cancelled.
Future<String?> askSnippetName(BuildContext context) => showDialog<String>(
  context: context,
  builder: (context) => const _NameDialog(),
);

/// Lists the snippets: a tap puts one into the composer at its cursor
/// (`onInserted` follows), the bin deletes one. While the composer holds
/// text, the first entry saves it as a snippet.
Future<void> pickSnippet(
  BuildContext context, {
  required String sessionId,
  required List<SnippetView> snippets,
  required bool composerBlank,
  required SendIntent send,
  required VoidCallback onInserted,
}) async {
  final picked = await showModalBottomSheet<_Picked>(
    context: context,
    showDragHandle: true,
    isScrollControlled: true,
    builder: (sheet) =>
        _SnippetSheet(snippets: snippets, canSave: !composerBlank, send: send),
  );
  if (!context.mounted) return;
  switch (picked) {
    case _Insert(:final snippetId):
      send(GuiIntentInsertSnippet(sessionId: sessionId, snippetId: snippetId));
      onInserted();
    case _SaveComposer():
      final name = await askSnippetName(context);
      if (name == null) return;
      send(GuiIntentSaveComposerSnippet(sessionId: sessionId, name: name));
    case null:
  }
}

sealed class _Picked {
  const _Picked();
}

class _Insert extends _Picked {
  const _Insert(this.snippetId);

  final String snippetId;
}

class _SaveComposer extends _Picked {
  const _SaveComposer();
}

class _SnippetSheet extends StatefulWidget {
  const _SnippetSheet({
    required this.snippets,
    required this.canSave,
    required this.send,
  });

  final List<SnippetView> snippets;
  final bool canSave;
  final SendIntent send;

  @override
  State<_SnippetSheet> createState() => _SnippetSheetState();
}

class _SnippetSheetState extends State<_SnippetSheet> {
  /// What the sheet lists: the snippets as it opened, less those deleted
  /// from it since.
  late final _shown = [...widget.snippets];

  void _delete(SnippetView snippet) {
    widget.send.act(ActionIntentDeleteSnippet(snippetId: snippet.snippetId));
    setState(() => _shown.remove(snippet));
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final colors = SkyColors.of(context);
    return SafeArea(
      child: ConstrainedBox(
        constraints: BoxConstraints(
          maxHeight: MediaQuery.sizeOf(context).height * 0.6,
        ),
        child: ListView(
          shrinkWrap: true,
          children: [
            Padding(
              padding: const EdgeInsets.fromLTRB(16, 0, 16, 8),
              child: Text('Snippets', style: theme.textTheme.titleMedium),
            ),
            if (widget.canSave)
              ListTile(
                leading: const Icon(Icons.bookmark_add_outlined),
                title: const Text('Save the message as a snippet'),
                onTap: () => Navigator.of(context).pop(const _SaveComposer()),
              ),
            if (_shown.isEmpty)
              Padding(
                padding: const EdgeInsets.fromLTRB(16, 8, 16, 16),
                child: Text(
                  'No snippets yet. Save a message you sent from its menu, '
                  'or what you are writing from here.',
                  style: theme.textTheme.bodyMedium?.copyWith(
                    color: colors.muted,
                  ),
                ),
              ),
            for (final snippet in _shown)
              ListTile(
                key: ValueKey(snippet.snippetId),
                leading: const Icon(Icons.bookmark_outline),
                title: Text(
                  snippet.label,
                  maxLines: 1,
                  overflow: TextOverflow.ellipsis,
                ),
                subtitle: snippet.detail == null
                    ? null
                    : Text(
                        snippet.detail!,
                        maxLines: 1,
                        overflow: TextOverflow.ellipsis,
                      ),
                trailing: IconButton(
                  tooltip: 'Delete snippet',
                  icon: const Icon(Icons.delete_outline),
                  onPressed: () => _delete(snippet),
                ),
                onTap: () =>
                    Navigator.of(context).pop(_Insert(snippet.snippetId)),
              ),
          ],
        ),
      ),
    );
  }
}

class _NameDialog extends StatefulWidget {
  const _NameDialog();

  @override
  State<_NameDialog> createState() => _NameDialogState();
}

class _NameDialogState extends State<_NameDialog> {
  final _name = TextEditingController();

  @override
  void dispose() {
    _name.dispose();
    super.dispose();
  }

  void _submit() => Navigator.of(context).pop(_name.text);

  @override
  Widget build(BuildContext context) {
    return AlertDialog(
      title: const Text('Save as snippet'),
      content: SizedBox(
        width: 420,
        child: TextField(
          key: const Key('snippet-name-field'),
          controller: _name,
          autofocus: true,
          decoration: const InputDecoration(
            labelText: 'Name (optional)',
            helperText: 'Empty shows its first line.',
          ),
          onSubmitted: (_) => _submit(),
        ),
      ),
      actions: [
        TextButton(
          onPressed: () => Navigator.of(context).pop(),
          child: const Text('Cancel'),
        ),
        FilledButton(onPressed: _submit, child: const Text('Save')),
      ],
    );
  }
}
