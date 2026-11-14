import 'package:material_ui/material_ui.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/screen/readable_width.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// The link stopped for a reason only pairing again fixes (revoked, the
/// server's CA or names changed, a damaged identity): a full-screen notice
/// over whatever screen was showing, saying which. Re-pair, try again
/// where a fix on the server may do, or browse the local data, which
/// stays readable.
class RepairPage extends StatelessWidget {
  const RepairPage({required this.notice, required this.send, super.key});

  final RepairNotice notice;
  final SendIntent send;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Scaffold(
      body: SafeArea(
        child: Center(
          child: SingleChildScrollView(
            padding: const EdgeInsets.all(24),
            child: ReadableWidth(
              maxWidth: 480,
              child: Column(
                mainAxisSize: MainAxisSize.min,
                crossAxisAlignment: CrossAxisAlignment.stretch,
                children: [
                  Icon(Icons.block, size: 56, color: theme.colorScheme.error),
                  const SizedBox(height: 16),
                  Text(
                    notice.title,
                    style: theme.textTheme.headlineSmall,
                    textAlign: TextAlign.center,
                  ),
                  const SizedBox(height: 12),
                  Text(
                    notice.body,
                    style: theme.textTheme.bodyLarge,
                    textAlign: TextAlign.center,
                  ),
                  const SizedBox(height: 24),
                  FilledButton.icon(
                    icon: const Icon(Icons.qr_code),
                    onPressed: () => send.act(
                      const ActionIntentNavigate(route: RouteView.pairing),
                    ),
                    label: const Text('Re-pair'),
                  ),
                  if (notice.canRetry) ...[
                    const SizedBox(height: 8),
                    OutlinedButton.icon(
                      icon: const Icon(Icons.refresh),
                      onPressed: () => send.act(const ActionIntentRetryNow()),
                      label: const Text('Try again'),
                    ),
                  ],
                  const SizedBox(height: 8),
                  OutlinedButton.icon(
                    icon: const Icon(Icons.folder_open),
                    onPressed: () => send(const GuiIntentRepairDismiss()),
                    label: const Text('Browse local data'),
                  ),
                ],
              ),
            ),
          ),
        ),
      ),
    );
  }
}
