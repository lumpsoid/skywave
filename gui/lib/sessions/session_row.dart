import 'package:material_ui/material_ui.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/session/look.dart' hide StateBadge;
import 'package:skywave_gui/sessions/remove_worktree_dialog.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// An agent state's badge with Rust's label.
class StateBadge extends StatelessWidget {
  const StateBadge({required this.state, required this.label, super.key});

  final AgentState state;
  final String label;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final color = SkyColors.of(context).agent(state);
    return Container(
      constraints: const BoxConstraints(minWidth: 72),
      padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 2),
      decoration: BoxDecoration(
        border: Border.all(color: color),
        borderRadius: BorderRadius.circular(8),
      ),
      child: Text(
        label,
        textAlign: TextAlign.center,
        style: theme.textTheme.labelMedium?.copyWith(color: color),
      ),
    );
  }
}

/// One session on the list. A tap opens it; its menu starts a new session
/// in its workdir, renames, archives or unarchives, removes a removable
/// session's worktree, and deletes an archived one from this device after
/// asking.
/// On a phone the state is an icon whose shape says it without colour,
/// the title takes two lines, the workdir is its last component and the
/// menu is a sheet.
class SessionRow extends StatelessWidget {
  const SessionRow({required this.row, required this.send, super.key});

  final SessionRowView row;
  final SendIntent send;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final colors = theme.colorScheme;
    final muted = theme.textTheme.bodySmall?.copyWith(
      color: colors.onSurfaceVariant,
    );
    final renameFailed = row.rename?.state == OutgoingState.failed;
    final compact = isCompact(context);
    // A phone shows the project folder, which the path's end would hide.
    final workdir = compact ? row.workdirName : row.workdir;
    // A screen reader reads the row as one sentence, title first; the
    // parts it is drawn from stay out of it. The menu keeps its own node.
    return Semantics(
      label: rowSemanticsLabel(row),
      child: ListTile(
        leading: ExcludeSemantics(
          child: compact
              ? Tooltip(
                  message: row.badgeLabel,
                  child: AgentStateIcon(
                    state: row.state,
                    key: Key('row-state-${row.sessionId}'),
                  ),
                )
              : StateBadge(state: row.state, label: row.badgeLabel),
        ),
        title: ExcludeSemantics(
          child: Text.rich(
            TextSpan(
              children: [
                TextSpan(
                  text: row.title,
                  style: const TextStyle(fontWeight: FontWeight.w600),
                ),
                if (row.renameLabel case final rename?)
                  TextSpan(
                    text: '  $rename',
                    style: theme.textTheme.bodySmall?.copyWith(
                      color: renameFailed ? colors.error : colors.primary,
                    ),
                  ),
              ],
            ),
            maxLines: compact ? 2 : 1,
            overflow: TextOverflow.ellipsis,
          ),
        ),
        subtitle: ExcludeSemantics(
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              if (workdir != null)
                Text(
                  workdir,
                  maxLines: 1,
                  overflow: TextOverflow.ellipsis,
                  style: muted?.merge(monoText),
                ),
              Wrap(
                spacing: 12,
                crossAxisAlignment: WrapCrossAlignment.center,
                children: [
                  if (row.importingLabel case final importing?)
                    _Marker(
                      key: const Key('row-importing'),
                      icon: Icons.history,
                      text: importing,
                      semanticLabel: importing,
                      color: colors.primary,
                    ),
                  if (row.unread > 0)
                    _Marker(
                      key: const Key('row-unread'),
                      icon: Icons.circle,
                      text: '${row.unread}',
                      semanticLabel: _unreadLabel(row.unread),
                      color: colors.tertiary,
                      tooltip: 'Unread',
                    ),
                  if (row.approvalsLabel case final approvals?)
                    _Marker(
                      icon: Icons.notification_important,
                      text: approvals,
                      semanticLabel: _approvalsLabel(approvals),
                      color: SkyColors.of(context).warning,
                    ),
                  if (row.error case final error?)
                    _Marker(
                      icon: Icons.error_outline,
                      text: error,
                      semanticLabel: _errorLabel(error),
                      color: colors.error,
                    ),
                  if (row.hasDraft)
                    _Marker(
                      icon: Icons.edit_note,
                      text: 'draft',
                      semanticLabel: _draftLabel,
                      color: colors.onSurfaceVariant,
                    ),
                  if (row.worktreeRemoved)
                    _Marker(
                      key: const Key('row-worktree-removed'),
                      icon: Icons.folder_off,
                      text: 'worktree removed',
                      semanticLabel: _worktreeRemovedLabel,
                      color: colors.onSurfaceVariant,
                    ),
                  Text(
                    [?row.costLabel, row.lastActivityLabel].join(' · '),
                    style: muted,
                  ),
                ],
              ),
            ],
          ),
        ),
        isThreeLine: workdir != null,
        trailing: _RowMenu(row: row, send: send),
        onTap: () =>
            send.act(ActionIntentOpenSession(sessionId: row.sessionId)),
      ),
    );
  }
}

