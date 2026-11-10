import 'dart:async';

import 'package:flutter/foundation.dart';
import 'package:rinf/rinf.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// The link as Rust last sent it. Lives for the app's lifetime.
class LinkHolder extends ChangeNotifier {
  StreamSubscription<RustSignalPack<LinkUpdate>>? _subscription;

  /// Null until the first update.
  LinkUpdate? get link => _link;
  LinkUpdate? _link;

  /// The login expiry warning the user dismissed. Rust words it by the
  /// day (`expires in 2 days`), so tomorrow's warning shows again.
  String? _dismissedLogin;

  /// The login expiry warning to show: Rust's, unless dismissed.
  String? get loginWarning => switch (_link?.loginLabel) {
    final label? when label != _dismissedLogin => label,
    _ => null,
  };

  /// The banner shows: the link stopped until the user acts, or the
  /// agent's login needs attention. The banner then takes the top inset,
  /// so the screen below must not take it too; both ask here.
  bool get bannerShown => switch (_link) {
    final link? =>
      link.label.isNotEmpty ||
          link.loggedOutLabel != null ||
          loginWarning != null,
    null => false,
  };

  void connect() {
    _subscription = LinkUpdate.rustSignalStream.listen(
      (pack) => apply(pack.message),
    );
  }

  /// Replaces the link.
  void apply(LinkUpdate update) {
    _link = update;
    notifyListeners();
  }

  /// Hides the login expiry warning until Rust words it differently.
  void dismissLoginWarning() {
    _dismissedLogin = _link?.loginLabel;
    notifyListeners();
  }

  @override
  void dispose() {
    unawaited(_subscription?.cancel());
    super.dispose();
  }
}
