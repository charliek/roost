import 'package:flutter/services.dart';

import '../core/models.dart';

final _namedKeys = <LogicalKeyboardKey, String>{
  LogicalKeyboardKey.enter: 'Enter',
  LogicalKeyboardKey.numpadEnter: 'Enter',
  LogicalKeyboardKey.tab: 'Tab',
  LogicalKeyboardKey.backspace: 'Backspace',
  LogicalKeyboardKey.escape: 'Escape',
  LogicalKeyboardKey.delete: 'Delete',
  LogicalKeyboardKey.insert: 'Insert',
  LogicalKeyboardKey.home: 'Home',
  LogicalKeyboardKey.end: 'End',
  LogicalKeyboardKey.pageUp: 'PageUp',
  LogicalKeyboardKey.pageDown: 'PageDown',
  LogicalKeyboardKey.arrowUp: 'ArrowUp',
  LogicalKeyboardKey.arrowDown: 'ArrowDown',
  LogicalKeyboardKey.arrowLeft: 'ArrowLeft',
  LogicalKeyboardKey.arrowRight: 'ArrowRight',
  LogicalKeyboardKey.f1: 'F1',
  LogicalKeyboardKey.f2: 'F2',
  LogicalKeyboardKey.f3: 'F3',
  LogicalKeyboardKey.f4: 'F4',
  LogicalKeyboardKey.f5: 'F5',
  LogicalKeyboardKey.f6: 'F6',
  LogicalKeyboardKey.f7: 'F7',
  LogicalKeyboardKey.f8: 'F8',
  LogicalKeyboardKey.f9: 'F9',
  LogicalKeyboardKey.f10: 'F10',
  LogicalKeyboardKey.f11: 'F11',
  LogicalKeyboardKey.f12: 'F12',
};

final _modifierKeys = <LogicalKeyboardKey>{
  LogicalKeyboardKey.shift,
  LogicalKeyboardKey.shiftLeft,
  LogicalKeyboardKey.shiftRight,
  LogicalKeyboardKey.control,
  LogicalKeyboardKey.controlLeft,
  LogicalKeyboardKey.controlRight,
  LogicalKeyboardKey.alt,
  LogicalKeyboardKey.altLeft,
  LogicalKeyboardKey.altRight,
  LogicalKeyboardKey.altGraph,
  LogicalKeyboardKey.meta,
  LogicalKeyboardKey.metaLeft,
  LogicalKeyboardKey.metaRight,
  LogicalKeyboardKey.superKey,
  LogicalKeyboardKey.hyper,
  LogicalKeyboardKey.capsLock,
  LogicalKeyboardKey.numLock,
  LogicalKeyboardKey.scrollLock,
  LogicalKeyboardKey.fn,
  LogicalKeyboardKey.fnLock,
};

bool isModifierKey(LogicalKeyboardKey key) => _modifierKeys.contains(key);

/// Every named key this side can send; the Rust encoder is tested against
/// the same list (`test/fixtures/key_names.json`).
Set<String> get namedKeyNames => _namedKeys.values.toSet();

bool _printable(String? s) =>
    s != null &&
    s.isNotEmpty &&
    !s.runes.any((r) => r < 0x20 || (r >= 0x7f && r < 0xa0));

/// Maps a hardware key event to a [KeyInput], or null for releases and pure
/// modifier presses (they only change what the next key means).
KeyInput? keyInputFor(
  KeyEvent event, {
  required bool ctrl,
  required bool alt,
  required bool shift,
  required bool meta,
}) {
  if (event is KeyUpEvent) return null;
  final logical = event.logicalKey;
  if (isModifierKey(logical)) return null;
  final repeat = event is KeyRepeatEvent;
  final named = _namedKeys[logical];
  if (named != null) {
    return KeyInput(
      named,
      ctrl: ctrl,
      alt: alt,
      shift: shift,
      meta: meta,
      repeat: repeat,
    );
  }
  final char = event.character;
  final text = _printable(char) ? char : null;
  final label = logical.keyLabel;
  final String key;
  if (label.runes.length == 1) {
    key = label.toLowerCase();
  } else if (text != null) {
    key = text.toLowerCase();
  } else {
    return null;
  }
  return KeyInput(
    key,
    text: text,
    ctrl: ctrl,
    alt: alt,
    shift: shift,
    meta: meta,
    repeat: repeat,
  );
}

/// Whether a key event can only have come from a physical keyboard.
///
/// Soft keyboards commit text through the input connection; the key events
/// they do send are for Backspace, Enter and the like. A printable character
/// or a modifier press as a key event means a hardware keyboard.
bool looksLikeHardwareKeyboard(KeyEvent event) {
  if (event is! KeyDownEvent) return false;
  if (isModifierKey(event.logicalKey)) return true;
  final c = event.character;
  return _printable(c) && c!.trim().isNotEmpty;
}
