import 'package:material_ui/material_ui.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/settings/notifications_page.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

void _ignore() {}

Widget page(
  NotificationsView view,
  List<GuiIntent> sent, {
  VoidCallback onBack = _ignore,
}) => MaterialApp(
  home: NotificationsPage(view: view, send: sent.add, onBack: onBack),
);

void main() {
  const all = NotificationsView(
    all: true,
    approvals: true,
    idle: true,
    shell: true,
  );

  testWidgets('shows the four switches with the view values', (tester) async {
    await tester.pumpWidget(
      page(
        const NotificationsView(
          all: true,
          approvals: false,
          idle: true,
          shell: false,
        ),
        [],
      ),
    );
    bool valueOf(String key) =>
        tester.widget<SwitchListTile>(find.byKey(Key(key))).value;
    expect(valueOf('notify-all'), isTrue);
    expect(valueOf('notify-approvals'), isFalse);
    expect(valueOf('notify-idle'), isTrue);
    expect(valueOf('notify-shell'), isFalse);
    expect(find.text('Agent idle'), findsOneWidget);
    expect(find.text('Long shell runs'), findsOneWidget);
  });

  testWidgets('tapping a kind sends the whole set with that one flipped', (
    tester,
  ) async {
    final sent = <GuiIntent>[];
    await tester.pumpWidget(page(all, sent));
    await tester.tap(find.byKey(const Key('notify-idle')));
    expect(sent, [
      const GuiIntentSetNotifications(
        all: true,
        approvals: true,
        idle: false,
        shell: true,
      ),
    ]);
  });

  testWidgets('with all off, the kinds are disabled and keep their values', (
    tester,
  ) async {
    final sent = <GuiIntent>[];
    await tester.pumpWidget(
      page(
        const NotificationsView(
          all: false,
          approvals: true,
          idle: false,
          shell: true,
        ),
        sent,
      ),
    );
    final approvals = tester.widget<SwitchListTile>(
      find.byKey(const Key('notify-approvals')),
    );
    expect(approvals.onChanged, isNull);
    expect(approvals.value, isTrue);
    await tester.tap(find.byKey(const Key('notify-approvals')));
    expect(sent, isEmpty);

    // Turning all on sends the set with the kinds as they were kept.
    await tester.tap(find.byKey(const Key('notify-all')));
    expect(sent, [
      const GuiIntentSetNotifications(
        all: true,
        approvals: true,
        idle: false,
        shell: true,
      ),
    ]);
  });

  testWidgets('the back button calls onBack', (tester) async {
    var backs = 0;
    await tester.pumpWidget(
      page(
        all,
        [],
        onBack: () {
          backs++;
        },
      ),
    );
    await tester.tap(find.byType(BackButton));
    expect(backs, 1);
  });
}
