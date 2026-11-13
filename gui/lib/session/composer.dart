import 'dart:io';

import 'package:desktop_drop/desktop_drop.dart';
import 'package:flutter/foundation.dart' hide listEquals;
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/session/history_picker.dart';
import 'package:skywave_gui/session/image_picking.dart';
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

/// Whether images can be pasted (Ctrl+V, Cmd+V on macOS) and dropped onto
/// the composer: the desktops, where the clipboard and drop plugins apply.
bool get _desktop => switch (defaultTargetPlatform) {
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

  bool get _blank => _field.text.trim().isEmpty;

  bool get _shell => widget.composer.mode == ComposerModeView.shell;

  /// Nothing to send: no text and no image. The hub refuses it, so the
  /// button is off and Enter does nothing.
  bool get _nothingToSend => _blank && widget.composer.images.isEmpty;

  /// An image is still being prepared (agent mode): the hub refuses Send
  /// until it is ready.
  bool get _preparing => !_shell && widget.composer.preparing > 0;

  /// Sends the draft, unless there is nothing to send or it must wait.
  void submit() {
    if (_nothingToSend || _preparing) return;
    widget.send(GuiIntentComposerSubmit(sessionId: widget.sessionId));
  }

  /// Picks images and attaches each one to the draft, in the order picked.
  Future<void> _attach() async {
    final paths = await pickImages(context);
    if (!mounted) return;
    _attachFiles(paths);
  }

  /// Attaches each file to the draft, in order. The hub refuses what is not
  /// an image, with a notice.
  void _attachFiles(List<String> paths) {
    for (final path in paths) {
      widget.send(
        GuiIntentAttachImageFile(sessionId: widget.sessionId, path: path),
      );
    }
  }

  /// Ctrl+V (Cmd+V on macOS): an image on the clipboard is attached, and
  /// anything else is pasted as text, as the field would paste it.
  Future<void> _paste() async {
    final image = await readClipboardImage();
    if (!mounted) return;
    if (image != null) {
      widget.send(
        GuiIntentAttachImageBytes(
          sessionId: widget.sessionId,
          bytes: image,
          mime: 'image/png',
        ),
      );
      return;
    }
    final text = (await Clipboard.getData(Clipboard.kTextPlain))?.text;
    if (!mounted || text == null) return;
    final selection = _field.selection.isValid
        ? _field.selection
        : TextSelection.collapsed(offset: _field.text.length);
    _field.value = _field.value.replaced(selection, text);
  }

  /// Content the phone keyboard inserts (a GIF from the keyboard's
  /// picker, say): attached when it came with its bytes. The mime types
  /// are limited to images by the configuration.
  void _contentInserted(KeyboardInsertedContent content) {
    final data = content.data;
    if (data == null) return;
    widget.send(
      GuiIntentAttachImageBytes(
        sessionId: widget.sessionId,
        bytes: data,
        mime: content.mimeType,
      ),
    );
  }

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

    final picking = _shell && widget.composer.historySearch != null;
    if (ctrl && key == LogicalKeyboardKey.keyR && _shell) {
      // Toggles on the key down only: a held key's repeats would flicker it
      // open and closed. The repeats are still handled, so no R is typed.
      if (event is KeyDownEvent) {
        widget.send(
          picking
              ? const GuiIntentHistorySearchClose()
              : const GuiIntentHistorySearchOpen(),
        );
      }
      return KeyEventResult.handled;
    }
    if (picking) {
      final intent = switch (key) {
        LogicalKeyboardKey.enter ||
        LogicalKeyboardKey.numpadEnter ||
        LogicalKeyboardKey.tab => const GuiIntentHistorySearchAccept(),
        LogicalKeyboardKey.escape => const GuiIntentHistorySearchClose(),
        LogicalKeyboardKey.arrowUp => const GuiIntentHistorySearchMove(
          older: true,
        ),
        LogicalKeyboardKey.arrowDown => const GuiIntentHistorySearchMove(
          older: false,
        ),
        _ when ctrl && key == LogicalKeyboardKey.keyP =>
          const GuiIntentHistorySearchMove(older: true),
        _ when ctrl && key == LogicalKeyboardKey.keyN =>
          const GuiIntentHistorySearchMove(older: false),
        _ => null,
      };
      if (intent != null) {
        widget.send(intent);
        return KeyEventResult.handled;
      }
    }

    final paste = defaultTargetPlatform == TargetPlatform.macOS
        ? keyboard.isMetaPressed
        : keyboard.isControlPressed;
    if (paste && key == LogicalKeyboardKey.keyV && _desktop && !_shell) {
      // Handled here so the field's own paste does not run as well. Only
      // the key down pastes: a held key repeats without pasting again.
      if (event is KeyDownEvent) _paste();
      return KeyEventResult.handled;
    }

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
    final notes = [?composer.toLabel, ?composer.historyLabel];
    final body = Material(
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
                        composer.historyLabel != null
                            ? Icons.history
                            : Icons.call_made,
                        size: 14,
                        color: colors.muted,
                      ),
                      const SizedBox(width: 6),
                      Expanded(
                        child: Text(
                          notes.join(' · '),
                          style: theme.textTheme.labelSmall?.copyWith(
                            color: colors.muted,
                          ),
                        ),
                      ),
                      if (composer.historyLabel != null) ...[
                        IconButton(
                          key: const Key('history-older-step'),
                          tooltip: 'Older prompt',
                          visualDensity: VisualDensity.compact,
                          iconSize: 18,
                          onPressed: () =>
                              widget.send(const GuiIntentHistoryOlder()),
                          icon: const Icon(Icons.keyboard_arrow_up),
                        ),
                        IconButton(
                          key: const Key('history-newer-step'),
                          tooltip: 'Newer prompt',
                          visualDensity: VisualDensity.compact,
                          iconSize: 18,
                          onPressed: () =>
                              widget.send(const GuiIntentHistoryNewer()),
                          icon: const Icon(Icons.keyboard_arrow_down),
                        ),
                      ],
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
              if (!shell &&
                  (composer.images.isNotEmpty || composer.preparing > 0))
                _ImageChips(
                  sessionId: widget.sessionId,
                  images: composer.images,
                  preparing: composer.preparing,
                  send: widget.send,
                ),
              if (shell && composer.historySearch != null)
                HistoryPicker(view: composer.historySearch!, send: widget.send),
              Row(
                crossAxisAlignment: CrossAxisAlignment.end,
                children: [
                  _InputMenu(
                    actions: [
                      _InputAction(
                        key: const Key('composer-snippets'),
                        icon: Icons.bookmark_outline,
                        title: shell ? 'Saved commands' : 'Snippets',
                        onTap: () => _pickSnippet(),
                      ),
                      if (!shell)
                        _InputAction(
                          key: const Key('attach-image'),
                          icon: Icons.image_outlined,
                          title: 'Attach image',
                          onTap: () => _attach(),
                        ),
                      _InputAction(
                        key: const Key('history-older'),
                        icon: Icons.history,
                        title: 'Previous prompt',
                        onTap: () => widget.send(const GuiIntentHistoryOlder()),
                      ),
                      if (shell)
                        _InputAction(
                          key: const Key('history-search'),
                          icon: Icons.manage_search,
                          title: 'Search history',
                          onTap: () =>
                              widget.send(const GuiIntentHistorySearchOpen()),
                        ),
                    ],
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
                      // The phone keyboard's images (agent mode only).
                      contentInsertionConfiguration: shell
                          ? null
                          : ContentInsertionConfiguration(
                              allowedMimeTypes: const [
                                'image/png',
                                'image/jpeg',
                                'image/gif',
                                'image/webp',
                              ],
                              onContentInserted: _contentInserted,
                            ),
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
                      onPressed: _nothingToSend || _preparing ? null : submit,
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
    // Files dropped on a desktop window; off in shell mode.
    if (!_desktop) return body;
    return DropTarget(
      enable: !shell,
      onDragDone: (details) =>
          _attachFiles([for (final file in details.files) file.path]),
      child: body,
    );
  }
}

/// Above the field: the draft's images, each with a ✕, and a spinner for
/// each one still being prepared.
class _ImageChips extends StatelessWidget {
  const _ImageChips({
    required this.sessionId,
    required this.images,
    required this.preparing,
    required this.send,
  });

  final String sessionId;
  final List<ComposerImage> images;
  final int preparing;
  final SendIntent send;

  @override
  Widget build(BuildContext context) {
    return SizedBox(
      height: 64,
      child: ListView(
        scrollDirection: Axis.horizontal,
        // Room above and below the thumbnails for the ✕ that overhangs.
        padding: const EdgeInsets.fromLTRB(4, 6, 4, 2),
        children: [
          for (final image in images)
            Padding(
              padding: const EdgeInsets.only(right: 8),
              child: _ImageChip(
                image: image,
                onDetach: () => send(
                  GuiIntentDetachImage(
                    sessionId: sessionId,
                    sha256: image.sha256,
                  ),
                ),
              ),
            ),
          for (var i = 0; i < preparing; i++)
            Padding(
              padding: const EdgeInsets.only(right: 8),
              child: SizedBox(
                key: ValueKey('image-preparing-$i'),
                width: 56,
                height: 56,
                child: const Center(
                  child: CircularProgressIndicator(strokeWidth: 2),
                ),
              ),
            ),
        ],
      ),
    );
  }
}

/// One image of the draft: its thumbnail, or its size where the file
/// cannot be shown, and a ✕ that takes it off the draft.
class _ImageChip extends StatelessWidget {
  const _ImageChip({required this.image, required this.onDetach});

  final ComposerImage image;
  final VoidCallback onDetach;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final colors = SkyColors.of(context);
    return Stack(
      clipBehavior: Clip.none,
      children: [
        ClipRRect(
          borderRadius: BorderRadius.circular(8),
          child: Image.file(
            File(image.path),
            width: 56,
            height: 56,
            fit: BoxFit.cover,
            cacheWidth: 112,
            errorBuilder: (context, error, stackTrace) => Container(
              width: 56,
              height: 56,
              color: theme.colorScheme.surfaceContainerHighest,
              padding: const EdgeInsets.all(2),
              child: Column(
                mainAxisAlignment: MainAxisAlignment.center,
                children: [
                  const Icon(Icons.image_outlined, size: 18),
                  Text(
                    image.sizeLabel,
                    maxLines: 1,
                    overflow: TextOverflow.ellipsis,
                    style: theme.textTheme.labelSmall?.copyWith(
                      color: colors.muted,
                    ),
                  ),
                ],
              ),
            ),
          ),
        ),
        Positioned(
          top: -6,
          right: -6,
          child: Material(
            color: theme.colorScheme.surfaceContainerHighest,
            shape: const CircleBorder(),
            child: IconButton(
              key: Key('detach-${image.sha256}'),
              tooltip: 'Remove image',
              onPressed: onDetach,
              padding: EdgeInsets.zero,
              constraints: const BoxConstraints.tightFor(width: 24, height: 24),
              iconSize: 14,
              icon: const Icon(Icons.close),
            ),
          ),
        ),
      ],
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

/// One entry of the input menu: its key (for tests), icon, title and what
/// it does.
class _InputAction {
  const _InputAction({
    required this.key,
    required this.icon,
    required this.title,
    required this.onTap,
  });

  final Key key;
  final IconData icon;
  final String title;
  final VoidCallback onTap;
}

/// The composer's input actions behind one button: a sheet on a phone (no
/// hardware keyboard, so no key for the history), a popup on a wide window.
class _InputMenu extends StatelessWidget {
  const _InputMenu({required this.actions});

  final List<_InputAction> actions;

  @override
  Widget build(BuildContext context) {
    if (isCompact(context)) {
      return IconButton(
        key: const Key('composer-menu'),
        tooltip: 'Input actions',
        icon: const Icon(Icons.add_circle_outline),
        onPressed: () => _showSheet(context),
      );
    }
    return PopupMenuButton<VoidCallback>(
      key: const Key('composer-menu'),
      tooltip: 'Input actions',
      icon: const Icon(Icons.add_circle_outline),
      onSelected: (action) => action(),
      itemBuilder: (context) => [
        for (final a in actions)
          PopupMenuItem(
            key: a.key,
            value: a.onTap,
            child: ListTile(
              leading: Icon(a.icon),
              title: Text(a.title),
              contentPadding: EdgeInsets.zero,
            ),
          ),
      ],
    );
  }

  /// The actions as a sheet; one tapped closes the sheet, then runs.
  void _showSheet(BuildContext context) {
    showModalBottomSheet<void>(
      context: context,
      showDragHandle: true,
      builder: (sheet) => SafeArea(
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            for (final a in actions)
              ListTile(
                key: a.key,
                leading: Icon(a.icon),
                title: Text(a.title),
                onTap: () {
                  Navigator.of(sheet).pop();
                  a.onTap();
                },
              ),
          ],
        ),
      ),
    );
  }
}
