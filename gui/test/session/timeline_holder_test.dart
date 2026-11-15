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

  test('rows and indexOf follow every update, read between them', () {
    final holder = TimelineHolder()
      ..apply(
        TimelineUpdateSnapshot(
          sessionId: 's1',
          items: [row('s:1', 'A'), row('s:2', 'B'), row('s:3', 'C')],
        ),
      );
    expect(holder.indexOf('s:3'), 2);

    holder.apply(
      TimelineUpdateUpserted(key: 's:0', item: row('s:0', 'Z'), index: 0),
    );
    expect(holder.indexOf('s:3'), 3);
    expect(holder.indexOf('s:0'), 0);
    expect(texts(holder), ['s:0=Z', 's:1=A', 's:2=B', 's:3=C']);

    holder.apply(const TimelineUpdateRemoved(key: 's:1'));
    expect(holder.indexOf('s:3'), 2);
    expect(holder.indexOf('s:1'), -1);

    holder.apply(
      TimelineUpdateUpserted(
        key: 's:4',
        item: row('s:4', 'D'),
        index: 1,
        replaces: 's:2',
      ),
    );
    expect(holder.indexOf('s:2'), -1);
    expect(holder.indexOf('s:4'), 1);
    expect(holder.indexOf('s:3'), 2);

    holder.apply(
      TimelineUpdateSnapshot(sessionId: 's1', items: [row('s:9', 'Y')]),
    );
    expect(holder.indexOf('s:3'), -1);
    expect(holder.indexOf('s:9'), 0);
  });

  test('rows is one snapshot per update', () {
    final holder = TimelineHolder()
      ..apply(
        TimelineUpdateSnapshot(
          sessionId: 's1',
          items: [row('s:1', 'A'), row('s:2', 'B')],
        ),
      );
    final a = holder.rows;
    expect(identical(holder.rows, a), isTrue);

    holder.apply(
      TimelineUpdateUpserted(key: 's:3', item: row('s:3', 'C'), index: 2),
    );
    expect(a, hasLength(2));
    expect(identical(holder.rows, a), isFalse);
    expect(holder.rows, hasLength(3));
    expect(holder.rows.last.key, 's:3');
    expect(() => a.add(row('s:x', 'x')), throwsUnsupportedError);
  });

  test('a snapshot with a repeated key indexes the first', () {
    final holder = TimelineHolder()
      ..apply(
        TimelineUpdateSnapshot(
          sessionId: 's1',
          items: [row('s:1', 'A'), row('s:1', 'B')],
        ),
      );
    expect(holder.indexOf('s:1'), 0);
  });
}
