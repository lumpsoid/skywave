import 'dart:async';

import 'package:flutter/foundation.dart';
import 'package:rinf/rinf.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// The link banner as Rust last sent it. Lives for the app's lifetime.
class LinkHolder extends ChangeNotifier {
  StreamSubscription<RustSignalPack<LinkUpdate>>? _subscription;

  /// Null until the first update.
  LinkUpdate? get link => _link;
  LinkUpdate? _link;

  void connect() {
    _subscription = LinkUpdate.rustSignalStream.listen(
      (pack) => apply(pack.message),
    );
  }

  /// Replaces the banner.
  void apply(LinkUpdate update) {
    _link = update;
    notifyListeners();
  }

  @override
  void dispose() {
    unawaited(_subscription?.cancel());
    super.dispose();
  }
}
