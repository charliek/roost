import 'dart:math' as math;
import 'dart:ui' as ui;

import 'package:characters/characters.dart';
import 'package:flutter/foundation.dart';
import 'package:flutter/rendering.dart';

import 'cell_metrics.dart';
import 'frame.dart';
import 'terminal_model.dart';

Color terminalColor(int rgb) => Color(0xFF000000 | (rgb & 0xFFFFFF));

/// Laid-out text for each row's runs, rebuilt only when the row's
/// generation in the [TerminalModel] moves or the font size changes.
class RowTextCache {
  final Map<int, _RowText> _rows = {};
  double? _fontSize;

  /// Paragraphs built since creation; tests watch it to prove that clean rows
  /// are not rebuilt.
  @visibleForTesting
  int builds = 0;

  List<RunText> textFor(
    int row,
    int generation,
    FrameRow data,
    CellMetrics metrics,
  ) {
    if (_fontSize != metrics.fontSize) {
      clear();
      _fontSize = metrics.fontSize;
    }
    final cached = _rows[row];
    if (cached != null && cached.generation == generation) return cached.runs;
    cached?.dispose();
    final runs = <RunText>[];
    for (final run in data.runs) {
      final underline = run.has(RunAttrs.underline);
      final strike = run.has(RunAttrs.strike);
      if (run.text.trim().isEmpty && !underline && !strike) continue;
      final color = terminalColor(run.fg);
      final paragraph = layoutTerminalText(
        run.text,
        terminalTextStyle(
          fontSize: metrics.fontSize,
          color: run.has(RunAttrs.faint)
              ? color.withValues(alpha: 0.55)
              : color,
          bold: run.has(RunAttrs.bold),
          italic: run.has(RunAttrs.italic),
          underline: underline,
          strike: strike,
        ),
        metrics.fontSize,
      );
      builds++;
      runs.add(RunText(run.col, paragraph));
    }
    _rows[row] = _RowText(generation, runs);
    return runs;
  }

  /// Drops rows past [rows] (the grid shrank).
  void retain(int rows) {
    _rows.removeWhere((row, text) {
      if (row < rows) return false;
      text.dispose();
      return true;
    });
  }

  void clear() {
    for (final r in _rows.values) {
      r.dispose();
    }
    _rows.clear();
  }
}

class RunText {
  RunText(this.col, this.paragraph);

  final int col;
  final ui.Paragraph paragraph;
}

class _RowText {
  _RowText(this.generation, this.runs);

  final int generation;
  final List<RunText> runs;

  void dispose() {
    for (final r in runs) {
      r.paragraph.dispose();
    }
  }
}

/// Paints a [TerminalModel]: background runs, cached text runs, cursor and
/// scrollbar. When the tab's grid is not the one this screen fits
/// ([localCols]×[localRows]), the grid is drawn top-left, shrunk to fit if
/// it is larger, and the rest is letterboxed.
class TerminalPainter extends CustomPainter {
  TerminalPainter({
    required this.model,
    required this.metrics,
    required this.textCache,
    required this.localCols,
    required this.localRows,
    required this.focused,
    this.inset = 0,
  }) : super(repaint: model);

  final TerminalModel model;
  final CellMetrics metrics;
  final RowTextCache textCache;
  final int localCols;
  final int localRows;
  final bool focused;

  /// Margin around the grid, painted in the terminal's background.
  final double inset;

  @override
  void paint(Canvas canvas, Size size) {
    final m = model;
    final bg = terminalColor(m.defaultBg);
    if (!m.hasFrame || m.cols == 0 || m.rows == 0) {
      canvas.drawRect(Offset.zero & size, Paint()..color = bg);
      return;
    }
    final cw = metrics.cellWidth;
    final ch = metrics.cellHeight;
    final letterbox = m.cols != localCols || m.rows != localRows;
    final inner = Size(
      math.max(0, size.width - 2 * inset),
      math.max(0, size.height - 2 * inset),
    );
    final scale = metrics.scaleToFit(m.cols, m.rows, inner);
    final grid = Rect.fromLTWH(0, 0, m.cols * cw, m.rows * ch);

    canvas.drawRect(
      Offset.zero & size,
      Paint()
        ..color = letterbox
            ? Color.lerp(bg, const Color(0xFF000000), 0.4)!
            : bg,
    );
    canvas
      ..save()
      ..clipRect(Offset.zero & size)
      ..translate(inset, inset)
      ..scale(scale)
      ..drawRect(grid, Paint()..color = bg);

    textCache.retain(m.rows);
    final fill = Paint()..isAntiAlias = false;
    for (var r = 0; r < m.rows; r++) {
      final row = m.row(r);
      if (row == null) continue;
      final top = r * ch;
      for (final run in row.runs) {
        if (run.has(RunAttrs.explicitBg) || run.bg != m.defaultBg) {
          fill.color = terminalColor(run.bg);
          canvas.drawRect(
            Rect.fromLTWH(run.col * cw, top, run.cells * cw, ch),
            fill,
          );
        }
      }
      for (final t in textCache.textFor(r, m.rowGeneration(r), row, metrics)) {
        canvas.drawParagraph(
          t.paragraph,
          Offset(t.col * cw, top + (ch - t.paragraph.height) / 2),
        );
      }
    }
    _paintCursor(canvas);
    if (letterbox) {
      canvas.drawRect(
        grid,
        Paint()
          ..style = PaintingStyle.stroke
          ..strokeWidth = 1 / scale
          ..color = const Color(0x33FFFFFF),
      );
    }
    canvas.restore();
    _paintScrollbar(
      canvas,
      Offset(inset, inset) & Size(grid.width * scale, grid.height * scale),
    );
  }

