import 'package:material_ui/material_ui.dart';
import 'package:skywave_gui/session/look.dart' show monoText;
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
/// The candidate list sits in a box of fixed height, with a loading bar
/// slot above it and a status line below, so the dialog never changes size
/// as the list does.
///
/// The workdir field is the user's; it takes Rust's input only when
/// `input_rev` changes (a completion filled it in). A second, optional
/// field takes the id of a Claude Code conversation started outside
/// Skywave, to continue it; a third takes an optional worktree branch.
/// Both take Rust's value the same way, when `input_rev` changes (a
/// recreate fills them with the refused create's), and their edits are
/// sent; Enter in either creates the session like Enter in the workdir
/// field.
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
  final _conversation = TextEditingController();
  final _worktree = TextEditingController();
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
    _input.value = _valueOf(prompt.input);
    _conversation.value = _valueOf(prompt.conversation);
    _worktree.value = _valueOf(prompt.worktree);
  }

  /// A field's value, with the caret at its end.
  static TextEditingValue _valueOf(String text) => TextEditingValue(
    text: text,
    selection: TextSelection.collapsed(offset: text.length),
  );

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
    final intent = switch (event.logicalKey) {
      LogicalKeyboardKey.tab when event is KeyDownEvent =>
        const GuiIntentCompletionComplete(),
      // Rust ignores a move when nothing is listed.
      LogicalKeyboardKey.arrowUp => const GuiIntentCompletionMove(up: true),
      LogicalKeyboardKey.arrowDown => const GuiIntentCompletionMove(up: false),
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
    _conversation.dispose();
    _worktree.dispose();
    _list.dispose();
    super.dispose();
  }

  /// The candidate box: up to `_visibleRows` rows, as much as the window
  /// allows (the dialog's Flexible bounds it), and never a height that
  /// follows the candidates. Stale rows are dimmed and take no taps; with
  /// no rows, the empty label sits in the middle of the box.
  Widget _candidateBox(CompletionPopup? popup, ThemeData theme) {
    final candidates = popup?.candidates ?? const <CandidateView>[];
    final stale = popup?.stale ?? false;
    return SizedBox(
      key: const Key('completion-popup'),
      height: _rowHeight * _visibleRows,
      child: Card.outlined(
        margin: EdgeInsets.zero,
        child: Opacity(
          opacity: stale ? 0.5 : 1.0,
          child: IgnorePointer(
            ignoring: stale,
            child: Stack(
              children: [
                ListView.builder(
                  controller: _list,
                  itemExtent: _rowHeight,
                  itemCount: candidates.length,
                  itemBuilder: (context, i) => _CandidateTile(
                    candidate: candidates[i],
                    selected: i == popup?.selected,
                    onTap: () => _send(GuiIntentCompletionPick(index: i)),
                  ),
                ),
                if (candidates.isEmpty)
                  Positioned.fill(
                    child: Center(
                      child: Text(
                        popup?.emptyLabel ?? '',
                        style: _mutedText(theme),
                      ),
                    ),
                  ),
              ],
            ),
          ),
        ),
      ),
    );
  }

  /// The line under the box: the popup's status when it has one, else the
  /// keys that apply to the highlight. Always one line tall.
  Widget _statusLine(CompletionPopup? popup, ThemeData theme) {
    final text =
        popup?.statusLabel ??
        (popup?.selected == null
            ? '↓ choose · Tab complete · Enter create'
            : 'Enter open · Esc unselect');
    return SizedBox(
      height: 20,
      child: Align(
        alignment: Alignment.centerLeft,
        child: Text(
          text,
          maxLines: 1,
          overflow: TextOverflow.ellipsis,
          style: _mutedText(theme),
        ),
      ),
    );
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
                  style: monoText,
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
              // Reserved whether or not Rust is listing: no movement.
              SizedBox(
                height: 2,
                child: (popup?.loading ?? false)
                    ? const LinearProgressIndicator(minHeight: 2)
                    : null,
              ),
              // Loose: the dialog's content is bounded by the window, so the
              // box gets what is left, at most its own height.
              Flexible(child: _candidateBox(popup, theme)),
              _statusLine(popup, theme),
              const SizedBox(height: 16),
              Focus(
                onKeyEvent: _idKey,
                child: TextField(
                  key: const Key('create-conversation'),
                  controller: _conversation,
                  autocorrect: false,
                  style: monoText,
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
              Focus(
                onKeyEvent: _idKey,
                child: TextField(
                  key: const Key('create-worktree'),
                  controller: _worktree,
                  autocorrect: false,
                  style: monoText,
                  decoration: const InputDecoration(
                    labelText: 'New worktree on branch (optional)',
                    hintText: 'leave empty to run in the directory itself',
                  ),
                  onChanged: (text) =>
                      _send(GuiIntentCreateWorktreeTyped(worktree: text)),
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
      leading: const Icon(Icons.folder_outlined),
      title: Text(
        candidate.label,
        maxLines: 1,
        overflow: TextOverflow.ellipsis,
        style: monoText,
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

TextStyle? _mutedText(ThemeData theme) {
  return theme.textTheme.bodySmall?.copyWith(
    color: theme.colorScheme.onSurfaceVariant,
  );
}
