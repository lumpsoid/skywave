import 'dart:math' as math;

import 'package:flutter/material.dart';
import 'package:flutter/rendering.dart';
import 'package:flutter/services.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/session/look.dart';
import 'package:skywave_gui/session/row_tile.dart';
import 'package:skywave_gui/session/search_holder.dart';
import 'package:skywave_gui/session/timeline_holder.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// How close to the newest end still counts as showing it.
const _endSlack = 32.0;

/// The widest the timeline's column grows on a desktop window.
const timelineMaxWidth = 920.0;

/// The open session's timeline: a lazily built list keyed by row key, so
/// rows that arrive, change or move keep their place and scroll offset.
///
/// It follows the newest row until the user scrolls away from it, and
/// tells the hub whenever that changes (`FollowingChanged`: the read marks
/// depend on it); a button brings it back. It scrolls to the current
/// search match (and its line), and [reveal]s rows on request.
///
/// With no rows it says how to start, once the hub's rows are of
/// [sessionId]: until then the rows are another session's or none sent
/// yet, so it shows nothing.
class TimelineView extends StatefulWidget {
  const TimelineView({
    required this.sessionId,
    required this.timeline,
    required this.search,
    required this.send,
    super.key,
  });

  /// The session shown.
  final String sessionId;
  final TimelineHolder timeline;
  final SearchHolder search;
  final SendIntent send;

  @override
  State<TimelineView> createState() => TimelineViewState();
}

class TimelineViewState extends State<TimelineView> {
  final _scroll = ScrollController();
  final _lineKey = GlobalKey(debugLabel: 'current match line');

  /// Holds keyboard focus for n/N and the arrows while no field does.
  final focusNode = FocusNode(debugLabel: 'timeline');

  /// The rows that are built, by key.
  final _slots = <String, BuildContext>{};

  bool _following = true;
  String? _sessionId;

  /// A row (and maybe its current match line) to bring into view, kept
  /// until it is in view or the user scrolls.
  ({String key, bool line})? _reveal;

  /// The search match last scrolled to.
  (String?, int?) _searchTarget = (null, null);

  /// Whether the newest row is in view.
  bool get following => _following;