  void _paintCursor(Canvas canvas) {
    final m = model;
    if (!m.cursorVisible) return;
    final c = m.cursorCol;
    final r = m.cursorRow;
    if (c >= m.cols || r >= m.rows) return;
    final cw = metrics.cellWidth;
    final ch = metrics.cellHeight;
    final run = _runAt(m.row(r)?.runs, c);
    final wide = run != null && run.has(RunAttrs.wide);
    final rect = Rect.fromLTWH(c * cw, r * ch, (wide ? 2 : 1) * cw, ch);
    final color = terminalColor(m.cursorColor);
    final paint = Paint()..color = color;
    var style = m.cursorStyle;
    if (!focused && style == CursorStyle.block) style = CursorStyle.blockHollow;
    switch (style) {
      case CursorStyle.block:
        canvas.drawRect(rect, paint);
        final glyph = _glyphAt(run, c);
        if (glyph != null && glyph.trim().isNotEmpty) {
          final p = layoutTerminalText(
            glyph,
            terminalTextStyle(
              fontSize: metrics.fontSize,
              color: terminalColor(m.defaultBg),
              bold: run!.has(RunAttrs.bold),
              italic: run.has(RunAttrs.italic),
            ),
            metrics.fontSize,
          );
          canvas.drawParagraph(
            p,
            Offset(rect.left, rect.top + (ch - p.height) / 2),
          );
          p.dispose();
        }
      case CursorStyle.blockHollow:
        canvas.drawRect(
          rect.deflate(0.5),
          paint
            ..style = PaintingStyle.stroke
            ..strokeWidth = 1,
        );
      case CursorStyle.bar:
        canvas.drawRect(
          Rect.fromLTWH(rect.left, rect.top, math.max(1.5, cw * 0.12), ch),
          paint,
        );
      case CursorStyle.underline:
        final h = math.max(1.5, ch * 0.08);
        canvas.drawRect(
          Rect.fromLTWH(rect.left, rect.bottom - h, rect.width, h),
          paint,
        );
    }
  }

  void _paintScrollbar(Canvas canvas, Rect grid) {
    final m = model;
    if (!m.scrolledBack) return;
    final track = grid.height;
    final thumb = math.max(24.0, track * m.scrollLen / m.scrollTotal);
    final travel = m.scrollTotal - m.scrollLen;
    final top = (track - thumb) * (m.scrollOffset / travel).clamp(0.0, 1.0);
    canvas.drawRRect(
      RRect.fromRectAndRadius(
        Rect.fromLTWH(grid.right - 5, grid.top + top, 3, thumb),
        const Radius.circular(1.5),
      ),
      Paint()..color = const Color(0x66FFFFFF),
    );
  }

  static FrameRun? _runAt(List<FrameRun>? runs, int col) {
    if (runs == null) return null;
    for (final run in runs) {
      if (col >= run.col && col < run.col + run.cells) return run;
    }
    return null;
  }

  static String? _glyphAt(FrameRun? run, int col) {
    if (run == null) return null;
    if (run.has(RunAttrs.wide)) return run.text;
    final chars = run.text.characters;
    final i = col - run.col;
    return i < chars.length ? chars.elementAt(i) : null;
  }

  @override
  bool shouldRepaint(TerminalPainter old) =>
      old.model != model ||
      old.metrics != metrics ||
      old.textCache != textCache ||
      old.localCols != localCols ||
      old.localRows != localRows ||
      old.focused != focused ||
      old.inset != inset;
}
