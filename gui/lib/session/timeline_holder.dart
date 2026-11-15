import 'dart:async';

import 'package:flutter/foundation.dart';
import 'package:rinf/rinf.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// The open session's timeline as Rust last sent it: a snapshot, with the
/// keyed diffs after it folded in. Lives for the app's lifetime.
class TimelineHolder extends ChangeNotifier {
  StreamSubscription<RustSignalPack<TimelineUpdate>>? _subscription;

  /// The session the rows are of; null when none is open.
  String? get sessionId => _sessionId;
  String? _sessionId;

  /// The rows, in timeline order, as of the last [apply]: the same list until
  /// the next one, so a reader holding it sees no change.
  List<TimelineRow> get rows => _rowsView ??= List.unmodifiable(_rows);
  List<TimelineRow> _rows = [];
  List<TimelineRow>? _rowsView;

  /// Bumped by every [apply], so a reader can tell the rows changed without
  /// comparing them.
  int get revision => _revision;
  int _revision = 0;

  /// Where the row keyed [key] is, or -1; for a keyed list's
  /// `findChildIndexCallback`. A map lookup; the map is rebuilt at most once
  /// per [apply], and the first row with a repeated key wins.
  int indexOf(String key) => (_indexByKey ??= _buildIndex())[key] ?? -1;

  Map<String, int>? _indexByKey;

  Map<String, int> _buildIndex() {
    final index = <String, int>{};
    for (var i = 0; i < _rows.length; i++) {
      index.putIfAbsent(_rows[i].key, () => i);
    }
    return index;
  }

  void connect() {
    _subscription = TimelineUpdate.rustSignalStream.listen(
      (pack) => apply(pack.message),
    );
  }

  /// Folds one update in: a snapshot replaces the rows; `Upserted` drops
  /// the row with its key (or the pending one it replaces) and inserts it
  /// at its index; `Removed` drops one.
  void apply(TimelineUpdate update) {
    if (update is TimelineUpdateSnapshot) {
      _sessionId = update.sessionId;
      _rows = List.of(update.items);
    } else if (update is TimelineUpdateUpserted) {
      _rows.removeWhere(
        (row) => row.key == update.key || row.key == update.replaces,
      );
      _rows.insert(update.index, update.item);
    } else if (update is TimelineUpdateRemoved) {
      _rows.removeWhere((row) => row.key == update.key);
    }
    _rowsView = null;
    _indexByKey = null;
    _revision++;
    notifyListeners();
  }

  @override
  void dispose() {
    unawaited(_subscription?.cancel());
    super.dispose();
  }
}
