import 'package:material_ui/material_ui.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// The note for a safety check feedback, trimmed (`''` for none), or `null`
/// when cancelled.
Future<String?> askSafetyFeedbackNote(
  BuildContext context,
  SafetyJudgmentView judgment,
) => showDialog<String>(
  context: context,
  builder: (context) => _FeedbackDialog(judgment: judgment),
);

class _FeedbackDialog extends StatefulWidget {
  const _FeedbackDialog({required this.judgment});

  final SafetyJudgmentView judgment;

  @override
  State<_FeedbackDialog> createState() => _FeedbackDialogState();
}

class _FeedbackDialogState extends State<_FeedbackDialog> {
  final _note = TextEditingController();

  @override
  void dispose() {
    _note.dispose();
    super.dispose();
  }

  void _submit() => Navigator.of(context).pop(_note.text.trim());

  @override
  Widget build(BuildContext context) {
    return AlertDialog(
      title: Text(switch (widget.judgment) {
        SafetyJudgmentView.shouldApprove => 'Should have been approved',
        SafetyJudgmentView.shouldAsk => 'Should have asked',
      }),
      content: SizedBox(
        width: 420,
        child: TextField(
          key: const Key('safety-feedback-note'),
          controller: _note,
          autofocus: true,
          decoration: const InputDecoration(
            labelText: 'Note (optional)',
            helperText: 'Kept on your server to improve the safety check.',
          ),
          onSubmitted: (_) => _submit(),
        ),
      ),
      actions: [
        TextButton(
          onPressed: () => Navigator.of(context).pop(),
          child: const Text('Cancel'),
        ),
        FilledButton(onPressed: _submit, child: const Text('Send')),
      ],
    );
  }
}
