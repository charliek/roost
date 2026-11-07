import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:roost_mobile/core/models.dart';
import 'package:roost_mobile/input/extra_keys.dart';
import 'package:roost_mobile/input/quick_replies.dart';

void main() {
  group('stickyKeyFor', () {
    test('nothing armed: text passes through', () {
      expect(stickyKeyFor('c', ctrl: false, alt: false), isNull);
    });

    test('armed Ctrl turns one character into a ctrl key', () {
      expect(
        stickyKeyFor('c', ctrl: true, alt: false),
        const KeyInput('c', text: 'c', ctrl: true),
      );
      expect(
        stickyKeyFor('D', ctrl: true, alt: false),
        const KeyInput('d', text: 'D', ctrl: true, shift: true),
      );
    });

    test('armed Alt applies too', () {
      expect(
        stickyKeyFor('b', ctrl: false, alt: true),
        const KeyInput('b', text: 'b', alt: true),
      );
    });

    test('longer text (an IME commit, a paste) is never mangled', () {
      expect(stickyKeyFor('hello', ctrl: true, alt: false), isNull);
    });
  });

  test('the bar has every key the spec lists, in order', () {
    expect(
      [escKey.label, tabKey.label, for (final k in extraKeys) k.label],
      ['Esc', 'Tab', '←', '↑', '↓', '→', '|', '~', '/', '-', 'PgUp', 'PgDn'],
    );
    expect(escKey.input, const KeyInput('Escape'));
    expect(
      extraKeys.firstWhere((k) => k.id == 'pgup').input,
      const KeyInput('PageUp'),
    );
    expect(
      extraKeys.firstWhere((k) => k.id == 'pipe').input,
      const KeyInput('|', text: '|'),
    );
  });

  test('quick replies are the raw bytes of keys', () {
    expect(
      {for (final r in quickReplies) r.label: r.bytes},
      {
        '1': '1',
        '2': '2',
        '3': '3',
        'Enter': '\r',
        'Esc': '\x1b',
        '↑': '\x1b[A',
        '↓': '\x1b[B',
      },
    );
  });

  testWidgets('taps on the bar never take focus from the terminal', (
    tester,
  ) async {
    final focus = FocusNode();
    addTearDown(focus.dispose);
    final sent = <KeyInput>[];
    var ctrl = 0;
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: Column(
            children: [
              Expanded(
                child: Focus(
                  focusNode: focus,
                  autofocus: true,
                  child: const SizedBox.expand(),
                ),
              ),
              ExtraKeysBar(
                ctrlArmed: false,
                altArmed: false,
                onToggleCtrl: () => ctrl++,
                onToggleAlt: () {},
                onKey: sent.add,
              ),
            ],
          ),
        ),
      ),
    );
    await tester.pump();
    expect(focus.hasFocus, isTrue);
    await tester.tap(find.byKey(const ValueKey('extra-key-esc')));
    await tester.tap(find.byKey(const ValueKey('extra-key-ctrl')));
    await tester.tap(find.byKey(const ValueKey('extra-key-up')));
    expect(sent, [const KeyInput('Escape'), const KeyInput('ArrowUp')]);
    expect(ctrl, 1);
    expect(focus.hasFocus, isTrue);
  });
}
