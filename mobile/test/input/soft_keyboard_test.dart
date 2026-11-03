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

TextEditingDeltaInsertion _insert(String old, String text, {int? at}) {
  final offset = at ?? old.length;
  return TextEditingDeltaInsertion(
    oldText: old,
    textInserted: text,
    insertionOffset: offset,
    selection: TextSelection.collapsed(offset: offset + text.length),
    composing: TextRange.empty,
  );
}

TextEditingDeltaDeletion _delete(String old, {int count = 1}) =>
    TextEditingDeltaDeletion(
      oldText: old,
      deletedRange: TextRange(start: old.length - count, end: old.length),
      selection: TextSelection.collapsed(offset: old.length - count),
      composing: TextRange.empty,
    );

void main() {
  group('diffText', () {
    test('added characters are text, removed ones Backspaces', () {
      expect(diffText('  ', '  a'), [const SoftText('a')]);
      expect(diffText('  ', ' '), [_backspace]);
      expect(diffText('  ', ''), [_backspace, _backspace]);
      expect(diffText('  ', '   '), [const SoftText(' ')]);
      expect(diffText('  ', 'x  '), [const SoftText('x')]);
      expect(diffText('  ', ' é'), [_backspace, const SoftText('é')]);
    });

    test('a newline is an Enter key between the text it splits', () {
      expect(diffText('  ', '  \n'), [_enter]);
      expect(diffText('  ', '  ls\npwd'), [
        const SoftText('ls'),
        _enter,
        const SoftText('pwd'),
      ]);
      expect(diffText('  ', '  a\r\n'), [const SoftText('a'), _enter]);
    });

    test('one deleted emoji is one Backspace', () {
      expect(diffText('  👍🏽', '  '), [_backspace]);
    });
  });

  group('SoftKeyboardDecoder, full values', () {
    test('each value is diffed against the one before, not the sentinel', () {
      // Android can send "  a" then "  ab" before a reset lands.
      final d = SoftKeyboardDecoder();
      expect(d.value(_value('  a')).inputs, [const SoftText('a')]);
      expect(d.value(_value('  ab')).inputs, [const SoftText('b')]);
    });

    test('two deletions are two Backspaces', () {
      final d = SoftKeyboardDecoder();
      expect(d.value(_value(' ')).inputs, [_backspace]);
      expect(d.value(_value('')).inputs, [_backspace]);
    });

    test('the field is reset only when it must be', () {
      final d = SoftKeyboardDecoder();
      expect(d.value(_value('  a')).reset, isFalse, reason: 'room to delete');
      expect(d.value(_value('  ')).reset, isFalse);
      expect(d.value(_value(' ')).reset, isTrue, reason: 'one char left');
      d.didReset();
      expect(d.value(_value('  \n')).reset, isTrue, reason: 'a newline');
      d.didReset();
      expect(d.value(_value('  ', cursor: 0)).reset, isTrue, reason: 'cursor');
      d.didReset();
      expect(d.value(_value('  ${'x' * 70}')).reset, isTrue, reason: 'long');
    });

    test('a composition is held, then sent once as committed', () {
      final d = SoftKeyboardDecoder();
      final ni = d.value(
        _value('  ni', composing: const TextRange(start: 2, end: 4)),
      );
      expect(ni.inputs, isEmpty);
      expect(ni.reset, isFalse, reason: 'never touch a composing field');
      expect(d.composing, isTrue);
      d.value(_value('  nihao', composing: const TextRange(start: 2, end: 7)));
      expect(d.value(_value('  你好')).inputs, [const SoftText('你好')]);
      expect(d.composing, isFalse);
    });

    test('an unchanged value sends nothing', () {
      final d = SoftKeyboardDecoder();
      final edit = d.value(softKeyboardSentinel);
      expect(edit.inputs, isEmpty);
      expect(edit.reset, isFalse);
    });
  });

  group('SoftKeyboardDecoder, deltas', () {
    test('insertions are sent as made, whatever the field held', () {
      final d = SoftKeyboardDecoder();
      final a = d.deltas([_insert('  ', 'a')]);
      expect(a.inputs, [const SoftText('a')]);
      expect(a.reset, isTrue);
      d.didReset();
      // The IME made this edit before the reset reached it.
      expect(d.deltas([_insert('  a', 'b')]).inputs, [const SoftText('b')]);
    });

    test('each deletion is its Backspaces', () {
      final d = SoftKeyboardDecoder();
      expect(d.deltas([_delete('  ')]).inputs, [_backspace]);
      expect(d.deltas([_delete(' ')]).inputs, [_backspace]);
      expect(d.deltas([_delete('  ab', count: 2)]).inputs, [
        _backspace,
        _backspace,
      ]);
    });

    test('a replacement deletes, then types', () {
      final d = SoftKeyboardDecoder();
      final edit = d.deltas([
        const TextEditingDeltaReplacement(
          oldText: '  teh',
          replacementText: 'the',
          replacedRange: TextRange(start: 2, end: 5),
          selection: TextSelection.collapsed(offset: 5),
          composing: TextRange.empty,
        ),
      ]);
      expect(edit.inputs, [
        _backspace,
        _backspace,
        _backspace,
        const SoftText('the'),
      ]);
    });

    test('a newline is Enter, a selection change nothing', () {
      final d = SoftKeyboardDecoder();
      expect(d.deltas([_insert('  ', '\n')]).inputs, [_enter]);
      d.didReset();
      final moved = d.deltas([
        const TextEditingDeltaNonTextUpdate(
          oldText: '  ',
          selection: TextSelection.collapsed(offset: 0),
          composing: TextRange.empty,
        ),
      ]);
      expect(moved.inputs, isEmpty);
    });

    test('a composition is held until it is committed', () {
      final d = SoftKeyboardDecoder();
      final held = d.deltas([
        const TextEditingDeltaInsertion(
          oldText: '  ',
          textInserted: 'n',
          insertionOffset: 2,
          selection: TextSelection.collapsed(offset: 3),
          composing: TextRange(start: 2, end: 3),
        ),
        const TextEditingDeltaInsertion(
          oldText: '  n',
          textInserted: 'i',
          insertionOffset: 3,
          selection: TextSelection.collapsed(offset: 4),
          composing: TextRange(start: 2, end: 4),
        ),
      ]);
      expect(held.inputs, isEmpty);
      expect(held.reset, isFalse);
      final done = d.deltas([
        const TextEditingDeltaReplacement(
          oldText: '  ni',
          replacementText: '你',
          replacedRange: TextRange(start: 2, end: 4),
          selection: TextSelection.collapsed(offset: 3),
          composing: TextRange.empty,
        ),
      ]);
      expect(done.inputs, [const SoftText('你')]);
      expect(done.reset, isTrue);
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

    testWidgets('asks for the delta model', (tester) async {
      await pump(tester);
      expect(tester.testTextInput.setClientArgs!['enableDeltaModel'], isTrue);
    });

    testWidgets(
      'soft keyboard text and Backspace reach the terminal; a newline resets',
      (tester) async {
        await pump(tester);
        int resets() => tester.testTextInput.log
            .where((call) => call.method == 'TextInput.setEditingState')
            .length;
        final opened = resets();
        tester.testTextInput.updateEditingValue(_value('  l'));
        tester.testTextInput.updateEditingValue(_value('  ls'));
        tester.testTextInput.updateEditingValue(_value('  l'));
        await tester.pump();
        expect(resets(), opened, reason: 'no reset while there is room');
        tester.testTextInput.updateEditingValue(_value('  l\n'));
        await tester.pump();
        expect(resets(), opened + 1, reason: 'a newline resets the field');
        expect(tester.testTextInput.editingState!['text'], '  ');
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
