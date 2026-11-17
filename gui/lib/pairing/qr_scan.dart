import 'dart:io';

import 'package:flutter/services.dart';

/// Opens the camera and gives the scanned QR code's text, or null when the
/// user backed out.
typedef ScanQr = Future<String?> Function();

const _channel = MethodChannel('io.github.lumpsoid.skywave/scan');

/// The platform's scanner: Android's ZXing activity (D20, MainActivity),
/// null elsewhere, where pairing is typed or pasted.
final ScanQr? platformScanQr = Platform.isAndroid ? _scan : null;

Future<String?> _scan() async {
  try {
    return await _channel.invokeMethod<String>('scan');
  } on PlatformException {
    return null; // A scan is already open.
  }
}
