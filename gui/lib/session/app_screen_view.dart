import 'dart:io';

import 'package:material_ui/material_ui.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/session/app_screen_holder.dart';
import 'package:skywave_gui/session/look.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// The most requests the screen lists, the newest last.
const appScreenRequestRows = 5;

/// A drag shorter than this, in logical pixels, is a tap at its start.
const _swipeMinimum = 12.0;

/// The open session's app screen, in place of the timeline: the device
/// picker and its refresh, the latest capture to tap, swipe and type into,
/// the hardware keys, and the last requests. Taps and drags go out in the
/// capture's pixels.
class AppScreenView extends StatelessWidget {
  const AppScreenView({
    required this.sessionId,
    required this.holder,
    required this.send,
    super.key,
  });

  final String sessionId;
  final AppScreenHolder holder;
  final SendIntent send;

  @override
  Widget build(BuildContext context) {
    final colors = SkyColors.of(context);
    return ListenableBuilder(
      listenable: holder,
      builder: (context, _) {
        final update = holder.update;
        final devices = update?.devices ?? const <AppScreenDevice>[];
        final capture = update?.capture;
        final path = capture?.path;
        final requests = update?.requests ?? const <AppScreenRow>[];
        final recent = requests.length > appScreenRequestRows
            ? requests.sublist(requests.length - appScreenRequestRows)
            : requests;
        final selected = update?.device;
        VoidCallback? press(AppScreenKeyView key) => capture == null
            ? null
            : () => send(
                GuiIntentAppScreenKey(
                  sessionId: sessionId,
                  captureId: capture.captureId,
                  key: key,
                ),
              );
        return Column(
          children: [
            Padding(
              padding: const EdgeInsets.symmetric(horizontal: 8),
              child: Row(
                children: [
                  Expanded(
                    child: DropdownButton<String>(
                      key: const Key('app-screen-device'),
                      isExpanded: true,
                      hint: const Text('No device'),
                      value: devices.any((device) => device.serial == selected)
                          ? selected
                          : null,
                      items: [
                        for (final device in devices)
                          DropdownMenuItem(
                            value: device.serial,
                            child: Text(device.label),
                          ),
                      ],
                      onChanged: devices.isEmpty
                          ? null
                          : (serial) {
                              if (serial == null) return;
                              send(
                                GuiIntentAppScreenSelect(
                                  sessionId: sessionId,
                                  serial: serial,
                                ),
                              );
                            },
                    ),
                  ),
                  IconButton(
                    key: const Key('app-screen-devices'),
                    tooltip: 'List devices',
                    onPressed: () =>
                        send(GuiIntentAppScreenDevices(sessionId: sessionId)),
                    icon: const Icon(Icons.devices),
                  ),
                  IconButton(
                    key: const Key('app-screen-capture'),
                    tooltip: 'Capture',
                    onPressed: () =>
                        send(GuiIntentAppScreenCapture(sessionId: sessionId)),
                    icon: const Icon(Icons.refresh),
                  ),
                ],
              ),
            ),
            Expanded(
              child: capture != null && path != null
                  ? _ScreenImage(
                      sessionId: sessionId,
                      capture: capture,
                      path: path,
                      send: send,
                    )
                  : Center(
                      child: Padding(
                        padding: const EdgeInsets.all(24),
                        child: Text(
                          capture?.note ??
                              (devices.isEmpty
                                  ? 'No devices listed yet'
                                  : 'No capture yet'),
                          textAlign: TextAlign.center,
                          style: TextStyle(color: colors.muted),
                        ),
                      ),
                    ),
            ),
            Padding(
              padding: const EdgeInsets.symmetric(horizontal: 8),
              child: Row(
                mainAxisAlignment: MainAxisAlignment.spaceEvenly,
                children: [
                  IconButton(
                    key: const Key('app-screen-back'),
                    tooltip: 'Back',
                    onPressed: press(AppScreenKeyView.back),
                    icon: const Icon(Icons.arrow_back),
                  ),
                  IconButton(
                    key: const Key('app-screen-home'),
                    tooltip: 'Home',
                    onPressed: press(AppScreenKeyView.home),
                    icon: const Icon(Icons.circle_outlined),
                  ),
                  IconButton(
                    key: const Key('app-screen-recents'),
                    tooltip: 'Recents',
                    onPressed: press(AppScreenKeyView.recents),
                    icon: const Icon(Icons.crop_square),
                  ),
                ],
              ),
            ),
            _TextEntry(
              sessionId: sessionId,
              captureId: capture?.captureId,
              send: send,
            ),
            Padding(
              padding: const EdgeInsets.fromLTRB(8, 4, 8, 8),
              child: Column(
                mainAxisSize: MainAxisSize.min,
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [for (final row in recent) _RequestLine(row: row)],
              ),
            ),
          ],
        );
      },
    );
  }
}

/// The capture's image. A tap goes out as a tap; a drag as a swipe from
/// where it began to where it ended, or as a tap at its start when it is
/// short. Positions are mapped onto the capture's pixels.
class _ScreenImage extends StatefulWidget {
  const _ScreenImage({
    required this.sessionId,
    required this.capture,
    required this.path,
    required this.send,
  });

