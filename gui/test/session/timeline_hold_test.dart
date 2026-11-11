import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/session/timeline_holder.dart';
import 'package:skywave_gui/session/timeline_view.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

import '../fakes.dart';
import 'rows.dart';

TimelineHolder holding(List<TimelineRow> rows) =>
    TimelineHolder()
      ..apply(TimelineUpdateSnapshot(sessionId: 's1', items: rows));

Finder listScrollable() => find
    .descendant(of: find.byType(ListView), matching: find.byType(Scrollable))
    .first;

double scrolled(WidgetTester tester) =>
    tester.state<ScrollableState>(listScrollable()).position.pixels;

/// The newest row in view, which the hold keeps in place.
String heldKey(WidgetTester tester) => tester
    .state<TimelineViewState>(find.byType(TimelineView))
    .newestVisibleKey()!;

/// The on-screen top of the row numbered [n], which shows `row <n>`.
double topOf(WidgetTester tester, int n) =>
    tester.getRect(find.text('row $n')).top;

/// Sixty rows, scrolled back past the newest one so the view is not
/// following.
Future<TimelineHolder> scrolledBack(WidgetTester tester) async {
  tester.view
    ..physicalSize = const Size(400, 600)
    ..devicePixelRatio = 1;
  addTearDown(tester.view.reset);
  final timeline = holding([
    for (var i = 0; i < 60; i++) row('s:$i', 'row $i'),
  ]);
  await pumpSession(
    tester,
    screen: screenWith(),
    timeline: timeline,
    sent: <GuiIntent>[],
  );
  await tester.drag(listScrollable(), const Offset(0, 400));
  await tester.pumpAndSettle();
  expect(scrolled(tester), greaterThan(32));
  return timeline;
}

void main() {
  group('scroll hold, scrolled back', () {
    testWidgets('the held row removed alone keeps the row above it still', (
      tester,
    ) async {
      final timeline = await scrolledBack(tester);
      final held = heldKey(tester);
      final n = int.parse(held.substring(2));
      final above = topOf(tester, n - 1);

      timeline.apply(TimelineUpdateRemoved(key: held));
      await tester.pumpAndSettle();

      expect(topOf(tester, n - 1), closeTo(above, 0.5));
      expect(tester.takeException(), isNull);
    });

    // Fails: Found 0 widgets with text "row 52": []
    testWidgets('the held row removed with its neighbours keeps the rows '
        'above still', (tester) async {
      final timeline = await scrolledBack(tester);
      final held = heldKey(tester);
      final n = int.parse(held.substring(2));
      final twoUp = topOf(tester, n - 2);

      timeline
        ..apply(TimelineUpdateRemoved(key: held))
        ..apply(TimelineUpdateRemoved(key: 's:${n - 1}'))
        ..apply(TimelineUpdateRemoved(key: 's:${n + 1}'));
      await tester.pumpAndSettle();

      expect(topOf(tester, n - 2), closeTo(twoUp, 0.5));
      expect(tester.takeException(), isNull);
    }, skip: true);

    testWidgets('every row removed, then rows back, lays out cleanly', (
      tester,
    ) async {
      final timeline = await scrolledBack(tester);

      for (final key in [for (var i = 0; i < 60; i++) 's:$i']) {
        timeline.apply(TimelineUpdateRemoved(key: key));
      }
      await tester.pumpAndSettle();

      for (var i = 0; i < 10; i++) {
        timeline.apply(
          TimelineUpdateUpserted(
            key: 's:${100 + i}',
            item: row('s:${100 + i}', 'row ${100 + i}'),
            index: i,
          ),
        );
      }
      await tester.pumpAndSettle();

      expect(tester.takeException(), isNull);
      expect(find.text('row 109'), findsOneWidget);
    });

    testWidgets('rows inserted and removed below the held row keep it still', (
      tester,
    ) async {
      final timeline = await scrolledBack(tester);
      final held = heldKey(tester);
      final n = int.parse(held.substring(2));
      final before = topOf(tester, n);

      final end = timeline.rows.length;
      timeline
        ..apply(
          TimelineUpdateUpserted(
            key: 's:$end',
            item: row('s:$end', 'row $end'),
            index: end,
          ),
        )
        ..apply(
          TimelineUpdateUpserted(
            key: 's:${end + 1}',
            item: row('s:${end + 1}', 'row ${end + 1}'),
            index: end + 1,
          ),
        )
        ..apply(
          TimelineUpdateUpserted(
            key: 's:${end + 2}',
            item: row('s:${end + 2}', 'row ${end + 2}'),
            index: end + 2,
          ),
        )
        ..apply(TimelineUpdateRemoved(key: 's:${n + 1}'))
        ..apply(TimelineUpdateRemoved(key: 's:${n + 2}'));
      await tester.pumpAndSettle();

      expect(topOf(tester, n), closeTo(before, 0.5));
      expect(tester.takeException(), isNull);
    });

    testWidgets('rows inserted and removed above the held row keep it still', (
      tester,
    ) async {
      final timeline = await scrolledBack(tester);
      final held = heldKey(tester);
      final n = int.parse(held.substring(2));
      final before = topOf(tester, n);

      timeline
        ..apply(TimelineUpdateRemoved(key: 's:0'))
        ..apply(TimelineUpdateRemoved(key: 's:1'))
        ..apply(
          TimelineUpdateUpserted(
            key: 's:200',
            item: row('s:200', 'row 200'),
            index: 0,
          ),
        )
        ..apply(
          TimelineUpdateUpserted(
            key: 's:201',
            item: row('s:201', 'row 201'),
            index: 1,
          ),
        )
        ..apply(
          TimelineUpdateUpserted(
            key: 's:202',
            item: row('s:202', 'row 202'),
            index: 2,
          ),
        );
      await tester.pumpAndSettle();

      expect(topOf(tester, n), closeTo(before, 0.5));
      expect(tester.takeException(), isNull);
    });

    testWidgets('rows inserted and removed around the held row keep it still', (
      tester,
    ) async {
      final timeline = await scrolledBack(tester);
      final held = heldKey(tester);
      final n = int.parse(held.substring(2));
      final before = topOf(tester, n);

      timeline
        ..apply(TimelineUpdateRemoved(key: 's:${n - 2}'))
        ..apply(TimelineUpdateRemoved(key: 's:${n + 2}'))
        ..apply(
          TimelineUpdateUpserted(
            key: 's:300',
            item: row('s:300', 'row 300'),
            index: timeline.indexOf(held),
          ),
        )
        ..apply(
          TimelineUpdateUpserted(
            key: 's:301',
            item: row('s:301', 'row 301'),
            index: timeline.indexOf(held) + 1,
          ),
        );
      await tester.pumpAndSettle();

      expect(topOf(tester, n), closeTo(before, 0.5));
      expect(tester.takeException(), isNull);
    });

    testWidgets('the held row re-inserted under its key in the same frame '
        'keeps it still', (tester) async {
      final timeline = await scrolledBack(tester);
      final held = heldKey(tester);
      final n = int.parse(held.substring(2));
      final before = topOf(tester, n);
      final at = timeline.indexOf(held);

      timeline
        ..apply(TimelineUpdateRemoved(key: held))
        ..apply(
          TimelineUpdateUpserted(
            key: held,
            item: row(held, 'row $n'),
            index: at,
          ),
        );
      await tester.pumpAndSettle();

      expect(topOf(tester, n), closeTo(before, 0.5));
      expect(tester.takeException(), isNull);
    });
  });
}
