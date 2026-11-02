import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/link/link_holder.dart';
import 'package:skywave_gui/session/look.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// The link banner across the top of every screen: Rust's label on the
/// state's colour, "Retry now" while reconnecting or after a protocol
/// mismatch (the server may have been updated since), and the approvals
/// waiting in other sessions as a button to the next one. Nothing while
/// Rust's label is empty: unpaired, or online with nothing waiting.
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
        final shown = link != null && link.label.isNotEmpty;
        // Slides in and out where the banner has no top inset to hand over:
        // the screen below takes the inset at once, so a phone's status bar
        // would get it twice for the length of the slide.
        final instant =
            MediaQuery.disableAnimationsOf(context) ||
            MediaQuery.paddingOf(context).top > 0;
        return AnimatedSwitcher(
          duration: instant ? Duration.zero : const Duration(milliseconds: 200),
          transitionBuilder: (child, animation) => SizeTransition(
            sizeFactor: animation,
            alignment: Alignment.topCenter,
            child: child,
          ),
          child: shown
              ? KeyedSubtree(
                  key: const ValueKey('link-banner'),
                  child: _banner(context, link),
                )
              : const SizedBox.shrink(key: ValueKey('no-link-banner')),
        );
      },
    );
  }

  Widget _banner(BuildContext context, LinkUpdate link) {
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
                            onPressed: () =>
                                send.act(const ActionIntentOpenNextApproval()),
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
  }

  /// The banner's colours (the theme's) and icon for each state.
  static ({Color background, Color foreground, IconData icon}) _look(
    LinkState state,
    BuildContext context,
  ) {
    final (:background, :foreground) = SkyColors.of(context).link(state);
    final icon = switch (state) {
      LinkState.online => Icons.cloud_done,
      LinkState.connecting || LinkState.catchingUp => Icons.cloud_sync,
      LinkState.reconnecting => Icons.cloud_off,
      LinkState.revoked || LinkState.protocolMismatch => Icons.block,
      LinkState.unpaired => Icons.link_off,
    };
    return (background: background, foreground: foreground, icon: icon);
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
