import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/link/link_holder.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// The link banner across the top of every screen: Rust's label on the
/// state's colour, "Retry now" while reconnecting, and the approvals
/// waiting in other sessions as a button to the next one. Nothing while
/// unpaired: the pairing screen says what to do.
class LinkBanner extends StatelessWidget {
  const LinkBanner({required this.holder, required this.send, super.key});

  final LinkHolder holder;
  final SendIntent send;

  @override
  Widget build(BuildContext context) {
    return ListenableBuilder(
      listenable: holder,
      builder: (context, _) {
        final link = holder.link;
        if (link == null || link.label.isEmpty) return const SizedBox.shrink();
        final (:background, :foreground, :icon) = _look(link.state, context);
        final buttonStyle = TextButton.styleFrom(
          foregroundColor: foreground,
          visualDensity: VisualDensity.compact,
        );
        return Material(
          color: background,
          child: SafeArea(
            bottom: false,
            child: ConstrainedBox(
              constraints: const BoxConstraints(minHeight: 36),
              child: Padding(
                padding: const EdgeInsets.symmetric(horizontal: 12),
                child: Row(
                  children: [
                    Icon(icon, size: 18, color: foreground),
                    const SizedBox(width: 8),
                    Expanded(
                      child: Text(
                        link.label,
                        key: const Key('link-label'),
                        style: TextStyle(color: foreground),
                        overflow: TextOverflow.ellipsis,
                        maxLines: 2,
                      ),
                    ),
                    // Side by side when there is room, stacked on a phone.
                    if (link.canRetryNow || link.elsewhereLabel != null)
                      Flexible(
                        child: OverflowBar(
                          alignment: MainAxisAlignment.end,
                          overflowAlignment: OverflowBarAlignment.end,
                          children: [
                            if (link.canRetryNow)
                              TextButton(
                                style: buttonStyle,
                                onPressed: () =>
                                    send.act(const ActionIntentRetryNow()),
                                child: const Text('Retry now'),
                              ),
                            if (link.elsewhereLabel case final elsewhere?)
                              TextButton.icon(
                                style: buttonStyle,
                                icon: const Icon(Icons.notification_important),
                                onPressed: () => send.act(
                                  const ActionIntentOpenNextApproval(),
                                ),
                                label: Text(elsewhere),
                              ),
                          ],
                        ),
                      ),
                  ],
                ),
              ),
            ),
          ),
        );
      },
    );
  }

  /// The banner's colours and icon for each state: online green, syncing
  /// blue, offline amber, refused red.
  static ({Color background, Color foreground, IconData icon}) _look(
    LinkState state,
    BuildContext context,
  ) {
    final colors = Theme.of(context).colorScheme;
    return switch (state) {
      LinkState.online => (
        background: const Color(0xFF2E7D32),
        foreground: Colors.white,
        icon: Icons.cloud_done,
      ),
      LinkState.connecting || LinkState.catchingUp => (
        background: const Color(0xFF0277BD),
        foreground: Colors.white,
        icon: Icons.cloud_sync,
      ),
      LinkState.reconnecting => (
        background: const Color(0xFFFFB300),
        foreground: Colors.black,
        icon: Icons.cloud_off,
      ),
      LinkState.revoked || LinkState.protocolMismatch => (
        background: colors.error,
        foreground: colors.onError,
        icon: Icons.block,
      ),
      LinkState.unpaired => (
        background: colors.surfaceContainerHighest,
        foreground: colors.onSurface,
        icon: Icons.link_off,
      ),
    };
  }
}

/// Desktop shortcuts that work on every screen, as in the TUI: Ctrl+R
/// retries the link now, Ctrl+G opens the next approval elsewhere. Rust
/// ignores either when it does not apply.
class LinkShortcuts extends StatelessWidget {
  const LinkShortcuts({required this.send, required this.child, super.key});

  final SendIntent send;
  final Widget child;

  @override
  Widget build(BuildContext context) {
    return CallbackShortcuts(
      bindings: {
        const SingleActivator(LogicalKeyboardKey.keyR, control: true): () =>
            send.act(const ActionIntentRetryNow()),
        const SingleActivator(LogicalKeyboardKey.keyG, control: true): () =>
            send.act(const ActionIntentOpenNextApproval()),
      },
      child: Focus(autofocus: true, child: child),
    );
  }
}