  final String sessionId;
  final AppScreenCapture capture;
  final String path;
  final SendIntent send;

  @override
  State<_ScreenImage> createState() => _ScreenImageState();
}

class _ScreenImageState extends State<_ScreenImage> {
  /// Where the drag in progress began, and where it is now, in logical
  /// pixels of the image.
  Offset _dragStart = Offset.zero;
  Offset _dragEnd = Offset.zero;

  @override
  Widget build(BuildContext context) {
    final capture = widget.capture;
    return Center(
      child: AspectRatio(
        aspectRatio: capture.width / capture.height,
        child: LayoutBuilder(
          builder: (context, box) {
            final shown = box.biggest;
            return GestureDetector(
              key: const Key('app-screen-image'),
              onTapUp: (details) => _tap(details.localPosition, shown),
              onPanStart: (details) {
                _dragStart = details.localPosition;
                _dragEnd = details.localPosition;
              },
              onPanUpdate: (details) => _dragEnd = details.localPosition,
              onPanEnd: (_) => _drag(shown),
              child: Image.file(
                File(widget.path),
                fit: BoxFit.fill,
                gaplessPlayback: true,
                errorBuilder: (context, error, stackTrace) =>
                    const ColoredBox(color: Colors.black12),
              ),
            );
          },
        ),
      ),
    );
  }

  void _tap(Offset at, Size shown) {
    final (x, y) = _pixel(at, shown);
    widget.send(
      GuiIntentAppScreenTap(
        sessionId: widget.sessionId,
        captureId: widget.capture.captureId,
        x: x,
        y: y,
      ),
    );
  }

  void _drag(Size shown) {
    final start = _dragStart;
    final end = _dragEnd;
    if ((end - start).distance < _swipeMinimum) {
      _tap(start, shown);
      return;
    }
    final (fromX, fromY) = _pixel(start, shown);
    final (toX, toY) = _pixel(end, shown);
    widget.send(
      GuiIntentAppScreenSwipe(
        sessionId: widget.sessionId,
        captureId: widget.capture.captureId,
        fromX: fromX,
        fromY: fromY,
        toX: toX,
        toY: toY,
      ),
    );
  }

  /// The capture's pixel under [at], in an image drawn [shown] logical
  /// pixels large.
  (int, int) _pixel(Offset at, Size shown) => (
    _axis(at.dx, shown.width, widget.capture.width),
    _axis(at.dy, shown.height, widget.capture.height),
  );

  static int _axis(double at, double shown, int size) =>
      (at / shown * size).floor().clamp(0, size - 1).toInt();
}

/// The text field and its send: the text goes to the capture's screen, then
/// the field clears.
class _TextEntry extends StatefulWidget {
  const _TextEntry({
    required this.sessionId,
    required this.captureId,
    required this.send,
  });

  final String sessionId;

  /// The capture the text types into; null while there is none, and then
  /// the send is off.
  final String? captureId;
  final SendIntent send;

  @override
  State<_TextEntry> createState() => _TextEntryState();
}

class _TextEntryState extends State<_TextEntry> {
  final _controller = TextEditingController();

  @override
  void dispose() {
    _controller.dispose();
    super.dispose();
  }

  void _type(String captureId) {
    widget.send(
      GuiIntentAppScreenText(
        sessionId: widget.sessionId,
        captureId: captureId,
        text: _controller.text,
      ),
    );
    _controller.clear();
  }

  @override
  Widget build(BuildContext context) {
    final captureId = widget.captureId;
    return Padding(
      padding: const EdgeInsets.symmetric(horizontal: 8),
      child: Row(
        children: [
          Expanded(
            child: TextField(
              key: const Key('app-screen-text'),
              controller: _controller,
              decoration: const InputDecoration(hintText: 'Type on the device'),
            ),
          ),
          IconButton(
            key: const Key('app-screen-send'),
            tooltip: 'Type',
            onPressed: captureId == null ? null : () => _type(captureId),
            icon: const Icon(Icons.send),
          ),
        ],
      ),
    );
  }
}

/// One request: its label and how it stands, in the error colour when the
/// screen moved on or the server refused it, with a spinner while it waits.
class _RequestLine extends StatelessWidget {
  const _RequestLine({required this.row});

  final AppScreenRow row;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final style = row.stale || row.refused
        ? theme.textTheme.bodySmall?.copyWith(color: theme.colorScheme.error)
        : theme.textTheme.bodySmall;
    return Row(
      children: [
        if (row.waiting) ...[
          const SizedBox(
            width: 12,
            height: 12,
            child: CircularProgressIndicator(strokeWidth: 2),
          ),
          const SizedBox(width: 8),
        ],
        Expanded(
          child: Text(
            '${row.label} — ${row.stateLabel}',
            maxLines: 1,
            overflow: TextOverflow.ellipsis,
            style: style,
          ),
        ),
      ],
    );
  }
}
