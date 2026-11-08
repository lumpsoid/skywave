import 'package:flutter/material.dart';
import 'package:skywave_gui/session/look.dart' show monoText;
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/pairing/qr_scan.dart';
import 'package:skywave_gui/screen/readable_width.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// Pairing, first or again: the three fields `skywaved pair` printed,
/// Pair, and how the last attempt went.
///
/// The fields are the user's; they take Rust's values only when the
/// form's `rev` changes (a pasted URI filled them, a re-pair prefilled
/// the address). Text pasted into any field also goes to Rust as
/// `PairPasted`: a `skywave://` URI fills the whole form. With a [scan],
/// the QR code `skywaved pair` printed goes to Rust as `PairScanned`.
class PairingPage extends StatefulWidget {
  const PairingPage({
    required this.screen,
    required this.send,
    this.canGoBack = false,
    this.scan,
    super.key,
  });

  final ScreenViewPairing screen;
  final bool canGoBack;
  final SendIntent send;

  /// The camera scanner, where the platform has one.
  final ScanQr? scan;

  @override
  State<PairingPage> createState() => _PairingPageState();
}

class _PairingPageState extends State<PairingPage> {
  late final _fields = [for (var i = 0; i < 3; i++) _PasteWatch(_pasted)];
  int? _rev;

  _PasteWatch get _address => _fields[0];
  _PasteWatch get _code => _fields[1];
  _PasteWatch get _fingerprint => _fields[2];

  @override
  void initState() {
    super.initState();
    _take(widget.screen.form);
  }

  @override
  void didUpdateWidget(PairingPage old) {
    super.didUpdateWidget(old);
    _take(widget.screen.form);
  }

  void _take(PairForm form) {
    if (form.rev == _rev) return;
    _rev = form.rev;
    _address.set(form.address);
    _code.set(form.code);
    _fingerprint.set(form.fingerprint);
  }

  void _pasted(String text) => widget.send(GuiIntentPairPasted(text: text));

  Future<void> _scan(ScanQr scan) async {
    final text = await scan();
    if (text != null && mounted) widget.send(GuiIntentPairScanned(text: text));
  }

  void _submit() => widget.send(
    GuiIntentPairSubmit(
      address: _address.controller.text,
      code: _code.controller.text,
      fingerprint: _fingerprint.controller.text,
    ),
  );

  @override
  void dispose() {
    for (final field in _fields) {
      field.controller.dispose();
    }
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final screen = widget.screen;
    final theme = Theme.of(context);
    final busy = screen.state == PairingState.inProgress;
    return Scaffold(
      appBar: AppBar(
        leading: backButton(
          canGoBack: widget.canGoBack,
          onBack: () => widget.send.act(const ActionIntentBack()),
        ),
        title: const Text('Pair this device'),
      ),
      body: ListView(
        padding: const EdgeInsets.all(16),
        children: [
          ReadableWidth(
            maxWidth: 560,
            child: AutofillGroup(
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.stretch,
                children: [
                  Text(
                    widget.scan == null
                        ? 'Enter what `skywaved pair` printed on the server, '
                              'or paste its skywave:// link into any field.'
                        : 'Scan the QR code `skywaved pair` printed on the '
                              'server, or enter what it printed.',
                    style: theme.textTheme.bodyLarge,
                  ),
                  if (widget.scan case final scan?) ...[
                    const SizedBox(height: 12),
                    OutlinedButton.icon(
                      icon: const Icon(Icons.qr_code_scanner),
                      onPressed: busy ? null : () => _scan(scan),
                      label: const Text('Scan QR code'),
                    ),
                  ],
                  if (screen.pairedDevice case final device?) ...[
                    const SizedBox(height: 12),
                    Card.outlined(
                      child: ListTile(
                        leading: const Icon(Icons.info_outline),
                        title: Text('Currently paired as $device'),
                        subtitle: Text(
                          [
                            ?screen.pairedServer,
                            'Pairing again replaces this identity.',
                          ].join('\n'),
                        ),
                      ),
                    ),
                  ],
                  const SizedBox(height: 16),
                  TextField(
                    key: const Key('pair-address'),
                    controller: _address.controller,
                    onChanged: _address.changed,
                    enabled: !busy,
                    keyboardType: TextInputType.url,
                    textInputAction: TextInputAction.next,
                    autocorrect: false,
                    decoration: const InputDecoration(
                      labelText: 'Address',
                      hintText: 'host:7443',
                    ),
                  ),
                  const SizedBox(height: 12),
                  TextField(
                    key: const Key('pair-code'),
                    controller: _code.controller,
                    onChanged: _code.changed,
                    enabled: !busy,
                    textInputAction: TextInputAction.next,
                    autocorrect: false,
                    textCapitalization: TextCapitalization.characters,
                    decoration: const InputDecoration(labelText: 'Code'),
                  ),
                  const SizedBox(height: 12),
                  TextField(
                    key: const Key('pair-fingerprint'),
                    controller: _fingerprint.controller,
                    onChanged: _fingerprint.changed,
                    enabled: !busy,
                    textInputAction: TextInputAction.done,
                    autocorrect: false,
                    style: monoText,
                    decoration: const InputDecoration(labelText: 'Fingerprint'),
                    onSubmitted: (_) => _submit(),
                  ),
                  const SizedBox(height: 16),
                  if (busy) ...[
                    const LinearProgressIndicator(),
                    const SizedBox(height: 8),
                  ],
                  if (screen.statusLabel case final status?)
                    Text(
                      status,
                      key: const Key('pair-status'),
                      style: TextStyle(
                        color: screen.state == PairingState.failed
                            ? theme.colorScheme.error
                            : theme.colorScheme.primary,
                      ),
                    ),
                  const SizedBox(height: 16),
                  Align(
                    alignment: Alignment.centerRight,
                    child: FilledButton.icon(
                      icon: const Icon(Icons.link),
                      onPressed: busy ? null : _submit,
                      label: const Text('Pair'),
                    ),
                  ),
                ],
              ),
            ),
          ),
        ],
      ),
    );
  }
}

/// One field and what it held before the last edit, to tell a paste (more
/// than one character arriving at once, by keys, menu or keyboard app)
/// from typing.
class _PasteWatch {
  _PasteWatch(this.onPaste);

  final void Function(String text) onPaste;
  final controller = TextEditingController();
  String _last = '';

  /// Rust's value: not the user's input, so not a paste.
  void set(String text) {
    controller.text = text;
    _last = text;
  }

  /// The user changed the field to [text].
  void changed(String text) {
    final before = _last;
    _last = text;
    var start = 0;
    final shorter = text.length < before.length ? text.length : before.length;
    while (start < shorter &&
        text.codeUnitAt(start) == before.codeUnitAt(start)) {
      start++;
    }
    var end = 0;
    while (end < shorter - start &&
        text.codeUnitAt(text.length - 1 - end) ==
            before.codeUnitAt(before.length - 1 - end)) {
      end++;
    }
    final inserted = text.substring(start, text.length - end);
    if (inserted.length > 1) onPaste(inserted);
  }
}
