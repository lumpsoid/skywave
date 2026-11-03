import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/screen/readable_width.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// The new-session prompt: a workdir on the server, with completion. It
/// shows while Rust's `CreatePrompt` is set and goes once Rust clears it.
///
/// Keys as in the TUI: Tab completes (or takes the highlighted candidate),
/// Up/Down move the highlight, Enter takes it or creates the session, Esc
/// closes the candidate list or else the prompt. Every one is an intent;
/// what it does is Rust's. A tap on a candidate takes it.
///
/// The workdir field is the user's; it takes Rust's input only when
/// `input_rev` changes (a completion filled it in). A second, optional
/// field takes the id of a Claude Code conversation started outside
/// Skywave, to continue it; it is wholly Dart's, only its edits are sent,
/// and Enter in it creates the session like Enter in the workdir field.
/// "Pick a conversation…" opens the server agent's conversations instead
/// (`ConversationPicker`), to continue one in its own workdir.
class CreateDialog extends StatefulWidget {
  const CreateDialog({required this.prompt, required this.send, super.key});

  final CreatePrompt prompt;
  final SendIntent send;

  @override
  State<CreateDialog> createState() => _CreateDialogState();
}

class _CreateDialogState extends State<CreateDialog> {
  static const _rowHeight = 40.0;
  static const _visibleRows = 6;

  final _input = TextEditingController();
  final _list = ScrollController();
  int? _rev;

  @override
  void initState() {
    super.initState();
    _take(widget.prompt);
  }

  @override
  void didUpdateWidget(CreateDialog old) {
    super.didUpdateWidget(old);
    _take(widget.prompt);
    if (widget.prompt.popup?.selected != old.prompt.popup?.selected) {
      WidgetsBinding.instance.addPostFrameCallback((_) => _reveal());
    }
  }

  void _take(CreatePrompt prompt) {
    if (prompt.inputRev == _rev) return;
    _rev = prompt.inputRev;
    _input.value = TextEditingValue(
      text: prompt.input,
      selection: TextSelection.collapsed(offset: prompt.input.length),
    );
  }

  /// Scrolls the highlighted candidate into the list's view.
  void _reveal() {
    final selected = widget.prompt.popup?.selected;
    if (selected == null || !_list.hasClients) return;
    final top = selected * _rowHeight;
    final position = _list.position;
    if (top < position.pixels) {
      _list.jumpTo(top);
    } else if (top + _rowHeight >
        position.pixels + position.viewportDimension) {
      _list.jumpTo(top + _rowHeight - position.viewportDimension);
    }
  }

  void _send(GuiIntent intent) => widget.send(intent);

  KeyEventResult _key(FocusNode _, KeyEvent event) {
    if (event is KeyUpEvent) return KeyEventResult.ignored;
    final listed = widget.prompt.popup?.candidates.isNotEmpty ?? false;
    final intent = switch (event.logicalKey) {
      LogicalKeyboardKey.tab when event is KeyDownEvent =>
        const GuiIntentCompletionComplete(),
      LogicalKeyboardKey.arrowUp when listed => const GuiIntentCompletionMove(
        up: true,
      ),
      LogicalKeyboardKey.arrowDown when listed => const GuiIntentCompletionMove(
        up: false,
      ),
      LogicalKeyboardKey.enter || LogicalKeyboardKey.numpadEnter
          when event is KeyDownEvent =>
        const GuiIntentCompletionAccept(),
      LogicalKeyboardKey.escape when event is KeyDownEvent =>
        const GuiIntentCompletionCancel(),
      _ => null,
    };
    if (intent == null) return KeyEventResult.ignored;
    _send(intent);
    return KeyEventResult.handled;
  }

  /// Enter and Esc in the id field, as in the workdir field.
  KeyEventResult _idKey(FocusNode _, KeyEvent event) {
    if (event is! KeyDownEvent) return KeyEventResult.ignored;
    final intent = switch (event.logicalKey) {
      LogicalKeyboardKey.enter ||
      LogicalKeyboardKey.numpadEnter => const GuiIntentCompletionAccept(),
      LogicalKeyboardKey.escape => const GuiIntentCompletionCancel(),
      _ => null,
    };
    if (intent == null) return KeyEventResult.ignored;
    _send(intent);
    return KeyEventResult.handled;
  }

