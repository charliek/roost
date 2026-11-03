import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:roost_mobile/core/models.dart';
import 'package:roost_mobile/input/soft_keyboard.dart';

const _backspace = SoftKey(KeyInput('Backspace'));
const _enter = SoftKey(KeyInput('Enter'));

TextEditingValue _value(
  String text, {
  int? cursor,
  TextRange composing = TextRange.empty,
}) => TextEditingValue(
  text: text,
  selection: TextSelection.collapsed(offset: cursor ?? text.length),
  composing: composing,
);

void main() {
  group('diffSoftKeyboard', () {
    test('a typed character is text', () {
      final edit = diffSoftKeyboard(_value('  a'));
      expect(edit.inputs, [const SoftText('a')]);
      expect(edit.reset, isTrue);
    });

    test('a committed word is one text', () {
      expect(diffSoftKeyboard(_value('  hello ')).inputs, [
        const SoftText('hello '),
      ]);
    });

    test('Backspace on an "empty" field still reaches the terminal', () {
      final edit = diffSoftKeyboard(_value(' '));
      expect(edit.inputs, [_backspace]);
      expect(edit.reset, isTrue);
    });

    test('a delete that clears the whole sentinel is two Backspaces', () {
      expect(diffSoftKeyboard(_value('')).inputs, [_backspace, _backspace]);
    });

    test('a newline is an Enter key, around the text it splits', () {
      expect(diffSoftKeyboard(_value('  \n')).inputs, [_enter]);
      expect(diffSoftKeyboard(_value('  ls\npwd')).inputs, [
        const SoftText('ls'),
        _enter,
        const SoftText('pwd'),
      ]);
      expect(diffSoftKeyboard(_value('  a\r\n')).inputs, [
        const SoftText('a'),
        _enter,
      ]);
    });

    test('a repeated space is text, not lost in the sentinel', () {
      expect(diffSoftKeyboard(_value('   ')).inputs, [const SoftText(' ')]);
    });

    test(
      'text typed before the sentinel (cursor moved home) is still text',
      () {
        expect(diffSoftKeyboard(_value('x  ', cursor: 1)).inputs, [
          const SoftText('x'),
        ]);
      },
    );

    test('a replacement deletes then types', () {
      // An IME that rewrites the last sentinel char (rare with suggestions
      // off) is a Backspace followed by the new text.
      expect(diffSoftKeyboard(_value(' é')).inputs, [
        _backspace,
        const SoftText('é'),
      ]);
    });

    test('nothing is sent mid-composition, and the field is left alone', () {
      final edit = diffSoftKeyboard(
        _value('  ni', composing: const TextRange(start: 2, end: 4)),
      );
      expect(edit.inputs, isEmpty);
      expect(edit.reset, isFalse);
    });

    test('the committed composition is sent once', () {
      expect(diffSoftKeyboard(_value('  nihao')).inputs, [
        const SoftText('nihao'),
      ]);
    });

    test('an unchanged value sends nothing and needs no reset', () {
      final edit = diffSoftKeyboard(softKeyboardSentinel);
      expect(edit.inputs, isEmpty);
      expect(edit.reset, isFalse);
    });

    test('a cursor moved inside the sentinel is put back', () {
      final edit = diffSoftKeyboard(_value('  ', cursor: 0));
      expect(edit.inputs, isEmpty);
      expect(edit.reset, isTrue);
    });
  });

  group('TerminalInput', () {
    late List<KeyInput> keys;
    late List<String> texts;
    late FocusNode focus;
    late GlobalKey<TerminalInputState> input;
    var hardware = 0;
    var soft = 0;

    Future<void> pump(WidgetTester tester) async {
      keys = [];
      texts = [];
      hardware = 0;
      soft = 0;
      focus = FocusNode();
      addTearDown(focus.dispose);
      input = GlobalKey<TerminalInputState>();
      await tester.pumpWidget(
        MaterialApp(
          home: TerminalInput(
            key: input,
            focusNode: focus,
            onKey: keys.add,
            onText: texts.add,
            onHardwareKeyboard: () => hardware++,
            onSoftKeyboard: () => soft++,
            child: const SizedBox.expand(),
          ),
        ),
      );
      input.currentState!.showKeyboard();
      await tester.pump();
    }

    testWidgets('opens a connection with suggestions and autocorrect off', (
      tester,
    ) async {
      await pump(tester);
      expect(tester.testTextInput.isVisible, isTrue);
      final config = tester.testTextInput.setClientArgs!;
      expect(config['autocorrect'], isFalse);
      expect(config['enableSuggestions'], isFalse);
      expect(config['enableIMEPersonalizedLearning'], isFalse);
      expect((config['inputType'] as Map)['name'], 'TextInputType.multiline');
      expect(tester.testTextInput.editingState!['text'], '  ');
    });

    testWidgets(
      'soft keyboard text and Backspace reach the terminal, then the field resets',
      (tester) async {
        await pump(tester);
        tester.testTextInput.updateEditingValue(_value('  l'));
        await tester.pump();
        expect(
          tester.testTextInput.editingState!['text'],
          '  ',
          reason: 'reset to the sentinel',
        );
        tester.testTextInput.updateEditingValue(_value('  s'));
        tester.testTextInput.updateEditingValue(_value(' '));
        tester.testTextInput.updateEditingValue(_value('  \n'));
        await tester.pump();
        expect(texts, ['l', 's']);
        expect(keys, [const KeyInput('Backspace'), const KeyInput('Enter')]);
        expect(soft, 4);
      },
    );

    testWidgets('the IME action key is Enter', (tester) async {
      await pump(tester);
      await tester.testTextInput.receiveAction(TextInputAction.newline);
      expect(keys, [const KeyInput('Enter')]);
    });

    testWidgets('hardware keys are handled as keys, not text', (tester) async {
      await pump(tester);
      await tester.sendKeyEvent(LogicalKeyboardKey.keyA);
      await tester.sendKeyEvent(LogicalKeyboardKey.arrowUp);
      await tester.sendKeyDownEvent(LogicalKeyboardKey.controlLeft);
      await tester.sendKeyEvent(LogicalKeyboardKey.keyC);
      await tester.sendKeyUpEvent(LogicalKeyboardKey.controlLeft);
      expect(keys, [
        const KeyInput('a', text: 'a'),
        const KeyInput('ArrowUp'),
        isA<KeyInput>().having((k) => (k.key, k.ctrl), 'ctrl+c', ('c', true)),
      ]);
      expect(texts, isEmpty);
      expect(hardware, greaterThan(0));
    });

    testWidgets('losing focus closes the connection', (tester) async {
      await pump(tester);
      expect(tester.testTextInput.hasAnyClients, isTrue);
      focus.unfocus();
      await tester.pump();
      expect(tester.testTextInput.hasAnyClients, isFalse);
    });
  });
}
