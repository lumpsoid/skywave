import 'package:flutter/gestures.dart';
import 'package:material_ui/material_ui.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/session/markdown_body.dart';
import 'package:skywave_gui/session/search_holder.dart';
import 'package:skywave_gui/session/timeline_holder.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

import 'rows.dart';

MarkdownSpan span(
  String text, {
  bool bold = false,
  bool code = false,
  String? link,
}) => MarkdownSpan(
  text: text,
  bold: bold,
  italic: false,
  code: code,
  strike: false,
  link: link,
);

MarkdownBlock block(MarkdownKind kind, {int depth = 0, String? marker}) =>
    MarkdownBlock(kind: kind, depth: depth, quote: 0, marker: marker);

MarkdownBlock paragraph(
  List<MarkdownSpan> spans, {
  int depth = 0,
  String? marker,
}) => block(
  MarkdownKindParagraph(spans: spans),
  depth: depth,
  marker: marker,
);

/// Every span of every rich text on screen, with its style.
List<TextSpan> spans(WidgetTester tester) {
  final out = <TextSpan>[];
  for (final text in tester.widgetList<RichText>(find.byType(RichText))) {
    text.text.visitChildren((span) {
      if (span is TextSpan && span.text != null) out.add(span);
      return true;
    });
  }
  return out;
}

void main() {
  Future<void> pump(WidgetTester tester, List<MarkdownBlock> blocks) =>
      tester.pumpWidget(
        MaterialApp(
          home: Scaffold(
            body: MarkdownBody(blocks: blocks, style: const TextStyle()),
          ),
        ),
      );

  testWidgets('blocks show styled, list items under their markers', (
    tester,
  ) async {
    await pump(tester, [
      block(MarkdownKindHeading(level: 1, spans: [span('Plan')])),
      paragraph([
        span('run '),
        span('cargo', code: true),
        span(' now', bold: true),
      ]),
      paragraph([span('first')], depth: 1, marker: '1.'),
      paragraph([span('nested')], depth: 2, marker: '•'),
      block(const MarkdownKindRule()),
      block(
        const MarkdownKindTable(
          align: [MarkdownAlign.left, MarkdownAlign.right],
          head: [
            MarkdownCell(
              spans: [
                MarkdownSpan(
                  text: 'a',
                  bold: false,
                  italic: false,
                  code: false,
                  strike: false,
                ),
              ],
            ),
            MarkdownCell(spans: []),
          ],
          rows: [
            MarkdownRow(
              cells: [
                MarkdownCell(
                  spans: [
                    MarkdownSpan(
                      text: '1',
                      bold: false,
                      italic: false,
                      code: false,
                      strike: false,
                    ),
                  ],
                ),
              ],
            ),
          ],
        ),
      ),
    ]);

    final all = spans(tester);
    TextSpan of(String text) => all.firstWhere((s) => s.text == text);
    final heading = tester
        .widgetList<RichText>(find.byType(RichText))
        .firstWhere((t) => t.text.toPlainText() == 'Plan');
    // Text.rich wraps the block's span in one carrying the default style.
    final title = (heading.text as TextSpan).children!.single;
    expect(title.style?.fontWeight, FontWeight.bold);
    expect(of('cargo').style?.backgroundColor, isNotNull);
    expect(of(' now').style?.fontWeight, FontWeight.bold);
    expect(find.text('1.'), findsOneWidget);
    expect(find.byType(Divider), findsOneWidget);
    expect(find.byType(Table), findsOneWidget);
    // The nested item sits further right than its parent.
    expect(
      tester.getTopLeft(find.text('•')).dx,
      greaterThan(tester.getTopLeft(find.text('1.')).dx),
    );
  });

  testWidgets('a code block copies its text', (tester) async {
    final copied = <String>[];
    tester.binding.defaultBinaryMessenger.setMockMethodCallHandler(
      SystemChannels.platform,
      (call) async {
        if (call.method == 'Clipboard.setData') {
          copied.add((call.arguments as Map)['text'] as String);
        }
        return null;
      },
    );
    await pump(tester, [
      block(const MarkdownKindCode(lang: 'rust', text: 'fn a() {}\nfn b() {}')),
    ]);
    expect(find.text('rust'), findsOneWidget);
    await tester.tap(find.byTooltip('Copy code'));
    await tester.pump();
    expect(copied, ['fn a() {}\nfn b() {}']);
  });

  testWidgets('a link asks before it opens, showing where it goes', (
    tester,
  ) async {
    await pump(tester, [
      paragraph([span('the docs', link: 'https://example.org/docs')]),
    ]);
    final link = spans(tester).firstWhere((s) => s.text == 'the docs');
    expect(link.recognizer, isNotNull);
    // Tapping the span runs its recogniser.
    (link.recognizer! as TapGestureRecognizer).onTap!();
    await tester.pumpAndSettle();
    expect(find.text('Open link?'), findsOneWidget);
    expect(find.text('https://example.org/docs'), findsOneWidget);
    await tester.tap(find.text('Cancel'));
    await tester.pumpAndSettle();
    expect(find.text('Open link?'), findsNothing);
  });

  testWidgets(
    'an agent message shows its markdown, raw while search marks it',
    (tester) async {
      final search = SearchHolder();
      await pumpSession(
        tester,
        screen: screenWith(),
        timeline: TimelineHolder()
          ..apply(
            TimelineUpdateSnapshot(
              sessionId: 's1',
              items: [
                anyRow(
                  's:1',
                  RowKind.agentMessage,
                  '**Done** here',
                  author: 'agent',
                  markdown: [
                    paragraph([span('Done', bold: true), span(' here')]),
                  ],
                ),
              ],
            ),
          ),
        search: search,
        sent: [],
      );
      expect(spans(tester).any((s) => s.text == 'Done'), isTrue);
      expect(
        spans(tester).any((s) => s.text?.contains('**') ?? false),
        isFalse,
      );

      search.apply(
        const SearchUpdate(
          open: true,
          query: 'here',
          countLabel: '1 of 1',
          wrapped: false,
          currentKey: 's:1',
          hits: [
            SearchHit(
              key: 's:1',
              text: [Utf16Range(start: 9, end: 13)],
              title: [],
              detail: [],
              lines: [],
            ),
          ],
        ),
      );
      await tester.pumpAndSettle();
      expect(spans(tester).any((s) => s.text == '**Done** '), isTrue);
      expect(
        spans(tester)
            .firstWhere((s) => s.text == 'here')
            .style
            ?.backgroundColor,
        isNotNull,
      );
    },
  );
}
