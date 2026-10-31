import 'package:flutter/material.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/session/look.dart';
import 'package:skywave_gui/session/section_block.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// The approval the agent is blocked on, above the composer: the gated
/// call and what it will do, then Approve, Deny and Always allow while
/// this device may answer, and this device's answer once it did.
class ApprovalPanelView extends StatelessWidget {
  const ApprovalPanelView({
    required this.approval,
    required this.send,
    this.onShowCall,
    super.key,
  });

  final ApprovalPanel approval;
  final SendIntent send;

  /// Reveals the gated call in the timeline; null when it is not there.
  final VoidCallback? onShowCall;

  void _resolve(DecisionView decision, {int? rule}) => send.act(
    ActionIntentResolveApproval(
      approvalId: approval.approvalId,
      decision: decision,
      rule: rule,
    ),
  );

  Future<void> _always(BuildContext context) async {
    final suggestions = approval.suggestions;
    if (!approval.alwaysPicks) {
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

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final colors = SkyColors.of(context);
    final mono = theme.textTheme.bodyMedium?.copyWith(
      fontFamily: monoFamily,
      fontFamilyFallback: monoFallback,
    );
    final small = theme.textTheme.bodySmall?.copyWith(color: colors.muted);
    final media = MediaQuery.sizeOf(context);

    return Card(
      margin: const EdgeInsets.fromLTRB(8, 4, 8, 4),
      color: theme.colorScheme.surfaceContainerHigh,
      shape: RoundedRectangleBorder(
        borderRadius: BorderRadius.circular(16),
        side: BorderSide(color: colors.warning.withValues(alpha: 0.6)),
      ),
      child: ConstrainedBox(
        constraints: BoxConstraints(maxHeight: media.height * 0.45),
        child: SingleChildScrollView(
          primary: false,
          padding: const EdgeInsets.fromLTRB(16, 12, 16, 12),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              Row(
                children: [
                  Icon(Icons.gpp_maybe_outlined, color: colors.warning),
                  const SizedBox(width: 8),
                  Text(
                    'Approval needed',
                    style: theme.textTheme.titleSmall?.copyWith(
                      color: colors.warning,
                    ),
                  ),
                  const Spacer(),
                  if (onShowCall != null)
                    TextButton.icon(
                      onPressed: onShowCall,
                      icon: const Icon(Icons.visibility_outlined, size: 18),
                      label: const Text('Show call'),
                    ),
                ],
              ),
              const SizedBox(height: 6),
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
                  padding: const EdgeInsets.only(top: 2, left: 26),
                  child: Text(detail, style: small),
                ),
              if (approval.preview case final preview?)
                Padding(
                  padding: const EdgeInsets.only(top: 8),
                  child: SectionBlock(
                    section: preview,
                    onDownload: preview.blob == null
                        ? null
                        : () => send.act(
                            ActionIntentDownloadBlob(
                              blobId: preview.blob!.blobId,
                            ),
                          ),
                    onOpenFull: preview.blob == null
                        ? null
                        : () => send(
                            GuiIntentOpenFull(blobId: preview.blob!.blobId),
                          ),
                  ),
                ),
              if (approval.inputLabel case final input?)
                Padding(
                  padding: const EdgeInsets.only(top: 4),
                  child: Text(input, style: small),
                ),
              if (approval.alwaysLabel case final always?)
                Padding(
                  padding: const EdgeInsets.only(top: 8),
                  child: Text(always, style: small),
                ),
              if (approval.answerLabel case final answer?)
                Padding(
                  padding: const EdgeInsets.only(top: 8),
                  child: Row(
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
                ),
              if (approval.canAnswer)
                Padding(
                  padding: const EdgeInsets.only(top: 10),
                  child: Wrap(
                    spacing: 8,
                    runSpacing: 8,
                    alignment: WrapAlignment.end,
                    children: [
                      if (approval.suggestions.isNotEmpty)
                        TextButton.icon(
                          onPressed: () => _always(context),
                          icon: const Icon(Icons.verified_outlined, size: 18),
                          label: Text(
                            approval.alwaysPicks
                                ? 'Always allow…'
                                : 'Always allow',
                          ),
                        ),
                      OutlinedButton.icon(
                        onPressed: () => _resolve(DecisionView.deny),
                        icon: const Icon(Icons.block, size: 18),
                        label: const Text('Deny'),
                      ),
                      FilledButton.icon(
                        onPressed: () => _resolve(DecisionView.approve),
                        icon: const Icon(Icons.check, size: 18),
                        label: const Text('Approve'),
                      ),
                    ],
                  ),
                ),
            ],
          ),
        ),
      ),
    );
  }
}
