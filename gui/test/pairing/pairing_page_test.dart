import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:skywave_gui/pairing/pairing_page.dart';
import 'package:skywave_gui/src/bindings/bindings.dart';

ScreenViewPairing pairing({
  PairForm form = const PairForm(
    address: '',
    code: '',
    fingerprint: '',
    rev: 0,
  ),
  PairingState state = PairingState.idle,
  String? status,
  String? device,
}) => ScreenViewPairing(
  state: state,
  form: form,
  statusLabel: status,
  pairedDevice: device,
  pairedServer: device == null ? null : 'srv:7443',
);

const _address = Key('pair-address');
const _code = Key('pair-code');
const _fingerprint = Key('pair-fingerprint');

String field(WidgetTester tester, Key key) =>
    tester.widget<TextField>(find.byKey(key)).controller!.text;

void main() {
  late List<GuiIntent> sent;

  Future<void> pump(WidgetTester tester, ScreenViewPairing screen) async {
    await tester.pumpWidget(
      MaterialApp(
        home: PairingPage(screen: screen, send: sent.add),
      ),
    );
  }

  setUp(() => sent = []);

  testWidgets('Pair sends the three fields', (tester) async {
    await pump(tester, pairing());
    await tester.enterText(find.byKey(_address), 'h:7443');
    await tester.enterText(find.byKey(_code), 'ABCDE12345');
    await tester.enterText(find.byKey(_fingerprint), 'ab:cd');
    sent.clear();
    await tester.tap(find.text('Pair'));
    expect(sent, [
      const GuiIntentPairSubmit(
        address: 'h:7443',
        code: 'ABCDE12345',
        fingerprint: 'ab:cd',
      ),
    ]);
  });

  testWidgets('a pasted URI goes to Rust; typing does not', (tester) async {
    await pump(tester, pairing());
    const uri = 'skywave://pair?addr=h%3A7443&code=ABCDE12345&fp=ab';
    await tester.enterText(find.byKey(_code), uri);
    expect(sent, [const GuiIntentPairPasted(text: uri)]);

    sent.clear();
    await tester.enterText(find.byKey(_address), 'h');
    await tester.enterText(find.byKey(_address), 'ho');
    expect(sent, isEmpty);
  });

  testWidgets('a new form rev overwrites the fields, the same rev does not', (
    tester,
  ) async {
    await pump(tester, pairing());
    await tester.enterText(find.byKey(_address), 'mine');
    await pump(
      tester,
      pairing(status: 'pairing failed: bad code', state: PairingState.failed),
    );
    expect(field(tester, _address), 'mine', reason: 'same rev keeps typing');
    expect(find.text('pairing failed: bad code'), findsOneWidget);

    await pump(
      tester,
      pairing(
        form: const PairForm(
          address: 'h:7443',
          code: 'ABCDE12345',
          fingerprint: 'ab:cd',
          rev: 1,
        ),
      ),
    );
    expect(field(tester, _address), 'h:7443');
    expect(field(tester, _code), 'ABCDE12345');
    expect(field(tester, _fingerprint), 'ab:cd');

    await tester.enterText(find.byKey(_code), 'X');
    await pump(
      tester,
      pairing(
        form: const PairForm(
          address: 'h:7443',
          code: 'ABCDE12345',
          fingerprint: 'ab:cd',
          rev: 1,
        ),
        state: PairingState.inProgress,
        status: 'pairing with h:7443…',
      ),
    );
    expect(field(tester, _code), 'X', reason: 'an old rev is not reapplied');
    expect(find.byType(LinearProgressIndicator), findsOneWidget);
  });

  testWidgets('re-pairing says who this device is, and Back goes back', (
    tester,
  ) async {
    await tester.pumpWidget(
      MaterialApp(
        home: PairingPage(
          screen: pairing(device: 'dev-1'),
          canGoBack: true,
          send: sent.add,
        ),
      ),
    );
    expect(find.text('Currently paired as dev-1'), findsOneWidget);
    await tester.tap(find.byType(BackButton));
    expect(sent, [const GuiIntentAct(action: ActionIntentBack())]);
  });

  testWidgets('a scan goes to Rust; cancelling sends nothing', (tester) async {
    String? scanned;
    Future<void> pumpScanning() => tester.pumpWidget(
      MaterialApp(
        home: PairingPage(
          screen: pairing(),
          send: sent.add,
          scan: () async => scanned,
        ),
      ),
    );
    await pumpScanning();
    sent.clear();
    await tester.tap(find.text('Scan QR code'));
    await tester.pump();
    expect(sent, isEmpty, reason: 'cancelled');

    scanned = 'skywave://pair?addr=h%3A7443&code=ABCDE12345&fp=ab';
    await tester.tap(find.text('Scan QR code'));
    await tester.pump();
    expect(sent, [GuiIntentPairScanned(text: scanned)]);
  });

  testWidgets('without a scanner there is no scan button', (tester) async {
    await pump(tester, pairing());
    expect(find.text('Scan QR code'), findsNothing);
  });
}