String _unreadLabel(int unread) => '$unread unread';
String _approvalsLabel(String approvals) => 'needs approval, $approvals';
String _errorLabel(String error) => 'agent error: $error';
const _draftLabel = 'draft saved';
const _worktreeRemovedLabel = 'worktree removed';

/// What a screen reader says for [row]: its title, a rename on its way,
/// its state (unless a marker says it), the markers, where it works, and
/// its cost and last activity.
String rowSemanticsLabel(SessionRowView row) {
  final rename = row.renameLabel;
  final saysState =
      !(row.state == AgentState.awaitingApproval &&
          row.approvalsLabel != null) &&
      !(row.state == AgentState.errored && row.error != null);
  return [
    row.title,
    if (rename != null) rename.startsWith('rename') ? rename : 'rename $rename',
    if (saysState) row.badgeLabel,
    ?row.importingLabel,
    if (row.unread > 0) _unreadLabel(row.unread),
    if (row.approvalsLabel case final approvals?) _approvalsLabel(approvals),
    if (row.error case final error?) _errorLabel(error),
    if (row.hasDraft) _draftLabel,
    if (row.worktreeRemoved) _worktreeRemovedLabel,
    ?row.workdir,
    ?row.costLabel,
    'last active ${row.lastActivityLabel}',
  ].join(', ');
}

class _Marker extends StatelessWidget {
  const _Marker({
    required this.icon,
    required this.text,
    required this.semanticLabel,
    required this.color,
    this.tooltip,
    super.key,
  });

  final IconData icon;
  final String text;

  /// What a screen reader says for it, in place of its icon and text.
  final String semanticLabel;
  final Color color;
  final String? tooltip;

  @override
  Widget build(BuildContext context) {
    final marker = Row(
      mainAxisSize: MainAxisSize.min,
      children: [
        Icon(icon, size: 12, color: color),
        const SizedBox(width: 4),
        Flexible(
          child: Text(
            text,
            maxLines: 1,
            overflow: TextOverflow.ellipsis,
            style: Theme.of(context).textTheme.bodySmall
                ?.copyWith(color: color, fontWeight: FontWeight.w600),
          ),
        ),
      ],
    );
    return Semantics(
      label: semanticLabel,
      excludeSemantics: true,
      child: tooltip == null
          ? marker
          : Tooltip(message: tooltip, child: marker),
    );
  }
}

/// A row action: an item of the menu, or a row of the phone's sheet.
class _RowAction {
  const _RowAction({
    required this.icon,
    required this.title,
    required this.onTap,
    this.destructive = false,
  });

  final IconData icon;
  final String title;
  final VoidCallback onTap;

  /// Drawn in the error colour in the sheet.
  final bool destructive;
}

class _RowMenu extends StatelessWidget {
  const _RowMenu({required this.row, required this.send});

  final SessionRowView row;
  final SendIntent send;

