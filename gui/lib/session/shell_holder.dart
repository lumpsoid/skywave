import 'dart:async';

import 'package:flutter/foundation.dart';
import 'package:rinf/rinf.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// The open session's shell runs as Rust last sent them: a snapshot, with
/// the keyed diffs after it folded in. A run keeps its key from the moment
/// it is queued to its end. Lives for the app's lifetime.
class ShellHolder extends ChangeNotifier {
  StreamSubscription<RustSignalPack<ShellUpdate>>? _subscription;

  /// The session the runs are of; null when none is open.
  String? get sessionId => _sessionId;
  String? _sessionId;

  /// The runs, oldest first.
  List<ShellRunRow> get runs => List.unmodifiable(_runs);
  List<ShellRunRow> _runs = [];

  void connect() {
    _subscription = ShellUpdate.rustSignalStream.listen(
      (pack) => apply(pack.message),
    );
  }

  /// Folds one update in: a snapshot replaces the runs; `Upserted` drops
  /// the run with its key and inserts it at its index; `Removed` drops
  /// one.
  void apply(ShellUpdate update) {
    if (update is ShellUpdateSnapshot) {
      _sessionId = update.sessionId;
      _runs = List.of(update.runs);
    } else if (update is ShellUpdateUpserted) {
      _runs
        ..removeWhere((run) => run.key == update.key)
        ..insert(update.index, update.run);
    } else if (update is ShellUpdateRemoved) {
      _runs.removeWhere((run) => run.key == update.key);
    }
    notifyListeners();
  }

  @override
  void dispose() {
    unawaited(_subscription?.cancel());
    super.dispose();
  }
}
