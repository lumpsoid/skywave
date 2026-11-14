import 'package:material_ui/material_ui.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/session/timeline_holder.dart';
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

/// The row keyed `s:<n>`, showing `row <n>`, with [count] lines of detail
/// under it: tall with many, short with one. Its first text is the same
/// whatever its size, so `row <n>` measures where the row is.
TimelineRow detailed(int n, {int count = 40}) => anyRow(
  's:$n',
  RowKind.agentMessage,
  'row $n',
  sections: [
    lines([for (var i = 0; i < count; i++) 'line $i']),
  ],
);

/// Pumps the session page over [rows] at rest, not scrolled.
Future<TimelineHolder> pumpRows(
  WidgetTester tester,
  List<TimelineRow> rows,
) async {
  tester.view
    ..physicalSize = const Size(400, 600)
    ..devicePixelRatio = 1;
  addTearDown(tester.view.reset);
  final timeline = holding(rows);
  await pumpSession(
    tester,
    screen: screenWith(),
    timeline: timeline,
    sent: <GuiIntent>[],
  );
  return timeline;
}

/// Pumps [count] rows in the scrollbar test's mix, then scrolls them from the
/// newest end toward the oldest: 150 steps of 400 px, one frame each. Prints a
/// summary of the frame times, then times one call of the list's extent
/// estimate. Nothing is asserted about the times: they depend on the machine.
Future<void> measureScroll(WidgetTester tester, int count) async {
  await pumpRows(tester, [
    for (var i = 0; i < count; i++)
      i % 3 == 0 ? detailed(i, count: 4 + i % 7) : row('s:$i', 'row $i'),
  ]);
  final position = tester.state<ScrollableState>(listScrollable()).position;
  final start = scrolled(tester);
  final micros = <int>[];
  for (var step = 0; step < 150; step++) {
    final watch = Stopwatch()..start();
    position.jumpTo(position.pixels + 400);
    await tester.pump();
    watch.stop();
    micros.add(watch.elapsedMicroseconds);
  }
  expect(scrolled(tester), greaterThan(start), reason: 'the scroll moved');

  // The first 5 frames are warm-up, dropped before the percentiles.
  final timed = micros.sublist(5)..sort();
  int nearestRank(double p) => timed[(timed.length * p).ceil() - 1];
  final mean = timed.reduce((a, b) => a + b) ~/ timed.length;
  debugPrint(
    'estimate cost: rows=$count frames=${timed.length} (5 warm-up dropped) '
    'mean=$meanµs p50=${nearestRank(0.5)}µs p90=${nearestRank(0.9)}µs '
    'max=${timed.last}µs',
  );

  // The list's own delegate, whose estimate runs on every sliver layout, timed
  // alone: the frame times above cannot resolve it. The call is a viewport at
  // the newest end: rows 0 to 9 built, 1000 px of them, the rest estimated.
  // The ListView's childrenDelegate is the one its sliver lays out with. A
  // finder for the SliverList found nothing in the 500-row run, so it is not
  // used.
  final delegate = tester
      .widget<ListView>(find.byType(ListView))
      .childrenDelegate;
  double? estimate;
  for (var i = 0; i < 20; i++) {
    estimate = delegate.estimateMaxScrollOffset(0, 9, 0, 1000);
  }
  final callWatch = Stopwatch()..start();
  for (var i = 0; i < 1000; i++) {
    estimate = delegate.estimateMaxScrollOffset(0, 9, 0, 1000);
  }
  callWatch.stop();
  // Both passes ran, and the calls were not optimized away.
  expect(estimate, greaterThan(1000), reason: 'the estimate covered the rows');
  final perCall = callWatch.elapsedMicroseconds / 1000;
  debugPrint(
    'estimate call: rows=$count calls=1000 '
    'mean=${perCall.toStringAsFixed(1)}µs per call',
  );
}
