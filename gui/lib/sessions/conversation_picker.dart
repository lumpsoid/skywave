import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/session/look.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// The conversations the server's agent keeps, in a sheet over the
/// new-session prompt, to continue one in a new session. Built from state
/// like the prompt: it shows while Rust's `ConversationsView` is set and
/// goes once Rust clears it (one was picked, or it was closed).
///
/// A tap on a row continues it (`ConversationPick`); a row already running
/// in a session offers Open on that session instead. Refresh lists again.
/// Close, Esc, a tap outside or "Paste an id instead" close the sheet; the
/// prompt's paste field is under it.
class ConversationPicker extends StatefulWidget {
  const ConversationPicker({
    required this.picker,
    required this.send,
    super.key,
  });

  final ConversationsView picker;
  final SendIntent send;

  @override
  State<ConversationPicker> createState() => _ConversationPickerState();
}

class _ConversationPickerState extends State<ConversationPicker> {
  /// Taken from the prompt's fields under the sheet, so Esc closes the
  /// sheet and not the prompt.
  final _focus = FocusNode(debugLabel: 'conversation picker');

  @override
  void initState() {
    super.initState();
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (mounted) _focus.requestFocus();
    });
  }

  @override
  void dispose() {
    _focus.dispose();
    super.dispose();
  }

  SendIntent get send => widget.send;
  ConversationsView get picker => widget.picker;

  void _close() => send(const GuiIntentConversationsClose());

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final colors = SkyColors.of(context);
    final muted = theme.textTheme.bodySmall?.copyWith(color: colors.muted);
    return Stack(
      children: [
        ModalBarrier(
          color: Colors.black54,
          onDismiss: _close,
          semanticsLabel: 'Close the conversations',
        ),
        Align(
          alignment: Alignment.bottomCenter,
          child: CallbackShortcuts(
            bindings: {
              const SingleActivator(LogicalKeyboardKey.escape): _close,
            },
            child: Focus(
              focusNode: _focus,
              child: ConstrainedBox(
                constraints: BoxConstraints(
                  maxWidth: 720,
                  maxHeight: MediaQuery.sizeOf(context).height * 0.7,
                ),
                child: Material(
                  key: const Key('conversation-picker'),
                  color: theme.colorScheme.surfaceContainerLow,
                  borderRadius: const BorderRadius.vertical(
                    top: Radius.circular(28),
                  ),
                  clipBehavior: Clip.antiAlias,
                  child: SafeArea(
                    top: false,
                    child: Column(
                      mainAxisSize: MainAxisSize.min,
                      crossAxisAlignment: CrossAxisAlignment.stretch,
                      children: [
                        Padding(
                          padding: const EdgeInsets.fromLTRB(24, 16, 8, 0),
                          child: Row(
                            children: [
                              Expanded(
                                child: Text(
                                  'Continue a conversation from outside '
                                  'Skywave',
                                  style: theme.textTheme.titleMedium,
                                ),
                              ),
                              IconButton(
                                tooltip: 'List again',
                                icon: const Icon(Icons.refresh),
                                onPressed: () =>
                                    send(const GuiIntentConversationsOpen()),
                              ),
                              IconButton(
                                tooltip: 'Close',
                                icon: const Icon(Icons.close),
                                onPressed: _close,
                              ),
                            ],
                          ),
                        ),
                        if (picker.statusLabel case final status?)
                          Padding(
                            padding: const EdgeInsets.symmetric(horizontal: 24),
                            child: Text(status, style: muted),
                          ),
                        if (picker.emptyLabel case final empty?)
                          Padding(
                            padding: const EdgeInsets.fromLTRB(24, 16, 24, 8),
                            child: Text(
                              empty,
                              style: theme.textTheme.bodyMedium?.copyWith(
                                color: picker.unavailable
                                    ? colors.warning
                                    : colors.muted,
                              ),
                            ),
                          ),
                        Flexible(
                          child: ListView.builder(
                            shrinkWrap: true,
                            itemCount: picker.rows.length,
                            itemBuilder: (context, i) => _ConversationTile(
                              row: picker.rows[i],
                              send: send,
                            ),
                          ),
                        ),
                        Padding(
                          padding: const EdgeInsets.fromLTRB(16, 0, 16, 8),
                          child: Align(
                            alignment: Alignment.centerLeft,
                            child: TextButton(
                              onPressed: _close,
                              child: const Text('Paste an id instead'),
                            ),
                          ),
                        ),
                      ],
                    ),
                  ),
                ),
              ),
            ),
          ),
        ),
      ],
    );
  }
}

class _ConversationTile extends StatelessWidget {
  const _ConversationTile({required this.row, required this.send});

  final ConversationRowView row;
  final SendIntent send;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final colors = SkyColors.of(context);
    final small = theme.textTheme.bodySmall;
    return ListTile(
      contentPadding: const EdgeInsets.symmetric(horizontal: 24),
      title: Text(row.title, maxLines: 2, overflow: TextOverflow.ellipsis),
      subtitle: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text(
            row.detail,
            maxLines: 1,
            overflow: TextOverflow.ellipsis,
            style: small?.copyWith(color: colors.muted),
          ),
          if (row.linkLabel case final link?)
            Text(link, style: small?.copyWith(color: colors.success)),
        ],
      ),
      trailing: switch (row.sessionId) {
        final session? => TextButton(
          onPressed: () =>
              send.act(ActionIntentOpenSession(sessionId: session)),
          child: const Text('Open'),
        ),
        null => null,
      },
      onTap: () =>
          send(GuiIntentConversationPick(conversationId: row.conversationId)),
    );
  }
}
