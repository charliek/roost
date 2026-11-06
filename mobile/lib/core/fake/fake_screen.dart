import 'dart:math' as math;

import 'package:characters/characters.dart';
import 'package:flutter/foundation.dart';

import '../../terminal/frame.dart';

/// Colors the fake screens draw with (the mockup's palette).
abstract final class FakeColors {
  static const int fg = 0xd7dae0;
  static const int bg = 0x0f1115;
  static const int cursor = 0xe6e8ee;
  static const int muted = 0x8a92a1;
  static const int blue = 0x79aaff;
  static const int orange = 0xf2a65a;
  static const int green = 0x7fd19b;
  static const int red = 0xf08a8a;
}

class FakeCell {
  const FakeCell(
    this.char, {
    this.fg = FakeColors.fg,
    this.bg = FakeColors.bg,
    this.attrs = 0,
  });

  static const blank = FakeCell(' ');

  final String char;
  final int fg;
  final int bg;
  final int attrs;

  bool get wide => attrs & RunAttrs.wide != 0;
  int get width => wide ? 2 : 1;

  bool get isBlank =>
      char == ' ' &&
      bg == FakeColors.bg &&
      attrs & (RunAttrs.underline | RunAttrs.strike | RunAttrs.explicitBg) == 0;

  bool sameStyle(FakeCell other) =>
      fg == other.fg && bg == other.bg && attrs == other.attrs;
}

/// Logical lines that soft-wrap to the grid when a frame is taken, so a
/// resize reflows like a real terminal. Frames carry only the visible rows
/// that differ from the last frame. No escape-sequence parser: scripts
/// write styled text directly.
class FakeScreen {
  FakeScreen({this.cols = 80, this.rows = 24});

  static const int maxLines = 1000;

  int cols;
  int rows;
  final List<List<FakeCell>> _lines = [[]];
  int _cursor = 0;
  int _scrollBack = 0;
  bool _full = true;
  bool _changed = true;
  final Map<int, List<FrameRun>> _sent = {};

  bool get dirty => _full || _changed;

  void markFull() => _full = true;

  void write(
    String text, {
    int fg = FakeColors.fg,
    int bg = FakeColors.bg,
    int attrs = 0,
  }) {
    _scrollBack = 0;
    for (final ch in text.characters) {
      switch (ch) {
        case '\n' || '\r\n':
          _lines.add([]);
          _cursor = 0;
          if (_lines.length > maxLines) _lines.removeAt(0);
        case '\r':
          _cursor = 0;
        default:
          _put(
            FakeCell(
              ch,
              fg: fg,
              bg: bg,
              attrs: attrs | (isWide(ch) ? RunAttrs.wide : 0),
            ),
          );
      }
    }
    _changed = true;
  }

  void _put(FakeCell cell) {
    final line = _lines.last;
    while (line.length < _cursor) {
      line.add(FakeCell.blank);
    }
    if (_cursor < line.length) {
      line[_cursor] = cell;
    } else {
      line.add(cell);
    }
    _cursor++;
  }

  void backspace() {
    if (_cursor == 0) return;
    _cursor--;
    final line = _lines.last;
    if (_cursor < line.length) line.removeRange(_cursor, line.length);
    _scrollBack = 0;
    _changed = true;
  }

  void clear() {
    _lines
      ..clear()
      ..add([]);
    _cursor = 0;
    _scrollBack = 0;
    _full = true;
  }

  void resize(int cols, int rows) {
    if (cols == this.cols && rows == this.rows) return;
    this.cols = math.max(2, cols);
    this.rows = math.max(1, rows);
    _full = true;
  }

  /// Positive scrolls up into history.
  void scroll(int delta) {
    final total = _wrap().rows.length;
    final next = (_scrollBack + delta)
        .clamp(0, math.max(0, total - rows))
        .toInt();
    if (next == _scrollBack) return;
    _scrollBack = next;
    _changed = true;
  }

