import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:rinf/rinf.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// Carries out platform effects; one method per [PlatformEffect] variant.
abstract interface class EffectSink {
  void notify({required String title, required String body, String? sessionId});
  void copy(String text);
  void openFile(String path);
  void notice(String text);
  void error(String text);
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
        title: effect.title,
        body: effect.body,
        sessionId: effect.sessionId,
      );
    } else if (effect is PlatformEffectCopyToClipboard) {
      sink.copy(effect.text);
    } else if (effect is PlatformEffectOpenFile) {
      sink.openFile(effect.path);
    } else if (effect is PlatformEffectNotice) {
      sink.notice(effect.text);
    } else if (effect is PlatformEffectError) {
      sink.error(effect.text);
    }
  }

  void dispose() => unawaited(_subscription?.cancel());
}

/// The app's sink: the clipboard, and snackbars through [messenger].
/// System notifications and opening files come with their plugins (G7);
/// until then they show as snackbars too.
class AppEffectSink implements EffectSink {
  AppEffectSink(this.messenger);

  final GlobalKey<ScaffoldMessengerState> messenger;

  @override
  void notify({
    required String title,
    required String body,
    String? sessionId,
  }) => _show(body);

  @override
  void copy(String text) =>
      unawaited(Clipboard.setData(ClipboardData(text: text)));

  @override
  void openFile(String path) => _show(path);

  @override
  void notice(String text) => _show(text);

  @override
  void error(String text) => _show(text, error: true);

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
