import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/effects/effects.dart';
import 'package:skywave_gui/link/link_holder.dart';
import 'package:skywave_gui/session/search_holder.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

import '../fakes.dart';

void main() {
  test('each effect reaches its sink method, and nothing is kept', () {
    final sink = FakeEffectSink();
    EffectsListener(sink)
      ..apply(
        const PlatformEffectNotify(
          title: 'work',
          body: 'approval needed · work',
          sessionId: 's1',
        ),
      )
      ..apply(const PlatformEffectCopyToClipboard(text: 'cargo test'))
      ..apply(const PlatformEffectOpenFile(path: '/tmp/blob'))
      ..apply(const PlatformEffectNotice(text: 'copied 10 B'))
      ..apply(const PlatformEffectError(text: 'disk full'));

    expect(sink.done, [
      'notify: approval needed · work (s1)',
      'copy: cargo test',
      'open: /tmp/blob',
      'notice: copied 10 B',
      'error: disk full',
    ]);
  });

  test('the banner and the search are replaced whole', () {
    final link = LinkHolder()
      ..apply(
        const LinkUpdate(
          state: LinkState.online,
          label: 'online · 2 queued',
          connectionLabel: 'Connected · 12 ms',
          canRetryNow: false,
          rejected: false,
          queued: 2,
          approvalsElsewhere: 0,
        ),
      );
    expect(link.link?.label, 'online · 2 queued');

    final search = SearchHolder()
      ..apply(
        const SearchUpdate(
          open: true,
          query: 'foo',
          countLabel: '1 match',
          wrapped: false,
          hits: [
            SearchHit(
              key: 's:3',
              text: [Utf16Range(start: 0, end: 3)],
              title: [],
              detail: [],
              lines: [],
            ),
          ],
        ),
      );
    expect(search.hitFor('s:3')?.text.single.end, 3);
    expect(search.hitFor('s:4'), isNull);
  });
}
