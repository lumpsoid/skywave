import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/app.dart';
import 'package:skywave_gui/boot/boot_holder.dart';

void main() {
  test('starts pending', () {
    expect(BootHolder().status, isA<BootPending>());
  });

  test('Booted makes it ready and notifies', () {
    final holder = BootHolder();
    var notified = 0;
    holder
      ..addListener(() => notified++)
      ..booted();

    expect(holder.status, isA<BootReady>());
    expect(notified, 1);
  });

  test('BootFailed keeps its message', () {
    final holder = BootHolder()..fail('Cannot open the data: disk full');

    expect(
      holder.status,
      isA<BootError>().having(
        (s) => s.message,
        'message',
        'Cannot open the data: disk full',
      ),
    );
  });

  test('Booted after a failure clears it', () {
    final holder = BootHolder()
      ..fail('Cannot open the data: locked')
      ..booted();

    expect(holder.status, isA<BootReady>());
  });

  test('a second Booted keeps it ready', () {
    final holder = BootHolder()
      ..booted()
      ..booted();

    expect(holder.status, isA<BootReady>());
  });

  testWidgets('shows a spinner, then the failure, then the app', (
    tester,
  ) async {
    final holder = BootHolder();
    await tester.pumpWidget(SkywaveApp(boot: holder));
    expect(find.byType(CircularProgressIndicator), findsOneWidget);

    holder.fail('Cannot open the data: disk full');
    await tester.pump();
    expect(find.text('Cannot open the data: disk full'), findsOneWidget);
    expect(find.text('Retry'), findsOneWidget);

    holder.booted();
    await tester.pump();
    expect(find.text('Booted'), findsOneWidget);
  });
}
