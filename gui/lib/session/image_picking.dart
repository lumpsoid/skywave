import 'package:file_selector/file_selector.dart';
import 'package:flutter/foundation.dart';
import 'package:material_ui/material_ui.dart';
import 'package:image_picker/image_picker.dart';
import 'package:pasteboard/pasteboard.dart';

/// Asks the user for files (images among them) and returns their local
/// paths; empty when they cancel. The hub tells an image from any other
/// file by its content.
typedef PickAttachments = Future<List<String>> Function(BuildContext context);

/// The picker the composer uses. Tests replace it.
PickAttachments pickAttachments = _pickPlatform;

/// The image on the system clipboard as PNG bytes, or null when it holds
/// none. The composer uses it on desktop paste. Tests replace it.
Future<Uint8List?> Function() readClipboardImage = _pasteboardImage;

Future<Uint8List?> _pasteboardImage() => Pasteboard.image;

Future<List<String>> _pickPlatform(BuildContext context) =>
    switch (defaultTargetPlatform) {
      TargetPlatform.android || TargetPlatform.iOS => _pickMobile(context),
      _ => _pickDesktop(),
    };

Future<List<String>> _pickDesktop() async {
  final files = await openFiles();
  return [for (final file in files) file.path];
}

enum _Source { gallery, camera, file }

/// A phone: the gallery, the camera, or a file from the phone's storage.
Future<List<String>> _pickMobile(BuildContext context) async {
  final source = await showModalBottomSheet<_Source>(
    context: context,
    builder: (context) => SafeArea(
      child: Column(
        mainAxisSize: MainAxisSize.min,
        children: [
          ListTile(
            leading: const Icon(Icons.photo_library_outlined),
            title: const Text('Gallery'),
            onTap: () => Navigator.pop(context, _Source.gallery),
          ),
          ListTile(
            leading: const Icon(Icons.photo_camera_outlined),
            title: const Text('Camera'),
            onTap: () => Navigator.pop(context, _Source.camera),
          ),
          ListTile(
            leading: const Icon(Icons.insert_drive_file_outlined),
            title: const Text('File'),
            onTap: () => Navigator.pop(context, _Source.file),
          ),
        ],
      ),
    ),
  );
  final picker = ImagePicker();
  switch (source) {
    case _Source.gallery:
      final images = await picker.pickMultiImage();
      return [for (final image in images) image.path];
    case _Source.camera:
      final image = await picker.pickImage(source: ImageSource.camera);
      return [?image?.path];
    case _Source.file:
      final files = await openFiles();
      return [for (final file in files) file.path];
    case null:
      return const [];
  }
}
