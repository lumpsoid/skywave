import 'package:material_ui/material_ui.dart';

/// Keeps a page's content to a readable width on wide windows, centred;
/// on a phone it is the whole width.
class ReadableWidth extends StatelessWidget {
  const ReadableWidth({required this.child, this.maxWidth = 840, super.key});

  final Widget child;
  final double maxWidth;

  @override
  Widget build(BuildContext context) {
    return Align(
      alignment: Alignment.topCenter,
      child: ConstrainedBox(
        constraints: BoxConstraints(maxWidth: maxWidth),
        child: child,
      ),
    );
  }
}

/// A back arrow that sends `Back` while Rust says it goes somewhere; the
/// navigator holds one page, so the app bar never implies one.
Widget? backButton({required bool canGoBack, required VoidCallback onBack}) =>
    canGoBack ? BackButton(onPressed: onBack) : null;

/// A dialog built from state rather than pushed: it shows while its
/// prompt is in the screen Rust sent and goes when Rust clears it. A tap
/// outside it is [onDismiss].
class StateDialog extends StatelessWidget {
  const StateDialog({
    required this.child,
    required this.onDismiss,
    required this.label,
    super.key,
  });

  final Widget child;
  final VoidCallback onDismiss;

  /// What the barrier announces to screen readers.
  final String label;

  @override
  Widget build(BuildContext context) {
    return Stack(
      children: [
        ModalBarrier(
          color: Colors.black54,
          onDismiss: onDismiss,
          semanticsLabel: label,
        ),
        Center(child: child),
      ],
    );
  }
}
