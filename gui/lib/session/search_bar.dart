import 'package:material_ui/material_ui.dart';
import 'package:flutter/services.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/session/look.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// The search through the timeline, while the hub has one open: how many
/// matches and where, and steps through them. ↑/↓ (or n/N on the timeline)
/// step older and newer; Esc closes it. The query is edited on the search
/// page: tapping it opens that page.
class SessionSearchBar extends StatefulWidget {
  const SessionSearchBar({
    required this.search,
    required this.send,
    required this.newestVisibleKey,
    required this.onStepped,
    required this.onEdit,
    this.focusNode,
    super.key,
  });

  final SearchUpdate search;
  final SendIntent send;

  /// The newest row in view, for Enter's step.
  final String? Function() newestVisibleKey;

  /// After Enter: hand the keyboard to the timeline, for n/N.
  final VoidCallback onStepped;

  /// Tapped: opens the search page, scoped to this session, to edit the
  /// query.
  final VoidCallback onEdit;
  final FocusNode? focusNode;

  @override
  State<SessionSearchBar> createState() => _SessionSearchBarState();
}

class _SessionSearchBarState extends State<SessionSearchBar> {
  late final _field = TextEditingController(text: widget.search.query);

  @override
  void didUpdateWidget(SessionSearchBar oldWidget) {
    super.didUpdateWidget(oldWidget);
    if (_field.text != widget.search.query) {
      _field.text = widget.search.query;
    }
  }

  @override
  void dispose() {
    _field.dispose();
    super.dispose();
  }

  void _step(bool? older) => widget.send(
    GuiIntentSearchStep(
      older: older,
      fromKey: older == null ? widget.newestVisibleKey() : null,
    ),
  );

  KeyEventResult _onKey(FocusNode node, KeyEvent event) {
    if (event is! KeyDownEvent && event is! KeyRepeatEvent) {
      return KeyEventResult.ignored;
    }
    final key = event.logicalKey;
    if (key == LogicalKeyboardKey.arrowUp) {
      _step(true);
      return KeyEventResult.handled;
    }
    if (key == LogicalKeyboardKey.arrowDown) {
      _step(false);
      return KeyEventResult.handled;
    }
    if (key == LogicalKeyboardKey.escape) {
      widget.send(const GuiIntentSearchClose());
      return KeyEventResult.handled;
    }
    return KeyEventResult.ignored;
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final colors = SkyColors.of(context);
    final search = widget.search;
    final small = theme.textTheme.labelSmall?.copyWith(color: colors.muted);
    return Material(
      color: theme.colorScheme.surfaceContainerHigh,
      child: Padding(
        padding: const EdgeInsets.fromLTRB(12, 6, 4, 6),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          mainAxisSize: MainAxisSize.min,
          children: [
            Row(
              children: [
                Expanded(
                  child: Focus(
                    onKeyEvent: _onKey,
                    child: TextField(
                      controller: _field,
                      focusNode: widget.focusNode,
                      autofocus: true,
                      readOnly: true,
                      textInputAction: TextInputAction.search,
                      onTap: widget.onEdit,
                      decoration: const InputDecoration(
                        isDense: true,
                        prefixIcon: Icon(Icons.search),
                        hintText: 'Search',
                        border: InputBorder.none,
                      ),
                    ),
                  ),
                ),
                Text(search.countLabel, style: theme.textTheme.labelMedium),
                IconButton(
                  tooltip: 'Older match (n)',
                  onPressed: () => _step(true),
                  icon: const Icon(Icons.keyboard_arrow_up),
                ),
                IconButton(
                  tooltip: 'Newer match (N)',
                  onPressed: () => _step(false),
                  icon: const Icon(Icons.keyboard_arrow_down),
                ),
                IconButton(
                  tooltip: 'Close search (Esc)',
                  onPressed: () => widget.send(const GuiIntentSearchClose()),
                  icon: const Icon(Icons.close),
                ),
              ],
            ),
            if (search.wrapped || search.partialLabel != null)
              Padding(
                padding: const EdgeInsets.only(left: 12, bottom: 2),
                child: Text(
                  [
                    if (search.wrapped) 'search wrapped',
                    ?search.partialLabel,
                  ].join(' · '),
                  style: small,
                ),
              ),
          ],
        ),
      ),
    );
  }
}
