import 'package:flutter/foundation.dart';

import 'frame.dart';

/// A tab's screen as the frames have described it so far: a per-row cache
/// the painter draws from, plus cursor, modes and scrollbar.
///
/// Each row carries a generation that changes whenever a frame replaces it,
/// so the painter rebuilds text only for rows that changed.
class TerminalModel extends ChangeNotifier {
  int cols = 0;
  int rows = 0;
  int defaultFg = 0xd7dae0;
  int defaultBg = 0x0f1115;
  int cursorColor = 0xe6e8ee;
  int flags = 0;
  int cursorCol = 0;
  int cursorRow = 0;
  CursorStyle cursorStyle = CursorStyle.block;
  int scrollTotal = 0;
  int scrollOffset = 0;
  int scrollLen = 0;

  List<FrameRow?> _rows = const [];
  List<int> _rowGenerations = const [];
  int _generation = 0;
  bool _hasFrame = false;

  bool get hasFrame => _hasFrame;
  bool get cursorVisible => flags & FrameFlags.cursorVisible != 0;
  bool get mouseTracking => flags & FrameFlags.mouseTracking != 0;
  bool get altScreen => flags & FrameFlags.altScreen != 0;
  bool get bracketedPaste => flags & FrameFlags.bracketedPaste != 0;

  /// Scrolled back from the live bottom.
  bool get scrolledBack =>
      scrollTotal > scrollLen && scrollOffset + scrollLen < scrollTotal;

  /// The row's runs, or null for a blank row.
  FrameRow? row(int index) =>
      index >= 0 && index < _rows.length ? _rows[index] : null;

  int rowGeneration(int index) =>
      index >= 0 && index < _rowGenerations.length ? _rowGenerations[index] : 0;

  void apply(Frame frame) {
    final resized = frame.cols != cols || frame.rows != rows;
    cols = frame.cols;
    rows = frame.rows;
    defaultFg = frame.defaultFg;
    defaultBg = frame.defaultBg;
    cursorColor = frame.cursorColor;
    flags = frame.flags;
    cursorCol = frame.cursorCol;
    cursorRow = frame.cursorRow;
    cursorStyle = frame.cursorStyle;
    scrollTotal = frame.scrollTotal;
    scrollOffset = frame.scrollOffset;
    scrollLen = frame.scrollLen;

    // A grid change without FULL would leave rows of the old width behind;
    // treat it as FULL rather than trust the sender.
    if (frame.isFull || resized) {
      _generation++;
      _rows = List<FrameRow?>.filled(rows, null);
      _rowGenerations = List<int>.filled(rows, _generation);
    }
    for (final row in frame.rowData) {
      if (row.index >= rows) continue;
      _generation++;
      _rows[row.index] = row.runs.isEmpty ? null : row;
      _rowGenerations[row.index] = _generation;
    }
    _hasFrame = true;
    notifyListeners();
  }

  /// The text of a row with blank gaps filled, for tests and previews.
  String rowText(int index) {
    final r = row(index);
    if (r == null) return '';
    final buf = StringBuffer();
    var col = 0;
    for (final run in r.runs) {
      if (run.col > col) buf.write(' ' * (run.col - col));
      buf.write(run.text);
      col = run.col + run.cells;
    }
    return buf.toString().trimRight();
  }
}
