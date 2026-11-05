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

  /// The rows, in timeline order.
  List<TimelineRow> get rows => List.unmodifiable(_rows);
  List<TimelineRow> _rows = [];

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
    notifyListeners();
  }

  @override
  void dispose() {
    unawaited(_subscription?.cancel());
    super.dispose();
  }
}
