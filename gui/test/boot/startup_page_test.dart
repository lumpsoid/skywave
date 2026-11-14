import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/boot/startup_page.dart';

void main() {
  testWidgets('the startup page names the app and never animates', (
    tester,
  ) async {
    await tester.pumpWidget(const MaterialApp(home: StartupPage()));
    await tester.pump();
    expect(find.text('Skywave'), findsOneWidget);
    expect(find.text('Starting…'), findsOneWidget);
    // A spinner keeps scheduling frames; this page must not.
    expect(tester.binding.hasScheduledFrame, isFalse);
  });
}
