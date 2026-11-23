import 'package:material_ui/material_ui.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/session/look.dart';
import 'package:skywave_gui/session/section_block.dart';
import 'package:skywave_gui/session/trust_command.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// Below this width the panel docks to the composer: no side margin,
/// square corners, a border on top only.
const approvalDockWidth = 600.0;

/// Preview lines shown until the user expands the preview.
const approvalPreviewLines = 8;

/// The approval the agent is blocked on, above the composer: which
/// subagent asks, if one does, the gated
/// call and what it will do, then Approve, Deny and Always allow while
/// this device may answer, and this device's answer once it did.
///
/// The title and the buttons stay put; only what lies between scrolls
/// when the panel has less height than it needs.
class ApprovalPanelView extends StatefulWidget {
  const ApprovalPanelView({
    required this.approval,
    required this.sessionId,
    required this.send,
    this.onShowCall,
    this.shortcuts = false,
    super.key,
  });

  final ApprovalPanel approval;
  final String sessionId;
  final SendIntent send;

  /// Reveals the gated call in the timeline; null when it is not there.
  final VoidCallback? onShowCall;

  /// The session page binds Ctrl+Shift+A / Ctrl+Shift+D to this panel: its
  /// buttons say so.
  final bool shortcuts;

  @override
  State<ApprovalPanelView> createState() => _ApprovalPanelViewState();
}

class _ApprovalPanelViewState extends State<ApprovalPanelView> {
  /// The preview shows all its lines.
  bool _expanded = false;

  @override
  void didUpdateWidget(ApprovalPanelView old) {
    super.didUpdateWidget(old);
    if (old.approval.approvalId != widget.approval.approvalId) {
      _expanded = false;
    }
  }

  void _resolve(DecisionView decision, {int? rule}) => widget.send.act(
    ActionIntentResolveApproval(
      approvalId: widget.approval.approvalId,
      decision: decision,
      rule: rule,
    ),
  );

  /// The button, with the key that answers it named when the page binds one.
  Widget _tipped(String tip, Widget button) =>
      widget.shortcuts ? Tooltip(message: tip, child: button) : button;

  Future<void> _always(BuildContext context) async {
    final suggestions = widget.approval.suggestions;
    if (!widget.approval.alwaysPicks) {
      _resolve(DecisionView.approve, rule: suggestions.first.index);
      return;
    }
    final rule = await showDialog<int>(
      context: context,
      builder: (context) => SimpleDialog(
        title: const Text('Always allow which rule?'),
        children: [
          for (final option in suggestions)
            SimpleDialogOption(
              onPressed: () => Navigator.of(context).pop(option.index),
              child: Row(
                children: [
                  const Icon(Icons.rule, size: 18),
                  const SizedBox(width: 12),
                  Expanded(child: Text(option.label)),
                ],
              ),
            ),
        ],
      ),
    );
    if (rule != null) _resolve(DecisionView.approve, rule: rule);
  }

  /// Trusts the prefixes the person keeps, then approves: rules do not
  /// re-check an approval already pending, so trusting alone would leave
  /// this one waiting.
  Future<void> _trust(BuildContext context) async {
    final rules = await askTrustCommand(
      context,
      widget.approval.ruleCandidates,
      confirmLabel: 'Trust and approve',
    );
    if (!mounted || rules == null || rules.isEmpty) return;
    for (final words in rules) {
      widget.send.act(
        ActionIntentSetSafetyRule(
          sessionId: widget.sessionId,
          words: words,
          on: true,
        ),
      );
    }
    _resolve(DecisionView.approve);
  }

