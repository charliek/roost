import 'dart:ui' as ui;

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:roost_mobile/terminal/cell_metrics.dart';
import 'package:roost_mobile/terminal/frame.dart';
import 'package:roost_mobile/terminal/terminal_model.dart';
import 'package:roost_mobile/terminal/terminal_painter.dart';

import '../support/fonts.dart';

const _fg = 0xd7dae0;
const _bg = 0x0f1115;

FrameRun _run(
  int col,
  String text, {
  int fg = _fg,
  int bg = _bg,
  int attrs = 0,
  int? cells,
}) => FrameRun(
  col: col,
  cells: cells ?? text.length,
  fg: fg,
  bg: bg,
  attrs: attrs,
  text: text,
);

Frame _styles({
  int cols = 32,
  int rows = 7,
  CursorStyle cursor = CursorStyle.block,
}) => Frame(
  flags: FrameFlags.full | FrameFlags.cursorVisible,
  cols: cols,
  rows: rows,
  defaultFg: _fg,
  defaultBg: _bg,
  cursorColor: 0xe6e8ee,
  cursorCol: 19,
  cursorRow: 5,
  cursorStyle: cursor,
  rowData: [
    FrameRow(
      index: 0,
      runs: [
        _run(0, '~/projects/roost', fg: 0x79aaff, attrs: RunAttrs.bold),
        _run(17, r'$', fg: 0x8a92a1),
        _run(19, 'ls -l'),
      ],
    ),
    FrameRow(
      index: 1,
      runs: [
        _run(0, 'bold', attrs: RunAttrs.bold),
        _run(5, 'italic', attrs: RunAttrs.italic),
        _run(12, 'under', attrs: RunAttrs.underline),
        _run(18, 'strike', attrs: RunAttrs.strike),
        _run(25, 'faint', attrs: RunAttrs.faint),
      ],
    ),
    FrameRow(
      index: 2,
      runs: [
        _run(0, ' inverse ', fg: _bg, bg: _fg, attrs: RunAttrs.explicitBg),
        _run(10, 'red', fg: 0xf08a8a),
        _run(14, 'green', fg: 0x7fd19b),
        _run(20, 'orange', fg: 0xf2a65a, bg: 0x3a2a17),
      ],
    ),
    FrameRow(index: 3, runs: [_run(0, '┌──────┐ │ -> != => ─│', fg: 0x9aa3b2)]),
    FrameRow(
      index: 5,
      runs: [_run(0, r'~/projects/roost $ cargo', fg: 0x7fd19b)],
    ),
  ],
);

TerminalModel _model(Frame frame) => TerminalModel()..apply(frame);

Future<void> _pumpPainter(
  WidgetTester tester, {
  required TerminalModel model,
  required CellMetrics metrics,
  required Size size,
  required int localCols,
  required int localRows,
  bool focused = true,
}) async {
  await tester.pumpWidget(
    Directionality(
      textDirection: TextDirection.ltr,
      child: Align(
        alignment: Alignment.topLeft,
        child: RepaintBoundary(
          key: const ValueKey('golden'),
          child: CustomPaint(
            size: size,
            painter: TerminalPainter(
              model: model,
              metrics: metrics,
              textCache: RowTextCache(),
              localCols: localCols,
              localRows: localRows,
              focused: focused,
              inset: 4,
            ),
          ),
        ),
      ),
    ),
  );
}

