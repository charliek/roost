import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:roost_mobile/core/models.dart';
import 'package:roost_mobile/input/key_mapping.dart';

KeyDownEvent _down(LogicalKeyboardKey key, {String? character}) => KeyDownEvent(
  physicalKey: PhysicalKeyboardKey.keyA,
  logicalKey: key,
  character: character,
  timeStamp: Duration.zero,
);

KeyInput? _map(
  KeyEvent e, {
  bool ctrl = false,
  bool alt = false,
  bool shift = false,
  bool meta = false,
}) => keyInputFor(e, ctrl: ctrl, alt: alt, shift: shift, meta: meta);

void main() {
  group('keyInputFor', () {
    test('named keys use W3C names', () {
      expect(
        _map(_down(LogicalKeyboardKey.enter, character: '\r')),
        const KeyInput('Enter'),
      );
      expect(
        _map(_down(LogicalKeyboardKey.arrowLeft)),
        const KeyInput('ArrowLeft'),
      );
      expect(
        _map(_down(LogicalKeyboardKey.pageDown)),
        const KeyInput('PageDown'),
      );
      expect(_map(_down(LogicalKeyboardKey.f5)), const KeyInput('F5'));
      expect(
        _map(_down(LogicalKeyboardKey.escape, character: '\x1b')),
        const KeyInput('Escape'),
      );
    });

    test('a letter is its lower-case key plus the text it types', () {
      expect(
        _map(_down(LogicalKeyboardKey.keyA, character: 'a')),
        const KeyInput('a', text: 'a'),
      );
      expect(
        _map(_down(LogicalKeyboardKey.keyA, character: 'A'), shift: true),
        const KeyInput('a', text: 'A', shift: true),
      );
    });

    test('ctrl+letter carries no control-character text', () {
      expect(
        _map(_down(LogicalKeyboardKey.keyC, character: '\x03'), ctrl: true),
        const KeyInput('c', ctrl: true),
      );
    });

    test('modifiers ride along on named keys', () {
      expect(
        _map(_down(LogicalKeyboardKey.arrowRight), alt: true, shift: true),
        const KeyInput('ArrowRight', alt: true, shift: true),
      );
    });

    test('a repeat is marked', () {
      const e = KeyRepeatEvent(
        physicalKey: PhysicalKeyboardKey.backspace,
        logicalKey: LogicalKeyboardKey.backspace,
        timeStamp: Duration.zero,
      );
      expect(_map(e), const KeyInput('Backspace', repeat: true));
    });

    test('releases and pure modifiers send nothing', () {
      expect(
        _map(
          const KeyUpEvent(
            physicalKey: PhysicalKeyboardKey.keyA,
            logicalKey: LogicalKeyboardKey.keyA,
            timeStamp: Duration.zero,
          ),
        ),
        isNull,
      );
      expect(_map(_down(LogicalKeyboardKey.shiftLeft)), isNull);
      expect(_map(_down(LogicalKeyboardKey.controlRight)), isNull);
      expect(_map(_down(LogicalKeyboardKey.metaLeft)), isNull);
    });
  });

  group('looksLikeHardwareKeyboard', () {
    test(
      'printable characters and modifiers come from a physical keyboard',
      () {
        expect(
          looksLikeHardwareKeyboard(
            _down(LogicalKeyboardKey.keyQ, character: 'q'),
          ),
          isTrue,
        );
        expect(
          looksLikeHardwareKeyboard(_down(LogicalKeyboardKey.controlLeft)),
          isTrue,
        );
      },
    );

    test('the keys soft keyboards send as events do not count', () {
      expect(
        looksLikeHardwareKeyboard(_down(LogicalKeyboardKey.backspace)),
        isFalse,
      );
      expect(
        looksLikeHardwareKeyboard(
          _down(LogicalKeyboardKey.enter, character: '\r'),
        ),
        isFalse,
      );
      expect(
        looksLikeHardwareKeyboard(
          _down(LogicalKeyboardKey.space, character: ' '),
        ),
        isFalse,
      );
    });
  });
}
