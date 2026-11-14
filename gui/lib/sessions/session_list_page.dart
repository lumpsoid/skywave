import 'package:material_ui/material_ui.dart';
import 'package:skywave_gui/session/look.dart' show monoText;
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/link/link_status.dart';
import 'package:skywave_gui/screen/readable_width.dart';
import 'package:skywave_gui/sessions/conversation_picker.dart';
import 'package:skywave_gui/sessions/create_dialog.dart';
import 'package:skywave_gui/sessions/rename_dialog.dart';
import 'package:skywave_gui/sessions/session_row.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// The session list, active or archived, with each row's labels as Rust
/// worded them; the sessions this device is creating; and the new-session
/// and rename prompts and the conversation picker while Rust holds them
/// open.
class SessionListPage extends StatelessWidget {
  const SessionListPage({
    required this.screen,
    required this.send,
    this.canGoBack = false,
    super.key,
  });

  final ScreenViewSessionList screen;
  final bool canGoBack;
  final SendIntent send;

  @override
  Widget build(BuildContext context) {
    final active = screen.kind == ListKindView.active;
    final entries = _groupEntries(screen.rows);
    return Stack(
      children: [
        Scaffold(
          appBar: AppBar(
            leading: backButton(
              canGoBack: canGoBack,
              onBack: () => send.act(const ActionIntentBack()),
            ),
            toolbarHeight: LinkTitle.toolbarHeight(context),
            flexibleSpace: const LinkProgress(),
            title: LinkTitle(title: Text(screen.title), send: send),
            actions: [
              ApprovalsButton(send: send),
              IconButton(
                tooltip: 'Usage',
                icon: const Icon(Icons.bar_chart),
                onPressed: () => send.act(
                  const ActionIntentNavigate(route: RouteView.usage),
                ),
              ),
              IconButton(
                tooltip: 'Server storage',
                icon: const Icon(Icons.storage),
                onPressed: () => send.act(
                  const ActionIntentNavigate(route: RouteView.storage),
                ),
              ),
              IconButton(
                tooltip: 'Settings',
                icon: const Icon(Icons.settings),
                onPressed: () => send.act(
                  const ActionIntentNavigate(route: RouteView.settings),
                ),
              ),
            ],
          ),
          floatingActionButton: active
              ? FloatingActionButton.extended(
                  onPressed: () => send(const GuiIntentCreateOpen()),
                  icon: const Icon(Icons.add),
                  label: const Text('New session'),
                )
              : null,
          body: ReadableWidth(
            child: CustomScrollView(
              slivers: [
                SliverToBoxAdapter(
                  child: _KindToggle(screen: screen, send: send),
                ),
                if (screen.creating.isNotEmpty) ...[
                  const SliverToBoxAdapter(child: _Header('Creating')),
                  SliverList.list(
                    children: [
                      for (final creating in screen.creating)
                        _CreatingRow(
                          key: ValueKey(creating.commandId),
                          creating: creating,
                          dismissable: creating.commandId == screen.dismissable,
                          send: send,
                        ),
                    ],
                  ),
                  const SliverToBoxAdapter(child: Divider()),
                ],
                if (screen.emptyLabel case final empty?)
                  SliverToBoxAdapter(
                    child: Padding(
                      padding: const EdgeInsets.all(24),
                      child: Text(
                        empty,
                        textAlign: TextAlign.center,
                        style: Theme.of(context).textTheme.bodyLarge?.copyWith(
                          color: Theme.of(context).colorScheme.onSurfaceVariant,
                        ),
                      ),
                    ),
                  ),
                SliverList.separated(
                  itemCount: entries.length,
                  // A divider only between two rows; a header has none.
                  separatorBuilder: (_, i) =>
                      entries[i] is _RowEntry && entries[i + 1] is _RowEntry
                      ? const Divider(height: 1)
                      : const SizedBox.shrink(),
                  itemBuilder: (context, i) => switch (entries[i]) {
                    _GroupEntry(:final row) => _Header(
                      row.groupName,
                      key: ValueKey('group:${row.group}'),
                    ),
                    _RowEntry(:final row) => SessionRow(
                      key: ValueKey(row.sessionId),
                      row: row,
                      send: send,
                    ),
                  },
                ),
                // Room for the floating button over the last row.
                const SliverToBoxAdapter(child: SizedBox(height: 88)),
              ],
            ),
          ),
        ),
        if (screen.create case final create?)
          CreateDialog(prompt: create, send: send),
        if (screen.conversations case final picker?)
          ConversationPicker(picker: picker, send: send),
        if (screen.rename case final rename?)
          RenameDialog(
            key: ValueKey(rename.sessionId),
            prompt: rename,
            send: send,
          ),
      ],
    );
  }
}

