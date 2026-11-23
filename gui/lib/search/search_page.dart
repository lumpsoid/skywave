import 'package:material_ui/material_ui.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/link/link_status.dart';
import 'package:skywave_gui/screen/readable_width.dart';
import 'package:skywave_gui/session/highlight.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// Search: the hits of a query over every session, or over the one session
/// the screen is scoped to (its chip drops the scope). Typing sends the query
/// and Rust answers with the hits; a tap on a hit opens it at its item.
class SearchPage extends StatefulWidget {
  const SearchPage({
    required this.screen,
    required this.send,
    this.canGoBack = false,
    super.key,
  });

  final ScreenViewSearch screen;
  final bool canGoBack;
  final SendIntent send;

  @override
  State<SearchPage> createState() => _SearchPageState();
}

class _SearchPageState extends State<SearchPage> {
  /// What is typed. The screen's query seeds it once: a later update must
  /// not overwrite what the user is typing.
  late final TextEditingController _query;

  @override
  void initState() {
    super.initState();
    _query = TextEditingController(text: widget.screen.query);
  }

  @override
  void dispose() {
    _query.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final muted = theme.textTheme.bodySmall?.copyWith(
      color: theme.colorScheme.onSurfaceVariant,
    );
    final match = MatchStyle.of(context).match;
    final screen = widget.screen;
    final send = widget.send;
    final scoped = screen.scopeId != null;
    final status = [?screen.countLabel, ?screen.partialLabel];
    return Scaffold(
      appBar: AppBar(
        leading: backButton(
          canGoBack: widget.canGoBack,
          onBack: () => send.act(const ActionIntentBack()),
        ),
        toolbarHeight: LinkTitle.toolbarHeight(context),
        flexibleSpace: const LinkProgress(),
        title: TextField(
          controller: _query,
          autofocus: true,
          decoration: InputDecoration(
            hintText: scoped ? 'Search this session' : 'Search all sessions',
            border: InputBorder.none,
          ),
          onChanged: (text) => send(GuiIntentGlobalSearchTyped(query: text)),
        ),
      ),
      body: ReadableWidth(
        child: Column(
          children: [
            if (scoped)
              Padding(
                padding: const EdgeInsets.fromLTRB(16, 12, 16, 0),
                child: Align(
                  alignment: Alignment.centerLeft,
                  child: Chip(
                    label: Text('In: ${screen.scopeTitle ?? ''}'),
                    deleteButtonTooltipMessage: 'Search all sessions',
                    onDeleted: () =>
                        send.act(const ActionIntentOpenSearch(sessionId: null)),
                  ),
                ),
              ),
            if (status.isNotEmpty)
              Padding(
                padding: const EdgeInsets.fromLTRB(16, 12, 16, 0),
                child: Text(status.join(' · '), style: muted),
              ),
            Expanded(
              child: ListView(
                children: [
                  for (final row in screen.rows)
                    ListTile(
                      key: Key('hit-${row.key}'),
                      title: Text.rich(
                        TextSpan(
                          children: highlighted(
                            row.snippet,
                            row.matched,
                            match,
                          ),
                        ),
                        maxLines: 2,
                        overflow: TextOverflow.ellipsis,
                      ),
                      subtitle: Text(
                        [
                          row.kindLabel,
                          if (!scoped) row.sessionTitle,
                          row.timeLabel,
                        ].join(' · '),
                      ),
                      onTap: () =>
                          send(GuiIntentGlobalSearchOpen(key: row.key)),
                    ),
                ],
              ),
            ),
          ],
        ),
      ),
    );
  }
}
