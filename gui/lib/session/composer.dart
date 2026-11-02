import 'package:flutter/foundation.dart' hide listEquals;
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/session/look.dart';
import 'package:skywave_gui/session/snippets.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// Whether Enter sends (a hardware keyboard is the norm) or makes a new
/// line (a soft keyboard, with the send button).
bool get enterSends => switch (defaultTargetPlatform) {
  TargetPlatform.linux ||
  TargetPlatform.macOS ||
  TargetPlatform.windows => true,
  _ => false,
};

/// The composer. Its field is the user's while typing: every change of
/// text or cursor goes to the hub as `DraftEdited` (UTF-16 cursor, as
/// Dart strings index), and the hub's `text` overwrites it only when
/// `rev` changes (the session opened, a prompt was recalled, a sent
/// message cleared it).
class Composer extends StatefulWidget {
  const Composer({
    required this.sessionId,
    required this.composer,
    required this.send,
    this.snippets = const [],
    this.focusNode,
    super.key,
  });

  final String sessionId;
  final ComposerView composer;
  final SendIntent send;

  /// What the snippet button offers to put in.
  final List<SnippetView> snippets;

  /// The field's focus, owned by the caller.
  final FocusNode? focusNode;

  @override
  State<Composer> createState() => ComposerState();
}

class ComposerState extends State<Composer> {
  final _field = TextEditingController();
  late FocusNode _focus;
  bool _ownsFocus = false;
  int? _rev;
  TextEditingValue _sent = TextEditingValue.empty;

  /// The snippets as the hub sent them last, so an open picker follows
  /// edits and deletions.
  late final _snippets = ValueNotifier(widget.snippets);

  @override
  void initState() {
    super.initState();
    _adoptFocus();
    _take(widget.composer);
    _field.addListener(_edited);
  }

  void _adoptFocus() {
    if (widget.focusNode case final node?) {
      _focus = node;
    } else {
      _focus = FocusNode(debugLabel: 'composer');
      _ownsFocus = true;
    }
    _focus.onKeyEvent = _onKey;
  }

  @override
  void didUpdateWidget(Composer oldWidget) {
    super.didUpdateWidget(oldWidget);
    if (oldWidget.focusNode != widget.focusNode) {
      if (_ownsFocus) _focus.dispose();
      _ownsFocus = false;
      _adoptFocus();
    }
    _take(widget.composer);
    if (!listEquals(oldWidget.snippets, widget.snippets)) {
      // An open picker is another route: it rebuilds after this frame.
      WidgetsBinding.instance.addPostFrameCallback((_) {
        if (mounted) _snippets.value = widget.snippets;
      });
    }
  }

  @override
  void dispose() {
    _field.dispose();
    _snippets.dispose();
    if (_ownsFocus) _focus.dispose();
    super.dispose();
  }

  /// Takes the hub's text when its revision is new; otherwise the field
  /// stays the user's.
  void _take(ComposerView composer) {
    if (composer.rev == _rev) return;
    _rev = composer.rev;
    final value = TextEditingValue(
      text: composer.text,
      selection: TextSelection.collapsed(
        offset: composer.cursorUtf16.clamp(0, composer.text.length),
      ),
    );
    // Not an edit of the user's: no DraftEdited for it.
    _sent = value;
    _field.value = value;
  }

  void _edited() {
    final value = _field.value;
    if (value.text == _sent.text &&
        value.selection.extentOffset == _sent.selection.extentOffset) {
      return;
    }
    _sent = value;
    final cursor = value.selection.isValid
        ? value.selection.extentOffset.clamp(0, value.text.length)
        : value.text.length;
    widget.send(
      GuiIntentDraftEdited(
        sessionId: widget.sessionId,
        text: value.text,
        cursorUtf16: cursor,
      ),
    );
  }

  /// Nothing to send: the hub refuses blank text, so the button is off
  /// and Enter does nothing.
  bool get _blank => _field.text.trim().isEmpty;

  /// Sends the field's text, unless it is blank.
  void submit() {
    if (_blank) return;
    widget.send(GuiIntentComposerSubmit(sessionId: widget.sessionId));
  }

  bool get _shell => widget.composer.mode == ComposerModeView.shell;

