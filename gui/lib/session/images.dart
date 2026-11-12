import 'dart:async';

import 'package:open_filex/open_filex.dart';

/// Opens a local image with the platform's viewer: a queued message's
/// image, whose file is still on this device. Tests replace it.
void Function(String path) openLocalImage = _openWithSystem;

void _openWithSystem(String path) => unawaited(OpenFilex.open(path));
