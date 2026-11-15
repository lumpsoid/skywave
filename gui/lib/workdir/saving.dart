import 'dart:io';

import 'package:file_selector/file_selector.dart';
import 'package:flutter/foundation.dart';
import 'package:flutter_file_dialog/flutter_file_dialog.dart';

/// Saves a downloaded file, at [source] on this device, under [name] at a
/// place the user picks; returns the path it saved to, or null when they
/// cancel.
typedef SaveWorkdirFile = Future<String?> Function({
  required String source,
  required String name,
});

/// The saver the workdir page uses. Tests replace it.
SaveWorkdirFile saveWorkdirFile = _savePlatform;

/// file_selector has no save dialog on a phone, so a phone gets the system
/// save-as sheet, which copies the file itself.
Future<String?> _savePlatform({required String source, required String name}) =>
    switch (defaultTargetPlatform) {
      TargetPlatform.android ||
      TargetPlatform.iOS => _saveMobile(source: source, name: name),
      _ => _saveDesktop(source: source, name: name),
    };

Future<String?> _saveDesktop({
  required String source,
  required String name,
}) async {
  final location = await getSaveLocation(suggestedName: name);
  if (location == null) {
    return null;
  }
  await File(source).copy(location.path);
  return location.path;
}

Future<String?> _saveMobile({required String source, required String name}) =>
    FlutterFileDialog.saveFile(
      params: SaveFileDialogParams(sourceFilePath: source, fileName: name),
    );
