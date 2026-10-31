import 'package:flutter/painting.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/session/highlight.dart';
import 'package:skywave_gui/session/status_line.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

void main() {
  const mark = TextStyle(fontWeight: FontWeight.bold);

  List<(String, bool)> parts(List<InlineSpan> spans) => [
    for (final span in spans.cast<TextSpan>()) (span.text!, span.style == mark),
  ];

  test('ranges are UTF-16, so an emoji takes two units', () {
    expect(
      parts(
        highlighted('héllo 👋 x', const [
          Utf16Range(start: 1, end: 2),
          Utf16Range(start: 6, end: 8),
        ], mark),
      ),
      [('h', false), ('é', true), ('llo ', false), ('👋', true), (' x', false)],
    );
  });

  test('ranges past the end are clipped', () {
    expect(
      parts(highlighted('abc', const [Utf16Range(start: 2, end: 9)], mark)),
      [('ab', false), ('c', true)],
    );
    expect(parts(highlighted('abc', const [], mark)), [('abc', false)]);
  });

  test('usage drops fields from the front when short of room', () {
    double width(String text) => text.length.toDouble();
    const fields = ['last turn 12.4s', r'$0.04', 'opus'];
    expect(fitTail(fields, width, 100), r'last turn 12.4s · $0.04 · opus');
    expect(fitTail(fields, width, 14), r'$0.04 · opus');
    expect(fitTail(fields, width, 4), 'opus');
    expect(fitTail(fields, width, 3), '');
  });
}