  /// Opens the snippets (in shell mode the saved commands); one put in
  /// brings the focus back to the field.
  Future<void> _pickSnippet() => pickSnippet(
    context,
    sessionId: widget.sessionId,
    snippets: _snippets,
    composerBlank: _blank,
    shell: _shell,
    send: widget.send,
    onInserted: _focus.requestFocus,
  );

  void _toggleShell() {
    widget.send(GuiIntentToggleShellMode(sessionId: widget.sessionId));
    _focus.requestFocus();
  }

  KeyEventResult _onKey(FocusNode node, KeyEvent event) {
    if (event is! KeyDownEvent && event is! KeyRepeatEvent) {
      return KeyEventResult.ignored;
    }
    final keyboard = HardwareKeyboard.instance;
    final key = event.logicalKey;
    final ctrl = keyboard.isControlPressed || keyboard.isMetaPressed;
    final composing =
        _field.value.composing.isValid && !_field.value.composing.isCollapsed;
    if (composing) return KeyEventResult.ignored;

    if (key == LogicalKeyboardKey.enter ||
        key == LogicalKeyboardKey.numpadEnter) {
      if (!enterSends || keyboard.isShiftPressed) return KeyEventResult.ignored;
      // Blank or not, Enter is the send key here: no stray new line.
      submit();
      return KeyEventResult.handled;
    }
    if (ctrl && key == LogicalKeyboardKey.keyP) {
      widget.send(const GuiIntentHistoryOlder());
      return KeyEventResult.handled;
    }
    if (ctrl && key == LogicalKeyboardKey.keyN) {
      widget.send(const GuiIntentHistoryNewer());
      return KeyEventResult.handled;
    }
    final text = _field.text;
    final cursor = _field.selection.extentOffset.clamp(0, text.length);
    if (key == LogicalKeyboardKey.arrowUp &&
        !text.substring(0, cursor).contains('\n')) {
      widget.send(const GuiIntentHistoryOlder());
      return KeyEventResult.handled;
    }
    if (key == LogicalKeyboardKey.arrowDown &&
        !text.substring(cursor).contains('\n')) {
      widget.send(const GuiIntentHistoryNewer());
      return KeyEventResult.handled;
    }
    return KeyEventResult.ignored;
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final colors = SkyColors.of(context);
    final composer = widget.composer;
    final shell = _shell;
    final notes = [
      ?composer.toLabel,
      ?composer.offlineLabel,
      ?composer.historyLabel,
    ];
    return Material(
      color: theme.colorScheme.surfaceContainer,
      child: SafeArea(
        top: false,
        child: Padding(
          padding: const EdgeInsets.fromLTRB(4, 8, 8, 8),
          child: Column(
            mainAxisSize: MainAxisSize.min,
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              if (notes.isNotEmpty)
                Padding(
                  padding: const EdgeInsets.only(left: 4, bottom: 4),
                  child: Row(
                    children: [
                      Icon(
                        composer.offlineLabel != null
                            ? Icons.cloud_off
                            : composer.historyLabel != null
                            ? Icons.history
                            : Icons.call_made,
                        size: 14,
                        color: composer.offlineLabel != null
                            ? colors.warning
                            : colors.muted,
                      ),
                      const SizedBox(width: 6),
                      Expanded(
                        child: Text(
                          notes.join(' · '),
                          style: theme.textTheme.labelSmall?.copyWith(
                            color: composer.offlineLabel != null
                                ? colors.warning
                                : colors.muted,
                          ),
                        ),
                      ),
                    ],
                  ),
                ),
              if (shell)
                _ShellBar(
                  composer: composer,
                  onCondition: () => widget.send(
                    GuiIntentCycleCondition(sessionId: widget.sessionId),
                  ),
                  onStop: () => widget.send(
                    GuiIntentStopShell(sessionId: widget.sessionId),
                  ),
                ),
              Row(
                crossAxisAlignment: CrossAxisAlignment.end,
                children: [
                  IconButton(
                    tooltip: shell ? 'Saved commands' : 'Snippets',
                    onPressed: _pickSnippet,
                    icon: const Icon(Icons.bookmark_outline),
                  ),
                  const SizedBox(width: 2),
                  Expanded(
                    child: TextField(
                      controller: _field,
                      focusNode: _focus,
                      minLines: 1,
                      maxLines: 8,
                      keyboardType: TextInputType.multiline,
                      textInputAction: TextInputAction.newline,
                      textCapitalization: shell
                          ? TextCapitalization.none
                          : TextCapitalization.sentences,
                      autocorrect: !shell,
                      enableSuggestions: !shell,
                      style: shell
                          ? const TextStyle(
                              fontFamily: monoFamily,
                              fontFamilyFallback: monoFallback,
                            )
                          : null,
                      decoration: InputDecoration(
                        hintText: shell
                            ? 'Run a shell command'
                            : 'Message the agent',
                        filled: true,
                        fillColor: theme.colorScheme.surfaceContainerHighest,
                        isDense: true,
                        contentPadding: const EdgeInsets.symmetric(
                          horizontal: 16,
                          vertical: 12,
                        ),
                        border: OutlineInputBorder(
                          borderRadius: BorderRadius.circular(24),
                          borderSide: BorderSide.none,
                        ),
                      ),
                    ),
                  ),
                  const SizedBox(width: 6),
                  // Hidden while the server runs no shell commands, unless
                  // in shell mode: switching back out always works.
                  if (composer.shellEnabled || shell)
                    IconButton(
                      key: const Key('shell-toggle'),
                      tooltip: shell ? 'Message the agent' : 'Shell command',
                      isSelected: shell,
                      onPressed: _toggleShell,
                      icon: const Text(
                        r'$',
                        style: TextStyle(
                          fontFamily: monoFamily,
                          fontFamilyFallback: monoFallback,
                          fontSize: 18,
                          fontWeight: FontWeight.bold,
                        ),
                      ),
                    ),
                  // Follows the field: typing, the hub's text (a recalled
                  // prompt, an edited queued message) and a sent clear.
                  ValueListenableBuilder(
                    valueListenable: _field,
                    builder: (context, _, _) => IconButton.filled(
                      tooltip: shell ? 'Run' : 'Send',
                      onPressed: _blank ? null : submit,
                      icon: const Icon(Icons.arrow_upward),
                    ),
                  ),
                ],
              ),
            ],
          ),
        ),
      ),
    );
  }
}

