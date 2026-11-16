import 'package:flutter/services.dart';
import 'package:flutter_svg/flutter_svg.dart';
import 'package:material_ui/material_ui.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/app.dart';
import 'package:skywave_gui/link/link_banner.dart';
import 'package:skywave_gui/link/link_holder.dart';
import 'package:skywave_gui/session/look.dart';
import 'package:skywave_gui/sessions/session_list_page.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

import '../link/link_banner_test.dart' show link;
import '../sessions/session_list_page_test.dart' show list, sessionRow;

/// WCAG's contrast ratio of [a] over [b].
double contrast(Color a, Color b) {
  final (la, lb) = (a.computeLuminance(), b.computeLuminance());
  return (la > lb ? la + 0.05 : lb + 0.05) / (la > lb ? lb + 0.05 : la + 0.05);
}

/// The app's colours in [brightness].
Future<SkyColors> colorsIn(WidgetTester tester, Brightness brightness) async {
  late SkyColors colors;
  await tester.pumpWidget(
    MaterialApp(
      theme: skywaveTheme(brightness),
      home: Builder(
        builder: (context) {
          colors = SkyColors.of(context);
          return const SizedBox();
        },
      ),
    ),
  );
  return colors;
}

void main() {
  test('each agent state has its own icon', () {
    final icons = AgentState.values.map(agentIconAsset).toSet();
    expect(icons, hasLength(AgentState.values.length));
  });

  test('every agent state icon is a bundled svg', () async {
    TestWidgetsFlutterBinding.ensureInitialized();
    for (final state in AgentState.values) {
      final svg = await rootBundle.loadString(agentIconAsset(state));
      expect(svg, contains('<svg'), reason: '$state');
    }
  });

  for (final brightness in Brightness.values) {
    testWidgets('$brightness state colours meet AA on every surface', (
      tester,
    ) async {
      final colors = await colorsIn(tester, brightness);
      final scheme = colors.scheme;
      for (final surface in [
        scheme.surface,
        scheme.surfaceContainerLowest,
        scheme.surfaceContainerLow,
        scheme.surfaceContainer,
        scheme.surfaceContainerHigh,
        scheme.surfaceContainerHighest,
      ]) {
        for (final state in AgentState.values) {
          expect(
            contrast(colors.agent(state), surface),
            greaterThanOrEqualTo(4.5),
            reason: '$state on $surface',
          );
        }
      }
    });

    testWidgets('$brightness banner text meets AA on its background', (
      tester,
    ) async {
      final colors = await colorsIn(tester, brightness);
      for (final refused in [true, false]) {
        final (:background, :foreground) = colors.banner(refused: refused);
        expect(
          contrast(foreground, background),
          greaterThanOrEqualTo(4.5),
          reason: 'refused: $refused',
        );
      }
    });
  }

  testWidgets('dark mode takes the dark working and banner colours', (
    tester,
  ) async {
    final working = SessionRowView(
      sessionId: 'w',
      title: 'busy',
      group: '',
      groupName: 'no directory',
      branch: null,
      state: AgentState.working,
      badgeLabel: 'working',
      unread: 0,
      approvals: 0,
      hasDraft: false,
      lastActivityLabel: sessionRow('a').lastActivityLabel,
      lastActivityMs: 0,
      deletable: false,
      deletableEverywhere: false,
      removable: false,
      worktreeRemoved: false,
      archived: false,
    );
    // The row shows the state as its icon, tinted with the state's colour.
    ColorFilter? tint() => tester
        .widget<SvgPicture>(
          find.descendant(
            of: find.byKey(const Key('row-state-w')),
            matching: find.byType(SvgPicture),
          ),
        )
        .colorFilter;

    for (final (brightness, expected) in [
      (Brightness.light, const Color(0xFF01579B)),
      (Brightness.dark, const Color(0xFF81D4FA)),
    ]) {
      await tester.pumpWidget(
        MaterialApp(
          theme: skywaveTheme(brightness),
          home: SessionListPage(
            screen: list(rows: [working]),
            send: (_) {},
          ),
        ),
      );
      // The theme cross-fades when it changes.
      await tester.pumpAndSettle();
      expect(find.text('working'), findsNothing, reason: '$brightness');
      expect(
        tester
            .widget<AgentStateIcon>(find.byKey(const Key('row-state-w')))
            .state,
        AgentState.working,
      );
      expect(
        tint(),
        ColorFilter.mode(expected, BlendMode.srcIn),
        reason: '$brightness',
      );
    }

    final holder = LinkHolder();
    await tester.pumpWidget(
      MaterialApp(
        theme: skywaveTheme(Brightness.dark),
        home: Scaffold(
          body: LinkBanner(holder: holder, send: (_) {}),
        ),
      ),
    );
    holder.apply(
      link(LinkState.online, '', loggedOutLabel: 'agent not logged in'),
    );
    await tester.pumpAndSettle();
    final banner = tester.widget<Material>(
      find
          .ancestor(
            of: find.byKey(const Key('logged-out-label')),
            matching: find.byType(Material),
          )
          .first,
    );
    expect(banner.color, const Color(0xFF4A3800));
    expect(
      tester
          .widget<Text>(find.byKey(const Key('logged-out-label')))
          .style!
          .color,
      const Color(0xFFFFE082),
    );
  });
}
