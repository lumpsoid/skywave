import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/screen/screen_navigator.dart';
import 'package:skywave_gui/session/row_tile.dart';
import 'package:skywave_gui/session/search_holder.dart';
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

/// The on-screen top of the whole row showing `row <n>`, its RowTile box. The
/// text sits lower in a short row than in a tall one, so when a row's size
/// changes the text can move inside a box whose top stays put.
double rowTopOf(WidgetTester tester, int n) => tester
    .getRect(
      find
          .ancestor(of: find.text('row $n'), matching: find.byType(RowTile))
          .first,
    )
    .top;

/// The number `n` of the newest row whose `row <n>` text lies entirely inside
/// the list's rect.
int wholeRowInView(WidgetTester tester) {
  final view = tester.getRect(listScrollable());
  var newest = -1;
  for (final text in tester.widgetList<Text>(find.byType(Text))) {
    final data = text.data ?? text.textSpan?.toPlainText();
    if (data == null || !data.startsWith('row ')) continue;
    final n = int.tryParse(data.substring(4));
    if (n == null || n <= newest) continue;
    final rect = tester.getRect(find.text(data));
    if (rect.top >= view.top && rect.bottom <= view.bottom) newest = n;
  }
  return newest;
}

/// Sixty rows, `row 0` to `row 59`, keyed `s:0` to `s:59`; the row keyed
/// `s:<tallAt>` has its detail of 40 lines.
List<TimelineRow> sixty({int? tallAt}) => [
  for (var i = 0; i < 60; i++)
    i == tallAt ? detailed(i) : row('s:$i', 'row $i'),
];

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

/// An update putting [item] at [index], replacing a row with its key.
TimelineUpdate upsert(TimelineRow item, int index) =>
    TimelineUpdateUpserted(key: item.key, item: item, index: index);

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

/// Sixty rows, scrolled back past the newest one so the view is not
/// following.
Future<TimelineHolder> scrolledBack(WidgetTester tester) async {
  final timeline = await pumpRows(tester, sixty());
  await tester.drag(listScrollable(), const Offset(0, 400));
  await tester.pumpAndSettle();
  expect(scrolled(tester), greaterThan(32));
  return timeline;
}

/// Scrolls the list to [pixels] from its newest end, and lets it settle.
Future<void> scrollTo(WidgetTester tester, double pixels) async {
  tester.state<ScrollableState>(listScrollable()).position.jumpTo(pixels);
  await tester.pumpAndSettle();
}

/// Sixty rows for the conversation-only tests: even `n` is a message showing
/// `row <n>`, odd `n` a tool row, shown as `Bash  ls <n>` (found with
/// `textContaining('ls <n>')`).
List<TimelineRow> mixed() => [
  for (var i = 0; i < 60; i++)
    i.isEven
        ? row('s:$i', 'row $i')
        : anyRow('s:$i', RowKind.tool, 'tool $i', tool: shellFacts('ls $i')),
];

/// The keys of the tool rows in [timeline], in timeline order.
List<String> toolKeys(TimelineHolder timeline) => [
  for (final item in timeline.rows)
    if (item.kind == RowKind.tool) item.key,
];

/// Hides the tool rows as "conversation only" on does: one removal per row,
/// in timeline order, all applied before any frame.
void hideTools(TimelineHolder timeline) {
  for (final key in toolKeys(timeline)) {
    timeline.apply(TimelineUpdateRemoved(key: key));
  }
}

/// Shows the tool rows as "conversation only" off does: each re-inserted at
/// its index in the full list, ascending, all applied before any frame.
void showTools(TimelineHolder timeline) {
  final full = mixed();
  for (var i = 1; i < full.length; i += 2) {
    timeline.apply(upsert(full[i], i));
  }
}

/// The number `n` of the newest row in view, `s:<n>`.
int heldNumber(WidgetTester tester) => int.parse(heldKey(tester).substring(2));

/// The newest message in view: the newest row in view, or the one above it
/// when that row is a tool row.
int newestMessage(WidgetTester tester) {
  final n = heldNumber(tester);
  return n.isOdd ? n - 1 : n;
}

/// Pumps [mixed] at rest, then scrolls it back in 10 px steps from 100 px
/// until the newest row in view is a tool row.
Future<TimelineHolder> pumpMixedAtTool(WidgetTester tester) async {
  final timeline = await pumpRows(tester, mixed());
  for (var at = 100.0; at <= 300; at += 10) {
    await scrollTo(tester, at);
    if (heldNumber(tester).isOdd) return timeline;
  }
  fail('no tool row came to the newest end in view');
}

