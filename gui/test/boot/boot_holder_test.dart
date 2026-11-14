import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/app.dart';
import 'package:skywave_gui/boot/boot_failure_page.dart';
import 'package:skywave_gui/boot/boot_holder.dart';
import 'package:skywave_gui/boot/startup_page.dart';
import 'package:skywave_gui/screen/screen_navigator.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

import '../fakes.dart';

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

  test('BootFailed keeps whether the data is unreadable', () {
    expect(
      (BootHolder()..fail('x')).status,
      isA<BootError>().having((s) => s.unreadable, 'unreadable', false),
    );
    expect(
      (BootHolder()..fail('x', unreadable: true)).status,
      isA<BootError>().having((s) => s.unreadable, 'unreadable', true),
    );
  });

  testWidgets('Reset local data and Retry call their own callbacks', (
    tester,
  ) async {
    final taps = <String>[];
    await tester.pumpWidget(
      MaterialApp(
        home: BootFailurePage(
          message: 'unreadable',
          onRetry: () => taps.add('retry'),
          onReset: () => taps.add('reset'),
        ),
      ),
    );
    await tester.tap(find.text('Reset local data'));
    await tester.tap(find.text('Retry'));
    expect(taps, ['reset', 'retry']);
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

  testWidgets('shows the startup page, then the failure, then the app', (
    tester,
  ) async {
    final holder = BootHolder();
    final sent = <GuiIntent>[];
    await tester.pumpWidget(
      SkywaveApp(boot: holder, holders: fakeHolders(), send: sent.add),
    );
    expect(find.byType(StartupPage), findsOneWidget);
    expect(find.byType(CircularProgressIndicator), findsNothing);

    holder.fail('Cannot open the data: disk full');
    await tester.pump();
    expect(find.text('Cannot open the data: disk full'), findsOneWidget);
    expect(find.text('Retry'), findsOneWidget);
    expect(find.text('Reset local data'), findsNothing);

    holder.fail('Cannot open the data: from another build', unreadable: true);
    await tester.pump();
    expect(find.text('Retry'), findsOneWidget);
    expect(find.text('Reset local data'), findsOneWidget);

    holder.booted();
    await tester.pump();
    expect(find.byType(ScreenNavigator), findsOneWidget);
    // Booted asks Rust for the whole state.
    expect(sent, [const GuiIntentAppReady()]);
  });

  testWidgets('a hub crash shows the failure with Restart, which boots', (
    tester,
  ) async {
    final answered = <GuiIntent>[];
    final holder = BootHolder(send: answered.add);
    await tester.pumpWidget(
      SkywaveApp(boot: holder, holders: fakeHolders(), send: (_) {}),
    );
    holder.booted();
    await tester.pump();
    expect(find.byType(ScreenNavigator), findsOneWidget);

    holder.crashed('index out of bounds', epoch: 3);
    // Answered at once, so Rust can drop what was sent before.
    expect(answered, [const GuiIntentHubFailedSeen(epoch: 3)]);
    await tester.pump();
    expect(find.byType(ScreenNavigator), findsNothing);
    expect(
      find.text('Skywave stopped after an internal error: index out of bounds'),
      findsOneWidget,
    );
    expect(find.text('Restart'), findsOneWidget);
    expect(find.text('Retry'), findsNothing);
    expect(find.text('Reset local data'), findsNothing);

    // Restart sends Boot again: the startup page until Rust answers.
    await tester.tap(find.text('Restart'));
    await tester.pump();
    expect(find.byType(StartupPage), findsOneWidget);
    expect(find.byType(CircularProgressIndicator), findsNothing);
  });

  testWidgets('leaving and returning to the screen is reported once each', (
    tester,
  ) async {
    final sent = <GuiIntent>[];
    await tester.pumpWidget(
      SkywaveApp(boot: BootHolder(), holders: fakeHolders(), send: sent.add),
    );

    // Away: through inactive and hidden to paused.
    for (final state in [
      AppLifecycleState.inactive,
      AppLifecycleState.hidden,
      AppLifecycleState.paused,
    ]) {
      tester.binding.handleAppLifecycleStateChanged(state);
    }
    expect(sent, [const GuiIntentAppBackgrounded()]);

    // Back: the same way up to resumed.
    sent.clear();
    for (final state in [
      AppLifecycleState.hidden,
      AppLifecycleState.inactive,
      AppLifecycleState.resumed,
    ]) {
      tester.binding.handleAppLifecycleStateChanged(state);
    }
    expect(sent, [const GuiIntentAppForegrounded()]);
  });

  testWidgets('a detached Activity keeps the core, and a new one reports '
      'the return and checks its launch', (tester) async {
    final sent = <GuiIntent>[];
    var finalized = 0;
    var reattached = 0;
    await tester.pumpWidget(
      SkywaveApp(
        boot: BootHolder(),
        holders: fakeHolders(),
        send: sent.add,
        finalize: () => finalized++,
        onReattach: () => reattached++,
      ),
    );

    // The Activity goes: through hidden and paused to detached.
    for (final state in [
      AppLifecycleState.inactive,
      AppLifecycleState.hidden,
      AppLifecycleState.paused,
      AppLifecycleState.detached,
    ]) {
      tester.binding.handleAppLifecycleStateChanged(state);
    }
    expect(sent, [const GuiIntentAppBackgrounded()]);
    expect(finalized, 0);
    expect(reattached, 0);

    // A new Activity attaches: detached straight to resumed.
    sent.clear();
    tester.binding.handleAppLifecycleStateChanged(AppLifecycleState.resumed);
    expect(sent, [const GuiIntentAppForegrounded()]);
    expect(reattached, 1);
    expect(finalized, 0);

    // A plain leave and return afterwards goes through shown only.
    sent.clear();
    for (final state in [
      AppLifecycleState.inactive,
      AppLifecycleState.hidden,
      AppLifecycleState.inactive,
      AppLifecycleState.resumed,
    ]) {
      tester.binding.handleAppLifecycleStateChanged(state);
    }
    expect(sent, [
      const GuiIntentAppBackgrounded(),
      const GuiIntentAppForegrounded(),
    ]);
    expect(reattached, 1);
    expect(finalized, 0);
  });

  testWidgets('an exit request finalizes once', (tester) async {
    var finalized = 0;
    await tester.pumpWidget(
      SkywaveApp(
        boot: BootHolder(),
        holders: fakeHolders(),
        send: (_) {},
        finalize: () => finalized++,
      ),
    );

    await tester.binding.handleRequestAppExit();
    await tester.binding.handleRequestAppExit();
    expect(finalized, 1);
  });
}
