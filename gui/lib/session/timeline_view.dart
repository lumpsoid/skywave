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
/// The list is reversed: the newest row is its first item, at scroll
/// offset 0 on the bottom edge. A viewport that shrinks (the keyboard
/// opening) then keeps the newest rows in view, and a short timeline sits
/// above the composer rather than under the app bar. While the user reads
/// older rows, the scroll physics keeps the newest row in view where it
/// was when the rows below it change.
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

  /// The rows in view, newest first, and where each one's top edge sits in
  /// the list, taken when the rows change while the user reads older ones,
  /// until they are laid out. The newest of them still in the list stays
  /// put; only if all of them left does the view keep its offset.
  List<({String key, double top})>? _held;

  late final _physics = _HoldPhysics(shift: _heldShift);

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
    // Shown again (a subagent's page over it went): the hub heard the
    // other page's following last.
    if (oldWidget.send != widget.send) {
      WidgetsBinding.instance.addPostFrameCallback((_) {
        if (!mounted) return;
        widget.send(GuiIntentFollowingChanged(following: _following));
      });
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
    if (!_following && _reveal == null && _held == null) {
      final held = [
        for (final key in _keysInView())
          if (_topOf(key) case final top?) (key: key, top: top),
      ];
      if (held.isNotEmpty) _held = held;
    }
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (!mounted) return;
      _held = null;
      if (_reveal != null) {
        _stepReveal(4);
      } else if (_following) {
        _jumpToEnd();
      }
    });
  }

  /// Where the top edge of the row keyed [key] sits on screen, measured from
  /// the newest end, as last laid out.
  double? _topOf(String key) {
    final slot = _slots[key]?.findRenderObject();
    if (slot is! _RenderSlotBox) return null;
    final height = slot.height;
    if (height == null) return null;
    // The list wraps each row (repaint boundary, semantics): the list's
    // own child is a few boxes up.
    RenderObject? box = slot;
    while (box != null) {
      final parentData = box.parentData;
      if (parentData is SliverMultiBoxAdaptorParentData) {
        final offset = parentData.layoutOffset;
        if (offset == null) return null;
        return offset + height;
      }
      box = box.parent;
    }
    return null;
  }

  /// How far the newest held row still in the list moved in this layout, so
  /// the view scrolls with it and keeps it in place.
  double _heldShift() {
    final held = _held;
    if (held == null) return 0;
    final kept = <({String key, double top})>[];
    var shift = 0.0;
    for (final row in held) {
      final top = _topOf(row.key);
      if (top == null) continue;
      if (kept.isEmpty) shift = top - row.top;
      kept.add((key: row.key, top: top));
    }
    _held = kept;
    return shift;
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
      _setFollowing(note.metrics.extentBefore <= _endSlack);
    }
    return false;
  }

  /// Scrolls to the newest row, at offset 0.
  void _jumpToEnd() {
    if (!_scroll.hasClients || _scroll.position.pixels <= 0) return;
    _scroll.jumpTo(0);
  }

  /// Scrolls to the oldest row; a lazily built list learns its full
  /// extent as it goes, so this checks again after each frame it jumped.
  void _jumpToOldest([int tries = 6]) {
    if (!_scroll.hasClients) return;
    final position = _scroll.position;
    if (position.pixels >= position.maxScrollExtent) return;
    _scroll.jumpTo(position.maxScrollExtent);
    if (tries <= 0) return;
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (mounted) _jumpToOldest(tries - 1);
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
    final step = position.viewportDimension * 0.85 * (older ? 1 : -1);
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
      // Alignment runs from the bottom edge in a reversed list: these put
      // the line 40% and the row 20% down from the top.
      Scrollable.ensureVisible(
        lineContext ?? rowContext!,
        alignment: lineContext != null ? 0.6 : 0.8,
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
  /// built: the nearest one's offset and their mean height. Offsets grow
  /// towards older rows.
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
    return nearest.offset + (nearest.index - index) * (total / count);
  }

  /// The keys of the rows in view that are in the timeline, newest first.
  List<String> _keysInView() {
    if (!_scroll.hasClients) return const [];
    final top = _scroll.position.pixels;
    final bottom = top + _scroll.position.viewportDimension;
    final inView = <({String key, int index})>[];
    for (final MapEntry(:key, value: context) in _slots.entries) {
      final box = context.findRenderObject();
      if (box is! RenderBox || !box.hasSize) continue;
      final start = RenderAbstractViewport.of(box)
          .getOffsetToReveal(box, 0)
          .offset;
      if (start >= bottom || start + box.size.height <= top) continue;
      final index = widget.timeline.indexOf(key);
      if (index >= 0) inView.add((key: key, index: index));
    }
    inView.sort((a, b) => b.index.compareTo(a.index));
    return [for (final item in inView) item.key];
  }

  /// The newest row in view, for a search step that starts from it.
  String? newestVisibleKey() {
    final keys = _keysInView();
    return keys.isEmpty ? null : keys.first;
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
      _reveal = null;
      _jumpToOldest();
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
                      physics: _physics,
                      reverse: true,
                      padding: EdgeInsets.fromLTRB(side, 8, side, 24),
                      itemCount: rows.length,
                      findChildIndexCallback: (key) {
                        final index = widget.timeline.indexOf(
                          (key as ValueKey<String>).value,
                        );
                        return index < 0 ? null : rows.length - 1 - index;
                      },
                      itemBuilder: (context, i) {
                        final row = rows[rows.length - 1 - i];
                        final current = row.key == currentKey;
                        return _Slot(
                          key: ValueKey(row.key),
                          rowKey: row.key,
                          slots: _slots,
                          child: RowTile(
                            sessionId: widget.sessionId,
                            row: row,
                            send: widget.send,
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

/// The platform's physics, plus a [shift] applied whenever a layout
/// changes the list's extent: the newest end is at offset 0, so rows that
/// arrive or grow there would otherwise push the rows in view up.
class _HoldPhysics extends ScrollPhysics {
  const _HoldPhysics({required this.shift, super.parent});

  final double Function() shift;

  @override
  _HoldPhysics applyTo(ScrollPhysics? ancestor) =>
      _HoldPhysics(shift: shift, parent: buildParent(ancestor));

  @override
  double adjustPositionForNewDimensions({
    required ScrollMetrics oldPosition,
    required ScrollMetrics newPosition,
    required bool isScrolling,
    required double velocity,
  }) =>
      super.adjustPositionForNewDimensions(
        oldPosition: oldPosition,
        newPosition: newPosition,
        isScrolling: isScrolling,
        velocity: velocity,
      ) +
      shift();
}

/// A built row, registered by key while it is in the tree so the timeline
/// can find where it is. A removed row leaves at once, when it is
/// deactivated: it is disposed only at the end of the frame, and asking
/// it for its render object during that frame's layout fails.
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
  void activate() {
    super.activate();
    widget.slots[widget.rowKey] = context;
  }

  @override
  void deactivate() {
    if (identical(widget.slots[widget.rowKey], context)) {
      widget.slots.remove(widget.rowKey);
    }
    super.deactivate();
  }

  @override
  Widget build(BuildContext context) => _SlotBox(child: widget.child);
}

/// Sizes its child and keeps the height, so the hold can read a row's height
/// from the row itself.
class _SlotBox extends SingleChildRenderObjectWidget {
  const _SlotBox({super.child});

  @override
  RenderObject createRenderObject(BuildContext context) => _RenderSlotBox();
}

/// Keeps the row's height from its own layout: the hold needs it during the
/// viewport's layout, when a row's size may not be read.
class _RenderSlotBox extends RenderProxyBox {
  double? height;

  @override
  void performLayout() {
    super.performLayout();
    height = size.height;
  }
}
