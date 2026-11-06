import 'dart:math' as math;

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:skywave_gui/crop/blob_menu.dart';
import 'package:skywave_gui/crop/crop_lines_holder.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/screen/readable_width.dart';
import 'package:skywave_gui/session/look.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// The crop page: which lines of an output every device keeps. The rules
/// are Rust's (`skywave_present::crop`): the presets, the selection the
/// core holds, whether Crop may be sent and what to ask first. This page
/// shows them, a virtualised list of the lines Rust sends a window at a
/// time (`CropLines`), and a gutter that toggles lines (`CropToggle`): a
/// tap toggles one, a shift-tap or long press the range from the line
/// tapped last.
class CropPage extends StatefulWidget {
  const CropPage({
    required this.screen,
    required this.lines,
    required this.send,
    this.canGoBack = false,
    super.key,
  });

  final CropScreen screen;
  final CropLinesHolder lines;
  final SendIntent send;
  final bool canGoBack;

  /// Lines asked for around the first one missing.
  static const window = 300;

  /// The height of one line.
  static const lineHeight = 22.0;

  @override
  State<CropPage> createState() => _CropPageState();
}

class _CropPageState extends State<CropPage> {
  /// The window last asked for, `(from, count)`, so a line missing from
  /// it asks only once.
  (int, int)? _asked;

  /// The line tapped last: where a shift-tap or long press extends from.
  int? _anchor;

  @override
  void didUpdateWidget(CropPage old) {
    super.didUpdateWidget(old);
    if (old.screen.blobId != widget.screen.blobId) {
      _asked = null;
      _anchor = null;
    }
  }

