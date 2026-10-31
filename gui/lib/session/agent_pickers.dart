import 'package:flutter/material.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/session/look.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// Lists every mode with what it lets the agent do; a pick switches the
/// session's agent to it, after one more question for a mode that lets
/// every tool run unasked.
Future<void> pickMode(
  BuildContext context,
  SessionScreen screen,
  SendIntent send,
) async {
  final agent = screen.agent;
  final picked = await showDialog<ModeOption>(
    context: context,
    builder: (context) => AlertDialog(
      title: const Text('Permission mode'),
      contentPadding: const EdgeInsets.fromLTRB(0, 16, 0, 0),
      content: SingleChildScrollView(
        child: Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            if (agent.modeRefusal case final refusal?) _Refusal(text: refusal),
            for (final option in agent.modes)
              _Choice(
                label: option.label,
                detail: option.detail,
                current: option.current,
                pending: option.pending,
                warn: option.needsConfirmation,
                onTap: () => Navigator.of(context).pop(option),
              ),
          ],
        ),
      ),
      actions: [
        TextButton(
          onPressed: () => Navigator.of(context).pop(),
          child: const Text('Close'),
        ),
      ],
    ),
  );
  if (picked == null || picked.current || !context.mounted) return;
  if (picked.needsConfirmation && !await _confirm(context, picked)) return;
  send.act(
    ActionIntentSetPermissionMode(
      sessionId: screen.sessionId,
      mode: picked.mode,
    ),
  );
}

/// Asks once more before a mode that lets every tool run unasked.
Future<bool> _confirm(BuildContext context, ModeOption option) async {
  final confirmed = await showDialog<bool>(
    context: context,
    builder: (context) {
      final scheme = Theme.of(context).colorScheme;
      return AlertDialog(
        icon: Icon(Icons.warning_amber_rounded, color: scheme.error),
        title: Text('Switch to ${option.label}?'),
        content: Text(
          '${option.label}: ${option.detail}. '
          'Nothing the agent runs waits for an approval.',
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.of(context).pop(false),
            child: const Text('Cancel'),
          ),
          FilledButton(
            style: FilledButton.styleFrom(
              backgroundColor: scheme.error,
              foregroundColor: scheme.onError,
            ),
            onPressed: () => Navigator.of(context).pop(true),
            child: Text('Switch to ${option.label}'),
          ),
        ],
      );
    },
  );
  return confirmed ?? false;
}

/// Lists the models the agents offer, and takes a model's name typed in:
/// the only way while none is offered. A pick switches the session's
/// agent to it.
Future<void> pickModel(
  BuildContext context,
  SessionScreen screen,
  SendIntent send,
) async {
  final model = await showDialog<String>(
    context: context,
    builder: (context) => _ModelDialog(agent: screen.agent),
  );
  if (model == null || model.trim().isEmpty) return;
  send.act(ActionIntentSetModel(sessionId: screen.sessionId, model: model));
}

class _ModelDialog extends StatefulWidget {
  const _ModelDialog({required this.agent});

  final AgentSettingsView agent;

  @override
  State<_ModelDialog> createState() => _ModelDialogState();
}

class _ModelDialogState extends State<_ModelDialog> {
  final _name = TextEditingController();

  @override
  void dispose() {
    _name.dispose();
    super.dispose();
  }

  void _submit() {
    final name = _name.text.trim();
    if (name.isNotEmpty) Navigator.of(context).pop(name);
  }

  @override
  Widget build(BuildContext context) {
    final agent = widget.agent;
    return AlertDialog(
      title: const Text('Model'),
      contentPadding: const EdgeInsets.fromLTRB(0, 16, 0, 0),
      content: SingleChildScrollView(
        child: Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            if (agent.modelRefusal case final refusal?) _Refusal(text: refusal),
            for (final option in agent.models)
              _Choice(
                label: option.label,
                detail: option.detail,
                current: option.current,
                pending: option.pending,
                onTap: option.current
                    ? () => Navigator.of(context).pop()
                    : () => Navigator.of(context).pop(option.id),
              ),
            Padding(
              padding: const EdgeInsets.fromLTRB(24, 8, 24, 0),
              child: TextField(
                controller: _name,
                autofocus: agent.models.isEmpty,
                decoration: InputDecoration(
                  labelText: 'Model name',
                  hintText: agent.modelName,
                  helperText: agent.models.isEmpty
                      ? 'The agent lists its models once it has started.'
                      : null,
                  helperMaxLines: 2,
                ),
                onSubmitted: (_) => _submit(),
              ),
            ),
          ],
        ),
      ),
      actions: [
        TextButton(
          onPressed: () => Navigator.of(context).pop(),
          child: const Text('Close'),
        ),
        ListenableBuilder(
          listenable: _name,
          builder: (context, _) => FilledButton(
            onPressed: _name.text.trim().isEmpty ? null : _submit,
            child: const Text('Switch'),
          ),
        ),
      ],
    );
  }
}

/// One mode or model in a picker: the current one checked, the one a
/// switch waits on marked.
class _Choice extends StatelessWidget {
  const _Choice({
    required this.label,
    required this.detail,
    required this.current,
    required this.pending,
    required this.onTap,
    this.warn = false,
  });

  final String label;
  final String? detail;
  final bool current;
  final bool pending;

  /// Lets every tool run unasked.
  final bool warn;
  final VoidCallback onTap;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final colors = SkyColors.of(context);
    return ListTile(
      contentPadding: const EdgeInsets.symmetric(horizontal: 24),
      selected: current,
      leading: Icon(
        current
            ? Icons.check_circle
            : pending
            ? Icons.hourglass_top
            : Icons.circle_outlined,
        color: pending ? colors.warning : null,
      ),
      title: Text(
        label,
        style: warn ? TextStyle(color: theme.colorScheme.error) : null,
      ),
      subtitle: detail == null ? null : Text(detail!),
      trailing: pending
          ? Text(
              'switching…',
              style: theme.textTheme.labelSmall?.copyWith(
                color: colors.warning,
              ),
            )
          : null,
      onTap: onTap,
    );
  }
}

/// The agent's refusal of the last switch, over a picker's choices.
class _Refusal extends StatelessWidget {
  const _Refusal({required this.text});

  final String text;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    return Padding(
      padding: const EdgeInsets.fromLTRB(24, 0, 24, 8),
      child: Row(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Icon(Icons.error_outline, size: 18, color: scheme.error),
          const SizedBox(width: 8),
          Expanded(
            child: Text(text, style: TextStyle(color: scheme.error)),
          ),
        ],
      ),
    );
  }
}
