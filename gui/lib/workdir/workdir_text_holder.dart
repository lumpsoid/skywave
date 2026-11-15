import 'dart:async';

import 'package:flutter/foundation.dart';
import 'package:rinf/rinf.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// The text of the diff or file the workdir page shows, as Rust last sent
/// it: one blob's lines, sent once per blob. Only the last text is held.
/// Lives for the app's lifetime.
class WorkdirTextHolder extends ChangeNotifier {
  StreamSubscription<RustSignalPack<WorkdirTextUpdate>>? _subscription;

  WorkdirTextUpdate? _text;

  void connect() {
    _subscription = WorkdirTextUpdate.rustSignalStream.listen(
      (pack) => apply(pack.message),
    );
  }

  /// Replaces the text.
  void apply(WorkdirTextUpdate update) {
    _text = update;
    notifyListeners();
  }

  /// The lines of the blob [blobId], if the held text is of it; none
  /// otherwise.
  List<WorkdirLine> lines(String blobId) {
    final text = _text;
    return text != null && text.blobId == blobId ? text.lines : const [];
  }

  @override
  void dispose() {
    unawaited(_subscription?.cancel());
    super.dispose();
  }
}
