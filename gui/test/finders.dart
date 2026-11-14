import 'package:flutter_test/flutter_test.dart';
import 'package:material_ui/material_ui.dart';

/// `find.byTooltip` that also matches material_ui's [Tooltip] when it
/// excludes itself from semantics: flutter_test only reads the message of
/// its own `Tooltip` class, and material_ui's leaves the [RawTooltip]
/// without a semantics tooltip then.
Finder byTooltip(Pattern message, {bool skipOffstage = true}) {
  bool matches(String text) =>
      message is RegExp ? message.hasMatch(text) : text == message;
  return find.byWidgetPredicate((widget) {
    if (widget is Tooltip && (widget.excludeFromSemantics ?? false)) {
      return matches(widget.message ?? widget.richMessage!.toPlainText());
    }
    return widget is RawTooltip && matches(widget.semanticsTooltip ?? '');
  }, skipOffstage: skipOffstage);
}