  @override
  Widget build(BuildContext context) {
    void archive() => send.act(
      ActionIntentSetSessionArchived(
        sessionId: row.sessionId,
        archived: !row.archived,
      ),
    );
    final actions = [
      if (row.workdir != null)
        _RowAction(
          icon: Icons.add,
          title: 'New session here',
          onTap: () =>
              send.act(ActionIntentCreateSessionLike(sessionId: row.sessionId)),
        ),
      _RowAction(
        icon: Icons.edit,
        title: 'Rename',
        onTap: () => send(GuiIntentRenameOpen(sessionId: row.sessionId)),
      ),
      if (row.archived)
        _RowAction(icon: Icons.unarchive, title: 'Unarchive', onTap: archive)
      else
        _RowAction(icon: Icons.archive, title: 'Archive', onTap: archive),
      if (row.removable)
        _RowAction(
          icon: Icons.folder_delete,
          title: 'Remove worktree',
          destructive: true,
          onTap: () => _removeWorktree(context),
        ),
      if (row.deletable || row.deleteBlockedLabel != null)
        _RowAction(
          icon: Icons.delete_forever,
          title: 'Delete from this device',
          destructive: true,
          onTap: () => _delete(context),
        ),
    ];
    if (isCompact(context)) {
      return IconButton(
        key: Key('row-menu-${row.sessionId}'),
        tooltip: 'Session actions',
        onPressed: () => _showActions(context, actions),
        icon: const Icon(Icons.more_vert),
      );
    }
    return PopupMenuButton<VoidCallback>(
      key: Key('row-menu-${row.sessionId}'),
      tooltip: 'Session actions',
      onSelected: (action) => action(),
      itemBuilder: (context) => [
        for (final action in actions)
          PopupMenuItem(
            value: action.onTap,
            child: ListTile(
              leading: Icon(action.icon),
              title: Text(action.title),
            ),
          ),
      ],
    );
  }

  /// The phone's sheet: the session's name and workdir in full, then
  /// [actions]. Each tap closes it first.
  void _showActions(BuildContext context, List<_RowAction> actions) {
    showModalBottomSheet<void>(
      context: context,
      showDragHandle: true,
      isScrollControlled: true,
      builder: (sheet) {
        final theme = Theme.of(sheet);
        final error = theme.colorScheme.error;
        return SafeArea(
          child: SingleChildScrollView(
            child: Column(
              mainAxisSize: MainAxisSize.min,
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: [
                Padding(
                  padding: const EdgeInsets.symmetric(horizontal: 24),
                  child: Column(
                    crossAxisAlignment: CrossAxisAlignment.start,
                    children: [
                      SelectableText(
                        row.title,
                        style: theme.textTheme.titleMedium,
                      ),
                      if (row.workdir case final workdir?)
                        SelectableText(
                          workdir,
                          style: theme.textTheme.bodySmall?.copyWith(
                            color: SkyColors.of(sheet).muted,
                          ),
                        ),
                    ],
                  ),
                ),
                const Divider(height: 24),
                for (final action in actions)
                  ListTile(
                    leading: Icon(
                      action.icon,
                      color: action.destructive ? error : null,
                    ),
                    title: Text(
                      action.title,
                      style: action.destructive
                          ? TextStyle(color: error)
                          : null,
                    ),
                    onTap: () {
                      Navigator.of(sheet).pop();
                      action.onTap();
                    },
                  ),
              ],
            ),
          ),
        );
      },
    );
  }

  /// Asks before deleting; or says why it cannot be deleted yet.
  void _delete(BuildContext context) {
    if (row.deleteBlockedLabel case final blocked? when !row.deletable) {
      showDialog<void>(
        context: context,
        builder: (context) => AlertDialog(
          title: const Text('Cannot delete yet'),
          content: Text(blocked),
          actions: [
            TextButton(
              onPressed: () => Navigator.of(context).pop(),
              child: const Text('OK'),
            ),
          ],
        ),
      );
      return;
    }
    showDialog<bool>(
      context: context,
      builder: (context) => AlertDialog(
        title: const Text('Delete from this device?'),
        content: Text(
          'Delete ${row.title} from this device for good? '
          'It stays on the server and other devices.',
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
            child: const Text('Delete'),
          ),
        ],
      ),
    ).then((confirmed) {
      if (confirmed ?? false) {
        send.act(ActionIntentDeleteSessionLocally(sessionId: row.sessionId));
      }
    });
  }

  void _removeWorktree(BuildContext context) {
    confirmRemoveWorktree(context, title: row.title, branch: row.branch).then((
      confirmed,
    ) {
      if (confirmed) {
        send.act(ActionIntentRemoveWorktree(sessionId: row.sessionId));
      }
    });
  }
}
