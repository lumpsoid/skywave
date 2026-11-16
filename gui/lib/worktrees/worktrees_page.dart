import 'package:material_ui/material_ui.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/link/link_status.dart';
import 'package:skywave_gui/screen/readable_width.dart';
import 'package:skywave_gui/session/look.dart' show monoText;
import 'package:skywave_gui/sessions/remove_worktree_dialog.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// Worktrees: each repository the server knows, with its worktrees, their
/// branch and tags, and the sessions working in each; a session opens on tap.
/// Otherwise read-only, except Remove worktree on a worktree that can go; the
/// server still refuses a busy, dirty or unmerged one.
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

/// One worktree: its branch, path, tags and removal, with Remove worktree when
/// it can go. Its active sessions are listed; the archived ones sit behind a
/// fold, collapsed until asked for.
class _WorktreeCard extends StatefulWidget {
  const _WorktreeCard({required this.worktree, required this.send});

  final WorktreeView worktree;
  final SendIntent send;

  @override
  State<_WorktreeCard> createState() => _WorktreeCardState();
}

class _WorktreeCardState extends State<_WorktreeCard> {
  /// The archived sessions are listed.
  bool _expanded = false;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final muted = theme.textTheme.bodySmall?.copyWith(
      color: theme.colorScheme.onSurfaceVariant,
    );
    final worktree = widget.worktree;
    final remove = worktree.remove;
    return Card(
      margin: const EdgeInsets.fromLTRB(16, 8, 16, 0),
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Row(
              children: [
                Expanded(
                  child: Column(
                    crossAxisAlignment: CrossAxisAlignment.start,
                    children: [
                      Text(
                        worktree.branch,
                        style: theme.textTheme.bodyMedium?.merge(monoText),
                      ),
                      Text(
                        worktree.path,
                        style: muted?.merge(monoText),
                        maxLines: 1,
                        overflow: TextOverflow.ellipsis,
                      ),
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
                      if (worktree.removalLabel case final label?)
                        Text(label, style: muted),
                    ],
                  ),
                ),
                if (remove != null)
                  IconButton(
                    key: Key('remove-worktree-${remove.sessionId}'),
                    tooltip: 'Remove worktree',
                    icon: const Icon(Icons.folder_delete),
                    onPressed: () => _remove(context, remove),
                  ),
              ],
            ),
            for (final session in worktree.sessions)
              _SessionLine(
                session: session,
                send: widget.send,
                archived: false,
              ),
            if (worktree.archived.isNotEmpty)
              TextButton.icon(
                key: Key('archived-${worktree.path}'),
                onPressed: () => setState(() => _expanded = !_expanded),
                icon: Icon(_expanded ? Icons.expand_less : Icons.expand_more),
                label: Text('${worktree.archived.length} archived'),
              ),
            if (_expanded)
              for (final session in worktree.archived)
                _SessionLine(
                  session: session,
                  send: widget.send,
                  archived: true,
                ),
          ],
        ),
      ),
    );
  }

  Future<void> _remove(BuildContext context, WorktreeRemoveView remove) async {
    final confirmed = await confirmRemoveWorktree(
      context,
      title: remove.title,
      branch: widget.worktree.branch,
    );
    if (confirmed) {
      widget.send.act(ActionIntentRemoveWorktree(sessionId: remove.sessionId));
    }
  }
}

/// A session working in a worktree; a tap opens it. An archived one is drawn
/// muted, the fold above it already says so.
class _SessionLine extends StatelessWidget {
  const _SessionLine({
    required this.session,
    required this.send,
    required this.archived,
  });

  final WorktreeSessionView session;
  final SendIntent send;
  final bool archived;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final muted = theme.textTheme.bodySmall?.copyWith(
      color: theme.colorScheme.onSurfaceVariant,
    );
    return ListTile(
      key: Key('worktree-session-${session.sessionId}'),
      dense: true,
      contentPadding: EdgeInsets.zero,
      title: Text(
        session.title,
        style: archived ? muted : null,
        maxLines: 1,
        overflow: TextOverflow.ellipsis,
      ),
      onTap: () =>
          send.act(ActionIntentOpenSession(sessionId: session.sessionId)),
    );
  }
}
