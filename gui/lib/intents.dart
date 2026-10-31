import 'package:skywave_gui/src/bindings/bindings.dart';

/// Hands a [GuiIntent] to Rust. Widgets take one instead of calling
/// `sendSignalToRust` themselves, so tests can record what they send.
typedef SendIntent = void Function(GuiIntent intent);

/// The real [SendIntent].
void sendToRust(GuiIntent intent) => intent.sendSignalToRust();

/// Shorthands for the intents widgets send most.
extension SendIntentShorthands on SendIntent {
  /// A plain core action.
  void act(ActionIntent action) => this(GuiIntentAct(action: action));
}