  /// Asks for the window around line [index] once the frame is built.
  void _want(int index) {
    final asked = _asked;
    if (asked != null && index >= asked.$1 && index < asked.$1 + asked.$2) {
      return;
    }
    final from = math.max(0, index - CropPage.window ~/ 3);
    _asked = (from, CropPage.window);
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (!mounted) return;
      widget.send(GuiIntentCropLines(from: from, count: CropPage.window));
    });
  }

  void _tap(int index, {required bool extend}) {
    final anchor = _anchor;
    final first = extend && anchor != null ? anchor : index;
    widget.send(GuiIntentCropToggle(first: first, last: index));
    setState(() => _anchor = index);
  }

  Future<void> _crop() async {
    final screen = widget.screen;
    final question = screen.confirmation;
    if (!screen.canCrop || question == null) return;
    final yes = await confirmIrreversible(
      context,
      question: question,
      action: 'Crop',
    );
    if (yes) {
      widget.send.act(
        ActionIntentCropBlob(blobId: screen.blobId, keep: screen.keep),
      );
    }
  }

  @override
  Widget build(BuildContext context) {
    final screen = widget.screen;
    final theme = Theme.of(context);
    final muted = theme.textTheme.bodySmall?.copyWith(
      color: theme.colorScheme.onSurfaceVariant,
    );
    return Scaffold(
      appBar: AppBar(
        leading: backButton(
          canGoBack: widget.canGoBack,
          onBack: () => widget.send.act(const ActionIntentBack()),
        ),
        title: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            const Text('Crop'),
            Text(
              screen.title,
              style: muted,
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
            ),
          ],
        ),
        actions: [
          Padding(
            padding: const EdgeInsets.only(right: 12),
            child: FilledButton.icon(
              key: const Key('crop-send'),
              onPressed: screen.canCrop ? _crop : null,
              icon: const Icon(Icons.content_cut),
              label: const Text('Crop'),
            ),
          ),
        ],
      ),
      body: ReadableWidth(
        maxWidth: 1100,
        child: screen.content == CropContentKind.ready
            ? _ready(context, screen, muted)
            : _notReady(screen, muted),
      ),
    );
  }

  /// The message, and for content not here its download.
  Widget _notReady(CropScreen screen, TextStyle? muted) {
    final download = screen.download;
    return Center(
      child: Padding(
        padding: const EdgeInsets.all(24),
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            if (screen.message case final message?)
              Text(message, textAlign: TextAlign.center),
            if (download != null)
              Padding(
                padding: const EdgeInsets.only(top: 8),
                child: Text(download.label, style: muted),
              ),
            if (download != null && download.canDownload)
              Padding(
                padding: const EdgeInsets.only(top: 12),
                child: FilledButton.icon(
                  key: const Key('crop-download'),
                  onPressed: () => widget.send.act(
                    ActionIntentDownloadBlob(blobId: download.blobId),
                  ),
                  icon: const Icon(Icons.download),
                  label: const Text('Download'),
                ),
              ),
            if (screen.pendingNote != null) _pending(screen),
          ],
        ),
      ),
    );
  }

  Widget _pending(CropScreen screen) => Padding(
    padding: const EdgeInsets.only(top: 8),
    child: Wrap(
      spacing: 8,
      crossAxisAlignment: WrapCrossAlignment.center,
      children: [
        Text(screen.pendingNote ?? ''),
        if (screen.discardId case final id?)
          TextButton(
            key: const Key('crop-discard'),
            onPressed: () =>
                widget.send.act(ActionIntentDiscard(commandId: id)),
            child: const Text('Discard'),
          ),
      ],
    ),
  );

  Widget _ready(BuildContext context, CropScreen screen, TextStyle? muted) {
    final theme = Theme.of(context);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Padding(
          padding: const EdgeInsets.fromLTRB(16, 12, 16, 4),
          child: Text(
            screen.keepsLabel ?? '',
            key: const Key('crop-keeps'),
            style: theme.textTheme.titleSmall,
          ),
        ),
        if (screen.pendingNote != null)
          Padding(
            padding: const EdgeInsets.symmetric(horizontal: 16),
            child: _pending(screen),
          ),
        if (screen.presets.isNotEmpty)
          Padding(
            padding: const EdgeInsets.fromLTRB(16, 4, 16, 8),
            child: Wrap(
              spacing: 8,
              runSpacing: 8,
              children: [
                for (final (i, preset) in screen.presets.indexed)
                  ActionChip(
                    key: Key('crop-preset-$i'),
                    label: Text(preset.label),
                    onPressed: () => widget.send.act(
                      ActionIntentSetCropKeep(keep: preset.keep),
                    ),
                  ),
              ],
            ),
          ),
        Padding(
          padding: const EdgeInsets.fromLTRB(16, 0, 16, 4),
          child: Text(
            'Tap a line to keep or drop it; shift-tap or long-press to '
            'keep or drop the lines from the one tapped last.',
            style: muted,
          ),
        ),
        const Divider(height: 1),
        Expanded(
          child: ListenableBuilder(
            listenable: widget.lines,
            builder: (context, _) {
              if (widget.lines.error(screen.blobId) case final error?) {
                return Center(
                  child: Text(
                    error,
                    style: TextStyle(color: theme.colorScheme.error),
                  ),
                );
              }
              return ListView.builder(
                key: const Key('crop-lines'),
                itemCount: screen.lines,
                itemExtent: CropPage.lineHeight,
                itemBuilder: (context, i) => _line(context, screen, i),
              );
            },
          ),
        ),
      ],
    );
  }

  Widget _line(BuildContext context, CropScreen screen, int i) {
    final theme = Theme.of(context);
    final colors = SkyColors.of(context);
    // Until this page asked, the holder's window may be an earlier page's
    // of the same output, with its selection.
    final line = _asked == null ? null : widget.lines.line(screen.blobId, i);
    if (line == null) _want(i);
    final kept = line?.kept ?? false;
    final digits = '${screen.lines}'.length;
    final mono = theme.textTheme.bodySmall?.copyWith(
      fontFamily: monoFamily,
      fontFamilyFallback: monoFallback,
    );
    return Material(
      color: kept
          ? theme.colorScheme.primaryContainer.withValues(alpha: 0.5)
          : Colors.transparent,
      child: InkWell(
        key: Key('crop-line-$i'),
        onTap: () => _tap(i, extend: HardwareKeyboard.instance.isShiftPressed),
        onLongPress: () => _tap(i, extend: true),
        child: Row(
          children: [
            Padding(
              padding: const EdgeInsets.symmetric(horizontal: 8),
              child: Icon(
                kept ? Icons.check_box : Icons.check_box_outline_blank,
                size: 16,
                color: kept ? theme.colorScheme.primary : colors.muted,
              ),
            ),
            Text(
              '${i + 1}'.padLeft(digits),
              style: mono?.copyWith(
                color: _anchor == i ? theme.colorScheme.primary : colors.muted,
              ),
            ),
            const SizedBox(width: 12),
            Expanded(
              child: Text(
                line?.text ?? '',
                style: mono,
                maxLines: 1,
                softWrap: false,
                overflow: TextOverflow.clip,
              ),
            ),
          ],
        ),
      ),
    );
  }
}
