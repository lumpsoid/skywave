import 'package:material_ui/material_ui.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/link/link_holder.dart';
import 'package:skywave_gui/settings/settings_page.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

const url = 'https://claude.ai/oauth/authorize?code=true&state=abc';

/// The login as the hub maps it (`agent_login_view`): the flags follow the
/// state, the lines are `flow_lines`' words.
AgentLoginView login(
  LoginFlowState state,
  List<String> lines, {
  bool offered = true,
  bool loggedOut = false,
  String? url,
}) => AgentLoginView(
  canLogIn: offered,
  loggedOut: loggedOut,
  state: state,
  url: url,
  lines: lines,
  canStart:
      offered &&
      state != LoginFlowState.starting &&
      state != LoginFlowState.finishing,
  canFinish: state == LoginFlowState.open,
  canRetry: state == LoginFlowState.noAnswer,
  canDismiss: state != LoginFlowState.idle,
);

Widget settings(
  AgentLoginView? login,
  List<GuiIntent> sent, {
  String? restorationScopeId,
}) => MaterialApp(
  restorationScopeId: restorationScopeId,
  home: SettingsPage(
    screen: ScreenViewSettings(
      blobPolicy: BlobPolicyView.onDemand,
      blobPolicyLabel: "A tool's blob downloads when you expand it.",
      server: 'srv:7443',
      device: 'dev-1',
      agentLogin: 'expires in 12 days · 2026-11-21',
      login: login,
      notifications: NotificationsView(
        all: true,
        approvals: true,
        idle: true,
        shell: true,
      ),
      safetyRules: const [],
    ),
    link: LinkHolder(),
    send: sent.add,
    onNotifications: () {},
  ),
);

