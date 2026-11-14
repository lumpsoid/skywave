import 'package:material_ui/material_ui.dart';
import 'package:skywave_gui/session/look.dart' show monoText;
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/link/link_holder.dart';
import 'package:skywave_gui/link/link_status.dart';
import 'package:skywave_gui/screen/readable_width.dart';
import 'package:skywave_gui/settings/agent_login_panel.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// Settings: the blob policy, Stay connected where offered (Android), and
/// the server and device this one is
/// paired as, the link to it with its round trip, the fingerprint to
/// compare with what `skywaved` prints, when the agent's login on the
/// server expires, with Log in where the server offers a login from a
/// device ([AgentLoginPanel] below it), and Re-pair. A paired server's tile opens its
/// addresses ([ServerAddressesPage]), to reach it at another without
/// re-pairing.
class SettingsPage extends StatelessWidget {
  const SettingsPage({
    required this.screen,
    required this.link,
    required this.send,
    this.canGoBack = false,
    super.key,
  });

  final ScreenViewSettings screen;
  final LinkHolder link;
  final bool canGoBack;
  final SendIntent send;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final muted = theme.textTheme.bodyMedium?.copyWith(
      color: theme.colorScheme.onSurfaceVariant,
    );
    return Scaffold(
      appBar: AppBar(
        leading: backButton(
          canGoBack: canGoBack,
          onBack: () => send.act(const ActionIntentBack()),
        ),
        toolbarHeight: LinkTitle.toolbarHeight(context),
        flexibleSpace: const LinkProgress(),
        title: LinkTitle(title: const Text('Settings'), send: send),
        actions: [ApprovalsButton(send: send)],
      ),
      body: ReadableWidth(
        child: ListView(
          children: [
            const _Section('Blobs'),
            Padding(
              padding: const EdgeInsets.symmetric(horizontal: 16),
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  Text(
                    'Blobs hold full tool inputs and outputs. Sessions sync '
                    'without them; workdir diffs and files always download '
                    'when opened.',
                    style: muted,
                  ),
                  const SizedBox(height: 12),
                  SegmentedButton<BlobPolicyView>(
                    key: const Key('blob-policy'),
                    segments: const [
                      ButtonSegment(
                        value: BlobPolicyView.onDemand,
                        icon: Icon(Icons.download_for_offline_outlined),
                        label: Text('On demand'),
                      ),
                      ButtonSegment(
                        value: BlobPolicyView.eager,
                        icon: Icon(Icons.downloading),
                        label: Text('Eager'),
                      ),
                    ],
                    selected: {screen.blobPolicy},
                    onSelectionChanged: (selected) => send.act(
                      ActionIntentSetBlobPolicy(
                        policy: switch (selected.first) {
                          BlobPolicyView.onDemand => BlobPolicyChoice.onDemand,
                          BlobPolicyView.eager => BlobPolicyChoice.eager,
                        },
                      ),
                    ),
                  ),
                  const SizedBox(height: 8),
                  // Always two lines tall, so switching modes doesn't move
                  // the page when one description wraps and the other not.
                  Stack(
                    children: [
                      const Text('\n'),
                      Text(
                        screen.blobPolicyLabel,
                        key: const Key('blob-policy-label'),
                        maxLines: 2,
                        overflow: TextOverflow.ellipsis,
                      ),
                    ],
                  ),
                ],
              ),
            ),
            if (screen.stayConnected case final on?) ...[
              const _Section('Background'),
              SwitchListTile(
                key: const Key('stay-connected'),
                secondary: const Icon(Icons.sync),
                title: const Text('Stay connected'),
                subtitle: const Text(
                  'Keeps the link open with the app in the background, for '
                  'approval alerts. Shows an ongoing notification.',
                ),
                value: on,
                onChanged: (value) =>
                    send(GuiIntentSetStayConnected(on: value)),
              ),
            ],
            const _Section('This device'),
            switch (screen.serverAddresses) {
              final addresses? => ListTile(
                key: const Key('server-addresses'),
                leading: const Icon(Icons.dns),
                title: const Text('Server'),
                subtitle: Text('${screen.server}\n$addresses'),
                trailing: const Icon(Icons.chevron_right),
                onTap: () => send.act(
                  const ActionIntentNavigate(route: RouteView.serverAddresses),
                ),
              ),
              null => ListTile(
                leading: const Icon(Icons.dns),
                title: const Text('Server'),
                subtitle: Text(screen.server),
              ),
            },
            ConnectionTile(holder: link),
            ListTile(
              leading: const Icon(Icons.devices),
              title: const Text('Device'),
              subtitle: SelectableText(screen.device),
            ),
            if (screen.fingerprint case final fingerprint?)
              ListTile(
                leading: const Icon(Icons.fingerprint),
                title: const Text('Server fingerprint'),
                subtitle: SelectableText(fingerprint, style: monoText),
              ),
            if (screen.agentLogin case final expiry?)
              ListTile(
                key: const Key('agent-login'),
                leading: const Icon(Icons.key),
                title: const Text('Agent login'),
                subtitle: Text(expiry),
                trailing: switch (screen.login) {
                  final login? when login.canLogIn => OutlinedButton(
                    key: const Key('agent-log-in'),
                    onPressed: login.canStart
                        ? () => send.act(const ActionIntentStartAgentLogin())
                        : null,
                    child: Text(
                      login.state == LoginFlowState.failed
                          ? 'Start again'
                          : 'Log in',
                    ),
                  ),
                  _ => null,
                },
              ),
            if (screen.login case final login?)
              AgentLoginPanel(
                key: const Key('agent-login-panel'),
                login: login,
                send: send,
              ),
            Padding(
              padding: const EdgeInsets.all(16),
              child: Align(
                alignment: Alignment.centerLeft,
                child: OutlinedButton.icon(
                  icon: const Icon(Icons.qr_code),
                  onPressed: () => send.act(
                    const ActionIntentNavigate(route: RouteView.pairing),
                  ),
                  label: const Text('Re-pair'),
                ),
              ),
            ),
          ],
        ),
      ),
    );
  }
}

/// The link to the server as Rust says it, `Connected · 12 ms` while
/// online. The latency changes on every ping, so only this tile listens.
class ConnectionTile extends StatelessWidget {
  const ConnectionTile({required this.holder, super.key});

  final LinkHolder holder;

  @override
  Widget build(BuildContext context) {
    return ListenableBuilder(
      listenable: holder,
      builder: (context, _) => ListTile(
        leading: Icon(
          holder.link?.state == LinkState.online ? Icons.link : Icons.link_off,
        ),
        title: const Text('Connection'),
        subtitle: Text(
          holder.link?.connectionLabel ?? 'Not known yet',
          key: const Key('connection-label'),
        ),
      ),
    );
  }
}

class _Section extends StatelessWidget {
  const _Section(this.text);

  final String text;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Padding(
      padding: const EdgeInsets.fromLTRB(16, 20, 16, 8),
      child: Text(
        text,
        style: theme.textTheme.titleSmall?.copyWith(
          color: theme.colorScheme.primary,
        ),
      ),
    );
  }
}
