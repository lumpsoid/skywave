import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/session/timeline_holder.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

import '../fakes.dart';

List<String> texts(TimelineHolder holder) => [
  for (final row in holder.rows) '${row.key}=${row.text}',
];

void main() {
  test('a snapshot, then diffs, fold into the list Rust holds', () {
    final holder = TimelineHolder();
    var notified = 0;
    holder
      ..addListener(() => notified++)
      ..apply(
        TimelineUpdateSnapshot(
          sessionId: 's1',
          items: [row('s:1', 'A'), row('s:2', 'B')],
        ),
      )
      // [A, B] -> [A, B', C]
      ..apply(
        TimelineUpdateUpserted(key: 's:2', item: row('s:2', "B'"), index: 1),
      )
      ..apply(
        TimelineUpdateUpserted(key: 's:3', item: row('s:3', 'C'), index: 2),
      )
      // -> [A, C]
      ..apply(const TimelineUpdateRemoved(key: 's:2'));

    expect(holder.sessionId, 's1');
    expect(texts(holder), ['s:1=A', 's:3=C']);
    expect(holder.indexOf('s:3'), 1);
    expect(holder.indexOf('s:2'), -1);
    expect(notified, 4);
  });

  test('a logged message replacing a pending one keeps the selection', () {
    final holder = TimelineHolder()
      ..apply(
        TimelineUpdateSnapshot(
          sessionId: 's1',
          items: [row('s:1', 'A'), message('p:c1', 'hi', pending: true)],
        ),
      )
      ..select('p:c1')
      ..apply(const TimelineUpdateRemoved(key: 'p:c1'));
    // Gone for a moment: nothing to show as selected, but kept.
    expect(holder.selected, isNull);
    holder.apply(
      TimelineUpdateUpserted(
        key: 's:2',
        item: message('s:2', 'hi', pending: false),
        index: 1,
        replaces: 'p:c1',
      ),
    );

    expect(holder.selectedKey, 's:2');
    expect(holder.selected?.text, 'hi');
    expect(texts(holder), ['s:1=A', 's:2=hi']);
  });

  test('an upsert that replaces drops the pending row itself too', () {
    final holder = TimelineHolder()
      ..apply(
        TimelineUpdateSnapshot(
          sessionId: 's1',
          items: [message('p:c1', 'hi', pending: true)],
        ),
      )
      ..apply(
        TimelineUpdateUpserted(
          key: 's:2',
          item: message('s:2', 'hi', pending: false),
          index: 0,
          replaces: 'p:c1',
        ),
      );
    expect(texts(holder), ['s:2=hi']);
  });

  test('another session\'s snapshot clears the selection', () {
    final holder = TimelineHolder()
      ..apply(TimelineUpdateSnapshot(sessionId: 's1', items: [row('s:1', 'A')]))
      ..select('s:1')
      ..apply(
        TimelineUpdateSnapshot(sessionId: 's1', items: [row('s:1', 'A')]),
      );
    expect(holder.selectedKey, 's:1');
    holder.apply(const TimelineUpdateSnapshot(sessionId: 's2', items: []));
    expect(holder.selectedKey, isNull);
    expect(holder.rows, isEmpty);
  });
}
