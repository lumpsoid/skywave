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

  /// The banner shows: Rust's label, or a login warning, has something to
  /// say. The banner then takes the top inset, so the screen below must
  /// not take it too; both ask here.
  bool get bannerShown => switch (_link) {
    final link? =>
      link.label.isNotEmpty ||
          link.loggedOutLabel != null ||
          link.loginLabel != null,
    null => false,
  };

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
