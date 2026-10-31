import 'package:flutter/material.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/session/timeline_holder.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// The open session: its header and status, the timeline's rows as plain
/// text, and the composer. A placeholder for G6's session view.
class SessionPage extends StatelessWidget {
  const SessionPage({
    required this.screen,
    required this.timeline,
    required this.send,
    super.key,
  });

  final SessionScreen screen;
  final TimelineHolder timeline;
  final SendIntent send;

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(
        title: Text(screen.title),
        bottom: PreferredSize(
          preferredSize: const Size.fromHeight(20),
          child: Text(
            [
              screen.agentLabel,
              ?screen.interruptLabel,
              ?screen.stopLabel,
              ?screen.rulesLabel,
            ].join(' · '),
          ),
        ),
      ),
      body: Column(
        children: [
          Expanded(
            child: ListenableBuilder(
              listenable: timeline,
              builder: (context, _) {
                final rows = timeline.rows;
                return ListView.builder(
                  itemCount: rows.length,
                  findChildIndexCallback: (key) {
                    final index = timeline.indexOf(
                      (key as ValueKey<String>).value,
                    );
                    return index < 0 ? null : index;
                  },
                  itemBuilder: (context, i) =>
                      _RowText(key: ValueKey(rows[i].key), row: rows[i]),
                );
              },
            ),
          ),
          if (screen.approval case final approval?)
            ListTile(
              title: Text('${approval.tool} ${approval.title}'),
              subtitle: Text(approval.answerLabel ?? 'needs approval'),
            ),
          _Composer(
            sessionId: screen.sessionId,
            composer: screen.composer,
            send: send,
          ),
        ],
      ),
    );
  }
}

class _RowText extends StatelessWidget {
  const _RowText({required this.row, super.key});

  final TimelineRow row;

  @override
  Widget build(BuildContext context) {
    final tool = row.tool;
    final head = tool == null
        ? [?row.author, row.text].join(': ')
        : '${tool.name} ${tool.title} · ${tool.stateLabel}';
    return ListTile(
      dense: true,
      leading: row.timeLabel == null ? null : Text(row.timeLabel!),
      title: Text(head),
      subtitle: row.deliveryLabel == null ? null : Text(row.deliveryLabel!),
    );
  }
}

/// The composer: the user's field, set from Rust only when `rev` changes.
class _Composer extends StatefulWidget {
  const _Composer({
    required this.sessionId,
    required this.composer,
    required this.send,
  });

  final String sessionId;
  final ComposerView composer;
  final SendIntent send;

  @override
  State<_Composer> createState() => _ComposerState();
}

class _ComposerState extends State<_Composer> {
  final _field = TextEditingController();
  int? _rev;

  @override
  void initState() {
    super.initState();
    _take(widget.composer);
  }

  @override
  void didUpdateWidget(_Composer old) {
    super.didUpdateWidget(old);
    _take(widget.composer);
  }

  void _take(ComposerView composer) {
    if (composer.rev == _rev) return;
    _rev = composer.rev;
    _field.value = TextEditingValue(
      text: composer.text,
      selection: TextSelection.collapsed(offset: composer.cursorUtf16),
    );
  }

  void _edited(String text) => widget.send(
    GuiIntentDraftEdited(
      sessionId: widget.sessionId,
      text: text,
      cursorUtf16: _field.selection.baseOffset.clamp(0, text.length),
    ),
  );

  @override
  void dispose() {
    _field.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return Padding(
      padding: const EdgeInsets.all(8),
      child: Row(
        children: [
          Expanded(
            child: TextField(
              controller: _field,
              onChanged: _edited,
              decoration: InputDecoration(
                hintText: 'Write to the agent…',
                helperText:
                    widget.composer.offlineLabel ??
                    widget.composer.historyLabel,
              ),
            ),
          ),
          IconButton(
            tooltip: 'Send',
            icon: const Icon(Icons.send),
            onPressed: () => widget.send(
              GuiIntentComposerSubmit(sessionId: widget.sessionId),
            ),
          ),
        ],
      ),
    );
  }
}
