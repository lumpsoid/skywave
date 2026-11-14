import 'package:flutter/material.dart';

/// Shown while Rust boots and until its first screen arrives. It never
/// animates: for a wait under a second a calm page reads better than a
/// spinner, and Flutter 3.50's Linux Wayland renderer can deadlock when the
/// app renders continuously during window setup (flutter/flutter#196529).
class StartupPage extends StatelessWidget {
  const StartupPage({super.key});

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Scaffold(
      body: Center(
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            Text('Skywave', style: theme.textTheme.headlineMedium),
            const SizedBox(height: 8),
            Text(
              'Starting…',
              style: theme.textTheme.bodyMedium?.copyWith(
                color: theme.colorScheme.onSurfaceVariant,
              ),
            ),
          ],
        ),
      ),
    );
  }
}
