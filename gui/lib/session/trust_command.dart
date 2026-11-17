import 'package:material_ui/material_ui.dart';
import 'package:skywave_gui/session/look.dart';

/// The command prefixes the person trusts, one per checked candidate in
/// candidate order, or `null` when cancelled. Each prefix is the candidate's
/// leading words as parsed: the person chooses how many to keep, never edits
/// them, so a quoted word like `-m "fix it"` stays exact.
Future<List<List<String>>?> askTrustCommand(
  BuildContext context,
  List<List<String>> candidates, {
  String confirmLabel = 'Trust',
}) => showDialog<List<List<String>>>(
  context: context,
  builder: (context) =>
      _TrustDialog(candidates: candidates, confirmLabel: confirmLabel),
);

class _TrustDialog extends StatefulWidget {
  const _TrustDialog({required this.candidates, required this.confirmLabel});

  final List<List<String>> candidates;
  final String confirmLabel;

  @override
  State<_TrustDialog> createState() => _TrustDialogState();
}

class _TrustDialogState extends State<_TrustDialog> {
  /// Per candidate, the index of its last kept word. Starts at the whole
  /// command.
  late final List<int> _cut = [
    for (final words in widget.candidates) words.length - 1,
  ];

  /// Per candidate, whether it is trusted at all.
  late final List<bool> _included = List.filled(widget.candidates.length, true);

  void _submit() => Navigator.of(context).pop([
    for (final (index, words) in widget.candidates.indexed)
      if (_included[index]) words.sublist(0, _cut[index] + 1),
  ]);

  @override
  Widget build(BuildContext context) {
    final muted = SkyColors.of(context).muted;
    return AlertDialog(
      title: const Text('Trust command'),
      content: SizedBox(
        width: 420,
        child: SingleChildScrollView(
          child: Column(
            mainAxisSize: MainAxisSize.min,
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Text(
                'Matching commands will run without asking. '
                'Tap a word to stop the rule there.',
                style: Theme.of(context).textTheme.bodySmall
                    ?.copyWith(color: muted),
              ),
              const SizedBox(height: 12),
              for (final (index, words) in widget.candidates.indexed)
                Padding(
                  padding: const EdgeInsets.only(bottom: 8),
                  child: Row(
                    crossAxisAlignment: CrossAxisAlignment.start,
                    children: [
                      Checkbox(
                        key: Key('trust-include-$index'),
                        value: _included[index],
                        onChanged: (value) =>
                            setState(() => _included[index] = value ?? false),
                      ),
                      Expanded(
                        child: Wrap(
                          spacing: 8,
                          runSpacing: 8,
                          crossAxisAlignment: WrapCrossAlignment.center,
                          children: [
                            for (final (i, word) in words.indexed)
                              ChoiceChip(
                                key: Key('trust-word-$index-$i'),
                                label: Text(word),
                                selected: i <= _cut[index],
                                onSelected: (_) =>
                                    setState(() => _cut[index] = i),
                              ),
                          ],
                        ),
                      ),
                    ],
                  ),
                ),
            ],
          ),
        ),
      ),
      actions: [
        TextButton(
          onPressed: () => Navigator.of(context).pop(),
          child: const Text('Cancel'),
        ),
        FilledButton(
          key: const Key('trust-confirm'),
          onPressed: _included.any((included) => included) ? _submit : null,
          child: Text(widget.confirmLabel),
        ),
      ],
    );
  }
}
