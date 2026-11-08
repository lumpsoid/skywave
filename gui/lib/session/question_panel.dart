import 'package:flutter/material.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/session/approval_panel.dart' show approvalDockWidth;
import 'package:skywave_gui/session/look.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// The question the agent is blocked on, above the composer: each
/// question with its options (description and preview beneath), a field
/// for an answer of the user's own, then Decline and Send while this
/// device may answer, and this device's answer once it did.
///
/// The picks stay here until Send, which goes once every question has an
/// answer: a single-select question takes one option or the user's own
/// text, the one replacing the other; a multi-select one takes any number
/// of options and text besides. The core drops answers that do not fit.
class QuestionPanelView extends StatefulWidget {
  const QuestionPanelView({
    required this.question,
    required this.send,
    this.onShowCall,
    super.key,
  });

  final QuestionPanel question;
  final SendIntent send;

  /// Reveals the call that asks in the timeline; null when it is not there.
  final VoidCallback? onShowCall;

  @override
  State<QuestionPanelView> createState() => _QuestionPanelViewState();
}

class _QuestionPanelViewState extends State<QuestionPanelView> {
  /// The options picked, by question.
  late List<Set<int>> _picked;

  /// The user's own answer, by question.
  late List<TextEditingController> _own;

  @override
  void initState() {
    super.initState();
    _reset();
  }

  @override
  void didUpdateWidget(QuestionPanelView old) {
    super.didUpdateWidget(old);
    if (old.question.approvalId != widget.question.approvalId) {
      _dispose();
      _reset();
    }
  }

  @override
  void dispose() {
    _dispose();
    super.dispose();
  }

  void _reset() {
    final count = widget.question.questions.length;
    _picked = List.generate(count, (_) => <int>{});
    _own = List.generate(count, (_) => TextEditingController());
  }

  void _dispose() {
    for (final own in _own) {
      own.dispose();
    }
  }

  String? _ownText(int index) {
    final text = _own[index].text;
    return text.trim().isEmpty ? null : text;
  }

  bool _answered(int index) {
    final given = _picked[index].length + (_ownText(index) == null ? 0 : 1);
    return widget.question.questions[index].multiSelect
        ? given >= 1
        : given == 1;
  }

  bool get _complete =>
      [for (var i = 0; i < _picked.length; i++) _answered(i)]
          .every((answered) => answered);

  void _pick(int index, int option) => setState(() {
    final picked = _picked[index];
    if (widget.question.questions[index].multiSelect) {
      if (!picked.remove(option)) picked.add(option);
      return;
    }
    picked
      ..clear()
      ..add(option);
    _own[index].clear();
  });

  void _write(int index) => setState(() {
    if (!widget.question.questions[index].multiSelect &&
        _ownText(index) != null) {
      _picked[index].clear();
    }
  });

  void _send() => widget.send.act(
    ActionIntentAnswerQuestion(
      approvalId: widget.question.approvalId,
      answers: [
        for (var i = 0; i < _picked.length; i++)
          QuestionAnswerView(
            picked: _picked[i].toList()..sort(),
            text: _ownText(i),
          ),
      ],
    ),
  );

  void _decline() => widget.send.act(
    ActionIntentResolveApproval(
      approvalId: widget.question.approvalId,
      decision: DecisionView.deny,
    ),
  );

