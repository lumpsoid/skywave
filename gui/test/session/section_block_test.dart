import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/session/section_block.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

import 'rows.dart';

void main() {
  Future<void> pump(WidgetTester tester, SectionView section) =>
      tester.pumpWidget(
        MaterialApp(
          home: Scaffold(body: SectionBlock(section: section)),
        ),
      );

  testWidgets('the note and the blob label show as the hub words them', (
    tester,
  ) async {
    const blob = BlobFacts(
      blobId: 'b1',
      state: BlobStateKind.notDownloaded,
      label: 'not downloaded · 70 B',
      canDownload: true,
    );
    await pump(tester, lines(['-old', '+new'], blob: blob));
    expect(find.text('not downloaded · 70 B'), findsOneWidget);
    expect(find.text('Download'), findsOneWidget);

    // Nothing is dropped for reading like something else.
    await pump(
      tester,
      lines(['-old'], blob: blob, note: 'not downloaded · 70 B'),
    );
    expect(find.text('not downloaded · 70 B'), findsNWidgets(2));
  });

  testWidgets('a label shows above the lines, only when the hub gives one', (
    tester,
  ) async {
    const label = "approval input: differs from the call's";
    await pump(tester, lines(['rm -rf /'], label: label));
    expect(find.text(label), findsOneWidget);
    expect(
      tester.getTopLeft(find.text(label)).dy,
      lessThan(tester.getTopLeft(find.textContaining('rm -rf /')).dy),
    );

    await pump(tester, lines(['rm -rf /']));
    expect(find.text(label), findsNothing);
  });
}
