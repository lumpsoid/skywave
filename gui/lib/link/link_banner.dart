import 'dart:async';

import 'package:material_ui/material_ui.dart';
import 'package:flutter/services.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/link/link_holder.dart';
import 'package:skywave_gui/session/look.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// The banner across the top of every screen, for what the user must act
/// on: the link stopped (Rust's label, on the error colours, with "Retry
/// now" after a protocol mismatch or a changed server certificate, and
/// "Reset local data" after a confirmation once the server lost
/// history), and below it the warnings that the agent is logged out or
/// that its login expires soon (amber, the expiry one dismissible).
/// Nothing otherwise: the link on its way back is the app bar's status
/// line (`LinkTitle`), approvals elsewhere its bell (`ApprovalsButton`).
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
        final shown = holder.bannerShown;
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
          child: shown && link != null
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
    final refused = link.label.isNotEmpty;
    final (:background, :foreground) = SkyColors.of(context)
        .banner(refused: refused);
    final buttonStyle = TextButton.styleFrom(
      foregroundColor: foreground,
      visualDensity: VisualDensity.compact,
    );
    final loginWarning = holder.loginWarning;
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
                Icon(
                  refused ? _icon(link.state) : Icons.warning_amber,
                  size: 18,
                  color: foreground,
                ),
                const SizedBox(width: 8),
                Expanded(
                  child: Column(
                    crossAxisAlignment: CrossAxisAlignment.start,
                    mainAxisSize: MainAxisSize.min,
                    children: [
                      if (refused)
                        Text(
                          link.label,
                          key: const Key('link-label'),
                          style: TextStyle(color: foreground),
                          overflow: TextOverflow.ellipsis,
                          maxLines: 2,
                        ),
                      for (final (key, warning) in [
                        ('logged-out-label', link.loggedOutLabel),
                        ('login-label', loginWarning),
                      ])
                        if (warning != null)
                          Text(
                            warning,
                            key: Key(key),
                            style: TextStyle(
                              color: foreground,
                              fontWeight: FontWeight.bold,
                            ),
                            overflow: TextOverflow.ellipsis,
                            maxLines: 2,
                          ),
                    ],
                  ),
                ),
                // Side by side when there is room, stacked on a phone.
                if ((link.canRetryNow && refused) || link.canResetReplica)
                  Flexible(
                    child: OverflowBar(
                      alignment: MainAxisAlignment.end,
                      overflowAlignment: OverflowBarAlignment.end,
                      children: [
                        if (link.canResetReplica)
                          TextButton(
                            style: buttonStyle,
                            onPressed: () => _confirmReset(context),
                            child: const Text('Reset local data'),
                          ),
                        if (link.canRetryNow && refused)
                          TextButton(
                            style: buttonStyle,
                            onPressed: () =>
                                send.act(const ActionIntentRetryNow()),
                            child: const Text('Retry now'),
                          ),
                      ],
                    ),
                  ),
                if (loginWarning != null)
                  IconButton(
                    key: const Key('login-dismiss'),
                    tooltip: 'Dismiss until tomorrow',
                    visualDensity: VisualDensity.compact,
                    color: foreground,
                    onPressed: holder.dismissLoginWarning,
                    icon: const Icon(Icons.close, size: 18),
                  ),
              ],
            ),
          ),
        ),
      ),
    );
  }

  /// Asks before `ResetReplica`: this device's copy is all that is left of
  /// what the server lost, and queued sends go with it.
  Future<void> _confirmReset(BuildContext context) async {
    final reset = await showDialog<bool>(
      context: context,
      builder: (context) => AlertDialog(
        title: const Text('Reset local data?'),
        content: const Text(
          'The server lost history this device still shows. Resetting '
          'replaces this device\'s copy, and anything still queued, with '
          'the server\'s log. Drafts and settings stay.',
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.pop(context, false),
            child: const Text('Cancel'),
          ),
          TextButton(
            onPressed: () => Navigator.pop(context, true),
            child: const Text('Reset'),
          ),
        ],
      ),
    );
    if (reset ?? false) send.act(const ActionIntentResetReplica());
  }

  /// The banner's icon for a link that stopped.
  static IconData _icon(LinkState state) => switch (state) {
    LinkState.historyLost => Icons.history_toggle_off,
    _ => Icons.block,
  };
}

/// Desktop shortcuts that work on every screen, as in the TUI: Ctrl+R
/// retries the link now, Ctrl+G opens the next approval elsewhere. Rust
/// ignores either when it does not apply. In the shell composer Ctrl+R opens
/// the history picker instead (D53). Escape is system back (ScreenNavigator's
/// PopScope), but never leaves the app.
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
        const SingleActivator(
          LogicalKeyboardKey.escape,
          includeRepeats: false,
        ): () =>
            unawaited(Navigator.maybePop(context)),
      },
      child: Focus(autofocus: true, child: child),
    );
  }
}
