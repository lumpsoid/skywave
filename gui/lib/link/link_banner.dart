import 'package:flutter/material.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/link/link_holder.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// The link banner: Rust's label, a retry while reconnecting, and the
/// approvals waiting elsewhere. A placeholder for G5's banner.
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
        return Material(
          color: Theme.of(context).colorScheme.surfaceContainerHighest,
          child: Row(
            children: [
              const SizedBox(width: 12),
              Expanded(child: Text(link.label)),
              if (link.canRetryNow)
                TextButton(
                  onPressed: () => send.act(const ActionIntentRetryNow()),
                  child: const Text('Retry now'),
                ),
              if (link.elsewhereLabel case final elsewhere?)
                TextButton(
                  onPressed: () =>
                      send.act(const ActionIntentOpenNextApproval()),
                  child: Text(elsewhere),
                ),
            ],
          ),
        );
      },
    );
  }
}