  @override
  Widget build(BuildContext context) {
    final question = widget.question;
    final theme = Theme.of(context);
    final colors = SkyColors.of(context);
    final docked = MediaQuery.sizeOf(context).width < approvalDockWidth;
    final pad = docked ? 12.0 : 16.0;
    final edge = colors.warning.withValues(alpha: 0.6);

    final header = Padding(
      padding: EdgeInsets.fromLTRB(pad, 4, 4, 0),
      child: Row(
        children: [
          Icon(Icons.help_outline, color: colors.warning, size: 20),
          const SizedBox(width: 8),
          Expanded(
            child: Text(
              question.questions.length == 1 ? 'Question' : 'Questions',
              style: theme.textTheme.titleSmall?.copyWith(
                color: colors.warning,
              ),
            ),
          ),
          if (widget.onShowCall != null)
            TextButton.icon(
              onPressed: widget.onShowCall,
              icon: const Icon(Icons.visibility_outlined, size: 18),
              label: const Text('Show call'),
            )
          else
            const SizedBox(height: 40),
        ],
      ),
    );

    final body = Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        if (question.from case final from?)
          Padding(
            padding: const EdgeInsets.only(bottom: 4),
            child: Text(
              from,
              style: theme.textTheme.labelMedium?.copyWith(
                color: colors.warning,
              ),
            ),
          ),
        for (var i = 0; i < question.questions.length; i++)
          _Question(
            question: question.questions[i],
            picked: _picked[i],
            own: _own[i],
            enabled: question.canAnswer,
            onPick: (option) => _pick(i, option),
            onWrite: () => _write(i),
          ),
      ],
    );

    final footer = Padding(
      padding: EdgeInsets.fromLTRB(pad, 8, pad, docked ? 8 : 12),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          if (question.answerLabel case final answer?)
            Row(
              children: [
                if (question.answer case final outgoing?) ...[
                  StateBadge(
                    label: outgoing.label,
                    color: colors.outgoing(outgoing.state),
                  ),
                  const SizedBox(width: 8),
                ],
                Expanded(
                  child: Text(answer, style: theme.textTheme.bodyMedium),
                ),
              ],
            ),
          if (question.answerLabel != null && question.canAnswer)
            const SizedBox(height: 8),
          if (question.canAnswer)
            Row(
              spacing: 8,
              children: [
                Expanded(
                  child: OutlinedButton(
                    onPressed: _decline,
                    child: const Text('Decline', maxLines: 1),
                  ),
                ),
                Expanded(
                  child: FilledButton(
                    onPressed: _complete ? _send : null,
                    child: const Text('Send', maxLines: 1),
                  ),
                ),
              ],
            ),
        ],
      ),
    );

    return Card(
      margin: docked ? EdgeInsets.zero : const EdgeInsets.fromLTRB(8, 4, 8, 4),
      color: theme.colorScheme.surfaceContainerHigh,
      shape: docked
          ? Border(top: BorderSide(color: edge))
          : RoundedRectangleBorder(
              borderRadius: BorderRadius.circular(16),
              side: BorderSide(color: edge),
            ),
      child: Column(
        mainAxisSize: MainAxisSize.min,
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          header,
          Flexible(
            // Its scrolling stays in the panel, as the approval panel's.
            child: NotificationListener<ScrollNotification>(
              onNotification: (_) => true,
              child: NotificationListener<ScrollMetricsNotification>(
                onNotification: (_) => true,
                child: SingleChildScrollView(
                  primary: false,
                  padding: EdgeInsets.symmetric(horizontal: pad),
                  child: body,
                ),
              ),
            ),
          ),
          footer,
        ],
      ),
    );
  }
}

/// One question: its header and text, its options to pick, and a field
/// for the user's own answer.
class _Question extends StatelessWidget {
  const _Question({
    required this.question,
    required this.picked,
    required this.own,
    required this.enabled,
    required this.onPick,
    required this.onWrite,
  });

  final QuestionView question;
  final Set<int> picked;
  final TextEditingController own;
  final bool enabled;
  final ValueChanged<int> onPick;
  final VoidCallback onWrite;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final colors = SkyColors.of(context);
    final small = theme.textTheme.bodySmall?.copyWith(color: colors.muted);
    final mono = theme.textTheme.bodySmall?.copyWith(
      fontFamily: monoFamily,
      fontFamilyFallback: monoFallback,
    );
    final multi = question.multiSelect;
    return Padding(
      padding: const EdgeInsets.only(bottom: 8),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          if (question.header.isNotEmpty)
            Text(
              question.header,
              style: theme.textTheme.labelMedium?.copyWith(color: colors.muted),
            ),
          Text(question.text, style: theme.textTheme.bodyLarge),
          Text(multi ? 'Pick any' : 'Pick one', style: small),
          for (var o = 0; o < question.options.length; o++)
            InkWell(
              onTap: enabled ? () => onPick(o) : null,
              child: Padding(
                padding: const EdgeInsets.symmetric(vertical: 4),
                child: Row(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Icon(switch ((multi, picked.contains(o))) {
                      (true, true) => Icons.check_box,
                      (true, false) => Icons.check_box_outline_blank,
                      (false, true) => Icons.radio_button_checked,
                      (false, false) => Icons.radio_button_unchecked,
                    }, size: 20),
                    const SizedBox(width: 8),
                    Expanded(
                      child: Column(
                        crossAxisAlignment: CrossAxisAlignment.stretch,
                        children: [
                          Text(question.options[o].label),
                          if (question.options[o].description.isNotEmpty)
                            Text(question.options[o].description, style: small),
                          if (question.options[o].preview case final preview?)
                            Container(
                              margin: const EdgeInsets.only(top: 4),
                              padding: const EdgeInsets.all(8),
                              color: theme.colorScheme.surfaceContainerHighest,
                              child: SingleChildScrollView(
                                scrollDirection: Axis.horizontal,
                                child: Text(preview, style: mono),
                              ),
                            ),
                        ],
                      ),
                    ),
                  ],
                ),
              ),
            ),
          TextField(
            controller: own,
            enabled: enabled,
            onChanged: (_) => onWrite(),
            decoration: InputDecoration(
              isDense: true,
              labelText: multi
                  ? 'Also, in your own words'
                  : 'Or your own answer',
            ),
          ),
        ],
      ),
    );
  }
}
