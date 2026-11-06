import 'package:characters/characters.dart';
import 'package:flutter/foundation.dart';

import '../../terminal/frame.dart';
import '../models.dart';
import 'fake_screen.dart';

/// A pretend shell behind one fake tab: echoes what is typed into a
/// [FakeScreen] and answers a few commands, so the app is drivable with no
/// host.
class FakeShell extends ChangeNotifier {
  FakeShell({this.cwd = '~'});

  final String cwd;
  final FakeScreen screen = FakeScreen();
  String _line = '';

  String get line => _line;

  /// The screen changed behind the shell's back (a resize by another client).
  void changed() => notifyListeners();

  void prompt() {
    screen
      ..write(cwd, fg: FakeColors.blue)
      ..write(r' $ ', fg: FakeColors.muted);
    notifyListeners();
  }

  /// Writes scripted output (a banner, an agent's screen).
  void write(
    String text, {
    int fg = FakeColors.fg,
    int bg = FakeColors.bg,
    int attrs = 0,
  }) {
    screen.write(text, fg: fg, bg: bg, attrs: attrs);
    notifyListeners();
  }

  /// Bytes as `tab.write` would deliver them: `\r` runs the line, ESC
  /// sequences are swallowed.
  void type(String bytes) {
    final chars = bytes.characters.toList();
    for (var i = 0; i < chars.length; i++) {
      final c = chars[i];
      if (c == '\x1b') {
        if (i + 1 < chars.length &&
            (chars[i + 1] == '[' || chars[i + 1] == 'O')) {
          i += 2;
          while (i < chars.length && !_isFinal(chars[i])) {
            i++;
          }
        }
        continue;
      }
      switch (c) {
        case '\r' || '\n' || '\r\n':
          _enter();
        case '\x7f' || '\b':
          _backspace();
        case '\x03':
          _interrupt();
        default:
          if (c.codeUnitAt(0) >= 0x20) _insert(c);
      }
    }
    notifyListeners();
  }

  void paste(String text) {
    for (final c in text.characters) {
      if (c == '\n' || c == '\r\n') {
        screen.write('\n');
        _line += ' ';
      } else if (c.codeUnitAt(0) >= 0x20) {
        _insert(c);
      }
    }
    notifyListeners();
  }

  void key(KeyInput key) {
    if (key.ctrl && key.key.length == 1) {
      switch (key.key.toLowerCase()) {
        case 'c':
          _interrupt();
        case 'l':
          screen.clear();
          prompt();
          screen.write(_line);
        case 'u':
          for (var i = 0; i < _line.characters.length; i++) {
            screen.backspace();
          }
          _line = '';
      }
      notifyListeners();
      return;
    }
    switch (key.key) {
      case 'Enter':
        _enter();
      case 'Backspace':
        _backspace();
      default:
        final text = key.text;
        if (text != null && !key.meta) _insert(text);
    }
    notifyListeners();
  }

  static bool _isFinal(String c) {
    final u = c.codeUnitAt(0);
    return u >= 0x40 && u <= 0x7e;
  }

  void _insert(String text) {
    _line += text;
    screen.write(text);
  }

  void _backspace() {
    if (_line.isEmpty) return;
    _line = _line.characters.skipLast(1).toString();
    screen.backspace();
  }

  void _interrupt() {
    screen.write('^C\n', fg: FakeColors.muted);
    _line = '';
    prompt();
  }

  void _enter() {
    screen.write('\n');
    final cmd = _line.trim();
    _line = '';
    _run(cmd);
    prompt();
  }

  void _run(String cmd) {
    if (cmd.isEmpty) return;
    final word = cmd.split(' ').first;
    switch (word) {
      case 'echo':
        screen.write('${cmd.substring(4).trim()}\n');
      case 'clear':
        screen.clear();
      case 'ls':
        screen
          ..write('Cargo.toml  ', fg: FakeColors.fg)
          ..write(
            'crates/  docs/  mobile/  ',
            fg: FakeColors.blue,
            attrs: RunAttrs.bold,
          )
          ..write('README.md\n');
      case 'help':
        screen.write(
          'fake shell: echo, ls, clear, colors, help\n',
          fg: FakeColors.muted,
        );
      case 'colors':
        screen
          ..write('bold ', attrs: RunAttrs.bold)
          ..write('italic ', attrs: RunAttrs.italic)
          ..write('underline', attrs: RunAttrs.underline)
          ..write(' ')
          ..write('strike', attrs: RunAttrs.strike)
          ..write(' ')
          ..write('faint ', attrs: RunAttrs.faint)
          ..write(' inverse ', fg: FakeColors.bg, bg: FakeColors.fg)
          ..write(' 漢字\n', fg: FakeColors.orange);
      default:
        screen.write(
          'fake-shell: $word: command not found\n',
          fg: FakeColors.red,
        );
    }
  }
}
