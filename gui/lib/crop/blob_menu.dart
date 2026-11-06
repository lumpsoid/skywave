import 'package:flutter/material.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// Asks [question] before something that can't be undone; true only when
/// the [action] button was pressed. Cancel, a tap outside and back are no.
Future<bool> confirmIrreversible(
  BuildContext context, {
  required String question,
  required String action,
}) async {
  final confirmed = await showDialog<bool>(
    context: context,
    builder: (context) => AlertDialog(
      content: Text(question),
      actions: [
        TextButton(
          key: const Key('confirm-cancel'),
          onPressed: () => Navigator.of(context).pop(false),
          child: const Text('Cancel'),
        ),
        FilledButton(
          key: const Key('confirm-ok'),
          onPressed: () => Navigator.of(context).pop(true),
          child: Text(action),
        ),
      ],
    ),
  );
  return confirmed ?? false;
}

enum _BlobChoice { crop, delete }

/// What may be done to an output kept as a blob, as Rust's [target] says:
/// a menu offering Crop (opens the crop page) and Delete (asks first),
/// or, while this device's own delete or crop of it is pending, how that
/// stands with a Discard while it can still be taken back.
class BlobMenu extends StatelessWidget {
  const BlobMenu({
    required this.target,
    required this.send,
    this.beforeCrop,
    super.key,
  });

  final BlobTargetView target;
  final SendIntent send;

  /// Run before the crop page opens: e.g. closing a dialog over the
  /// session, which would cover the page.
  final VoidCallback? beforeCrop;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    if (target.pendingNote case final note?) {
      return Wrap(
        spacing: 4,
        crossAxisAlignment: WrapCrossAlignment.center,
        children: [
          Text(
            note,
            style: theme.textTheme.labelSmall?.copyWith(
              color: theme.colorScheme.tertiary,
            ),
          ),
          if (target.discardId case final id?)
            TextButton(
              key: Key('blob-discard-${target.blobId}'),
              style: TextButton.styleFrom(
                visualDensity: VisualDensity.compact,
                tapTargetSize: MaterialTapTargetSize.shrinkWrap,
              ),
              onPressed: () => send.act(ActionIntentDiscard(commandId: id)),
              child: const Text('Discard'),
            ),
        ],
      );
    }
    final question = target.deleteQuestion;
    if (!target.canCrop && question == null) return const SizedBox.shrink();
    return PopupMenuButton<_BlobChoice>(
      key: Key('blob-menu-${target.blobId}'),
      tooltip: 'Delete or crop',
      icon: const Icon(Icons.more_horiz, size: 18),
      padding: EdgeInsets.zero,
      itemBuilder: (_) => [
        if (target.canCrop)
          const PopupMenuItem(
            key: Key('blob-crop'),
            value: _BlobChoice.crop,
            child: ListTile(
              leading: Icon(Icons.content_cut),
              title: Text('Crop…'),
            ),
          ),
        if (question != null)
          const PopupMenuItem(
            key: Key('blob-delete'),
            value: _BlobChoice.delete,
            child: ListTile(
              leading: Icon(Icons.delete_outline),
              title: Text('Delete…'),
            ),
          ),
      ],
      onSelected: (choice) async {
        switch (choice) {
          case _BlobChoice.crop:
            beforeCrop?.call();
            send.act(
              ActionIntentOpenCrop(
                blobId: target.blobId,
                sessionId: target.sessionId,
                holds: target.holds,
                title: target.title,
              ),
            );
          case _BlobChoice.delete:
            if (question == null) return;
            final yes = await confirmIrreversible(
              context,
              question: question,
              action: 'Delete',
            );
            if (yes) send.act(ActionIntentDeleteBlob(blobId: target.blobId));
        }
      },
    );
  }
}