void main() {
  group('RowTextCache', () {
    test('rebuilds text only for rows a frame changed', () {
      const metrics = CellMetrics(
        fontSize: 13,
        cellWidth: 8,
        cellHeight: 16,
        devicePixelRatio: 1,
      );
      final model = _model(_styles());
      final cache = RowTextCache();
      final painter = TerminalPainter(
        model: model,
        metrics: metrics,
        textCache: cache,
        localCols: 32,
        localRows: 7,
        focused: true,
      );
      void paint() {
        final recorder = ui.PictureRecorder();
        painter.paint(Canvas(recorder), const Size(256, 112));
        recorder.endRecording().dispose();
      }

      paint();
      final first = cache.builds;
      expect(first, 14, reason: 'one paragraph per non-blank run');
      paint();
      expect(
        cache.builds,
        first,
        reason: 'a repaint with no new frame builds nothing',
      );

      model.apply(
        Frame(
          cols: 32,
          rows: 7,
          defaultFg: _fg,
          defaultBg: _bg,
          cursorColor: 0,
          rowData: [
            FrameRow(index: 5, runs: [_run(0, 'one'), _run(4, 'two')]),
          ],
        ),
      );
      paint();
      expect(cache.builds, first + 2, reason: 'only row 5 is rebuilt');
    });

    test('a font size change rebuilds everything', () {
      final model = _model(_styles());
      final cache = RowTextCache();
      final row = model.row(0)!;
      const a = CellMetrics(
        fontSize: 13,
        cellWidth: 8,
        cellHeight: 16,
        devicePixelRatio: 1,
      );
      const b = CellMetrics(
        fontSize: 15,
        cellWidth: 9,
        cellHeight: 18,
        devicePixelRatio: 1,
      );
      cache.textFor(0, model.rowGeneration(0), row, a);
      cache.textFor(0, model.rowGeneration(0), row, a);
      expect(cache.builds, 3);
      cache.textFor(0, model.rowGeneration(0), row, b);
      expect(cache.builds, 6);
    });
  });

  group('CellMetrics', () {
    const m = CellMetrics(
      fontSize: 13,
      cellWidth: 8,
      cellHeight: 16,
      devicePixelRatio: 2.5,
    );

    test('the grid is what fits, never zero', () {
      final g = m.geometryFor(const Size(805, 330));
      expect((g.cols, g.rows), (100, 20));
      expect((g.cellWidthPx, g.cellHeightPx), (20, 40));
      expect(m.geometryFor(Size.zero).cols, 1);
    });

    test('a grid that fits is drawn at full size, a larger one shrinks', () {
      expect(m.scaleToFit(100, 20, const Size(800, 320)), 1);
      expect(m.scaleToFit(50, 10, const Size(800, 320)), 1);
      expect(m.scaleToFit(200, 20, const Size(800, 320)), 0.5);
      expect(m.scaleToFit(100, 40, const Size(800, 320)), 0.5);
    });
  });

  group('goldens', () {
    late CellMetrics metrics;

    setUp(() async {
      await loadTerminalFont();
      metrics = CellMetrics.measure(14, 1);
    });

    testWidgets('styles, colors, box drawing and a block cursor', (
      tester,
    ) async {
      final frame = _styles();
      await _pumpPainter(
        tester,
        model: _model(frame),
        metrics: metrics,
        size: Size(
          frame.cols * metrics.cellWidth + 8,
          frame.rows * metrics.cellHeight + 8,
        ),
        localCols: frame.cols,
        localRows: frame.rows,
      );
      await expectLater(
        find.byKey(const ValueKey('golden')),
        matchesGoldenFile('goldens/terminal_styles.png'),
      );
    });

    testWidgets(
      'a larger grid set by another client is shrunk and letterboxed',
      (tester) async {
        final frame = _styles(cols: 48, rows: 7, cursor: CursorStyle.bar);
        const localCols = 32;
        const localRows = 12;
        await _pumpPainter(
          tester,
          model: _model(frame),
          metrics: metrics,
          size: Size(
            localCols * metrics.cellWidth + 8,
            localRows * metrics.cellHeight + 8,
          ),
          localCols: localCols,
          localRows: localRows,
          focused: false,
        );
        await expectLater(
          find.byKey(const ValueKey('golden')),
          matchesGoldenFile('goldens/terminal_letterbox.png'),
        );
      },
    );
  });
}
