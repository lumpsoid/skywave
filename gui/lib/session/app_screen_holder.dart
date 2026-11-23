import 'dart:async';

import 'package:flutter/foundation.dart';
import 'package:rinf/rinf.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// The open session's app screen as Rust last sent it: each update is the
/// whole screen, so this keeps only the last one. Lives for the app's
/// lifetime.
class AppScreenHolder extends ChangeNotifier {
  StreamSubscription<RustSignalPack<AppScreenUpdate>>? _subscription;

  /// The screen as Rust last sent it; null before any update.
  AppScreenUpdate? get update => _update;
  AppScreenUpdate? _update;

  void connect() {
    _subscription = AppScreenUpdate.rustSignalStream.listen(
      (pack) => apply(pack.message),
    );
  }

  /// Replaces the screen with [update].
  void apply(AppScreenUpdate update) {
    _update = update;
    notifyListeners();
  }

  @override
  void dispose() {
    unawaited(_subscription?.cancel());
    super.dispose();
  }
}
