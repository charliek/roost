import 'package:flutter/services.dart';
import 'package:flutter/widgets.dart';

import '../core/models.dart';
import 'key_mapping.dart';

/// What the input connection holds between edits. An IME only reports a
/// Backspace when there is something before the cursor to delete, so the
/// field is never empty: every edit is diffed against this value and the
/// field is reset to it.
const softKeyboardSentinel = TextEditingValue(
  text: '  ',
  selection: TextSelection.collapsed(offset: 2),
);

/// The Samsung Keyboard / Gboard friendly configuration. `enableSuggestions:
/// false` makes Flutter's Android plugin add `TYPE_TEXT_VARIATION_VISIBLE_
/// PASSWORD` + `NO_SUGGESTIONS`, which is what turns off composition,
/// prediction and autocorrect on both keyboards (Termux's char-based mode);
/// `multiline` keeps an Enter key rather than Done.
const terminalTextInputConfiguration = TextInputConfiguration(
  inputType: TextInputType.multiline,
  inputAction: TextInputAction.newline,
  autocorrect: false,
  enableSuggestions: false,
  enableIMEPersonalizedLearning: false,
  enableInteractiveSelection: false,
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

  /// The field drifted from [softKeyboardSentinel] and must be put back.
  final bool reset;

  @override
  String toString() => 'SoftKeyboardEdit($inputs, reset: $reset)';
}

/// Diffs the field's new value against [softKeyboardSentinel]: characters
/// gone from it are Backspaces, characters added are text, and each newline
/// is an Enter key.
SoftKeyboardEdit diffSoftKeyboard(TextEditingValue next) {
  if (next.composing.isValid && !next.composing.isCollapsed) {
    return SoftKeyboardEdit.composing;
  }
  const base = softKeyboardSentinel;
  final old = base.text;
  final now = next.text;
  var prefix = 0;
  while (prefix < old.length &&
      prefix < now.length &&
      old.codeUnitAt(prefix) == now.codeUnitAt(prefix)) {
    prefix++;
  }
  var suffix = 0;
  while (suffix < old.length - prefix &&
      suffix < now.length - prefix &&
      old.codeUnitAt(old.length - 1 - suffix) ==
          now.codeUnitAt(now.length - 1 - suffix)) {
    suffix++;
  }
  final deleted = old.length - prefix - suffix;
  final inserted = now.substring(prefix, now.length - suffix);

  final inputs = <SoftInput>[
    for (var i = 0; i < deleted; i++) const SoftKey(KeyInput('Backspace')),
  ];
  final lines = inserted.replaceAll('\r\n', '\n').split('\n');
  for (var i = 0; i < lines.length; i++) {
    if (i > 0) inputs.add(const SoftKey(KeyInput('Enter')));
    if (lines[i].isNotEmpty) inputs.add(SoftText(lines[i]));
  }
  return SoftKeyboardEdit(
    inputs,
    reset: now != old || next.selection != base.selection,
  );
}

/// Keyboard input for a terminal: hardware keys through [Focus.onKeyEvent]
/// (handled there, so the IME never sees them twice), the soft keyboard
/// through a [TextInputClient] holding [softKeyboardSentinel].
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

class TerminalInputState extends State<TerminalInput> with TextInputClient {
  TextInputConnection? _connection;
  TextEditingValue _value = softKeyboardSentinel;

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
    _value = softKeyboardSentinel;
    _connection = TextInput.attach(this, terminalTextInputConfiguration)
      ..setEditingState(_value);
  }

  void _closeConnection() {
    if (hasConnection) _connection!.close();
    _connection = null;
  }

  KeyEventResult _onKeyEvent(FocusNode node, KeyEvent event) {
    // While an IME composes, its own key events must reach it.
    if (_value.composing.isValid && !_value.composing.isCollapsed) {
      return KeyEventResult.skipRemainingHandlers;
    }
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
  TextEditingValue? get currentTextEditingValue => _value;

  @override
  AutofillScope? get currentAutofillScope => null;

  @override
  void updateEditingValue(TextEditingValue value) {
    _value = value;
    final edit = diffSoftKeyboard(value);
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
      _value = softKeyboardSentinel;
      _connection?.setEditingState(_value);
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
    _value = softKeyboardSentinel;
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