/// Active or archived, with how many each holds.
class _KindToggle extends StatelessWidget {
  const _KindToggle({required this.screen, required this.send});

  final ScreenViewSessionList screen;
  final SendIntent send;

  @override
  Widget build(BuildContext context) {
    final count = screen.rows.length;
    final (active, archived) = switch (screen.kind) {
      ListKindView.active => (count, screen.otherCount),
      ListKindView.archived => (screen.otherCount, count),
    };
    return Padding(
      padding: const EdgeInsets.fromLTRB(16, 12, 16, 4),
      child: Align(
        alignment: Alignment.centerLeft,
        child: SegmentedButton<ListKindView>(
          showSelectedIcon: false,
          segments: [
            ButtonSegment(
              value: ListKindView.active,
              icon: const Icon(Icons.forum),
              label: Text('Active ($active)'),
            ),
            ButtonSegment(
              value: ListKindView.archived,
              icon: const Icon(Icons.archive),
              label: Text('Archived ($archived)'),
            ),
          ],
          selected: {screen.kind},
          onSelectionChanged: (selected) => send.act(
            ActionIntentNavigate(
              route: switch (selected.first) {
                ListKindView.active => RouteView.sessionList,
                ListKindView.archived => RouteView.archivedSessions,
              },
            ),
          ),
        ),
      ),
    );
  }
}

/// One line of the rows sliver: the header that opens a group, or a row.
sealed class _Entry {
  const _Entry(this.row);

  final SessionRowView row;
}

final class _GroupEntry extends _Entry {
  const _GroupEntry(super.row);
}

final class _RowEntry extends _Entry {
  const _RowEntry(super.row);
}

/// Each row, preceded by a header where its group starts. Rows of one group
/// are adjacent, so comparing `group` with the row before is enough.
List<_Entry> _groupEntries(List<SessionRowView> rows) => [
  for (var i = 0; i < rows.length; i++) ...[
    if (i == 0 || rows[i].group != rows[i - 1].group) _GroupEntry(rows[i]),
    _RowEntry(rows[i]),
  ],
];

class _Header extends StatelessWidget {
  const _Header(this.text, {super.key});

  final String text;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Padding(
      padding: const EdgeInsets.fromLTRB(16, 16, 16, 4),
      child: Text(
        text,
        style: theme.textTheme.titleSmall?.copyWith(
          color: theme.colorScheme.primary,
        ),
      ),
    );
  }
}

/// A session this device asked for that is not in the replica yet: how
/// the request stands, and Dismiss for a refused one.
class _CreatingRow extends StatelessWidget {
  const _CreatingRow({
    required this.creating,
    required this.dismissable,
    required this.send,
    super.key,
  });

  final CreatingView creating;
  final bool dismissable;
  final SendIntent send;

  @override
  Widget build(BuildContext context) {
    final colors = Theme.of(context).colorScheme;
    final failed = creating.outgoing.state == OutgoingState.failed;
    return ListTile(
      leading: failed
          ? Icon(Icons.error_outline, color: colors.error)
          : const SizedBox.square(
              dimension: 20,
              child: CircularProgressIndicator(strokeWidth: 2),
            ),
      title: Text(
        creating.workdir,
        style: monoText,
        overflow: TextOverflow.ellipsis,
      ),
      subtitle: Text(
        // The note says more than the bare state; a failure's note is
        // the outgoing label's reason without `failed: `.
        creating.note ?? creating.outgoing.label,
        style: failed ? TextStyle(color: colors.error) : null,
      ),
      trailing: dismissable
          ? TextButton(
              onPressed: () =>
                  send.act(ActionIntentDiscard(commandId: creating.commandId)),
              child: const Text('Dismiss'),
            )
          : null,
    );
  }
}
