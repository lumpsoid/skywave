import 'dart:async';

import 'package:material_ui/material_ui.dart';
import 'package:flutter/services.dart';
import 'package:open_filex/open_filex.dart';
import 'package:rinf/rinf.dart';
import 'package:skywave_gui/effects/background_service.dart';
import 'package:skywave_gui/effects/notifications.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// Carries out platform effects; one method per [PlatformEffect] variant.
abstract interface class EffectSink {
  void notify({
    required int id,
    required String title,
    required String body,
    String? sessionId,
  });
  void dismiss(int id);
  void copy(String text);
  void openFile(String path);
  void notice(String text);
  void error(String text);
  void backgroundService({required bool run});
}

/// Hands every [PlatformEffect] Rust sends to its [EffectSink] method, and
/// keeps nothing. Lives for the app's lifetime.
class EffectsListener {
  EffectsListener(this.sink);

  final EffectSink sink;
  StreamSubscription<RustSignalPack<PlatformEffect>>? _subscription;

  void connect() {
    _subscription = PlatformEffect.rustSignalStream.listen(
      (pack) => apply(pack.message),
    );
  }

  /// Performs one effect.
  void apply(PlatformEffect effect) {
    if (effect is PlatformEffectNotify) {
      sink.notify(
        id: effect.id,
        title: effect.title,
        body: effect.body,
        sessionId: effect.sessionId,
      );
    } else if (effect is PlatformEffectDismiss) {
      sink.dismiss(effect.id);
    } else if (effect is PlatformEffectCopyToClipboard) {
      sink.copy(effect.text);
    } else if (effect is PlatformEffectOpenFile) {
      sink.openFile(effect.path);
    } else if (effect is PlatformEffectNotice) {
      sink.notice(effect.text);
    } else if (effect is PlatformEffectError) {
      sink.error(effect.text);
    } else if (effect is PlatformEffectBackgroundService) {
      sink.backgroundService(run: effect.run);
    }
  }

  void dispose() => unawaited(_subscription?.cancel());
}

/// The app's sink: system notifications, the clipboard, the platform's
/// file opener, snackbars through [messenger], and Android's
/// stay-connected service.
class AppEffectSink implements EffectSink {
  AppEffectSink(this.messenger, this.notifications, this.background);

  final GlobalKey<ScaffoldMessengerState> messenger;
  final SystemNotifications notifications;
  final BackgroundService background;

  @override
  void notify({
    required int id,
    required String title,
    required String body,
    String? sessionId,
  }) => unawaited(
    notifications.show(id: id, title: title, body: body, sessionId: sessionId),
  );

  @override
  void dismiss(int id) => unawaited(notifications.dismiss(id));

  @override
  void copy(String text) =>
      unawaited(Clipboard.setData(ClipboardData(text: text)));

  /// Opens a downloaded blob with the platform's default handler. Blobs are
  /// text files without an extension, so Android and iOS are told so.
  @override
  void openFile(String path) => unawaited(_open(path));

  Future<void> _open(String path) async {
    final result = await OpenFilex.open(
      path,
      type: 'text/plain',
      uti: 'public.plain-text',
    );
    if (result.type != ResultType.done) {
      _show('Cannot open the file: ${result.message}', error: true);
    }
  }

  @override
  void notice(String text) => _show(text);

  @override
  void error(String text) => _show(text, error: true);

  @override
  void backgroundService({required bool run}) =>
      unawaited(run ? background.start() : background.stop());

  void _show(String text, {bool error = false}) {
    final state = messenger.currentState;
    if (state == null) return;
    final colors = Theme.of(state.context).colorScheme;
    state.showSnackBar(
      SnackBar(
        content: Text(text),
        backgroundColor: error ? colors.error : null,
      ),
    );
  }
}