  /// Every visual row, plus where the cursor lands among them.
  ({List<List<FakeCell>> rows, int cursorRow, int cursorCol}) _wrap() {
    final out = <List<FakeCell>>[];
    var cursorRow = 0;
    var cursorCol = 0;
    for (var li = 0; li < _lines.length; li++) {
      final line = _lines[li];
      final last = li == _lines.length - 1;
      var row = <FakeCell>[];
      var width = 0;
      for (var i = 0; i < line.length; i++) {
        final cell = line[i];
        if (width + cell.width > cols) {
          out.add(row);
          row = [];
          width = 0;
        }
        if (last && i == _cursor) {
          cursorRow = out.length;
          cursorCol = width;
        }
        row.add(cell);
        width += cell.width;
      }
      if (last && _cursor >= line.length) {
        if (width + (_cursor - line.length) >= cols) {
          out.add(row);
          row = [];
          width = 0;
        }
        cursorRow = out.length;
        cursorCol = width + (_cursor - line.length);
      }
      out.add(row);
    }
    return (rows: out, cursorRow: cursorRow, cursorCol: cursorCol);
  }

  /// The visible rows that changed since the last frame (all of them after
  /// [markFull], a resize or a clear).
  Frame takeFrame({bool cursorVisible = true}) {
    final wrapped = _wrap();
    final total = wrapped.rows.length;
    final bottom = total - _scrollBack;
    final top = math.max(0, bottom - rows);
    final full = _full;
    if (full) _sent.clear();
    final rowData = <FrameRow>[];
    for (var r = 0; r < rows; r++) {
      final vi = top + r;
      final runs = vi < bottom ? runsOf(wrapped.rows[vi], cols) : <FrameRun>[];
      final before = _sent[r];
      if (full ? runs.isEmpty : listEquals(before, runs)) {
        _sent[r] = runs;
        continue;
      }
      if (!full && before == null && runs.isEmpty) continue;
      _sent[r] = runs;
      rowData.add(FrameRow(index: r, runs: runs));
    }
    _full = false;
    _changed = false;
    final cursorRow = wrapped.cursorRow - top;
    final showCursor =
        cursorVisible && _scrollBack == 0 && cursorRow >= 0 && cursorRow < rows;
    return Frame(
      flags:
          (full ? FrameFlags.full : 0) |
          (showCursor ? FrameFlags.cursorVisible : 0),
      cols: cols,
      rows: rows,
      defaultFg: FakeColors.fg,
      defaultBg: FakeColors.bg,
      cursorColor: FakeColors.cursor,
      cursorCol: math.min(wrapped.cursorCol, cols - 1),
      cursorRow: cursorRow.clamp(0, rows - 1),
      scrollTotal: math.max(total, rows),
      scrollOffset: top,
      scrollLen: rows,
      rowData: rowData,
    );
  }

  /// C6's run rule: maximal same-style stretches, blank default cells left
  /// out as gaps, a wide character alone with `cells == 2`.
  static List<FrameRun> runsOf(List<FakeCell> row, int cols) {
    final runs = <FrameRun>[];
    FakeCell? style;
    var start = 0;
    var cells = 0;
    final text = StringBuffer();
    void flush() {
      final s = style;
      if (s != null && cells > 0) {
        runs.add(
          FrameRun(
            col: start,
            cells: cells,
            fg: s.fg,
            bg: s.bg,
            attrs: s.attrs,
            text: text.toString(),
          ),
        );
      }
      style = null;
      cells = 0;
      text.clear();
    }

    var col = 0;
    for (final cell in row) {
      if (col + cell.width > cols) break;
      if (cell.isBlank) {
        flush();
      } else if (cell.wide) {
        flush();
        runs.add(
          FrameRun(
            col: col,
            cells: 2,
            fg: cell.fg,
            bg: cell.bg,
            attrs: cell.attrs,
            text: cell.char,
          ),
        );
      } else {
        if (style == null || !style!.sameStyle(cell)) {
          flush();
          style = cell;
          start = col;
        }
        text.write(cell.char);
        cells++;
      }
      col += cell.width;
    }
    flush();
    return runs;
  }

  /// East Asian wide and emoji ranges: enough for the fake's scripts.
  static bool isWide(String char) {
    if (char.isEmpty) return false;
    final r = char.runes.first;
    return (r >= 0x1100 && r <= 0x115f) ||
        (r >= 0x2e80 && r <= 0xa4cf) ||
        (r >= 0xac00 && r <= 0xd7a3) ||
        (r >= 0xf900 && r <= 0xfaff) ||
        (r >= 0xfe30 && r <= 0xfe4f) ||
        (r >= 0xff00 && r <= 0xff60) ||
        (r >= 0xffe0 && r <= 0xffe6) ||
        (r >= 0x1f300 && r <= 0x1faff) ||
        (r >= 0x20000 && r <= 0x3fffd);
  }
}
