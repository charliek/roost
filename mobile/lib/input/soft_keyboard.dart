import 'package:flutter/services.dart';
import 'package:flutter/widgets.dart';

import '../core/models.dart';
import 'key_mapping.dart';

/// What the input connection is reset to. An IME only reports a Backspace
/// when there is something before the cursor to delete, so the field is
/// never left empty.
const softKeyboardSentinel = TextEditingValue(
  text: '  ',
  selection: TextSelection.collapsed(offset: 2),
);

/// The Samsung Keyboard / Gboard friendly configuration. `enableSuggestions:
/// false` makes Flutter's Android plugin add `TYPE_TEXT_VARIATION_VISIBLE_
/// PASSWORD` + `NO_SUGGESTIONS`, which is what turns off composition,
/// prediction and autocorrect on both keyboards (Termux's char-based mode);
/// `multiline` keeps an Enter key rather than Done. The delta model reports
/// each edit the IME made, which stays right while a reset is still on its
/// way to the platform.
const terminalTextInputConfiguration = TextInputConfiguration(
  inputType: TextInputType.multiline,
  inputAction: TextInputAction.newline,
  autocorrect: false,
  enableSuggestions: false,
  enableIMEPersonalizedLearning: false,
  enableInteractiveSelection: false,
  enableDeltaModel: true,
  smartDashesType: SmartDashesType.disabled,
  smartQuotesType: SmartQuotesType.disabled,
  keyboardAppearance: Brightness.dark,
);

/// One thing the soft keyboard typed.
sealed class SoftInput {
  const SoftInput();
}

final class SoftKey extends SoftInput {
  const SoftKey(this.key);

  final KeyInput key;

  @override
  bool operator ==(Object other) => other is SoftKey && other.key == key;
  @override
  int get hashCode => key.hashCode;
  @override
  String toString() => 'SoftKey($key)';
}

final class SoftText extends SoftInput {
  const SoftText(this.text);

  final String text;

  @override
  bool operator ==(Object other) => other is SoftText && other.text == text;
  @override
  int get hashCode => text.hashCode;
  @override
  String toString() => 'SoftText("$text")';
}

@immutable
class SoftKeyboardEdit {
  const SoftKeyboardEdit(this.inputs, {required this.reset});

  /// Mid-composition: nothing to send yet, and the field must not be touched.
  static const composing = SoftKeyboardEdit([], reset: false);

  final List<SoftInput> inputs;

  /// Put the field back to [softKeyboardSentinel] (and call
  /// [SoftKeyboardDecoder.didReset]).
  final bool reset;

  @override
  String toString() => 'SoftKeyboardEdit($inputs, reset: $reset)';
}

/// Turns what the IME reports into keys and text for the terminal.
///
/// With the delta model ([deltas]) each edit is forwarded as the IME made
/// it (an insertion is text, a deletion is Backspaces), so a reset that has
/// not reached the platform yet changes nothing. Without it ([value]), each
/// full value is diffed against the last one the IME reported, and the field
/// is reset only when it must be (too little left to delete, a newline, too
/// long), so a reset racing the IME's next edit stays rare.
class SoftKeyboardDecoder {
  TextEditingValue _last = softKeyboardSentinel;
  String? _beforeComposition;

  /// The field as the IME last reported it (or as reset).
  TextEditingValue get current => _last;

  bool get composing => _beforeComposition != null;

  void didReset() {
    _last = softKeyboardSentinel;
    _beforeComposition = null;
  }

  SoftKeyboardEdit deltas(List<TextEditingDelta> deltas) {
    final inputs = <SoftInput>[];
    for (final delta in deltas) {
      final after = delta.apply(
        TextEditingValue(
          text: delta.oldText,
          selection: TextSelection.collapsed(offset: delta.oldText.length),
        ),
      );
      if (_isComposing(after)) {
        _beforeComposition ??= delta.oldText;
      } else if (_beforeComposition != null) {
        inputs.addAll(diffText(_beforeComposition!, after.text));
        _beforeComposition = null;
      } else {
        inputs.addAll(_edit(delta));
      }
      _last = after;
    }
    return SoftKeyboardEdit(
      inputs,
      reset: !composing && _last != softKeyboardSentinel,
    );
  }

