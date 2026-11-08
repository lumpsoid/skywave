import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/session/info_page.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

ScreenViewSessionInfo infoScreen({
  List<SubagentRowView> subagents = const [],
  String? subagentsEmptyLabel,
}) => ScreenViewSessionInfo(
  sessionId: 's1',
  sections: const [
    InfoSectionView(
      title: 'Session',
      fields: [
        InfoFieldView(label: 'Name', value: 'fix the flaky test'),
        InfoFieldView(label: 'Workdir', value: '/srv/projects/app'),
      ],
    ),
    InfoSectionView(
      title: 'Usage',
      fields: [
        InfoFieldView(label: 'Tokens in', value: '153k'),
        InfoFieldView(label: 'Context', value: '19k/1M · 2%'),
      ],
    ),
    InfoSectionView(
      title: 'Agent',
      fields: [InfoFieldView(label: 'Model', value: 'Opus')],
    ),
  ],
  limits: const LimitsView(
    readLabel: 'Read 12 min ago',
    rows: [
      LimitRowView(
        label: 'Weekly',
        percentLabel: '40%',
        fillPercent: 40,
        warning: false,
      ),
    ],
  ),
  subagents: subagents,
  subagentsEmptyLabel: subagentsEmptyLabel,
);

void main() {
  testWidgets('the info page shows the sections, limits and subagents', (
    tester,
  ) async {
    final sent = <GuiIntent>[];
    await tester.pumpWidget(
      MaterialApp(
        home: InfoPage(
          screen: infoScreen(
            subagents: const [
              SubagentRowView(
                title: 'find the bug',
                detail: 'Explore',
                trail: 'outer',
                statusLabel: 'finished · 14 steps · 4.3s · ctx 12k',
                live: false,
              ),
            ],
          ),
          canGoBack: true,
          send: sent.add,
        ),
      ),
    );
    for (final text in [
      'fix the flaky test',
      '/srv/projects/app',
      'Usage limits',
      'Read 12 min ago',
      'Weekly',
      '153k',
      '19k/1M · 2%',
      'Opus',
      'Subagents (1)',
      'outer › find the bug',
      'Explore · finished · 14 steps · 4.3s · ctx 12k',
    ]) {
      expect(find.text(text), findsOneWidget, reason: text);
    }

    await tester.tap(find.byTooltip('Refresh usage limits'));
    await tester.tap(find.byType(BackButton));
    expect(sent, [
      const GuiIntentAct(action: ActionIntentRefreshUsageLimits()),
      const GuiIntentAct(action: ActionIntentBack()),
    ]);
  });

  testWidgets('a model name too long for the label column shows whole', (
    tester,
  ) async {
    const model = 'claude-opus-5-5-20261101[1m] (1M context)';
    await tester.pumpWidget(
      MaterialApp(
        home: InfoPage(
          screen: ScreenViewSessionInfo(
            sessionId: 's1',
            sections: [
              ...infoScreen().sections,
              const InfoSectionView(
                title: 'Models',
                fields: [
                  InfoFieldView(
                    label: model,
                    value: r'$1.23 · 3.1k in · 812 out · 2 turns',
                  ),
                ],
              ),
            ],
            limits: const LimitsView(rows: []),
            subagents: const [],
          ),
          send: (_) {},
        ),
      ),
    );
    final label = find.text(model);
    expect(label, findsOneWidget);
    // Laid out whole on its own line, wider than the label column.
    expect(tester.getSize(label).width, greaterThan(112));
    expect(
      tester.getTopLeft(find.text(r'$1.23 · 3.1k in · 812 out · 2 turns')).dy,
      greaterThan(tester.getBottomLeft(label).dy - 1),
    );
  });

  testWidgets('a session without subagents says so', (tester) async {
    await tester.pumpWidget(
      MaterialApp(
        home: InfoPage(
          screen: infoScreen(
            subagentsEmptyLabel: 'No subagents ran in this session.',
          ),
          send: (_) {},
        ),
      ),
    );
    expect(find.text('Subagents (0)'), findsOneWidget);
    expect(find.text('No subagents ran in this session.'), findsOneWidget);
  });
}