  @override
  void dispose() {
    _input.dispose();
    _list.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final popup = widget.prompt.popup;
    final theme = Theme.of(context);
    return StateDialog(
      label: 'Cancel new session',
      onDismiss: () => _send(const GuiIntentCompletionCancel()),
      child: AlertDialog(
        title: const Text('New session'),
        content: SizedBox(
          width: 520,
          child: Column(
            mainAxisSize: MainAxisSize.min,
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              Focus(
                onKeyEvent: _key,
                child: TextField(
                  key: const Key('create-workdir'),
                  controller: _input,
                  autofocus: true,
                  autocorrect: false,
                  style: const TextStyle(fontFamily: 'monospace'),
                  decoration: InputDecoration(
                    labelText: 'Workdir on the server',
                    suffixIcon: IconButton(
                      tooltip: 'Complete',
                      icon: const Icon(Icons.keyboard_tab),
                      onPressed: () =>
                          _send(const GuiIntentCompletionComplete()),
                    ),
                  ),
                  onChanged: (text) =>
                      _send(GuiIntentCompletionTyped(input: text)),
                  // A soft keyboard's done key; hardware Enter is in _key.
                  onSubmitted: (_) => _send(const GuiIntentCompletionAccept()),
                ),
              ),
              if (popup != null) ...[
                const SizedBox(height: 8),
                Card.outlined(
                  key: const Key('completion-popup'),
                  margin: EdgeInsets.zero,
                  child: Column(
                    mainAxisSize: MainAxisSize.min,
                    crossAxisAlignment: CrossAxisAlignment.stretch,
                    children: [
                      if (popup.candidates.isNotEmpty)
                        ConstrainedBox(
                          constraints: const BoxConstraints(
                            maxHeight: _rowHeight * _visibleRows,
                          ),
                          child: ListView.builder(
                            controller: _list,
                            shrinkWrap: true,
                            itemExtent: _rowHeight,
                            itemCount: popup.candidates.length,
                            itemBuilder: (context, i) => _CandidateTile(
                              candidate: popup.candidates[i],
                              selected: i == popup.selected,
                              onTap: () =>
                                  _send(GuiIntentCompletionPick(index: i)),
                            ),
                          ),
                        ),
                      if (popup.emptyLabel case final empty?)
                        _Note(empty, color: theme.colorScheme.onSurfaceVariant),
                      if (popup.statusLabel case final status?)
                        _Note(
                          status,
                          color: theme.colorScheme.onSurfaceVariant,
                        ),
                    ],
                  ),
                ),
              ],
              const SizedBox(height: 16),
              Focus(
                onKeyEvent: _idKey,
                child: TextField(
                  key: const Key('create-conversation'),
                  autocorrect: false,
                  style: const TextStyle(fontFamily: 'monospace'),
                  decoration: const InputDecoration(
                    labelText: 'Continue a Claude Code session (paste its id)',
                    hintText: 'leave empty for a new conversation',
                  ),
                  onChanged: (text) => _send(
                    GuiIntentCreateConversationTyped(conversation: text),
                  ),
                  onSubmitted: (_) => _send(const GuiIntentCompletionAccept()),
                ),
              ),
              const SizedBox(height: 8),
              Align(
                alignment: Alignment.centerLeft,
                child: TextButton.icon(
                  key: const Key('create-pick-conversation'),
                  icon: const Icon(Icons.history),
                  label: const Text('Pick a conversation…'),
                  onPressed: () => _send(const GuiIntentConversationsOpen()),
                ),
              ),
            ],
          ),
        ),
        actions: [
          TextButton(
            onPressed: () => _send(const GuiIntentCompletionCancel()),
            child: const Text('Cancel'),
          ),
          FilledButton(
            onPressed: () => _send(const GuiIntentCompletionAccept()),
            child: const Text('Create'),
          ),
        ],
      ),
    );
  }
}

class _CandidateTile extends StatelessWidget {
  const _CandidateTile({
    required this.candidate,
    required this.selected,
    required this.onTap,
  });

  final CandidateView candidate;
  final bool selected;
  final VoidCallback onTap;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return ListTile(
      dense: true,
      selected: selected,
      selectedTileColor: theme.colorScheme.secondaryContainer,
      title: Text(
        candidate.path,
        overflow: TextOverflow.ellipsis,
        style: const TextStyle(fontFamily: 'monospace'),
      ),
      trailing: candidate.recent
          ? Text(
              'recent',
              style: theme.textTheme.labelSmall?.copyWith(
                color: theme.colorScheme.onSurfaceVariant,
              ),
            )
          : null,
      onTap: onTap,
    );
  }
}

class _Note extends StatelessWidget {
  const _Note(this.text, {required this.color});

  final String text;
  final Color color;

  @override
  Widget build(BuildContext context) {
    return Padding(
      padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 8),
      child: Text(
        text,
        style: Theme.of(context).textTheme.bodySmall?.copyWith(color: color),
      ),
    );
  }
}
