import 'package:material_ui/material_ui.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/session/highlight.dart';
import 'package:skywave_gui/session/look.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// The shell history picker, an inline list over the composer's field (not
/// a modal). Rust owns the rows and the filter, which follows the field's
/// text; this only draws them. Newest is at the bottom, nearest the field,
/// as fzf lays it out, and the count sits under the list as fzf's info line.
class HistoryPicker extends StatefulWidget {
  const HistoryPicker({required this.view, required this.send, super.key});

  final HistorySearchView view;
  final SendIntent send;

  @override
  State<HistoryPicker> createState() => _HistoryPickerState();
}

class _HistoryPickerState extends State<HistoryPicker> {
  static const _rowHeight = 32.0;
  static const _visibleRows = 8;

  final _list = ScrollController();

  @override
  void didUpdateWidget(HistoryPicker old) {
    super.didUpdateWidget(old);
    if (widget.view.selected != old.view.selected) {
      WidgetsBinding.instance.addPostFrameCallback((_) => _reveal());
    }
  }

  /// Scrolls the selected row into view. The list is reversed, so offset 0
  /// is the bottom and row `i` starts `i` rows up from it.
  void _reveal() {
    final selected = widget.view.selected;
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

  @override
  void dispose() {
    _list.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final colors = SkyColors.of(context);
    final view = widget.view;
    final rows = view.rows;
    // Room for one row when empty, so the empty label has a line to sit in.
    final shown = rows.isEmpty ? 1 : rows.length.clamp(1, _visibleRows);
    return Padding(
      padding: const EdgeInsets.only(bottom: 6),
      child: Card.outlined(
        key: const Key('history-picker'),
        margin: EdgeInsets.zero,
        child: Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            SizedBox(
              height: shown * _rowHeight,
              child: rows.isEmpty
                  ? Center(
                      child: Text(
                        view.emptyLabel ?? '',
                        style: theme.textTheme.bodySmall?.copyWith(
                          color: colors.muted,
                        ),
                      ),
                    )
                  : ListView.builder(
                      controller: _list,
                      reverse: true,
                      itemExtent: _rowHeight,
                      itemCount: rows.length,
                      itemBuilder: (context, i) => _HistoryTile(
                        key: Key('history-row-$i'),
                        row: rows[i],
                        selected: i == view.selected,
                        onTap: () =>
                            widget.send(GuiIntentHistorySearchPick(index: i)),
                      ),
                    ),
            ),
            Padding(
              padding: const EdgeInsets.fromLTRB(12, 2, 12, 4),
              child: Text(
                view.countLabel,
                key: const Key('history-count'),
                style: theme.textTheme.labelSmall?.copyWith(
                  color: colors.muted,
                ),
              ),
            ),
          ],
        ),
      ),
    );
  }
}

/// One command: its text in mono with the matched characters marked, on the
/// selected row's background when it is the highlight.
class _HistoryTile extends StatelessWidget {
  const _HistoryTile({
    required this.row,
    required this.selected,
    required this.onTap,
    super.key,
  });

  final HistoryRowView row;
  final bool selected;
  final VoidCallback onTap;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final mark = MatchStyle.of(context).match;
    return InkWell(
      onTap: onTap,
      child: Container(
        color: selected ? theme.colorScheme.secondaryContainer : null,
        padding: const EdgeInsets.symmetric(horizontal: 12),
        alignment: Alignment.centerLeft,
        child: Text.rich(
          TextSpan(children: highlighted(row.command, row.matched, mark)),
          maxLines: 1,
          overflow: TextOverflow.ellipsis,
          style: monoText,
        ),
      ),
    );
  }
}
