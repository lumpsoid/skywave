import 'package:material_ui/material_ui.dart';
import 'package:skywave_gui/intents.dart';
import 'package:skywave_gui/screen/readable_width.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

/// Settings → Notifications: the switches for this device's notifications,
/// all of them or per kind. They are this device's own preferences, so the
/// page has no core route. [ScreenNavigator] shows it over Settings while
/// `Holders.notificationsOpen` is set, and [onBack] clears that.
class NotificationsPage extends StatelessWidget {
  const NotificationsPage({
    required this.view,
    required this.send,
    required this.onBack,
    super.key,
  });

  final NotificationsView view;
  final SendIntent send;
  final VoidCallback onBack;

  @override
  Widget build(BuildContext context) {
    // Each change sends the whole set, with the one value flipped.
    void change({bool? all, bool? approvals, bool? idle, bool? shell}) => send(
      GuiIntentSetNotifications(
        all: all ?? view.all,
        approvals: approvals ?? view.approvals,
        idle: idle ?? view.idle,
        shell: shell ?? view.shell,
      ),
    );
    return Scaffold(
      appBar: AppBar(
        leading: BackButton(onPressed: onBack),
        title: const Text('Notifications'),
      ),
      body: ReadableWidth(
        child: ListView(
          children: [
            SwitchListTile(
              key: const Key('notify-all'),
              title: const Text('Notifications'),
              subtitle: const Text('System notifications on this device.'),
              value: view.all,
              onChanged: (value) => change(all: value),
            ),
            // The kinds keep their values while all of them is off.
            SwitchListTile(
              key: const Key('notify-approvals'),
              title: const Text('Approvals'),
              subtitle: const Text(
                'An agent waits for an approval or an answer.',
              ),
              value: view.approvals,
              onChanged: view.all ? (value) => change(approvals: value) : null,
            ),
            SwitchListTile(
              key: const Key('notify-idle'),
              title: const Text('Agent idle'),
              subtitle: const Text(
                "An agent finished all its work, background tasks included. "
                "Loops don't count.",
              ),
              value: view.idle,
              onChanged: view.all ? (value) => change(idle: value) : null,
            ),
            SwitchListTile(
              key: const Key('notify-shell'),
              title: const Text('Long shell runs'),
              subtitle: const Text(
                'A shell command you ran for 10 seconds or more finished.',
              ),
              value: view.shell,
              onChanged: view.all ? (value) => change(shell: value) : null,
            ),
          ],
        ),
      ),
    );
  }
}
