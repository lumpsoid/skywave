import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// The rename prompt over the session while the hub holds one open: a
/// barrier and a dialog. The hub closes it once the core took the name
/// (`RenameSubmit`) or on `RenameCancel`.
class RenameOverlay extends StatefulWidget {
  const RenameOverlay({required this.prompt, required this.send, super.key});

  final RenamePrompt prompt;
  final SendIntent send;

  @override
  State<RenameOverlay> createState() => _RenameOverlayState();
}

class _RenameOverlayState extends State<RenameOverlay> {
  late final _field = TextEditingController(text: widget.prompt.initial)
    ..selection = TextSelection(
      baseOffset: 0,
      extentOffset: widget.prompt.initial.length,
    );

  @override
  void dispose() {
    _field.dispose();
    super.dispose();
  }

  void _submit() => widget.send(GuiIntentRenameSubmit(name: _field.text));
  void _cancel() => widget.send(const GuiIntentRenameCancel());

  @override
  Widget build(BuildContext context) {
    return CallbackShortcuts(
      bindings: {const SingleActivator(LogicalKeyboardKey.escape): _cancel},
      child: Stack(
        children: [
          ModalBarrier(
            color: Colors.black54,
            onDismiss: _cancel,
            semanticsLabel: 'Cancel rename',
          ),
          Center(
            child: AlertDialog(
              title: const Text('Rename session'),
              content: SizedBox(
                width: 420,
                child: TextField(
                  controller: _field,
                  autofocus: true,
                  onSubmitted: (_) => _submit(),
                  decoration: const InputDecoration(
                    labelText: 'Name',
                    helperText: 'Empty restores the default title',
                  ),
                ),
              ),
              actions: [
                TextButton(onPressed: _cancel, child: const Text('Cancel')),
                FilledButton(onPressed: _submit, child: const Text('Rename')),
              ],
            ),
          ),
        ],
      ),
    );
  }
}
