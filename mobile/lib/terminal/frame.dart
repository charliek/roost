import 'dart:convert';

import 'package:flutter/foundation.dart';

/// Frame payload v1 (plan 089 C6): one pull of a tab's changed rows, encoded
/// by `roost-mobile` and decoded here. All integers little-endian; colors are
/// `0x00RRGGBB`.
const int frameVersion = 1;

/// Header bytes before the first row.
const int frameHeaderLength = 37;

abstract final class FrameFlags {
  /// The row cache is stale: drop it, every non-blank row follows.
  static const int full = 1 << 0;
  static const int cursorVisible = 1 << 1;
  static const int mouseTracking = 1 << 2;
  static const int altScreen = 1 << 3;
  static const int bracketedPaste = 1 << 4;
}

abstract final class RunAttrs {
  static const int bold = 1 << 0;
  static const int italic = 1 << 1;
  static const int underline = 1 << 2;
  static const int strike = 1 << 3;
  static const int faint = 1 << 4;
  static const int explicitBg = 1 << 5;

  /// One wide (two-cell) character; its run has `cells == 2`.
  static const int wide = 1 << 6;
}

enum CursorStyle {
  block,
  bar,
  underline,
  blockHollow;

  static CursorStyle fromWire(int value) =>
      value >= 0 && value < values.length ? values[value] : block;
}

/// Consecutive cells sharing colors and attributes, drawn as one text run.
@immutable
class FrameRun {
  const FrameRun({
    required this.col,
    required this.cells,
    required this.fg,
    required this.bg,
    this.attrs = 0,
    required this.text,
  });

  final int col;
  final int cells;
  final int fg;
  final int bg;
  final int attrs;
  final String text;

  bool has(int attr) => attrs & attr != 0;

  @override
  bool operator ==(Object other) =>
      other is FrameRun &&
      other.col == col &&
      other.cells == cells &&
      other.fg == fg &&
      other.bg == bg &&
      other.attrs == attrs &&
      other.text == text;

  @override
  int get hashCode => Object.hash(col, cells, fg, bg, attrs, text);

  @override
  String toString() =>
      'FrameRun(col $col, $cells cells, fg ${_hex(fg)}, bg ${_hex(bg)}, '
      'attrs $attrs, "$text")';
}

@immutable
class FrameRow {
  const FrameRow({required this.index, required this.runs});

  final int index;
  final List<FrameRun> runs;

  @override
  bool operator ==(Object other) =>
      other is FrameRow && other.index == index && listEquals(other.runs, runs);

  @override
  int get hashCode => Object.hash(index, Object.hashAll(runs));

  @override
  String toString() => 'FrameRow($index, $runs)';
}

@immutable
class Frame {
  const Frame({
    this.flags = 0,
    required this.cols,
    required this.rows,
    required this.defaultFg,
    required this.defaultBg,
    required this.cursorColor,
    this.cursorCol = 0,
    this.cursorRow = 0,
    this.cursorStyle = CursorStyle.block,
    this.scrollTotal = 0,
    this.scrollOffset = 0,
    this.scrollLen = 0,
    this.rowData = const [],
  });

  final int flags;
  final int cols;
  final int rows;
  final int defaultFg;
  final int defaultBg;
  final int cursorColor;
  final int cursorCol;
  final int cursorRow;
  final CursorStyle cursorStyle;
  final int scrollTotal;
  final int scrollOffset;
  final int scrollLen;
  final List<FrameRow> rowData;

  bool has(int flag) => flags & flag != 0;
  bool get isFull => has(FrameFlags.full);

  @override
  bool operator ==(Object other) =>
      other is Frame &&
      other.flags == flags &&
      other.cols == cols &&
      other.rows == rows &&
      other.defaultFg == defaultFg &&
      other.defaultBg == defaultBg &&
      other.cursorColor == cursorColor &&
      other.cursorCol == cursorCol &&
      other.cursorRow == cursorRow &&
      other.cursorStyle == cursorStyle &&
      other.scrollTotal == scrollTotal &&
      other.scrollOffset == scrollOffset &&
      other.scrollLen == scrollLen &&
      listEquals(other.rowData, rowData);

