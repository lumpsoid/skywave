import 'dart:async';

import 'package:flutter/foundation.dart';
import 'package:rinf/rinf.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// Which page the navigator shows; one per [ScreenView] variant.
enum ScreenKind {
  pairing,
  sessionList,
  session,
  settings,
  usage,
  storage,
  usageLimits,
  sessionInfo,
  workdir,
  crop,
}

/// The screen as Rust last sent it: what the navigator shows, and the
/// data of the pairing, session list, session, settings, usage, storage,
/// usage limits, session info, workdir and crop pages. Lives for the app's lifetime.
class ScreenHolder extends ChangeNotifier {
  StreamSubscription<RustSignalPack<ScreenUpdate>>? _subscription;

  /// Null until the first update.
  ScreenUpdate? get screen => _screen;
  ScreenUpdate? _screen;

  /// The page for [screen], or null before the first update.
  ScreenKind? get kind => switch (_screen?.screen) {
    ScreenViewPairing() => ScreenKind.pairing,
    ScreenViewSessionList() => ScreenKind.sessionList,
    ScreenViewSession() => ScreenKind.session,
    ScreenViewSettings() => ScreenKind.settings,
    ScreenViewUsage() => ScreenKind.usage,
    ScreenViewStorage() => ScreenKind.storage,
    ScreenViewUsageLimits() => ScreenKind.usageLimits,
    ScreenViewSessionInfo() => ScreenKind.sessionInfo,
    ScreenViewWorkdir() => ScreenKind.workdir,
    ScreenViewCrop() => ScreenKind.crop,
    _ => null,
  };

  void connect() {
    _subscription = ScreenUpdate.rustSignalStream.listen(
      (pack) => apply(pack.message),
    );
  }

  /// Replaces the screen.
  void apply(ScreenUpdate update) {
    _screen = update;
    notifyListeners();
  }

  @override
  void dispose() {
    unawaited(_subscription?.cancel());
    super.dispose();
  }
}
