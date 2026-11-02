import 'package:flutter/material.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/screen/readable_width.dart';
import 'package:skywave_gui/sessions/create_dialog.dart';
import 'package:skywave_gui/sessions/rename_dialog.dart';
import 'package:skywave_gui/sessions/session_row.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// The session list, active or archived, with each row's labels as Rust
/// worded them; the sessions this device is creating; and the new-session
/// and rename prompts while Rust holds them open.
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
    return Stack(
      children: [
        Scaffold(
          appBar: AppBar(
            leading: backButton(
              canGoBack: canGoBack,
              onBack: () => send.act(const ActionIntentBack()),
            ),
            title: Text(screen.title),
            actions: [
              IconButton(
                tooltip: 'Usage',
                icon: const Icon(Icons.bar_chart),
                onPressed: () => send.act(
                  const ActionIntentNavigate(route: RouteView.usage),
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
                  itemCount: screen.rows.length,
                  separatorBuilder: (_, _) => const Divider(height: 1),
                  itemBuilder: (context, i) => SessionRow(
                    key: ValueKey(screen.rows[i].sessionId),
                    row: screen.rows[i],
                    send: send,
                  ),
                ),
                // Room for the floating button over the last row.
                const SliverToBoxAdapter(child: SizedBox(height: 88)),
              ],
            ),
          ),
        ),
        if (screen.create case final create?)
          CreateDialog(prompt: create, send: send),
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

class _Header extends StatelessWidget {
  const _Header(this.text);

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
        style: const TextStyle(fontFamily: 'monospace'),
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