  @override
  void initState() {
    super.initState();
    _sessionId = widget.timeline.sessionId;
    widget.timeline.addListener(_rowsChanged);
    widget.search.addListener(_searchChanged);
    _searchTarget = _targetOf(widget.search.search);
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (!mounted) return;
      _jumpToEnd();
      widget.send(const GuiIntentFollowingChanged(following: true));
    });
  }

  @override
  void didUpdateWidget(TimelineView oldWidget) {
    super.didUpdateWidget(oldWidget);
    if (oldWidget.timeline != widget.timeline) {
      oldWidget.timeline.removeListener(_rowsChanged);
      widget.timeline.addListener(_rowsChanged);
    }
    if (oldWidget.search != widget.search) {
      oldWidget.search.removeListener(_searchChanged);
      widget.search.addListener(_searchChanged);
    }
  }

  @override
  void dispose() {
    widget.timeline.removeListener(_rowsChanged);
    widget.search.removeListener(_searchChanged);
    _scroll.dispose();
    focusNode.dispose();
    super.dispose();
  }

  void _rowsChanged() {
    setState(() {});
    if (widget.timeline.sessionId != _sessionId) {
      // Another session: start at its newest row.
      _sessionId = widget.timeline.sessionId;
      _reveal = null;
      _setFollowing(true, force: true);
    }
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (!mounted) return;
      if (_reveal != null) {
        _stepReveal(4);
      } else if (_following) {
        _jumpToEnd();
      }
    });
  }

  (String?, int?) _targetOf(SearchUpdate? search) =>
      (search?.currentKey, search?.currentLine);

  void _searchChanged() {
    setState(() {});
    final target = _targetOf(widget.search.search);
    if (target == _searchTarget) return;
    _searchTarget = target;
    final (key, line) = target;
    if (key != null) reveal(key, line: line != null);
  }

  void _setFollowing(bool following, {bool force = false}) {
    if (following == _following && !force) return;
    setState(() => _following = following);
    widget.send(GuiIntentFollowingChanged(following: following));
  }

  bool _onScroll(ScrollNotification note) {
    if (note.depth != 0) return false;
    if (note is UserScrollNotification) _reveal = null;
    if (note is ScrollUpdateNotification || note is ScrollEndNotification) {
      _setFollowing(note.metrics.extentAfter <= _endSlack);
    }
    return false;
  }

  /// Scrolls to the newest row; a lazily built list learns its full
  /// extent as it goes, so this checks again after each frame it jumped.
  void _jumpToEnd([int tries = 6]) {
    if (!_scroll.hasClients) return;
    final position = _scroll.position;
    if (position.pixels >= position.maxScrollExtent) return;
    _scroll.jumpTo(position.maxScrollExtent);
    if (tries <= 0) return;
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (mounted && _following) _jumpToEnd(tries - 1);
    });
  }

  /// Jumps to the newest row and follows it again.
  void jumpToNewest() {
    _reveal = null;
    _setFollowing(true);
    _jumpToEnd();
  }

  /// Scrolls a viewport's worth towards older ([older]) or newer rows.
  void page({required bool older}) {
    if (!_scroll.hasClients) return;
    final position = _scroll.position;
    final step = position.viewportDimension * 0.85 * (older ? -1 : 1);
    _scroll.animateTo(
      (position.pixels + step).clamp(0, position.maxScrollExtent),
      duration: const Duration(milliseconds: 180),
      curve: Curves.easeOut,
    );
  }

  /// Brings the row keyed [key] into view, and with [line] the current
  /// search match's line in it, once it is built.
  void reveal(String key, {bool line = false}) {
    _reveal = (key: key, line: line);
    _stepReveal(10);
  }

  void _stepReveal(int tries) {
    final target = _reveal;
    if (target == null || !_scroll.hasClients) return;
    final lineContext = target.line ? _lineKey.currentContext : null;
    final rowContext = _slots[target.key];
    if (lineContext != null || rowContext != null) {
      Scrollable.ensureVisible(
        lineContext ?? rowContext!,
        alignment: lineContext != null ? 0.4 : 0.2,
        alignmentPolicy: ScrollPositionAlignmentPolicy.explicit,
      );
      // A match line may show only once the hub expanded its row: keep
      // the target for the next rows until it does.
      if (!target.line || lineContext != null) _reveal = null;
      return;
    }
    final index = widget.timeline.indexOf(target.key);
    if (index < 0 || tries <= 0) return;
    final guess = _estimate(index);
    if (guess == null) return;
    final position = _scroll.position;
    _scroll.jumpTo(guess.clamp(0, position.maxScrollExtent));
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (mounted) _stepReveal(tries - 1);
    });
  }

  /// Where the row at [index] probably starts, from the rows that are
  /// built: the nearest one's offset and their mean height.
  double? _estimate(int index) {
    var total = 0.0;
    var count = 0;
    ({int index, double offset})? nearest;
    for (final MapEntry(:key, value: context) in _slots.entries) {
      final box = context.findRenderObject();
      if (box is! RenderBox || !box.hasSize) continue;
      final at = widget.timeline.indexOf(key);
      if (at < 0) continue;
      total += box.size.height;
      count++;
      final offset = RenderAbstractViewport.of(box)
          .getOffsetToReveal(box, 0)
          .offset;
      if (nearest == null ||
          (at - index).abs() < (nearest.index - index).abs()) {
        nearest = (index: at, offset: offset);
      }
    }
    if (nearest == null || count == 0) return null;
    return nearest.offset + (index - nearest.index) * (total / count);
  }

  /// The newest row in view, for a search step that starts from it.
  String? newestVisibleKey() {
    if (!_scroll.hasClients) return null;
    final top = _scroll.position.pixels;
    final bottom = top + _scroll.position.viewportDimension;
    String? newest;
    var newestIndex = -1;
    for (final MapEntry(:key, value: context) in _slots.entries) {
      final box = context.findRenderObject();
      if (box is! RenderBox || !box.hasSize) continue;
      final start = RenderAbstractViewport.of(box)
          .getOffsetToReveal(box, 0)
          .offset;
      if (start >= bottom || start + box.size.height <= top) continue;
      final index = widget.timeline.indexOf(key);
      if (index > newestIndex) {
        newestIndex = index;
        newest = key;
      }
    }
    return newest;
  }

  KeyEventResult _onKey(FocusNode node, KeyEvent event) {
    if (event is! KeyDownEvent && event is! KeyRepeatEvent) {
      return KeyEventResult.ignored;
    }
    final search = widget.search.search;
    final searching = search != null && search.open;
    final shift = HardwareKeyboard.instance.isShiftPressed;
    final key = event.logicalKey;
    if (searching && key == LogicalKeyboardKey.keyN) {
      widget.send(GuiIntentSearchStep(older: !shift));
      return KeyEventResult.handled;
    }
    if (key == LogicalKeyboardKey.home) {
      if (_scroll.hasClients) _scroll.jumpTo(0);
      return KeyEventResult.handled;
    }
    if (key == LogicalKeyboardKey.end) {
      jumpToNewest();
      return KeyEventResult.handled;
    }
    return KeyEventResult.ignored;
  }

  @override
  Widget build(BuildContext context) {
    final rows = widget.timeline.rows;
    final selectedKey = widget.timeline.selectedKey;
    final search = widget.search.search;
    final currentKey = search != null && search.open ? search.currentKey : null;
    final searching = search != null && search.open;

    return Focus(
      focusNode: focusNode,
      onKeyEvent: _onKey,
      child: Listener(
        onPointerDown: (_) {
          if (!focusNode.hasFocus) focusNode.requestFocus();
        },
        child: Stack(
          children: [
            NotificationListener<ScrollNotification>(
              onNotification: _onScroll,
              child: LayoutBuilder(
                builder: (context, constraints) {
                  final side = math.max(
                    0.0,
                    (constraints.maxWidth - timelineMaxWidth) / 2,
                  );
                  return Scrollbar(
                    controller: _scroll,
                    child: ListView.builder(
                      controller: _scroll,
                      padding: EdgeInsets.fromLTRB(side, 8, side, 24),
                      itemCount: rows.length,
                      findChildIndexCallback: (key) {
                        final index = widget.timeline.indexOf(
                          (key as ValueKey<String>).value,
                        );
                        return index < 0 ? null : index;
                      },
                      itemBuilder: (context, i) {
                        final row = rows[i];
                        final current = row.key == currentKey;
                        return _Slot(
                          key: ValueKey(row.key),
                          rowKey: row.key,
                          slots: _slots,
                          child: RowTile(
                            row: row,
                            send: widget.send,
                            selected: row.key == selectedKey,
                            onSelect: () => widget.timeline.select(row.key),
                            hit: searching
                                ? widget.search.hitFor(row.key)
                                : null,
                            current: current,
                            currentLine: current ? search?.currentLine : null,
                            currentLineKey: current ? _lineKey : null,
                          ),
                        );
                      },
                    ),
                  );
                },
              ),
            ),
            if (rows.isEmpty && widget.timeline.sessionId == widget.sessionId)
              Positioned.fill(
                child: Center(
                  child: Padding(
                    padding: const EdgeInsets.all(24),
                    child: Text(
                      'Send a message to start the agent.',
                      textAlign: TextAlign.center,
                      style: Theme.of(context).textTheme.bodyMedium
                          ?.copyWith(color: SkyColors.of(context).muted),
                    ),
                  ),
                ),
              ),
            Positioned(
              right: 16,
              bottom: 16,
              child: AnimatedScale(
                scale: _following ? 0 : 1,
                duration: const Duration(milliseconds: 150),
                child: FloatingActionButton.small(
                  heroTag: null,
                  tooltip: 'Jump to newest',
                  onPressed: _following ? null : jumpToNewest,
                  child: const Icon(Icons.keyboard_double_arrow_down),
                ),
              ),
            ),
          ],
        ),
      ),
    );
  }
}

/// A built row, registered by key while it is mounted so the timeline can
/// find where it is.
class _Slot extends StatefulWidget {
  const _Slot({
    required this.rowKey,
    required this.slots,
    required this.child,
    super.key,
  });

  final String rowKey;
  final Map<String, BuildContext> slots;
  final Widget child;

  @override
  State<_Slot> createState() => _SlotState();
}

class _SlotState extends State<_Slot> {
  @override
  void initState() {
    super.initState();
    widget.slots[widget.rowKey] = context;
  }

  @override
  void dispose() {
    if (identical(widget.slots[widget.rowKey], context)) {
      widget.slots.remove(widget.rowKey);
    }
    super.dispose();
  }

  @override
  Widget build(BuildContext context) => widget.child;
}