  @override
  int get hashCode => Object.hash(
    flags,
    cols,
    rows,
    defaultFg,
    defaultBg,
    cursorColor,
    cursorCol,
    cursorRow,
    cursorStyle,
    scrollTotal,
    scrollOffset,
    scrollLen,
    Object.hashAll(rowData),
  );

  @override
  String toString() =>
      'Frame(${cols}x$rows flags $flags cursor $cursorCol,$cursorRow '
      '${cursorStyle.name} scroll $scrollTotal/$scrollOffset/$scrollLen '
      'rows $rowData)';
}

class FrameFormatException implements Exception {
  const FrameFormatException(this.message);

  final String message;

  @override
  String toString() => 'FrameFormatException: $message';
}

/// Decodes one C6 v1 payload. Throws [FrameFormatException] on a version it
/// does not know or a payload that ends early.
Frame decodeFrame(Uint8List bytes) {
  final r = _Reader(bytes);
  final version = r.u8();
  if (version != frameVersion) {
    throw FrameFormatException(
      'frame version $version, expected $frameVersion',
    );
  }
  final flags = r.u8();
  final cols = r.u16();
  final rows = r.u16();
  final defaultFg = r.u32();
  final defaultBg = r.u32();
  final cursorColor = r.u32();
  final cursorCol = r.u16();
  final cursorRow = r.u16();
  final cursorStyle = CursorStyle.fromWire(r.u8());
  final scrollTotal = r.u32();
  final scrollOffset = r.u32();
  final scrollLen = r.u32();
  final rowCount = r.u16();
  final rowData = <FrameRow>[];
  for (var i = 0; i < rowCount; i++) {
    final index = r.u16();
    final runCount = r.u16();
    final runs = <FrameRun>[];
    for (var j = 0; j < runCount; j++) {
      final col = r.u16();
      final cells = r.u16();
      final fg = r.u32();
      final bg = r.u32();
      final attrs = r.u8();
      final textLen = r.u16();
      final text = r.utf8String(textLen);
      runs.add(
        FrameRun(
          col: col,
          cells: cells,
          fg: fg,
          bg: bg,
          attrs: attrs,
          text: text,
        ),
      );
    }
    rowData.add(FrameRow(index: index, runs: runs));
  }
  return Frame(
    flags: flags,
    cols: cols,
    rows: rows,
    defaultFg: defaultFg,
    defaultBg: defaultBg,
    cursorColor: cursorColor,
    cursorCol: cursorCol,
    cursorRow: cursorRow,
    cursorStyle: cursorStyle,
    scrollTotal: scrollTotal,
    scrollOffset: scrollOffset,
    scrollLen: scrollLen,
    rowData: rowData,
  );
}

class _Reader {
  _Reader(this._bytes) : _data = ByteData.sublistView(_bytes);

  final Uint8List _bytes;
  final ByteData _data;
  int _at = 0;

  void _need(int n) {
    if (_at + n > _bytes.length) {
      throw FrameFormatException(
        'payload ends at byte ${_bytes.length}, needed ${_at + n}',
      );
    }
  }

  int u8() {
    _need(1);
    return _data.getUint8(_at++);
  }

  int u16() {
    _need(2);
    final v = _data.getUint16(_at, Endian.little);
    _at += 2;
    return v;
  }

  int u32() {
    _need(4);
    final v = _data.getUint32(_at, Endian.little);
    _at += 4;
    return v;
  }

  String utf8String(int length) {
    _need(length);
    final s = utf8.decode(
      Uint8List.sublistView(_bytes, _at, _at + length),
      allowMalformed: true,
    );
    _at += length;
    return s;
  }
}

String _hex(int color) => '0x${color.toRadixString(16).padLeft(6, '0')}';
