import 'dart:async';
import 'dart:io';

import 'package:flutter/foundation.dart';
import 'package:path_provider/path_provider.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// Where boot stands, as Rust last reported it.
sealed class BootStatus {
  const BootStatus();
}

/// [Boot] sent, no answer yet.
final class BootPending extends BootStatus {
  const BootPending();
}

/// Rust opened the core and runs the hub task.
final class BootReady extends BootStatus {
  const BootReady();
}

/// Rust could not open the core; [message] says why. [unreadable]: the
/// local data is from another build or damaged, so a retry alone fails
/// again and resetting it is offered.
final class BootError extends BootStatus {
  const BootError(this.message, {this.unreadable = false});

  final String message;
  final bool unreadable;
}

/// Whether Rust has booted. Created at start, lives for the app's lifetime.
class BootHolder extends ChangeNotifier {
  StreamSubscription<void>? _booted;
  StreamSubscription<void>? _failed;

  BootStatus get status => _status;
  BootStatus _status = const BootPending();

  /// Subscribes to Rust's answers, then sends the first [Boot].
  Future<void> connect() async {
    _booted = Booted.rustSignalStream.listen((_) => booted());
    _failed = BootFailed.rustSignalStream.listen(
      (pack) => fail(pack.message.message, unreadable: pack.message.unreadable),
    );
    await boot();
  }

  /// Sends [Boot] with the app's support directory and this device's name.
  /// Also the retry after a failure; with [resetReplica], Rust first drops
  /// the local replica so it syncs again from the server.
  Future<void> boot({bool resetReplica = false}) async {
    _set(const BootPending());
    try {
      final dir = await getApplicationSupportDirectory();
      Boot(
        dir: dir.path,
        deviceName: deviceName(),
        resetReplica: resetReplica,
      ).sendSignalToRust();
    } on Exception catch (error) {
      fail('$error');
    }
  }

  /// Rust answered [Booted].
  void booted() => _set(const BootReady());

  /// Shows [message] instead of the spinner.
  void fail(String message, {bool unreadable = false}) =>
      _set(BootError(message, unreadable: unreadable));

  void _set(BootStatus status) {
    _status = status;
    notifyListeners();
  }

  @override
  void dispose() {
    unawaited(_booted?.cancel());
    unawaited(_failed?.cancel());
    super.dispose();
  }
}

/// The name pairing shows the server: the host name on desktop. Mobile
/// host names say nothing ("localhost"), so those get the platform's name.
String deviceName() {
  if (Platform.isAndroid || Platform.isIOS) {
    return 'skywave on ${Platform.operatingSystem}';
  }
  return Platform.localHostname;
}
