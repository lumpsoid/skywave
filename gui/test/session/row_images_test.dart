import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/session/images.dart';
import 'package:skywave_gui/session/timeline_holder.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

import 'rows.dart';

final _originalOpen = openLocalImage;

TimelineHolder _holding(List<TimelineRow> rows) =>
    TimelineHolder()
      ..apply(TimelineUpdateSnapshot(sessionId: 's1', items: rows));

void _tall(WidgetTester tester) {
  tester.view
    ..physicalSize = const Size(1200, 3000)
    ..devicePixelRatio = 1;
  addTearDown(tester.view.reset);
}

void main() {
  final opened = <String>[];

  tearDown(() {
    openLocalImage = _originalOpen;
    opened.clear();
  });

  testWidgets('a logged image opens its blob', (tester) async {
    _tall(tester);
    final sent = <GuiIntent>[];
    await pumpSession(
      tester,
      screen: screenWith(),
      timeline: _holding([
        anyRow(
          'm:1',
          RowKind.userMessage,
          'see this',
          author: 'me',
          own: true,
          images: const [
            RowImage(
              blobId: 'b1',
              path: '/nope.png',
              label: '1.0 KiB',
              canDownload: false,
            ),
          ],
        ),
      ]),
      sent: sent,
    );
    sent.clear();

    await tester.tap(find.byKey(const Key('row-image-0')));
    expect(sent, [const GuiIntentOpenFull(blobId: 'b1')]);
  });

  testWidgets('a loading image keeps its tap target', (tester) async {
    _tall(tester);
    final sent = <GuiIntent>[];
    await pumpSession(
      tester,
      screen: screenWith(),
      timeline: _holding([
        anyRow(
          'm:4',
          RowKind.userMessage,
          'still loading',
          author: 'me',
          own: true,
          images: const [
            RowImage(
              blobId: 'b1',
              path: '/nope.png',
              label: '1.0 KiB',
              canDownload: false,
            ),
          ],
        ),
      ]),
      sent: sent,
    );
    sent.clear();

    expect(
      tester.getSize(find.byKey(const Key('row-image-0'))).width,
      greaterThan(0),
    );
    await tester.tap(find.byKey(const Key('row-image-0')));
    expect(sent, [const GuiIntentOpenFull(blobId: 'b1')]);
  });

  testWidgets('an image not downloaded shows its size and downloads on tap', (
    tester,
  ) async {
    _tall(tester);
    final sent = <GuiIntent>[];
    await pumpSession(
      tester,
      screen: screenWith(),
      timeline: _holding([
        anyRow(
          'm:2',
          RowKind.userMessage,
          'a big one',
          author: 'me',
          own: true,
          images: const [
            RowImage(blobId: 'b2', label: '340.0 KiB', canDownload: true),
          ],
        ),
      ]),
      sent: sent,
    );
    sent.clear();

    expect(find.text('340.0 KiB'), findsOneWidget);
    await tester.tap(find.byKey(const Key('row-image-0')));
    expect(sent, [
      const GuiIntentAct(action: ActionIntentDownloadBlob(blobId: 'b2')),
    ]);
  });

  testWidgets('a queued image opens its local file', (tester) async {
    _tall(tester);
    openLocalImage = opened.add;
    final sent = <GuiIntent>[];
    await pumpSession(
      tester,
      screen: screenWith(),
      timeline: _holding([
        anyRow(
          'p:1',
          RowKind.pendingMessage,
          'queued one',
          author: 'me',
          own: true,
          images: const [
            RowImage(path: '/q.png', label: '2.0 KiB', canDownload: false),
          ],
        ),
      ]),
      sent: sent,
    );
    sent.clear();

    await tester.tap(find.byKey(const Key('row-image-0')));
    expect(opened, ['/q.png']);
    expect(sent, isEmpty);
  });

  testWidgets('a deleted image shows a note and does nothing', (tester) async {
    _tall(tester);
    openLocalImage = opened.add;
    final sent = <GuiIntent>[];
    await pumpSession(
      tester,
      screen: screenWith(),
      timeline: _holding([
        anyRow(
          'm:3',
          RowKind.userMessage,
          'gone',
          author: 'me',
          own: true,
          images: const [RowImage(label: 'image deleted', canDownload: false)],
        ),
      ]),
      sent: sent,
    );
    sent.clear();

    expect(find.text('image deleted'), findsOneWidget);
    await tester.tap(find.byKey(const Key('row-image-0')));
    expect(sent, isEmpty);
    expect(opened, isEmpty);
  });
}
