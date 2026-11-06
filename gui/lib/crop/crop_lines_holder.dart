import 'dart:async';

import 'package:flutter/foundation.dart';
import 'package:rinf/rinf.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// The crop page's line window as Rust last sent it: the lines `GuiIntentCropLines`
/// asked for, each marked kept or not, sent again whenever they or the
/// selection change. Only the last window is held. Lives for the app's
/// lifetime.
class CropLinesHolder extends ChangeNotifier {
  StreamSubscription<RustSignalPack<CropLinesUpdate>>? _subscription;

  CropLinesUpdate? _window;

  void connect() {
    _subscription = CropLinesUpdate.rustSignalStream.listen(
      (pack) => apply(pack.message),
    );
  }

  /// Replaces the window.
  void apply(CropLinesUpdate update) {
    _window = update;
    notifyListeners();
  }

  /// Line [index] (0 the first) of the blob [blobId], if the window holds
  /// it.
  CropLine? line(String blobId, int index) {
    final window = _window;
    if (window == null || window.blobId != blobId) return null;
    final at = index - window.from;
    if (at < 0 || at >= window.lines.length) return null;
    return window.lines[at];
  }

  /// Why the window of [blobId] could not be read, if it could not.
  String? error(String blobId) {
    final window = _window;
    return window != null && window.blobId == blobId ? window.error : null;
  }

  @override
  void dispose() {
    unawaited(_subscription?.cancel());
    super.dispose();
  }
}
