import 'package:material_ui/material_ui.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/link/link_status.dart';
import 'package:skywave_gui/screen/readable_width.dart';
import 'package:skywave_gui/session/look.dart' show monoText;
import 'package:skywave_gui/sessions/remove_worktree_dialog.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// Worktrees: each repository the server knows, with its worktrees, their
/// branch and tags, and the sessions working in each. Read-only, except
/// Remove worktree on a session whose worktree can go; the server still
/// refuses a busy, dirty or unmerged one.
class WorktreesPage extends StatelessWidget {
  const WorktreesPage({
    required this.screen,
    required this.send,
    this.canGoBack = false,
    super.key,
  });

  final ScreenViewWorktrees screen;
  final bool canGoBack;
  final SendIntent send;

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
        title: LinkTitle(title: const Text('Worktrees'), send: send),
        actions: [
          ApprovalsButton(send: send),
          IconButton(
            tooltip: 'Refresh',
            icon: const Icon(Icons.refresh),
            onPressed: () => send.act(const ActionIntentRefreshWorktrees()),
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
            for (final repo in screen.repos) ...[
              Padding(
                padding: const EdgeInsets.fromLTRB(16, 16, 16, 0),
                child: Text(
                  repo.repo,
                  style: theme.textTheme.titleSmall,
                  overflow: TextOverflow.ellipsis,
                ),
              ),
              if (repo.note case final note?)
                Padding(
                  padding: const EdgeInsets.fromLTRB(16, 4, 16, 0),
                  child: Text(note, style: muted),
                ),
              for (final worktree in repo.worktrees)
                _WorktreeCard(worktree: worktree, send: send),
            ],
            if (emptyLabel != null)
              Padding(
                padding: const EdgeInsets.all(16),
                child: Text(emptyLabel, style: muted),
              ),
          ],
        ),
      ),
    );
  }
}

/// One worktree: its path, branch and tags, then its sessions.
class _WorktreeCard extends StatelessWidget {
  const _WorktreeCard({required this.worktree, required this.send});

  final WorktreeView worktree;
  final SendIntent send;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final muted = theme.textTheme.bodySmall?.copyWith(
      color: theme.colorScheme.onSurfaceVariant,
    );
    return Card(
      margin: const EdgeInsets.fromLTRB(16, 8, 16, 0),
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text(
              worktree.path,
              style: monoText,
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
            ),
            Text(worktree.branch, style: muted?.merge(monoText)),
            if (worktree.tags.isNotEmpty)
              Padding(
                padding: const EdgeInsets.only(top: 4),
                child: Wrap(
                  spacing: 6,
                  runSpacing: 4,
                  children: [
                    for (final tag in worktree.tags)
                      Chip(
                        label: Text(tag),
                        visualDensity: VisualDensity.compact,
                      ),
                  ],
                ),
              ),
            for (final session in worktree.sessions)
              _SessionLine(
                session: session,
                branch: worktree.branch,
                send: send,
              ),
          ],
        ),
      ),
    );
  }
}

/// A session working in a worktree, with Remove worktree when it can go.
class _SessionLine extends StatelessWidget {
  const _SessionLine({
    required this.session,
    required this.branch,
    required this.send,
  });

  final WorktreeSessionView session;
  final String branch;
  final SendIntent send;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final muted = theme.textTheme.bodySmall?.copyWith(
      color: theme.colorScheme.onSurfaceVariant,
    );
    return Padding(
      padding: const EdgeInsets.only(top: 8),
      child: Row(
        children: [
          Expanded(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text(
                  session.title,
                  maxLines: 1,
                  overflow: TextOverflow.ellipsis,
                ),
                Wrap(
                  spacing: 12,
                  children: [
                    if (session.archived) Text('archived', style: muted),
                    if (session.removalLabel case final label?)
                      Text(label, style: muted),
                  ],
                ),
              ],
            ),
          ),
          if (session.removable)
            IconButton(
              key: Key('remove-worktree-${session.sessionId}'),
              tooltip: 'Remove worktree',
              icon: const Icon(Icons.folder_delete),
              onPressed: () => _remove(context),
            ),
        ],
      ),
    );
  }

  Future<void> _remove(BuildContext context) async {
    final confirmed = await confirmRemoveWorktree(
      context,
      title: session.title,
      branch: branch,
    );
    if (confirmed) {
      send.act(ActionIntentRemoveWorktree(sessionId: session.sessionId));
    }
  }
}
