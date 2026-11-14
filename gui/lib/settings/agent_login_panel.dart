import 'dart:async';

import 'package:material_ui/material_ui.dart';
import 'package:flutter/services.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/session/look.dart' show monoText;
import 'package:skywave_gui/src/bindings/bindings.dart';
import 'package:url_launcher/url_launcher.dart';

/// Logging the server's agent in from this device, under Settings' agent
/// login tile: where the login stands in Rust's words
/// (`login::flow_lines`), the URL to sign in at with copy and open, the
/// field to paste the code shown there, and Retry, for a code that got no
/// answer, and Close, which forgets the login (`DismissAgentLogin`).
///
/// It sits on the Settings screen itself, so the core stays on Settings,
/// the only place it acts on the login intents. The pasted code lives in
/// this widget's state only: no restoration id, no draft, nothing stored.
/// It is cleared once the login takes no code (sent, started again, ended)
/// or shows another URL, and dropped with the widget when Settings is
/// left. Leaving does not dismiss the login: back on Settings the open
/// login is still there, waiting for a code.
class AgentLoginPanel extends StatefulWidget {
  const AgentLoginPanel({required this.login, required this.send, super.key});

  final AgentLoginView login;
  final SendIntent send;

  @override
  State<AgentLoginPanel> createState() => _AgentLoginPanelState();
}

class _AgentLoginPanelState extends State<AgentLoginPanel>
    with AutomaticKeepAliveClientMixin {
  final _code = TextEditingController();

  // Kept while Settings shows, scrolled away or not, so a code being
  // pasted is not lost to the list building lazily.
  @override
  bool get wantKeepAlive => true;

  /// The code belongs to the open login it was pasted for: it goes once
  /// that login takes no code (sent, started again, ended, dismissed) or
  /// another URL is shown. A code the core refused locally (blank, too
  /// long) leaves the login open, so the text stays to be fixed.
  @override
  void didUpdateWidget(AgentLoginPanel old) {
    super.didUpdateWidget(old);
    if (!widget.login.canFinish || widget.login.url != old.login.url) {
      _code.clear();
    }
  }

  @override
  void dispose() {
    _code
      ..clear()
      ..dispose();
    super.dispose();
  }

  void _finish() => widget.send.act(
    ActionIntentFinishAgentLogin(code: PastedCode(text: _code.text)),
  );

  void _dismiss() => widget.send.act(const ActionIntentDismissAgentLogin());

  @override
  Widget build(BuildContext context) {
    super.build(context);
    final login = widget.login;
    final theme = Theme.of(context);
    final failed = login.state == LoginFlowState.failed;
    final lineStyle = theme.textTheme.bodyMedium?.copyWith(
      color: failed ? theme.colorScheme.error : null,
    );
    final busy =
        login.state == LoginFlowState.starting ||
        login.state == LoginFlowState.finishing;
    if (login.lines.isEmpty && login.url == null && !login.canDismiss) {
      return const SizedBox.shrink();
    }
    return Padding(
      padding: const EdgeInsets.fromLTRB(72, 0, 16, 8),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          for (final line in login.lines) Text(line, style: lineStyle),
          if (busy) ...[
            const SizedBox(height: 8),
            const LinearProgressIndicator(),
          ],
          if (login.url case final url?) ...[
            const SizedBox(height: 8),
            SelectableText(url, key: const Key('login-url'), style: monoText),
            Row(
              children: [
                IconButton(
                  tooltip: 'Copy URL',
                  icon: const Icon(Icons.copy),
                  onPressed: () =>
                      unawaited(Clipboard.setData(ClipboardData(text: url))),
                ),
                IconButton(
                  tooltip: 'Open in browser',
                  icon: const Icon(Icons.open_in_browser),
                  onPressed: () => unawaited(_open(context, url)),
                ),
              ],
            ),
          ],
          if (login.canFinish)
            Row(
              children: [
                Expanded(
                  child: TextField(
                    key: const Key('login-code'),
                    controller: _code,
                    // A secret: no keyboard learns it, and nothing restores
                    // it.
                    autocorrect: false,
                    enableSuggestions: false,
                    enableIMEPersonalizedLearning: false,
                    obscureText: false,
                    keyboardType: TextInputType.visiblePassword,
                    decoration: const InputDecoration(
                      labelText: 'Code',
                      hintText: 'Paste the code shown',
                    ),
                    onSubmitted: (_) => _finish(),
                  ),
                ),
                const SizedBox(width: 8),
                FilledButton(
                  onPressed: _finish,
                  child: const Text('Send code'),
                ),
              ],
            ),
          if (login.canRetry || login.canDismiss)
            OverflowBar(
              spacing: 8,
              children: [
                if (login.canRetry)
                  FilledButton(
                    onPressed: () =>
                        widget.send.act(const ActionIntentRetryAgentLogin()),
                    child: const Text('Retry'),
                  ),
                if (login.canDismiss)
                  TextButton(onPressed: _dismiss, child: const Text('Close')),
              ],
            ),
        ],
      ),
    );
  }

  static Future<void> _open(BuildContext context, String url) async {
    final uri = Uri.tryParse(url);
    final opened =
        uri != null &&
        await launchUrl(
          uri,
          mode: LaunchMode.externalApplication,
        ).catchError((_) => false);
    if (!opened && context.mounted) {
      ScaffoldMessenger.maybeOf(context)
          ?.showSnackBar(SnackBar(content: Text('Could not open $url')));
    }
  }
}
