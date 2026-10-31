import 'dart:async';

import 'package:flutter/foundation.dart';
import 'package:rinf/rinf.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// The open session's timeline as Rust last sent it: a snapshot, with the
/// keyed diffs after it folded in. Also which row is selected, by key, so
/// the selection stays on its row as rows arrive. Lives for the app's
/// lifetime.
class TimelineHolder extends ChangeNotifier {
  StreamSubscription<RustSignalPack<TimelineUpdate>>? _subscription;

  /// The session the rows are of; null when none is open.
  String? get sessionId => _sessionId;
  String? _sessionId;

  /// The rows, in timeline order.
  List<TimelineRow> get rows => List.unmodifiable(_rows);
  List<TimelineRow> _rows = [];

  /// The selected row's key. May name a row that is gone; [selected] is
  /// then null.
  String? get selectedKey => _selectedKey;
  String? _selectedKey;

  /// The selected row, if it is in the list.
  TimelineRow? get selected {
    for (final row in _rows) {
      if (row.key == _selectedKey) return row;
    }
    return null;
  }

  /// Where the row keyed [key] is, or -1; for a keyed list's
  /// `findChildIndexCallback`.
  int indexOf(String key) => _rows.indexWhere((row) => row.key == key);

  void connect() {
    _subscription = TimelineUpdate.rustSignalStream.listen(
      (pack) => apply(pack.message),
    );
  }

  /// Folds one update in: a snapshot replaces the rows; `Upserted` drops
  /// the row with its key (or the pending one it replaces) and inserts it
  /// at its index; `Removed` drops one. A selection on a pending message
  /// follows it to the logged one that replaces it.
  void apply(TimelineUpdate update) {
    if (update is TimelineUpdateSnapshot) {
      if (update.sessionId != _sessionId) _selectedKey = null;
      _sessionId = update.sessionId;
      _rows = List.of(update.items);
    } else if (update is TimelineUpdateUpserted) {
      _rows.removeWhere(
        (row) => row.key == update.key || row.key == update.replaces,
      );
      _rows.insert(update.index, update.item);
      if (update.replaces != null && _selectedKey == update.replaces) {
        _selectedKey = update.key;
      }
    } else if (update is TimelineUpdateRemoved) {
      _rows.removeWhere((row) => row.key == update.key);
    }
    notifyListeners();
  }

  /// Selects the row keyed [key]; null clears the selection.
  void select(String? key) {
    _selectedKey = key;
    notifyListeners();
  }

  @override
  void dispose() {
    unawaited(_subscription?.cancel());
    super.dispose();
  }
}
