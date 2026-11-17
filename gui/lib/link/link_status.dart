import 'dart:math';

import 'package:material_ui/material_ui.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/link/link_holder.dart';
import 'package:skywave_gui/session/look.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// The link for every page's app bar: [LinkTitle], [LinkProgress] and
/// [ApprovalsButton] find it here. Above the app's navigator, so a dialog
/// sees it too. The holder never changes, so finding it registers no
/// dependency: each widget listens to the holder itself, and only it
/// rebuilds on an update.
class LinkScope extends InheritedWidget {
  const LinkScope({required this.holder, required super.child, super.key});

  final LinkHolder holder;

  /// Null outside a [LinkScope]: a page shown on its own, in a test.
  static LinkHolder? maybeOf(BuildContext context) =>
      context.getInheritedWidgetOfExactType<LinkScope>()?.holder;

  @override
  bool updateShouldNotify(LinkScope oldWidget) => holder != oldWidget.holder;
}

/// An app bar's title, with the link's status line under it while the
/// link is on its way back (Rust's `statusLabel`: `Connecting…`,
/// `Syncing · 4/9`, `Offline · retry in 07s · …`) in place of the page's
/// own [subtitle]. The line fits the toolbar ([toolbarHeight]), so
/// nothing below moves when it comes and goes. While offline a tap on it
/// says the link in full, with Retry now.
class LinkTitle extends StatelessWidget {
  const LinkTitle({
    required this.title,
    required this.send,
    this.subtitle,
    super.key,
  });

  final Widget title;
  final Widget? subtitle;
  final SendIntent send;

  /// The toolbar's height for a [LinkTitle] in the theme's app bar title
  /// style: the standard one, or taller when the text scale makes the
  /// title and the status line with a margin (8) need more. The same with
  /// or without the status line.
  static double toolbarHeight(BuildContext context) {
    final theme = Theme.of(context);
    final title =
        theme.appBarTheme.titleTextStyle ?? theme.textTheme.titleLarge!;
    return heightFor(context, title);
  }

  /// [toolbarHeight] for a title in [titleStyle].
  static double heightFor(BuildContext context, TextStyle titleStyle) {
    final lines =
        lineHeightOf(context, titleStyle) +
        lineHeightOf(context, Theme.of(context).textTheme.labelMedium!);
    return max(kToolbarHeight, (lines + 8).ceilToDouble());
  }

  @override
  Widget build(BuildContext context) {
    final holder = LinkScope.maybeOf(context);
    if (holder == null) return _column(subtitle);
    return ListenableBuilder(
      listenable: holder,
      builder: (context, _) {
        final link = holder.link;
        return _column(switch (link?.statusLabel) {
          final status? => _Status(
            link: link!,
            status: status,
            onTap: link.canRetryNow
                ? () => _showLink(context, holder, send)
                : null,
          ),
          null => subtitle,
        });
      },
    );
  }

  Widget _column(Widget? under) => Column(
    crossAxisAlignment: CrossAxisAlignment.start,
    mainAxisSize: MainAxisSize.min,
    children: [title, ?under],
  );

  /// The link in full and Retry now, for a status line cut short.
  static void _showLink(
    BuildContext context,
    LinkHolder holder,
    SendIntent send,
  ) {
    showModalBottomSheet<void>(
      context: context,
      showDragHandle: true,
      builder: (context) => SafeArea(
        child: Padding(
          padding: const EdgeInsets.fromLTRB(24, 0, 24, 16),
          child: ListenableBuilder(
            listenable: holder,
            builder: (context, _) {
              final link = holder.link;
              return Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                mainAxisSize: MainAxisSize.min,
                children: [
                  Text(
                    link?.statusLabel ?? link?.connectionLabel ?? '',
                    key: const Key('link-status-full'),
                    style: Theme.of(context).textTheme.bodyLarge,
                  ),
                  const SizedBox(height: 16),
                  Align(
                    alignment: Alignment.centerRight,
                    child: FilledButton.icon(
                      icon: const Icon(Icons.refresh),
                      label: const Text('Retry now'),
                      onPressed: (link?.canRetryNow ?? false)
                          ? () {
                              send.act(const ActionIntentRetryNow());
                              Navigator.pop(context);
                            }
                          : null,
                    ),
                  ),
                ],
              );
            },
          ),
        ),
      ),
    );
  }
}

/// The status line: the state's icon and Rust's label, amber while
/// offline, muted while connecting or syncing.
class _Status extends StatelessWidget {
  const _Status({required this.link, required this.status, this.onTap});

  final LinkUpdate link;
  final String status;
  final VoidCallback? onTap;

  @override
  Widget build(BuildContext context) {
    final colors = SkyColors.of(context);
    final offline = link.state == LinkState.reconnecting;
    final color = offline ? colors.warning : colors.muted;
    final style = Theme.of(context).textTheme.labelMedium
        ?.copyWith(color: color);
    final line = Row(
      mainAxisSize: MainAxisSize.min,
      children: [
        Icon(
          offline ? Icons.cloud_off : Icons.cloud_sync,
          size: (style?.fontSize ?? 12) + 2,
          color: color,
        ),
        const SizedBox(width: 4),
        Flexible(
          child: Text(
            status,
            key: const Key('link-status'),
            style: style,
            maxLines: 1,
            overflow: TextOverflow.ellipsis,
          ),
        ),
      ],
    );
    return onTap == null
        ? line
        : InkWell(
            onTap: onTap,
            borderRadius: BorderRadius.circular(4),
            child: line,
          );
  }
}

/// A 2 dp line along the bottom of an app bar while the link connects or
/// syncs, for its `flexibleSpace`: it takes no room.
class LinkProgress extends StatelessWidget {
  const LinkProgress({super.key});

  @override
  Widget build(BuildContext context) {
    final holder = LinkScope.maybeOf(context);
    if (holder == null) return const SizedBox.shrink();
    return ListenableBuilder(
      listenable: holder,
      builder: (context, _) {
        final busy = switch (holder.link?.state) {
          LinkState.connecting || LinkState.catchingUp => true,
          _ => false,
        };
        if (!busy) return const SizedBox.shrink();
        return const Align(
          alignment: Alignment.bottomCenter,
          child: LinearProgressIndicator(
            key: Key('link-progress'),
            minHeight: 2,
            backgroundColor: Colors.transparent,
          ),
        );
      },
    );
  }
}

/// The approvals waiting in other sessions, for an app bar's actions: a
/// badged bell whose tap opens the approvals page, which answers them all;
/// Ctrl+G opens the next one instead. Nothing while none waits.
class ApprovalsButton extends StatelessWidget {
  const ApprovalsButton({required this.send, super.key});

  final SendIntent send;

  @override
  Widget build(BuildContext context) {
    final holder = LinkScope.maybeOf(context);
    if (holder == null) return const SizedBox.shrink();
    return ListenableBuilder(
      listenable: holder,
      builder: (context, _) {
        final link = holder.link;
        final label = link?.elsewhereLabel;
        if (link == null || label == null) return const SizedBox.shrink();
        final colors = SkyColors.of(context);
        return IconButton(
          key: const Key('approvals-elsewhere'),
          tooltip: '$label · see them all (Ctrl+G opens the next)',
          onPressed: () =>
              send.act(const ActionIntentNavigate(route: RouteView.approvals)),
          icon: Badge.count(
            count: link.approvalsElsewhere,
            backgroundColor: colors.warning,
            textColor: Theme.of(context).colorScheme.surface,
            child: Icon(Icons.notification_important, color: colors.warning),
          ),
        );
      },
    );
  }
}
