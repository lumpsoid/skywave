import 'package:file_selector/file_selector.dart';
import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';
import 'package:image_picker/image_picker.dart';

/// Asks the user for images and returns their local paths; empty when
/// they cancel.
typedef PickImages = Future<List<String>> Function(BuildContext context);

/// The picker the composer uses. Tests replace it.
PickImages pickImages = _pickPlatform;

Future<List<String>> _pickPlatform(BuildContext context) =>
    switch (defaultTargetPlatform) {
      TargetPlatform.android || TargetPlatform.iOS => _pickMobile(context),
      _ => _pickDesktop(),
    };

Future<List<String>> _pickDesktop() async {
  final files = await openFiles(
    acceptedTypeGroups: const [
      XTypeGroup(
        label: 'Images',
        extensions: ['png', 'jpg', 'jpeg', 'gif', 'webp'],
        mimeTypes: ['image/png', 'image/jpeg', 'image/gif', 'image/webp'],
        uniformTypeIdentifiers: ['public.image'],
      ),
    ],
  );
  return [for (final file in files) file.path];
}

enum _Source { gallery, camera }

/// A phone has no file dialog worth showing: the gallery or the camera.
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
    case null:
      return const [];
  }
}