  @override
  Widget build(BuildContext context) {
    final approval = widget.approval;
    final theme = Theme.of(context);
    final colors = SkyColors.of(context);
    final mono = theme.textTheme.bodyMedium?.copyWith(
      fontFamily: monoFamily,
      fontFamilyFallback: monoFallback,
    );
    final small = theme.textTheme.bodySmall?.copyWith(color: colors.muted);
    final docked = MediaQuery.sizeOf(context).width < approvalDockWidth;
    final pad = docked ? 12.0 : 16.0;
    final edge = colors.warning.withValues(alpha: 0.6);

    final header = Padding(
      padding: EdgeInsets.fromLTRB(pad, 4, 4, 0),
      child: Row(
        children: [
          Icon(Icons.gpp_maybe_outlined, color: colors.warning, size: 20),
          const SizedBox(width: 8),
          Expanded(
            child: Text(
              'Approval needed',
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
            // The height the button would give the row.
            const SizedBox(height: 40),
        ],
      ),
    );

    final body = Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        if (approval.from case final from?)
          Padding(
            padding: const EdgeInsets.only(bottom: 4),
            child: Text(
              from,
              style: theme.textTheme.labelMedium?.copyWith(
                color: colors.warning,
              ),
            ),
          ),
        Row(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Icon(toolIcon(approval.kind), size: 18),
            const SizedBox(width: 8),
            Expanded(
              child: Text.rich(
                TextSpan(
                  children: [
                    TextSpan(
                      text: approval.tool,
                      style: const TextStyle(fontWeight: FontWeight.bold),
                    ),
                    const TextSpan(text: '  '),
                    TextSpan(text: approval.title, style: mono),
                  ],
                ),
              ),
            ),
          ],
        ),
        if (approval.detail case final detail?)
          Padding(
            padding: const EdgeInsets.only(top: 2),
            child: Text(detail, style: small),
          ),
        // Before what the call will do: the user decides on it, and
        // it is not all of it.
        if (approval.lost case final lost?)
          Padding(
            padding: const EdgeInsets.only(top: 8),
            child: Row(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Icon(
                  Icons.warning_amber_rounded,
                  size: 18,
                  color: colors.warning,
                ),
                const SizedBox(width: 8),
                Expanded(
                  child: Text(
                    lost,
                    style: theme.textTheme.bodyMedium?.copyWith(
                      color: colors.warning,
                      fontWeight: FontWeight.bold,
                    ),
                  ),
                ),
              ],
            ),
          ),
        if (approval.preview case final preview?)
          Padding(
            padding: const EdgeInsets.only(top: 8),
            child: _Preview(
              preview: preview,
              expanded: _expanded,
              onToggle: () => setState(() => _expanded = !_expanded),
              send: widget.send,
            ),
          ),
        if (approval.inputLabel case final input?)
          Padding(
            padding: const EdgeInsets.only(top: 4),
            child: Text(input, style: small),
          ),
        // What the approval asked differs from the call shown: Show call
        // reveals it, labelled.
        if (approval.askedLabel case final asked?)
          Padding(
            padding: const EdgeInsets.only(top: 4),
            child: Text(asked, style: small?.copyWith(color: colors.warning)),
          ),
        if (approval.alwaysLabel case final always?)
          Padding(
            padding: const EdgeInsets.only(top: 6),
            child: Text(always, style: small),
          ),
      ],
    );

    final footer = Padding(
      padding: EdgeInsets.fromLTRB(pad, 8, pad, docked ? 8 : 12),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          if (approval.answerLabel case final answer?)
            Row(
              children: [
                if (approval.answer case final outgoing?) ...[
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
          if (approval.answerLabel != null && approval.canAnswer)
            const SizedBox(height: 8),
          if (approval.canAnswer)
            // One line on any width: equal shares, labels scaled down
            // rather than wrapped onto a second row.
            Row(
              spacing: 8,
              children: [
                if (approval.suggestions.isNotEmpty)
                  Expanded(
                    child: TextButton(
                      style: _compact,
                      onPressed: () => _always(context),
                      child: _Label(
                        approval.alwaysPicks ? 'Always allow…' : 'Always allow',
                      ),
                    ),
                  ),
                if (approval.ruleCandidates.isNotEmpty)
                  Expanded(
                    child: TextButton(
                      key: const Key('approval-trust'),
                      style: _compact,
                      onPressed: () => _trust(context),
                      child: const _Label('Trust…'),
                    ),
                  ),
                Expanded(
                  child: _tipped(
                    'Deny (Ctrl+Shift+D)',
                    OutlinedButton(
                      style: _compact,
                      onPressed: () => _resolve(DecisionView.deny),
                      child: const _Label('Deny'),
                    ),
                  ),
                ),
                Expanded(
                  child: _tipped(
                    'Approve (Ctrl+Shift+A)',
                    FilledButton(
                      style: _compact,
                      onPressed: () => _resolve(DecisionView.approve),
                      child: const _Label('Approve'),
                    ),
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
            // Its scrolling stays in the panel: the app bar tints for the
            // timeline's, and would take this for it.
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

/// The approval's preview, cut to [approvalPreviewLines] until expanded.
class _Preview extends StatelessWidget {
  const _Preview({
    required this.preview,
    required this.expanded,
    required this.onToggle,
    required this.send,
  });

  final SectionView preview;
  final bool expanded;
  final VoidCallback onToggle;
  final SendIntent send;

  @override
  Widget build(BuildContext context) {
    final hidden = preview.lines.length - approvalPreviewLines;
    final cut = hidden > 0 && !expanded;
    final blob = preview.blob;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        SectionBlock(
          section: cut
              ? preview.copyWith(
                  lines: preview.lines.take(approvalPreviewLines).toList(),
                )
              : preview,
          onDownload: blob == null
              ? null
              : () => send.act(ActionIntentDownloadBlob(blobId: blob.blobId)),
          onOpenFull: blob == null
              ? null
              : () => send(GuiIntentOpenFull(blobId: blob.blobId)),
        ),
        if (hidden > 0)
          Align(
            alignment: Alignment.centerLeft,
            child: TextButton.icon(
              style: TextButton.styleFrom(
                visualDensity: VisualDensity.compact,
                padding: const EdgeInsets.symmetric(horizontal: 8),
                tapTargetSize: MaterialTapTargetSize.shrinkWrap,
              ),
              onPressed: onToggle,
              icon: Icon(expanded ? Icons.expand_less : Icons.expand_more),
              label: Text(
                expanded
                    ? 'Show less'
                    : 'Show $hidden more line${hidden == 1 ? '' : 's'}',
              ),
            ),
          ),
      ],
    );
  }
}

final _compact = ButtonStyle(
  padding: WidgetStateProperty.all(const EdgeInsets.symmetric(horizontal: 8)),
);

/// A button label that shrinks to fit its share of the row.
class _Label extends StatelessWidget {
  const _Label(this.text);

  final String text;

  @override
  Widget build(BuildContext context) =>
      FittedBox(fit: BoxFit.scaleDown, child: Text(text, maxLines: 1));
}
