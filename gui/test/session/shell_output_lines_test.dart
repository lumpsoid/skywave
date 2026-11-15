import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/session/shell_output.dart';

void main() {
  test('null and empty text give no lines', () {
    final cache = OutputLines();
    expect(cache.of(null), isEmpty);
    expect(cache.of(''), isEmpty);
  });

  test('a final line break ends the last line instead of starting one', () {
    expect(OutputLines().of('a\nb\n'), ['a', 'b']);
    expect(OutputLines().of('a\nb'), ['a', 'b']);
    expect(OutputLines().of('a\n\n'), ['a', '']);
  });

  test('the same content in another string returns the identical list', () {
    final cache = OutputLines();
    final a = 'x\ny\n' * 1000;
    final b = List.filled(1000, 'x\ny\n').join();
    expect(identical(a, b), isFalse);
    expect(identical(cache.of(a), cache.of(b)), isTrue);
  });

  test('a different text splits again, and the first text splits again', () {
    final cache = OutputLines();
    final a = 'x\ny\n' * 1000;
    final first = cache.of(a);
    expect(cache.of('z\n'), ['z']);
    expect(cache.of(a), equals(first));
  });
}
