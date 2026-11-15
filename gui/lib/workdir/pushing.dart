import 'package:file_selector/file_selector.dart';

/// Picks a file on this device to push into a workdir; its path, or null
/// when the user cancels.
typedef PickPushFile = Future<String?> Function();

/// The picker the workdir page uses. Tests replace it.
PickPushFile pickPushFile = _pickPlatform;

/// file_selector's open dialog on every platform. Not yet tried on a phone.
Future<String?> _pickPlatform() async => (await openFile())?.path;
