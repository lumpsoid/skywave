import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';

import '../test/session/timeline_cost.dart';

/// Runs the timeline estimate measurement on a device, in profile mode:
/// `flutter drive --profile -d <device> --driver=test_driver/integration_test.dart --target=integration_test/timeline_cost_test.dart`.
/// It is a measurement that prints, not a guard: nothing here asserts on times.
void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  group('estimate cost on device', () {
    testWidgets(
      'scrolling 500 rows reports per-frame layout time',
      (tester) => measureScroll(tester, 500),
    );
    testWidgets(
      'scrolling 5000 rows reports per-frame layout time',
      (tester) => measureScroll(tester, 5000),
    );
    testWidgets(
      'scrolling 20000 rows reports per-frame layout time',
      (tester) => measureScroll(tester, 20000),
    );
  });
}
