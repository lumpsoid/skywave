import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/crop/crop_lines_holder.dart';
import 'package:skywave_gui/crop/crop_page.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

const _question =
    "Keep 50 of 120 lines? Every device keeps only these. This can't be undone.";

/// A counted page of 120 lines, keeping [keep]; Crop is offered once
/// something is kept.
CropScreen _ready({List<LineSpanView> keep = const []}) => CropScreen(
  blobId: 'b1',
  title: 'make test',
  content: CropContentKind.ready,
  lines: 120,
  keep: keep,
  presets: const [
    CropPresetView(
      label: 'First 50',
      keep: [LineSpanView(first: 0, count: 50)],
    ),
    CropPresetView(
      label: 'Last 50',
      keep: [LineSpanView(first: 70, count: 50)],
    ),
  ],
  keepsLabel: 'keeps ${keep.isEmpty ? 0 : 50} of 120 lines',
  canCrop: keep.isNotEmpty,
  confirmation: keep.isEmpty ? null : _question,
);

Future<void> _pump(
  WidgetTester tester,
  CropScreen screen,
  CropLinesHolder lines,
  List<GuiIntent> sent,
) => tester.pumpWidget(
  MaterialApp(
    home: CropPage(
      screen: screen,
      lines: lines,
      canGoBack: true,
      send: sent.add,
    ),
  ),
);

List<GuiIntent> _acts(List<GuiIntent> sent) =>
    sent.whereType<GuiIntentAct>().toList();

void main() {
  testWidgets('a preset, then the confirmed crop sends its spans; cancel '
      'sends nothing', (tester) async {
    final sent = <GuiIntent>[];
    final lines = CropLinesHolder();
    await _pump(tester, _ready(), lines, sent);
    expect(find.text('keeps 0 of 120 lines'), findsOneWidget);
    expect(find.text('make test'), findsOneWidget);

    // Nothing kept: Crop does nothing.
    await tester.tap(find.byKey(const Key('crop-send')));
    await tester.pumpAndSettle();
    expect(find.byType(AlertDialog), findsNothing);
    expect(_acts(sent), isEmpty);

    await tester.tap(find.byKey(const Key('crop-preset-0')));
    expect(_acts(sent), [
      const GuiIntentAct(
        action: ActionIntentSetCropKeep(
          keep: [LineSpanView(first: 0, count: 50)],
        ),
      ),
    ]);
    sent.clear();

    // Rust's view of it: the selection kept, Crop offered.
    await _pump(
      tester,
      _ready(keep: const [LineSpanView(first: 0, count: 50)]),
      lines,
      sent,
    );
    expect(find.text('keeps 50 of 120 lines'), findsOneWidget);

    await tester.tap(find.byKey(const Key('crop-send')));
    await tester.pumpAndSettle();
    expect(find.text(_question), findsOneWidget);
    expect(_acts(sent), isEmpty, reason: 'nothing before the answer');
    await tester.tap(find.byKey(const Key('confirm-cancel')));
    await tester.pumpAndSettle();
    expect(find.byType(AlertDialog), findsNothing);
    expect(_acts(sent), isEmpty, reason: 'cancel sends none');

    await tester.tap(find.byKey(const Key('crop-send')));
    await tester.pumpAndSettle();
    await tester.tap(find.byKey(const Key('confirm-ok')));
    await tester.pumpAndSettle();
    expect(_acts(sent), [
      const GuiIntentAct(
        action: ActionIntentCropBlob(
          blobId: 'b1',
          keep: [LineSpanView(first: 0, count: 50)],
        ),
      ),
    ]);
  });

  testWidgets('the lines come a window at a time and the gutter toggles '
      'them', (tester) async {
    final sent = <GuiIntent>[];
    final lines = CropLinesHolder();
    await _pump(tester, _ready(), lines, sent);
    await tester.pump();
    expect(sent.whereType<GuiIntentCropLines>().toList(), [
      const GuiIntentCropLines(from: 0, count: CropPage.window),
    ]);

    lines.apply(
      const CropLinesUpdate(
        blobId: 'b1',
        from: 0,
        lines: [
          CropLine(index: 0, text: 'line 1', kept: true),
          CropLine(index: 1, text: 'line 2', kept: false),
          CropLine(index: 2, text: 'line 3', kept: false),
        ],
      ),
    );
    await tester.pump();
    expect(find.text('line 1'), findsOneWidget);
    expect(find.text('line 3'), findsOneWidget);
    // A kept line is marked.
    expect(
      tester
          .widget<Icon>(
            find.descendant(
              of: find.byKey(const Key('crop-line-0')),
              matching: find.byType(Icon),
            ),
          )
          .icon,
      Icons.check_box,
    );

    sent.clear();
    await tester.tap(find.byKey(const Key('crop-line-1')));
    await tester.longPress(find.byKey(const Key('crop-line-2')));
    expect(sent, [
      const GuiIntentCropToggle(first: 1, last: 1),
      const GuiIntentCropToggle(first: 1, last: 2),
    ]);
  });

  testWidgets('another output\'s window shows no lines', (tester) async {
    final lines = CropLinesHolder()
      ..apply(
        const CropLinesUpdate(
          blobId: 'other',
          from: 0,
          lines: [CropLine(index: 0, text: 'not mine', kept: false)],
        ),
      );
    await _pump(tester, _ready(), lines, []);
    await tester.pump();
    expect(find.text('not mine'), findsNothing);
    expect(find.byKey(const Key('crop-line-0')), findsOneWidget);
  });

  testWidgets('content not here says so and offers one Download', (
    tester,
  ) async {
    final sent = <GuiIntent>[];
    await _pump(
      tester,
      const CropScreen(
        blobId: 'b1',
        title: 'make test',
        content: CropContentKind.notHere,
        message:
            "This output isn't on this device. Download 4.0 KiB to choose "
            'lines.',
        download: BlobFacts(
          blobId: 'b1',
          state: BlobStateKind.notDownloaded,
          label: 'not downloaded · 4.0 KiB',
          canDownload: true,
        ),
        lines: 0,
        keep: [],
        presets: [],
        canCrop: false,
      ),
      CropLinesHolder(),
      sent,
    );
    expect(find.textContaining("isn't on this device"), findsOneWidget);
    expect(find.text('Download'), findsOneWidget);
    await tester.tap(find.byKey(const Key('crop-download')));
    expect(sent, [
      const GuiIntentAct(action: ActionIntentDownloadBlob(blobId: 'b1')),
    ]);
    expect(find.byKey(const Key('crop-lines')), findsNothing);
  });
}
