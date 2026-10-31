import 'dart:async';

import 'package:flutter/foundation.dart';
import 'package:rinf/rinf.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// The timeline search as Rust last sent it. Lives for the app's lifetime.
class SearchHolder extends ChangeNotifier {
  StreamSubscription<RustSignalPack<SearchUpdate>>? _subscription;

  /// Null until the first update.
  SearchUpdate? get search => _search;
  SearchUpdate? _search;
  Map<String, SearchHit> _hits = const {};

  /// Where the query matches in the row keyed [key], if it does.
  SearchHit? hitFor(String key) => _hits[key];

  void connect() {
    _subscription = SearchUpdate.rustSignalStream.listen(
      (pack) => apply(pack.message),
    );
  }

  /// Replaces the search.
  void apply(SearchUpdate update) {
    _search = update;
    _hits = {for (final hit in update.hits) hit.key: hit};
    notifyListeners();
  }

  @override
  void dispose() {
    unawaited(_subscription?.cancel());
    super.dispose();
  }
}
