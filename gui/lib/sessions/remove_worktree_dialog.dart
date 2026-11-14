import 'package:material_ui/material_ui.dart';

/// Asks before removing the worktree of [title] and deleting its [branch],
/// and says whether the user confirmed. The server decides whether the
/// branch is merged and the worktree clean, and refuses if not; the dialog
/// only says so.
Future<bool> confirmRemoveWorktree(
  BuildContext context, {
  required String title,
  String? branch,
}) async {
  final branchLabel = switch (branch) {
    final b? => ' $b',
    null => '',
  };
  final confirmed = await showDialog<bool>(
    context: context,
    builder: (context) => AlertDialog(
      title: const Text('Remove worktree?'),
      content: Text(
        'Remove the worktree of $title and delete its branch$branchLabel? '
        'The server refuses if the branch is not merged or the worktree '
        'has changes.',
      ),
      actions: [
        TextButton(
          onPressed: () => Navigator.of(context).pop(false),
          child: const Text('Keep'),
        ),
        FilledButton(
          style: FilledButton.styleFrom(
            backgroundColor: Theme.of(context).colorScheme.error,
            foregroundColor: Theme.of(context).colorScheme.onError,
          ),
          onPressed: () => Navigator.of(context).pop(true),
          child: const Text('Remove'),
        ),
      ],
    ),
  );
  return confirmed ?? false;
}
