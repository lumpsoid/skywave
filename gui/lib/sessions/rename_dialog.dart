import 'package:material_ui/material_ui.dart';
import 'package:flutter/services.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/screen/readable_width.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// The rename prompt, over a list row or the open session. It shows while
/// Rust's `RenamePrompt` is set and goes once Rust clears it (the core took
/// the rename, or it was cancelled). Blank restores the default title.
class RenameDialog extends StatefulWidget {
  const RenameDialog({required this.prompt, required this.send, super.key});

  final RenamePrompt prompt;
  final SendIntent send;

  @override
  State<RenameDialog> createState() => _RenameDialogState();
}

class _RenameDialogState extends State<RenameDialog> {
  late final _name = TextEditingController(text: widget.prompt.initial)
    ..selection = TextSelection(
      baseOffset: 0,
      extentOffset: widget.prompt.initial.length,
    );

  void _submit() => widget.send(GuiIntentRenameSubmit(name: _name.text));

  void _cancel() => widget.send(const GuiIntentRenameCancel());

  @override
  void dispose() {
    _name.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return StateDialog(
      label: 'Cancel rename',
      onDismiss: _cancel,
      child: CallbackShortcuts(
        bindings: {const SingleActivator(LogicalKeyboardKey.escape): _cancel},
        child: AlertDialog(
          title: const Text('Rename session'),
          content: SizedBox(
            width: 420,
            child: TextField(
              key: const Key('rename-field'),
              controller: _name,
              autofocus: true,
              decoration: const InputDecoration(
                labelText: 'Name',
                helperText: 'Empty restores the default title.',
              ),
              onSubmitted: (_) => _submit(),
            ),
          ),
          actions: [
            TextButton(onPressed: _cancel, child: const Text('Cancel')),
            FilledButton(onPressed: _submit, child: const Text('Rename')),
          ],
        ),
      ),
    );
  }
}