/// Expects the row showing [text] to be laid out inside the list's rect.
void expectInView(WidgetTester tester, String text) {
  final view = tester.getRect(listScrollable());
  final rect = tester.getRect(
    find.descendant(of: find.byType(ListView), matching: find.text(text)),
  );
  expect(rect.top, greaterThanOrEqualTo(view.top));
  expect(rect.bottom, lessThanOrEqualTo(view.bottom));
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
    });

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

  group('scroll hold, rows changing size', () {
    testWidgets('a row above the held row growing keeps the held row still', (
      tester,
    ) async {
      final timeline = await scrolledBack(tester);
      final held = heldKey(tester);
      final n = int.parse(held.substring(2));
      final before = topOf(tester, n);

      timeline.apply(upsert(detailed(n - 1), n - 1));
      await tester.pumpAndSettle();

      expect(find.text('line 39'), findsOneWidget, reason: 'the row grew');
      expect(topOf(tester, n), closeTo(before, 0.5));
      expect(tester.takeException(), isNull);
    });

    // Measures the row's box, not its text: a tall row lays its text out 12 px
    // below its top and a short one 28 px below (measured). The text moved
    // 16 px inside a box whose top stayed put.
    testWidgets('the held row growing keeps its top still', (tester) async {
      final timeline = await scrolledBack(tester);
      final held = heldKey(tester);
      final n = int.parse(held.substring(2));
      final before = rowTopOf(tester, n);
      final above = rowTopOf(tester, n - 1);

      timeline.apply(upsert(detailed(n), n));
      await tester.pumpAndSettle();

      expect(find.text('line 39'), findsOneWidget, reason: 'the row grew');
      expect(rowTopOf(tester, n), closeTo(before, 0.5));
      expect(rowTopOf(tester, n - 1), closeTo(above, 0.5));
      expect(tester.takeException(), isNull);
    });

    // At pixel 500 the view would clamp at the newest end, the fallback.
    testWidgets('the held row shrinking keeps its top still', (tester) async {
      final timeline = await pumpRows(tester, sixty(tallAt: 57));
      await scrollTo(tester, 700);
      expect(heldKey(tester), 's:57');
      final before = rowTopOf(tester, 57);
      expect(find.text('line 39'), findsOneWidget, reason: 'tall to start');

      timeline.apply(upsert(detailed(57, count: 1), 57));
      await tester.pumpAndSettle();

      expect(find.text('line 39'), findsNothing, reason: 'the row shrank');
      expect(rowTopOf(tester, 57), closeTo(before, 0.5));
      expect(tester.takeException(), isNull);
    });

    testWidgets('a held row taller than the viewport stays put when a row '
        'is added below', (tester) async {
      final timeline = await pumpRows(tester, sixty());
      timeline.apply(upsert(detailed(58, count: 100), 58));
      await tester.pumpAndSettle();
      await scrollTo(tester, 90);
      expect(heldKey(tester), 's:58');
      final before = tester.getRect(find.text('line 90')).top;
      final pixels = scrolled(tester);

      timeline.apply(upsert(row('s:60', 'row 60'), 60));
      await tester.pumpAndSettle();

      expect(
        scrolled(tester),
        greaterThan(pixels),
        reason: 'the row was added',
      );
      expect(tester.getRect(find.text('line 90')).top, closeTo(before, 0.5));
      expect(tester.takeException(), isNull);
    });
  });

  group('scroll hold, mixed heights', () {
    testWidgets('newer rows removed out of sight with mixed heights keeps '
        'the rows in view still', (tester) async {
      // From the newest: two short rows, a medium one, a tall one, then plain.
      final timeline = await pumpRows(tester, [
        for (var i = 0; i < 60; i++)
          if (i == 56)
            detailed(i, count: 15)
          else if (i == 57)
            detailed(i, count: 6)
          else if (i >= 58)
            detailed(i, count: 1)
          else
            row('s:$i', 'row $i'),
      ]);
      await scrollTo(tester, 600);
      expect(find.text('row 58'), findsNothing, reason: 'out of the build');
      expect(find.text('row 56'), findsOneWidget, reason: 'the tall row');

      final n = wholeRowInView(tester);
      final before = rowTopOf(tester, n);

      timeline
        ..apply(TimelineUpdateRemoved(key: 's:59'))
        ..apply(TimelineUpdateRemoved(key: 's:58'));
      await tester.pumpAndSettle();

      expect(rowTopOf(tester, n), closeTo(before, 0.5));
      expect(tester.takeException(), isNull);
    });
  });

  group('scroll hold, snapshots and the end', () {
    testWidgets('a snapshot of the same session keeps the held row still', (
      tester,
    ) async {
      final timeline = await scrolledBack(tester);
      final held = heldKey(tester);
      final n = int.parse(held.substring(2));
      final before = topOf(tester, n);
      final pixels = scrolled(tester);

      timeline.apply(
        TimelineUpdateSnapshot(
          sessionId: 's1',
          items: [...sixty(), row('s:60', 'row 60')],
        ),
      );
      await tester.pumpAndSettle();

      expect(scrolled(tester), greaterThan(pixels), reason: 'the list moved');
      expect(topOf(tester, n), closeTo(before, 0.5));
      expect(tester.takeException(), isNull);
    });

    testWidgets('a snapshot of another session starts at its newest row', (
      tester,
    ) async {
      final timeline = await scrolledBack(tester);

      timeline.apply(
        TimelineUpdateSnapshot(
          sessionId: 's2',
          items: [for (var i = 200; i < 230; i++) row('s:$i', 'row $i')],
        ),
      );
      await tester.pumpAndSettle();

      expect(scrolled(tester), 0);
      expect(find.text('row 229'), findsOneWidget);
      expect(tester.takeException(), isNull);
    });

    testWidgets('a subagent page there and back lays out cleanly', (
      tester,
    ) async {
      tester.view
        ..physicalSize = const Size(1000, 600)
        ..devicePixelRatio = 1;
      addTearDown(tester.view.reset);
      final holders = fakeHolders();
      final sent = <GuiIntent>[];
      holders.timeline.apply(
        TimelineUpdateSnapshot(sessionId: 's1', items: sixty()),
      );
      holders.screen.apply(sessionScreen());
      await tester.pumpWidget(
        MaterialApp(
          home: ScreenNavigator(holders: holders, send: sent.add),
        ),
      );
      await tester.pumpAndSettle();
      await tester.drag(listScrollable(), const Offset(0, 400));
      await tester.pumpAndSettle();
      expect(scrolled(tester), greaterThan(32));

      holders.timeline.apply(
        TimelineUpdateSnapshot(
          sessionId: 's1',
          items: [for (var i = 100; i < 120; i++) row('s:$i', 'row $i')],
        ),
      );
      holders.screen.apply(sessionScreen(subagent: subagentHeader('call-1')));
      await tester.pumpAndSettle();
      expect(find.text('Explore sync'), findsOneWidget);

      holders.timeline.apply(
        TimelineUpdateSnapshot(sessionId: 's1', items: sixty()),
      );
      holders.screen.apply(sessionScreen());
      await tester.pumpAndSettle();

      expect(tester.takeException(), isNull);
      expect(find.textContaining('row '), findsWidgets);
    });

    testWidgets('just inside the end slack, a new row is followed', (
      tester,
    ) async {
      final timeline = await pumpRows(tester, sixty());
      await scrollTo(tester, 20);

      timeline.apply(upsert(row('s:60', 'row 60'), 60));
      await tester.pumpAndSettle();

      expect(scrolled(tester), 0);
      final view = tester.getRect(listScrollable());
      final rowRect = tester.getRect(find.text('row 60'));
      expect(rowRect.top, greaterThanOrEqualTo(view.top));
      expect(rowRect.bottom, lessThanOrEqualTo(view.bottom));
      expect(tester.takeException(), isNull);
    });

    testWidgets('just outside the end slack, a new row is held', (
      tester,
    ) async {
      final timeline = await pumpRows(tester, sixty());
      await scrollTo(tester, 50);
      expect(scrolled(tester), 50);
      final held = heldKey(tester);
      final n = int.parse(held.substring(2));
      final before = topOf(tester, n);

      timeline.apply(upsert(row('s:60', 'row 60'), 60));
      await tester.pumpAndSettle();

      expect(topOf(tester, n), closeTo(before, 0.5));
      expect(scrolled(tester), greaterThan(50));
      expect(tester.takeException(), isNull);
    });
  });

  group('scroll hold, conversation only, reveals and gestures', () {
    testWidgets('conversation only on keeps the message in view still', (
      tester,
    ) async {
      final timeline = await pumpMixedAtTool(tester);
      final watched = newestMessage(tester);
      final before = topOf(tester, watched);

      hideTools(timeline);
      await tester.pumpAndSettle();

      expect(timeline.rows.length, 30, reason: 'the tool rows went');
      expect(topOf(tester, watched), closeTo(before, 0.5));
      expect(tester.takeException(), isNull);
    });

    testWidgets('conversation only off keeps the message in view still', (
      tester,
    ) async {
      final timeline = await pumpMixedAtTool(tester);
      hideTools(timeline);
      await tester.pumpAndSettle();
      final watched = newestMessage(tester);
      final before = topOf(tester, watched);

      showTools(timeline);
      await tester.pumpAndSettle();

      expect(timeline.rows.length, 60, reason: 'the tool rows came back');
      expect(topOf(tester, watched), closeTo(before, 0.5));
      expect(tester.takeException(), isNull);
    });

    testWidgets('conversation only toggled five times keeps the message in '
        'view still', (tester) async {
      final timeline = await pumpMixedAtTool(tester);

      for (var i = 0; i < 5; i++) {
        // Re-picked before each toggle, as the off test picks its message.
        final watched = newestMessage(tester);
        final before = topOf(tester, watched);
        if (i.isEven) {
          hideTools(timeline);
        } else {
          showTools(timeline);
        }
        await tester.pumpAndSettle();
        expect(
          topOf(tester, watched),
          closeTo(before, 0.5),
          reason: 'after toggle ${i + 1}',
        );
      }
      expect(tester.takeException(), isNull);
    });

    testWidgets('conversation only toggled across frames keeps the message in '
        'view still', (tester) async {
      final timeline = await pumpMixedAtTool(tester);
      final watched = newestMessage(tester);
      final before = topOf(tester, watched);

      for (final key in toolKeys(timeline)) {
        timeline.apply(TimelineUpdateRemoved(key: key));
        await tester.pump();
      }
      await tester.pumpAndSettle();

      expect(timeline.rows.length, 30, reason: 'the tool rows went');
      expect(topOf(tester, watched), closeTo(before, 0.5));
      expect(tester.takeException(), isNull);
    });

    testWidgets('conversation only during a search jump lands on the match', (
      tester,
    ) async {
      tester.view
        ..physicalSize = const Size(400, 600)
        ..devicePixelRatio = 1;
      addTearDown(tester.view.reset);
      final search = SearchHolder();
      final timeline = holding(mixed());
      await pumpSession(
        tester,
        screen: screenWith(),
        timeline: timeline,
        search: search,
        sent: <GuiIntent>[],
      );
      search.apply(
        const SearchUpdate(
          open: true,
          query: 'row 4',
          countLabel: '1 of 1',
          wrapped: false,
          currentKey: 's:4',
          hits: [],
        ),
      );
      await tester.pump();

      hideTools(timeline);
      await tester.pumpAndSettle();

      expectInView(tester, 'row 4');
      expect(tester.takeException(), isNull);
    });

    testWidgets('conversation only during a reveal lands on the row', (
      tester,
    ) async {
      final timeline = await pumpRows(tester, mixed());
      tester.state<TimelineViewState>(find.byType(TimelineView)).reveal('s:4');
      await tester.pump();

      hideTools(timeline);
      await tester.pumpAndSettle();

      expectInView(tester, 'row 4');
      expect(tester.takeException(), isNull);
    });

    testWidgets('rows changing during a drag keep the held row under the '
        'finger', (tester) async {
      final timeline = await scrolledBack(tester);
      final g = await tester.startGesture(tester.getCenter(listScrollable()));
      await g.moveBy(const Offset(0, 40));
      await tester.pump();
      final n = wholeRowInView(tester);
      final top = topOf(tester, n);

      timeline.apply(
        TimelineUpdateSnapshot(
          sessionId: 's1',
          items: [
            for (var i = 0; i < 58; i++) row('s:$i', 'row $i'),
            row('s:60', 'row 60'),
            row('s:61', 'row 61'),
            row('s:62', 'row 62'),
          ],
        ),
      );
      await tester.pump();

      expect(topOf(tester, n), closeTo(top, 0.5));
      await g.moveBy(const Offset(0, 30));
      await tester.pump();
      expect(topOf(tester, n), closeTo(top + 30, 0.5));
      await g.up();
      await tester.pumpAndSettle();

      expect(tester.takeException(), isNull);
    });

    testWidgets('rows changing during a fling back lay out cleanly', (
      tester,
    ) async {
      final timeline = await scrolledBack(tester);
      await tester.fling(listScrollable(), const Offset(0, 300), 1500);
      await tester.pump(const Duration(milliseconds: 50));

      timeline
        ..apply(upsert(row('s:60', 'row 60'), 60))
        ..apply(upsert(row('s:61', 'row 61'), 61))
        ..apply(upsert(row('s:62', 'row 62'), 62))
        ..apply(TimelineUpdateRemoved(key: 's:58'))
        ..apply(TimelineUpdateRemoved(key: 's:59'));
      for (var i = 0; i < 5; i++) {
        await tester.pump(const Duration(milliseconds: 50));
      }
      await tester.pumpAndSettle();

      expect(tester.takeException(), isNull);
      expect(scrolled(tester), greaterThan(32));
    });

    testWidgets('rows arriving during a fling to the newest end follow them', (
      tester,
    ) async {
      final timeline = await scrolledBack(tester);
      await tester.fling(listScrollable(), const Offset(0, -2000), 3000);
      await tester.pump(const Duration(milliseconds: 50));

      timeline
        ..apply(upsert(row('s:60', 'row 60'), 60))
        ..apply(upsert(row('s:61', 'row 61'), 61))
        ..apply(upsert(row('s:62', 'row 62'), 62));
      await tester.pumpAndSettle();

      expect(tester.takeException(), isNull);
      expect(scrolled(tester), 0);
      expectInView(tester, 'row 62');
    });
  });
}
