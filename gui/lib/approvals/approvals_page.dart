import 'package:material_ui/material_ui.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/link/link_status.dart';
import 'package:skywave_gui/screen/readable_width.dart';
import 'package:skywave_gui/session/approval_panel.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// The approvals waiting in every session, oldest first: each one's session
/// title with Open, then its panel (answered here, Trust… included, with the
/// approval's own session) or the note that says where it is answered.
/// Rust sends the list; an answered approval leaves it on the next update.
class ApprovalsPage extends StatelessWidget {
  const ApprovalsPage({
    required this.screen,
    required this.send,
    this.canGoBack = false,
    super.key,
  });

  final ScreenViewApprovals screen;
  final bool canGoBack;
  final SendIntent send;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final items = screen.items;
    return Scaffold(
      appBar: AppBar(
        leading: backButton(
          canGoBack: canGoBack,
          onBack: () => send.act(const ActionIntentBack()),
        ),
        toolbarHeight: LinkTitle.toolbarHeight(context),
        flexibleSpace: const LinkProgress(),
        title: LinkTitle(title: const Text('Approvals'), send: send),
      ),
      body: ReadableWidth(
        child: items.isEmpty
            ? Center(
                child: Column(
                  key: const Key('approvals-empty'),
                  mainAxisSize: MainAxisSize.min,
                  children: [
                    const Icon(Icons.task_alt, size: 48),
                    const SizedBox(height: 8),
                    Text(
                      'Nothing waits for you',
                      style: theme.textTheme.bodyLarge,
                    ),
                  ],
                ),
              )
            : ListView.builder(
                padding: const EdgeInsets.symmetric(vertical: 8),
                itemCount: items.length,
                itemBuilder: (context, i) {
                  final item = items[i];
                  final panel = item.panel;
                  return Column(
                    key: ValueKey(item.approvalId),
                    crossAxisAlignment: CrossAxisAlignment.stretch,
                    children: [
                      Padding(
                        padding: const EdgeInsets.fromLTRB(16, 8, 16, 0),
                        child: Row(
                          children: [
                            Expanded(
                              child: Text(
                                item.sessionTitle,
                                style: theme.textTheme.titleSmall,
                                maxLines: 1,
                                overflow: TextOverflow.ellipsis,
                              ),
                            ),
                            TextButton.icon(
                              key: Key('approvals-open-${item.approvalId}'),
                              icon: const Icon(Icons.open_in_new, size: 18),
                              label: const Text('Open'),
                              onPressed: () => send.act(
                                ActionIntentOpenSession(
                                  sessionId: item.sessionId,
                                ),
                              ),
                            ),
                          ],
                        ),
                      ),
                      if (panel != null)
                        // A list gives its children unbounded height, and the
                        // panel's body needs a bound to scroll within.
                        ConstrainedBox(
                          constraints: const BoxConstraints(maxHeight: 420),
                          child: ApprovalPanelView(
                            key: ValueKey('panel-${item.approvalId}'),
                            approval: panel,
                            sessionId: item.sessionId,
                            send: send,
                          ),
                        )
                      else if (item.note case final note?)
                        Padding(
                          padding: const EdgeInsets.symmetric(
                            horizontal: 16,
                            vertical: 8,
                          ),
                          child: Text(
                            note,
                            style: theme.textTheme.bodyMedium?.copyWith(
                              color: theme.colorScheme.onSurfaceVariant,
                            ),
                          ),
                        ),
                    ],
                  );
                },
              ),
      ),
    );
  }
}
