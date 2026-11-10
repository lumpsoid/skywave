import 'package:flutter/material.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/link/link_holder.dart';
import 'package:skywave_gui/link/link_status.dart';
import 'package:skywave_gui/screen/readable_width.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// Server addresses: where the paired server is reached. A field on top
/// connects to a new address; under it, the one in use with the link's
/// state, then the others the server was reached by, one tap to switch
/// back and one to forget. Only the address changes; the pairing stays.
class ServerAddressesPage extends StatefulWidget {
  const ServerAddressesPage({
    required this.screen,
    required this.link,
    required this.send,
    this.canGoBack = false,
    super.key,
  });

  final ScreenViewServerAddresses screen;
  final LinkHolder link;
  final bool canGoBack;
  final SendIntent send;

  @override
  State<ServerAddressesPage> createState() => _ServerAddressesPageState();
}

class _ServerAddressesPageState extends State<ServerAddressesPage> {
  final _address = TextEditingController();

  /// The address last sent, cleared from the field once Rust shows it in
  /// use; one it refused stays there to correct.
  String? _sent;

  @override
  void didUpdateWidget(ServerAddressesPage old) {
    super.didUpdateWidget(old);
    if (_sent != null && widget.screen.current == _sent) {
      _sent = null;
      _address.clear();
    }
  }

  @override
  void dispose() {
    _address.dispose();
    super.dispose();
  }

  void _connect() {
    final address = _address.text.trim();
    if (address.isEmpty) return;
    _sent = address;
    widget.send.act(ActionIntentSetServerAddress(address: address));
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final muted = theme.textTheme.bodyMedium?.copyWith(
      color: theme.colorScheme.onSurfaceVariant,
    );
    final send = widget.send;
    return Scaffold(
      appBar: AppBar(
        leading: backButton(
          canGoBack: widget.canGoBack,
          onBack: () => send.act(const ActionIntentBack()),
        ),
        toolbarHeight: LinkTitle.toolbarHeight(context),
        flexibleSpace: const LinkProgress(),
        title: LinkTitle(title: const Text('Server addresses'), send: send),
        actions: [ApprovalsButton(send: send)],
      ),
      body: ReadableWidth(
        child: Column(
          children: [
            Padding(
              padding: const EdgeInsets.fromLTRB(16, 16, 16, 8),
              child: Row(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  Expanded(
                    child: TextField(
                      key: const Key('server-address-field'),
                      controller: _address,
                      keyboardType: TextInputType.url,
                      decoration: const InputDecoration(
                        labelText: 'host:port',
                        helperText:
                            'The same server at another address; no '
                            're-pairing.',
                      ),
                      onSubmitted: (_) => _connect(),
                    ),
                  ),
                  const SizedBox(width: 12),
                  Padding(
                    padding: const EdgeInsets.only(top: 8),
                    child: FilledButton(
                      key: const Key('connect-address'),
                      onPressed: _connect,
                      child: const Text('Connect'),
                    ),
                  ),
                ],
              ),
            ),
            Expanded(
              child: ListView(
                children: [
                  ListTile(
                    key: const Key('current-address'),
                    leading: const Icon(Icons.check_circle),
                    title: SelectableText(widget.screen.current),
                    subtitle: ListenableBuilder(
                      listenable: widget.link,
                      builder: (context, _) => Text(
                        widget.link.link?.connectionLabel ?? 'Not known yet',
                      ),
                    ),
                  ),
                  for (final address in widget.screen.others)
                    ListTile(
                      key: Key('switch-address-$address'),
                      leading: const Icon(Icons.radio_button_unchecked),
                      title: Text(address),
                      onTap: () => send.act(
                        ActionIntentSetServerAddress(address: address),
                      ),
                      trailing: IconButton(
                        key: Key('forget-address-$address'),
                        icon: const Icon(Icons.close),
                        tooltip: 'Forget this address',
                        onPressed: () => send.act(
                          ActionIntentForgetServerAddress(address: address),
                        ),
                      ),
                    ),
                  if (widget.screen.others.isEmpty)
                    Padding(
                      padding: const EdgeInsets.all(16),
                      child: Text(
                        'An address you switch away from is kept here, '
                        'to switch back in one tap.',
                        style: muted,
                      ),
                    ),
                ],
              ),
            ),
          ],
        ),
      ),
    );
  }
}