  SoftKeyboardEdit value(TextEditingValue next) {
    if (_isComposing(next)) {
      _beforeComposition ??= _last.text;
      _last = next;
      return SoftKeyboardEdit.composing;
    }
    final before = _beforeComposition ?? _last.text;
    _beforeComposition = null;
    _last = next;
    return SoftKeyboardEdit(
      diffText(before, next.text),
      reset: _mustReset(next),
    );
  }

  static bool _isComposing(TextEditingValue v) =>
      v.composing.isValid && !v.composing.isCollapsed;

  /// Reset only when the field cannot take the next edit as it is.
  static bool _mustReset(TextEditingValue v) {
    final selection = v.selection;
    if (!selection.isValid || !selection.isCollapsed) return true;
    return selection.baseOffset < softKeyboardSentinel.text.length ||
        v.text.contains('\n') ||
        v.text.length > 64;
  }

  static List<SoftInput> _edit(TextEditingDelta delta) => switch (delta) {
    TextEditingDeltaInsertion(:final textInserted) => _typed(textInserted),
    TextEditingDeltaDeletion(:final textDeleted) => _backspaces(textDeleted),
    TextEditingDeltaReplacement(:final replacedRange, :final replacementText) =>
      [
        ..._backspaces(
          delta.oldText.substring(replacedRange.start, replacedRange.end),
        ),
        ..._typed(replacementText),
      ],
    _ => const [],
  };
}

/// What turned [before] into [after]: the characters gone from it are
/// Backspaces, the ones added are text, each newline an Enter key.
List<SoftInput> diffText(String before, String after) {
  var prefix = 0;
  while (prefix < before.length &&
      prefix < after.length &&
      before.codeUnitAt(prefix) == after.codeUnitAt(prefix)) {
    prefix++;
  }
  var suffix = 0;
  while (suffix < before.length - prefix &&
      suffix < after.length - prefix &&
      before.codeUnitAt(before.length - 1 - suffix) ==
          after.codeUnitAt(after.length - 1 - suffix)) {
    suffix++;
  }
  return [
    ..._backspaces(before.substring(prefix, before.length - suffix)),
    ..._typed(after.substring(prefix, after.length - suffix)),
  ];
}

List<SoftInput> _backspaces(String deleted) => [
  for (var i = 0; i < deleted.characters.length; i++)
    const SoftKey(KeyInput('Backspace')),
];

List<SoftInput> _typed(String text) {
  final inputs = <SoftInput>[];
  final lines = text.replaceAll('\r\n', '\n').split('\n');
  for (var i = 0; i < lines.length; i++) {
    if (i > 0) inputs.add(const SoftKey(KeyInput('Enter')));
    if (lines[i].isNotEmpty) inputs.add(SoftText(lines[i]));
  }
  return inputs;
}

/// Keyboard input for a terminal: hardware keys through [Focus.onKeyEvent]
/// (handled there, so the IME never sees them twice), the soft keyboard
/// through a [DeltaTextInputClient] decoded by [SoftKeyboardDecoder].
class TerminalInput extends StatefulWidget {
  const TerminalInput({
    super.key,
    required this.focusNode,
    required this.onKey,
    required this.onText,
    required this.child,
    this.onHardwareKeyboard,
    this.onSoftKeyboard,
    this.onPasteShortcut,
  });

  final FocusNode focusNode;
  final ValueChanged<KeyInput> onKey;
  final ValueChanged<String> onText;
  final Widget child;

  /// A key event only a physical keyboard sends arrived.
  final VoidCallback? onHardwareKeyboard;

  /// The soft keyboard typed something.
  final VoidCallback? onSoftKeyboard;

  /// Ctrl+Shift+V / Cmd+V on a hardware keyboard.
  final VoidCallback? onPasteShortcut;

  @override
  State<TerminalInput> createState() => TerminalInputState();
}