void main() {
  late List<GuiIntent> sent;

  setUp(() => sent = []);

  Future<void> show(WidgetTester tester, AgentLoginView? login) async {
    // Tall enough that the whole page, the login under the blob wording,
    // is built.
    tester.view
      ..physicalSize = const Size(800, 1400)
      ..devicePixelRatio = 1;
    addTearDown(tester.view.reset);
    await tester.pumpWidget(settings(login, sent));
  }

  final logIn = find.byKey(const Key('agent-log-in'));
  final code = find.byKey(const Key('login-code'));
  final progress = find.byType(LinearProgressIndicator);

  bool enabled(WidgetTester tester, Finder button) =>
      tester.widget<ButtonStyleButton>(button).onPressed != null;

  testWidgets('Log in shows only where the server offers it, and starts', (
    tester,
  ) async {
    await show(tester, login(LoginFlowState.idle, [], offered: false));
    expect(find.text('expires in 12 days · 2026-11-21'), findsOneWidget);
    expect(logIn, findsNothing);

    await show(tester, login(LoginFlowState.idle, []));
    expect(find.text('Log in'), findsOneWidget);
    expect(find.text('Close'), findsNothing);
    await tester.tap(logIn);
    expect(sent, [const GuiIntentAct(action: ActionIntentStartAgentLogin())]);
  });

  testWidgets('a logged-out agent says so under the tile', (tester) async {
    await show(
      tester,
      login(LoginFlowState.idle, ['agent not logged in'], loggedOut: true),
    );
    expect(find.text('agent not logged in'), findsOneWidget);
    expect(enabled(tester, logIn), isTrue);
  });

  testWidgets('starting shows progress, and no second start', (tester) async {
    await show(tester, login(LoginFlowState.starting, ['starting the login…']));
    expect(find.text('starting the login…'), findsOneWidget);
    expect(progress, findsOneWidget);
    expect(enabled(tester, logIn), isFalse);
    expect(code, findsNothing);
    await tester.tap(find.text('Close'));
    expect(sent, [const GuiIntentAct(action: ActionIntentDismissAgentLogin())]);
  });

  testWidgets('an open login shows the URL to copy or open, and the code '
      'pasted goes out once', (tester) async {
    final copied = <String>[];
    tester.binding.defaultBinaryMessenger.setMockMethodCallHandler(
      SystemChannels.platform,
      (call) async {
        if (call.method == 'Clipboard.setData') {
          copied.add((call.arguments as Map)['text'] as String);
        }
        return null;
      },
    );
    addTearDown(
      () => tester.binding.defaultBinaryMessenger.setMockMethodCallHandler(
        SystemChannels.platform,
        null,
      ),
    );
    await show(
      tester,
      login(LoginFlowState.open, [
        'open this URL, sign in, paste the code shown',
      ], url: url),
    );
    expect(
      find.text('open this URL, sign in, paste the code shown'),
      findsOneWidget,
    );
    expect(find.text(url), findsOneWidget);
    expect(find.byTooltip('Open in browser'), findsOneWidget);
    expect(progress, findsNothing);
    await tester.tap(find.byTooltip('Copy URL'));
    expect(copied, [url]);

    await tester.enterText(code, '  abc#def  ');
    expect(sent, isEmpty, reason: 'typing sends nothing, no draft');
    await tester.tap(find.text('Send code'));
    await tester.pump();
    expect(sent, [
      const GuiIntentAct(
        action: ActionIntentFinishAgentLogin(
          code: PastedCode(text: '  abc#def  '),
        ),
      ),
    ]);
    expect(find.text('  abc#def  '), findsOneWidget, reason: 'not yet sent');

    await show(
      tester,
      login(LoginFlowState.finishing, ['sending the code…'], url: url),
    );
    expect(code, findsNothing);
    // Were the code to come back to the same URL, the field is empty.
    await show(tester, login(LoginFlowState.open, [], url: url));
    expect(code, findsOneWidget);
    expect(find.text('  abc#def  '), findsNothing, reason: 'gone once sent');
  });

  testWidgets('a code refused locally says why and stays to be fixed', (
    tester,
  ) async {
    const long = 'abc#def-and-much-more';
    await show(tester, login(LoginFlowState.open, [], url: url));
    await tester.enterText(code, long);
    await tester.tap(find.text('Send code'));
    await show(
      tester,
      login(LoginFlowState.open, [
        'open this URL, sign in, paste the code shown',
        'code too long (over 4096 bytes) · paste the code shown',
      ], url: url),
    );
    expect(
      find.text('code too long (over 4096 bytes) · paste the code shown'),
      findsOneWidget,
    );
    expect(find.text(long), findsOneWidget);
  });

  testWidgets('an unsent code does not outlive its login', (tester) async {
    const other = 'https://claude.ai/oauth/authorize?code=true&state=new';
    await show(tester, login(LoginFlowState.open, [], url: url));
    await tester.enterText(code, 'old-code');

    // Log in again, unsent: the start fails offline, then a new login
    // opens under another URL.
    await tester.tap(logIn);
    await show(tester, login(LoginFlowState.starting, []));
    await show(
      tester,
      login(LoginFlowState.failed, [
        'offline · nothing was asked · start again once connected',
      ]),
    );
    await show(tester, login(LoginFlowState.open, [], url: other));
    expect(find.text(other), findsOneWidget);
    expect(code, findsOneWidget);
    expect(find.text('old-code'), findsNothing);

    // The URL changing alone, the field still open, clears it too.
    await tester.enterText(code, 'stale');
    await show(tester, login(LoginFlowState.open, [], url: url));
    expect(find.text('stale'), findsNothing);
  });

  testWidgets('finishing shows progress and the URL, without the field', (
    tester,
  ) async {
    await show(
      tester,
      login(LoginFlowState.finishing, ['sending the code…'], url: url),
    );
    expect(find.text('sending the code…'), findsOneWidget);
    expect(progress, findsOneWidget);
    expect(find.text(url), findsOneWidget);
    expect(code, findsNothing);
    expect(enabled(tester, logIn), isFalse);
  });

  testWidgets('logged in shows the expiry and closes', (tester) async {
    await show(
      tester,
      login(LoginFlowState.loggedIn, [
        'agent logged in',
        'expires in 365 days · 2027-11-09',
      ]),
    );
    expect(find.text('agent logged in'), findsOneWidget);
    expect(find.text('expires in 365 days · 2027-11-09'), findsOneWidget);
    expect(code, findsNothing);
    expect(find.text('Log in'), findsOneWidget, reason: 'renew early');
    await tester.tap(find.text('Close'));
    expect(sent, [const GuiIntentAct(action: ActionIntentDismissAgentLogin())]);
  });

  for (final reason in [
    'login failed: the account has no subscription · start again',
    'the server no longer knows this login · start again',
    'another login is under way on the server · start again',
    'offline · nothing was asked · start again once connected',
  ]) {
    testWidgets('an ended login says why and starts again: $reason', (
      tester,
    ) async {
      await show(tester, login(LoginFlowState.failed, [reason]));
      expect(find.text(reason), findsOneWidget);
      expect(find.text('Retry'), findsNothing);
      expect(code, findsNothing);
      expect(find.text('Start again'), findsOneWidget);
      await tester.tap(logIn);
      expect(sent, [const GuiIntentAct(action: ActionIntentStartAgentLogin())]);
    });
  }

  testWidgets('a code sent offline is retried', (tester) async {
    await show(
      tester,
      login(LoginFlowState.noAnswer, [
        'offline · the code was not sent · retry once connected',
      ], url: url),
    );
    expect(
      find.text('offline · the code was not sent · retry once connected'),
      findsOneWidget,
    );
    expect(code, findsNothing);
    await tester.tap(find.text('Retry'));
    expect(sent, [const GuiIntentAct(action: ActionIntentRetryAgentLogin())]);
  });

  testWidgets('the paste field keeps nothing a keyboard or restoration '
      'could store', (tester) async {
    tester.view
      ..physicalSize = const Size(800, 1400)
      ..devicePixelRatio = 1;
    addTearDown(tester.view.reset);
    final open = login(LoginFlowState.open, [], url: url);
    await tester.pumpWidget(settings(open, sent, restorationScopeId: 'app'));
    final field = tester.widget<TextField>(code);
    expect(field.restorationId, isNull);
    expect(field.autocorrect, isFalse);
    expect(field.enableSuggestions, isFalse);
    expect(field.enableIMEPersonalizedLearning, isFalse);
    expect(field.obscureText, isFalse);
    expect(field.keyboardType, TextInputType.visiblePassword);

    await tester.enterText(code, 'secret-code');
    expect(find.text('secret-code'), findsOneWidget);
    await tester.restartAndRestore();
    expect(find.text('secret-code'), findsNothing);
    expect(code, findsOneWidget, reason: 'the field is back, empty');
    expect(sent, isEmpty);
  });

  testWidgets('leaving Settings drops the code; staying keeps it', (
    tester,
  ) async {
    tester.view
      ..physicalSize = const Size(800, 1400)
      ..devicePixelRatio = 1;
    addTearDown(tester.view.reset);
    final open = login(LoginFlowState.open, [], url: url);
    await tester.pumpWidget(settings(open, sent));
    await tester.enterText(code, 'secret-code');

    // A new screen update for Settings rebuilds it: the code stays.
    await tester.pumpWidget(
      settings(login(LoginFlowState.open, ['steps'], url: url), sent),
    );
    expect(find.text('secret-code'), findsOneWidget);

    await tester.pumpWidget(const MaterialApp(home: SizedBox()));
    await tester.pumpWidget(settings(open, sent));
    expect(code, findsOneWidget);
    expect(find.text('secret-code'), findsNothing);
    expect(sent, isEmpty, reason: 'leaving does not dismiss');
  });
}
