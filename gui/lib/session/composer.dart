import 'package:flutter/foundation.dart';
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
  }

  @override
  void dispose() {
    _field.dispose();
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

  /// Opens the snippets; one put in brings the focus back to the field.
  Future<void> _snippets() => pickSnippet(
    context,
    sessionId: widget.sessionId,
    snippets: widget.snippets,
    composerBlank: _blank,
    send: widget.send,
    onInserted: _focus.requestFocus,
  );

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
    final notes = [?composer.offlineLabel, ?composer.historyLabel];
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
                            : Icons.history,
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
              Row(
                crossAxisAlignment: CrossAxisAlignment.end,
                children: [
                  IconButton(
                    tooltip: 'Snippets',
                    onPressed: _snippets,
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
                      textCapitalization: TextCapitalization.sentences,
                      decoration: InputDecoration(
                        hintText: 'Message the agent',
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
                  // Follows the field: typing, the hub's text (a recalled
                  // prompt, an edited queued message) and a sent clear.
                  ValueListenableBuilder(
                    valueListenable: _field,
                    builder: (context, _, _) => IconButton.filled(
                      tooltip: 'Send',
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
