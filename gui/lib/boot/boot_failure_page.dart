import 'package:flutter/material.dart';

/// Rust's boot failure with a Retry (worded [retryLabel]: "Restart" after
/// the hub crashed), and "Reset local data" when [onReset] is given (the
/// local data cannot be read, so a retry alone fails again).
class BootFailurePage extends StatelessWidget {
  const BootFailurePage({
    required this.message,
    required this.onRetry,
    this.retryLabel = 'Retry',
    this.onReset,
    super.key,
  });

  final String message;
  final VoidCallback onRetry;
  final String retryLabel;
  final VoidCallback? onReset;

  @override
  Widget build(BuildContext context) {
    final onReset = this.onReset;
    return Scaffold(
      body: Center(
        child: Padding(
          padding: const EdgeInsets.all(16),
          child: Column(
            mainAxisSize: MainAxisSize.min,
            children: [
              Text(message, textAlign: TextAlign.center),
              const SizedBox(height: 16),
              Wrap(
                spacing: 8,
                runSpacing: 8,
                alignment: WrapAlignment.center,
                children: [
                  ElevatedButton(onPressed: onRetry, child: Text(retryLabel)),
                  if (onReset != null)
                    OutlinedButton(
                      onPressed: onReset,
                      child: const Text('Reset local data'),
                    ),
                ],
              ),
              if (onReset != null) ...[
                const SizedBox(height: 8),
                const Text(
                  'Resetting syncs your sessions again from the server. '
                  'Pairing, settings, drafts and snippets stay; '
                  'messages not sent yet are lost.',
                  textAlign: TextAlign.center,
                ),
              ],
            ],
          ),
        ),
      ),
    );
  }
}
