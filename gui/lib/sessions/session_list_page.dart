import 'package:flutter/material.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// The session list, active or archived: each row's labels as Rust worded
/// them, a tap opening it. A placeholder for G5's list.
class SessionListPage extends StatelessWidget {
  const SessionListPage({required this.screen, required this.send, super.key});

  final ScreenViewSessionList screen;
  final SendIntent send;

  @override
  Widget build(BuildContext context) {
    final rows = screen.rows;
    return Scaffold(
      appBar: AppBar(
        title: Text(screen.title),
        actions: [
          IconButton(
            tooltip: 'Settings',
            icon: const Icon(Icons.settings),
            onPressed: () =>
                send.act(const ActionIntentNavigate(route: RouteView.settings)),
          ),
        ],
      ),
      body: ListView(
        children: [
          if (screen.emptyLabel case final empty?) ListTile(title: Text(empty)),
          for (final row in rows)
            ListTile(
              key: ValueKey(row.sessionId),
              leading: Text(row.badgeLabel),
              title: Text(row.title),
              subtitle: Text(
                [
                  ?row.workdir,
                  ?row.approvalsLabel,
                  ?row.error,
                  ?row.costLabel,
                  row.lastActivityLabel,
                ].join(' · '),
              ),
              trailing: row.unread > 0 ? Text('${row.unread}') : null,
              onTap: () =>
                  send.act(ActionIntentOpenSession(sessionId: row.sessionId)),
            ),
          for (final creating in screen.creating)
            ListTile(
              title: Text(creating.workdir),
              subtitle: Text(
                [creating.outgoing.label, ?creating.note].join(' · '),
              ),
            ),
        ],
      ),
    );
  }
}
