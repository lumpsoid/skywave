import 'package:material_ui/material_ui.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/session/look.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// Asks what to call a snippet about to be saved: the name, blank for
/// none, or `null` when cancelled.
Future<String?> askSnippetName(BuildContext context) => showDialog<String>(
  context: context,
  builder: (context) => const _NameDialog(),
);

/// Lists the snippets as `snippets` holds them now: a tap puts one into
/// the composer at its cursor (`onInserted` follows), the pencil edits
/// one, the bin deletes one. While the composer holds text, the first
/// entry saves it as a snippet. In `shell` mode they are the saved
/// commands (the hub sends only those), and a long press runs one at
/// once, leaving the composer as it is.
Future<void> pickSnippet(
  BuildContext context, {
  required String sessionId,
  required ValueNotifier<List<SnippetView>> snippets,
  required bool composerBlank,
  required SendIntent send,
  required VoidCallback onInserted,
  bool shell = false,
}) async {
  final picked = await showModalBottomSheet<_Picked>(
    context: context,
    showDragHandle: true,
    isScrollControlled: true,
    builder: (sheet) => _SnippetSheet(
      snippets: snippets,
      canSave: !composerBlank,
      shell: shell,
      send: send,
    ),
  );
  if (!context.mounted) return;
  switch (picked) {
    case _Insert(:final snippetId):
      send(GuiIntentInsertSnippet(sessionId: sessionId, snippetId: snippetId));
      onInserted();
    case _Run(:final snippetId):
      send(GuiIntentRunSnippet(sessionId: sessionId, snippetId: snippetId));
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

class _Run extends _Picked {
  const _Run(this.snippetId);

  final String snippetId;
}

class _SaveComposer extends _Picked {
  const _SaveComposer();
}

class _SnippetSheet extends StatelessWidget {
  const _SnippetSheet({
    required this.snippets,
    required this.canSave,
    required this.shell,
    required this.send,
  });

  final ValueNotifier<List<SnippetView>> snippets;
  final bool canSave;
  final bool shell;
  final SendIntent send;

  Future<void> _edit(BuildContext context, SnippetView snippet) async {
    final edited = await showDialog<(String, String)>(
      context: context,
      builder: (context) => _EditDialog(snippet: snippet),
    );
    if (edited case (final name, final text)) {
      send.act(
        ActionIntentEditSnippet(
          snippetId: snippet.snippetId,
          name: name,
          text: text,
        ),
      );
    }
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
        child: ValueListenableBuilder(
          valueListenable: snippets,
          builder: (context, shown, _) => ListView(
            shrinkWrap: true,
            children: [
              Padding(
                padding: const EdgeInsets.fromLTRB(16, 0, 16, 8),
                child: Text(
                  shell ? 'Saved commands' : 'Snippets',
                  style: theme.textTheme.titleMedium,
                ),
              ),
              if (shell && shown.isNotEmpty)
                Padding(
                  padding: const EdgeInsets.fromLTRB(16, 0, 16, 8),
                  child: Text(
                    'Tap to put one in, long-press to run it now.',
                    style: theme.textTheme.bodySmall?.copyWith(
                      color: colors.muted,
                    ),
                  ),
                ),
              if (canSave)
                ListTile(
                  leading: const Icon(Icons.bookmark_add_outlined),
                  title: Text(
                    shell
                        ? 'Save the command'
                        : 'Save the message as a snippet',
                  ),
                  onTap: () => Navigator.of(context).pop(const _SaveComposer()),
                ),
              if (shown.isEmpty)
                Padding(
                  padding: const EdgeInsets.fromLTRB(16, 8, 16, 16),
                  child: Text(
                    shell
                        ? 'No saved commands yet. Save what you are writing '
                              'from here.'
                        : 'No snippets yet. Save a message you sent from its '
                              'menu, or what you are writing from here.',
                    style: theme.textTheme.bodyMedium?.copyWith(
                      color: colors.muted,
                    ),
                  ),
                ),
              for (final snippet in shown)
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
                  trailing: Row(
                    mainAxisSize: MainAxisSize.min,
                    children: [
                      IconButton(
                        tooltip: 'Edit snippet',
                        icon: const Icon(Icons.edit_outlined),
                        onPressed: () => _edit(context, snippet),
                      ),
                      IconButton(
                        tooltip: 'Delete snippet',
                        icon: const Icon(Icons.delete_outline),
                        onPressed: () => send.act(
                          ActionIntentDeleteSnippet(
                            snippetId: snippet.snippetId,
                          ),
                        ),
                      ),
                    ],
                  ),
                  onTap: () =>
                      Navigator.of(context).pop(_Insert(snippet.snippetId)),
                  onLongPress: shell
                      ? () => Navigator.of(context).pop(_Run(snippet.snippetId))
                      : null,
                ),
            ],
          ),
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

/// Edits a snippet's name and text: pops both, or `null` when cancelled.
/// Blank text cannot be saved.
class _EditDialog extends StatefulWidget {
  const _EditDialog({required this.snippet});

  final SnippetView snippet;

  @override
  State<_EditDialog> createState() => _EditDialogState();
}

class _EditDialogState extends State<_EditDialog> {
  late final _name = TextEditingController(text: widget.snippet.name ?? '');
  late final _text = TextEditingController(text: widget.snippet.text);

  @override
  void dispose() {
    _name.dispose();
    _text.dispose();
    super.dispose();
  }

  void _submit() => Navigator.of(context).pop((_name.text, _text.text));

  @override
  Widget build(BuildContext context) {
    return AlertDialog(
      title: const Text('Edit snippet'),
      content: SizedBox(
        width: 520,
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            TextField(
              key: const Key('snippet-edit-name'),
              controller: _name,
              decoration: const InputDecoration(
                labelText: 'Name (optional)',
                helperText: 'Empty shows its first line.',
              ),
            ),
            const SizedBox(height: 12),
            TextField(
              key: const Key('snippet-edit-text'),
              controller: _text,
              autofocus: true,
              minLines: 3,
              maxLines: 10,
              keyboardType: TextInputType.multiline,
              decoration: const InputDecoration(
                labelText: 'Text',
                border: OutlineInputBorder(),
              ),
            ),
          ],
        ),
      ),
      actions: [
        TextButton(
          onPressed: () => Navigator.of(context).pop(),
          child: const Text('Cancel'),
        ),
        ListenableBuilder(
          listenable: _text,
          builder: (context, _) => FilledButton(
            onPressed: _text.text.trim().isEmpty ? null : _submit,
            child: const Text('Save'),
          ),
        ),
      ],
    );
  }
}