/// Over the field in shell mode: the prompt in the hub's order (the cwd,
/// the condition chip, the sigil: `work [if ok ✓] $`; the chip steps
/// always → if ok → if failed, and resets after each run sent) and the
/// stop, off while there is nothing to stop.
class _ShellBar extends StatelessWidget {
  const _ShellBar({
    required this.composer,
    required this.onCondition,
    required this.onStop,
  });

  final ComposerView composer;
  final VoidCallback onCondition;
  final VoidCallback onStop;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final prompt = composer.prompt;
    final mono = theme.textTheme.labelLarge?.copyWith(
      fontFamily: monoFamily,
      fontFamilyFallback: monoFallback,
    );
    return Padding(
      padding: const EdgeInsets.only(left: 12, bottom: 4),
      child: Row(
        children: [
          if (prompt?.cwd case final cwd?) ...[
            Flexible(
              child: Text(
                cwd,
                key: const Key('shell-cwd'),
                maxLines: 1,
                overflow: TextOverflow.ellipsis,
                style: mono,
              ),
            ),
            const SizedBox(width: 8),
          ],
          ActionChip(
            key: const Key('shell-condition'),
            tooltip:
                'When it runs: always, if the last run succeeded, or '
                'if it failed',
            avatar: const Icon(Icons.rule, size: 16),
            label: Text(prompt?.whenLabel ?? 'always'),
            onPressed: onCondition,
            visualDensity: VisualDensity.compact,
          ),
          if (prompt != null) ...[
            const SizedBox(width: 8),
            Text(prompt.sigil, key: const Key('shell-sigil'), style: mono),
          ],
          const Spacer(),
          IconButton(
            key: const Key('shell-stop'),
            tooltip: 'Stop the running command (Ctrl+X)',
            onPressed: composer.canStop ? onStop : null,
            icon: Icon(
              Icons.stop_circle_outlined,
              color: composer.canStop ? theme.colorScheme.error : null,
            ),
          ),
        ],
      ),
    );
  }
}
