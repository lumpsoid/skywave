import 'package:flutter/services.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// Android's stay-connected foreground service (MainActivity,
/// StayConnectedService). [start] and [stop] only ask; Kotlin reports what
/// happened on the same channel, `running` once the service is in the
/// foreground and `stopped` when it ends or a start was refused, and each
/// report goes to Rust as [GuiIntentBackgroundService].
class BackgroundService {
  BackgroundService(
    this.send, {
    this.channel = const MethodChannel('io.github.lumpsoid.skywave/background'),
  });

  final SendIntent send;
  final MethodChannel channel;

  /// Takes Kotlin's reports. Called before `runApp`, so an early one is
  /// not lost.
  void listen() => channel.setMethodCallHandler(_report);

  Future<void> start() => _invoke('start');

  Future<void> stop() => _invoke('stop');

  Future<void> _invoke(String method) async {
    try {
      await channel.invokeMethod<void>(method);
    } on MissingPluginException {
      // Not Android: there is no service to run.
    }
  }

  Future<void> _report(MethodCall call) async {
    switch (call.method) {
      case 'running':
        send(const GuiIntentBackgroundService(running: true));
      case 'stopped':
        send(const GuiIntentBackgroundService(running: false));
      default:
        throw MissingPluginException(call.method);
    }
  }
}
