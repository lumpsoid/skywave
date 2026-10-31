import 'package:flutter/material.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// Pairing: the three fields, the status, and Pair. A placeholder for
/// G5's page. The fields are the user's; they take Rust's values only
/// when the form's `rev` changes (a pasted URI, a re-pair's address).
class PairingPage extends StatefulWidget {
  const PairingPage({required this.screen, required this.send, super.key});

  final ScreenViewPairing screen;
  final SendIntent send;

  @override
  State<PairingPage> createState() => _PairingPageState();
}

class _PairingPageState extends State<PairingPage> {
  final _address = TextEditingController();
  final _code = TextEditingController();
  final _fingerprint = TextEditingController();
  int? _rev;

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
    _address.text = form.address;
    _code.text = form.code;
    _fingerprint.text = form.fingerprint;
  }

  void _submit() => widget.send(
    GuiIntentPairSubmit(
      address: _address.text,
      code: _code.text,
      fingerprint: _fingerprint.text,
    ),
  );

  @override
  void dispose() {
    _address.dispose();
    _code.dispose();
    _fingerprint.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final screen = widget.screen;
    return Scaffold(
      appBar: AppBar(title: const Text('Pair this device')),
      body: ListView(
        padding: const EdgeInsets.all(16),
        children: [
          TextField(
            controller: _address,
            decoration: const InputDecoration(labelText: 'Address'),
          ),
          TextField(
            controller: _code,
            decoration: const InputDecoration(labelText: 'Code'),
          ),
          TextField(
            controller: _fingerprint,
            decoration: const InputDecoration(labelText: 'Fingerprint'),
            onSubmitted: (_) => _submit(),
          ),
          const SizedBox(height: 16),
          if (screen.statusLabel case final status?) Text(status),
          const SizedBox(height: 16),
          FilledButton(onPressed: _submit, child: const Text('Pair')),
        ],
      ),
    );
  }
}