class TerminalInputState extends State<TerminalInput>
    with TextInputClient, DeltaTextInputClient {
  TextInputConnection? _connection;
  final _decoder = SoftKeyboardDecoder();

  bool get hasConnection => _connection?.attached ?? false;

  @override
  void initState() {
    super.initState();
    widget.focusNode.addListener(_onFocusChange);
  }

  @override
  void didUpdateWidget(TerminalInput oldWidget) {
    super.didUpdateWidget(oldWidget);
    if (oldWidget.focusNode != widget.focusNode) {
      oldWidget.focusNode.removeListener(_onFocusChange);
      widget.focusNode.addListener(_onFocusChange);
    }
  }

  @override
  void dispose() {
    widget.focusNode.removeListener(_onFocusChange);
    _closeConnection();
    super.dispose();
  }

  /// Focuses the terminal and raises the soft keyboard.
  void showKeyboard() {
    if (!widget.focusNode.hasFocus) widget.focusNode.requestFocus();
    _openConnection();
    _connection?.show();
  }

  void _onFocusChange() {
    if (widget.focusNode.hasFocus) {
      _openConnection();
    } else {
      _closeConnection();
    }
  }

  void _openConnection() {
    if (hasConnection) return;
    _decoder.didReset();
    _connection = TextInput.attach(this, terminalTextInputConfiguration)
      ..setEditingState(softKeyboardSentinel);
  }

  void _closeConnection() {
    if (hasConnection) _connection!.close();
    _connection = null;
  }

  KeyEventResult _onKeyEvent(FocusNode node, KeyEvent event) {
    // While an IME composes, its own key events must reach it.
    if (_decoder.composing) return KeyEventResult.skipRemainingHandlers;
    if (looksLikeHardwareKeyboard(event)) widget.onHardwareKeyboard?.call();
    final kb = HardwareKeyboard.instance;
    if (event is KeyDownEvent &&
        event.logicalKey == LogicalKeyboardKey.keyV &&
        ((kb.isControlPressed && kb.isShiftPressed) || kb.isMetaPressed)) {
      widget.onPasteShortcut?.call();
      return KeyEventResult.handled;
    }
    final input = keyInputFor(
      event,
      ctrl: kb.isControlPressed,
      alt: kb.isAltPressed,
      shift: kb.isShiftPressed,
      meta: kb.isMetaPressed,
    );
    if (input != null) {
      widget.onKey(input);
      return KeyEventResult.handled;
    }
    // Swallow the release of a key whose press was handled, so the platform
    // never sees an unpaired key up.
    if (event is KeyUpEvent && !isModifierKey(event.logicalKey)) {
      return KeyEventResult.handled;
    }
    return KeyEventResult.ignored;
  }

  @override
  TextEditingValue? get currentTextEditingValue => _decoder.current;

  @override
  AutofillScope? get currentAutofillScope => null;

  @override
  void updateEditingValueWithDeltas(List<TextEditingDelta> textEditingDeltas) =>
      _apply(_decoder.deltas(textEditingDeltas));

  @override
  void updateEditingValue(TextEditingValue value) =>
      _apply(_decoder.value(value));

  void _apply(SoftKeyboardEdit edit) {
    for (final input in edit.inputs) {
      switch (input) {
        case SoftKey(:final key):
          widget.onKey(key);
        case SoftText(:final text):
          widget.onText(text);
      }
    }
    if (edit.inputs.isNotEmpty) widget.onSoftKeyboard?.call();
    if (edit.reset) {
      _decoder.didReset();
      _connection?.setEditingState(softKeyboardSentinel);
    }
  }

  @override
  void performAction(TextInputAction action) {
    widget.onKey(const KeyInput('Enter'));
    widget.onSoftKeyboard?.call();
  }

  @override
  void connectionClosed() {
    _connection = null;
    _decoder.didReset();
  }

  @override
  void performPrivateCommand(String action, Map<String, dynamic> data) {}

  @override
  void updateFloatingCursor(RawFloatingCursorPoint point) {}

  @override
  void showAutocorrectionPromptRect(int start, int end) {}

  @override
  Widget build(BuildContext context) {
    return Focus(
      focusNode: widget.focusNode,
      onKeyEvent: _onKeyEvent,
      child: widget.child,
    );
  }
}
