import 'dart:ui' as ui;

import 'package:flutter/material.dart';
import 'package:flutter/rendering.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:roost_mobile/terminal/cell_metrics.dart';
import 'package:roost_mobile/terminal/contrast.dart';
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

  group('readability', () {
    const metrics = CellMetrics(
      fontSize: 13,
      cellWidth: 8,
      cellHeight: 16,
      devicePixelRatio: 1,
    );
    const on = TextReadability(minimumContrast: 3.0);

    test('a change of readability rebuilds, an equal one does not', () {
      final model = _model(_styles());
      final cache = RowTextCache();
      TerminalPainter painter(TextReadability r) => TerminalPainter(
        model: model,
        metrics: metrics,
        textCache: cache,
        localCols: 32,
        localRows: 7,
        focused: true,
        readability: r,
      );
      void paint(TerminalPainter p) {
        final recorder = ui.PictureRecorder();
        p.paint(Canvas(recorder), const Size(256, 112));
        recorder.endRecording().dispose();
      }

      final off = painter(const TextReadability());
      paint(off);
      final first = cache.builds;
      final toggled = painter(on);
      expect(toggled.shouldRepaint(off), isTrue);
      expect(painter(on).shouldRepaint(toggled), isFalse);
      expect(
        painter(const TextReadability(minimumContrast: 3.0, onlyOnLight: true))
            .shouldRepaint(toggled),
        isTrue,
      );
      paint(toggled);
      expect(cache.builds, first * 2, reason: 'every row is rebuilt once');
      final three = double.parse('3.0');
      paint(painter(TextReadability(minimumContrast: three)));
      expect(cache.builds, first * 2, reason: 'an equal value is no change');
    });

    Future<ui.Image> render(
      WidgetTester tester,
      Frame frame, {
      bool letterbox = false,
      TextReadability readability = const TextReadability(),
    }) async {
      await tester.pumpWidget(
        Directionality(
          textDirection: TextDirection.ltr,
          child: Align(
            alignment: Alignment.topLeft,
            child: RepaintBoundary(
              key: const ValueKey('shot'),
              child: CustomPaint(
                size: const Size(256, 112),
                painter: TerminalPainter(
                  model: _model(frame),
                  metrics: metrics,
                  textCache: RowTextCache(),
                  localCols: letterbox ? 40 : frame.cols,
                  localRows: frame.rows,
                  focused: true,
                  readability: readability,
                ),
              ),
            ),
          ),
        ),
      );
      final boundary = tester.renderObject<RenderRepaintBoundary>(
        find.byKey(const ValueKey('shot')),
      );
      return (await tester.runAsync(() => boundary.toImage()))!;
    }

    Frame scrolled(int bg) => Frame(
      flags: FrameFlags.full,
      cols: 32,
      rows: 7,
      defaultFg: 0x888888,
      defaultBg: bg,
      cursorColor: 0,
      scrollTotal: 100,
      scrollOffset: 10,
      scrollLen: 7,
      rowData: [],
    );

    Future<List<int>> pixel(
      WidgetTester tester,
      ui.Image image,
      int x,
      int y,
    ) async {
      final data = (await tester.runAsync(image.toByteData))!;
      final i = (y * image.width + x) * 4;
      return [data.getUint8(i), data.getUint8(i + 1), data.getUint8(i + 2)];
    }

    testWidgets('the scrollbar thumb is dark on a light background', (
      tester,
    ) async {
      final light = await render(tester, scrolled(0xEFF1F5));
      final dark = await render(tester, scrolled(0x0F1115));
      // The thumb is 3 px wide, 5 px in from the right edge of the grid.
      final x = 32 * 8 - 4;
      final l = await pixel(tester, light, x, 30);
      final d = await pixel(tester, dark, x, 30);
      expect(l[0], lessThan(0xEF), reason: 'black over light is darker');
      expect(d[0], greaterThan(0x0F), reason: 'white over dark is lighter');
    });

    testWidgets('the letterbox border is dark on a light background', (
      tester,
    ) async {
      Frame small(int bg) => Frame(
        flags: FrameFlags.full,
        cols: 32,
        rows: 7,
        defaultFg: 0x888888,
        defaultBg: bg,
        cursorColor: 0,
        rowData: [],
      );
      final light = await render(tester, small(0xEFF1F5), letterbox: true);
      final dark = await render(tester, small(0x0F1115), letterbox: true);
      // The grid's right edge is a 1 px stroke at x = 256 on a 7 x 16 grid.
      final l = await pixel(tester, light, 255, 50);
      final d = await pixel(tester, dark, 255, 50);
      expect(l[0], lessThan(0xEF));
      expect(d[0], greaterThan(0x0F));
    });

    // The pixel of cell (col, row) that is farthest in contrast from [from],
    // and the pixel at the cell's top-left corner.
    Future<(Color ink, Color corner)> inkIn(
      WidgetTester tester,
      Frame frame,
      int col,
      int row,
      Color from, {
      TextReadability readability = on,
    }) async {
      final image = await render(tester, frame, readability: readability);
      final data = (await tester.runAsync(image.toByteData))!;
      Color at(int x, int y) {
        final i = (y * image.width + x) * 4;
        return Color.fromARGB(
          255,
          data.getUint8(i),
          data.getUint8(i + 1),
          data.getUint8(i + 2),
        );
      }

      var ink = at(col * 8 + 1, row * 16 + 1);
      for (var y = row * 16; y < (row + 1) * 16; y++) {
        for (var x = col * 8; x < (col + 1) * 8; x++) {
          final c = at(x, y);
          if (contrastRatio(c, from) > contrastRatio(ink, from)) ink = c;
        }
      }
      return (ink, at(col * 8, row * 16));
    }

    Frame oneRun(FrameRun run, {int bg = 0xEFF1F5}) => Frame(
      flags: FrameFlags.full,
      cols: 32,
      rows: 7,
      defaultFg: 0x4C4F69,
      defaultBg: bg,
      cursorColor: 0,
      rowData: [
        FrameRow(index: 1, runs: [run]),
      ],
    );

    testWidgets(
      'auto raises text on the light frame it paints, not a dark one',
      (tester) async {
        await loadTerminalFont();
        const auto = TextReadability(minimumContrast: 3.0, onlyOnLight: true);
        const light = Color(0xFFEFF1F5);
        final (ink, _) = await inkIn(
          tester,
          oneRun(_run(0, '█', fg: 0xDDDDDD, bg: 0xEFF1F5)),
          0,
          1,
          light,
          readability: auto,
        );
        expect(contrastRatio(ink, light), greaterThanOrEqualTo(3));

        final dark = oneRun(
          _run(0, '█', fg: 0x202020, bg: 0x0F1115),
          bg: 0x0F1115,
        );
        Future<List<int>> bytes(TextReadability r) async {
          final image = await render(tester, dark, readability: r);
          final data = (await tester.runAsync(image.toByteData))!;
          return data.buffer.asUint8List();
        }

        final off = await bytes(const TextReadability());
        expect(await bytes(auto), off, reason: 'auto on dark draws as off');
        expect(await bytes(on), isNot(off), reason: 'on raises the same run');
      },
    );

    testWidgets('a faint run is blended, then raised to 3:1', (tester) async {
      await loadTerminalFont();
      const bg = Color(0xFFEFF1F5);
      const fg = 0x707070;
      expect(contrastRatio(terminalColor(fg), bg), greaterThan(3));
      final blended = Color.alphaBlend(
        terminalColor(fg).withValues(alpha: 0.55),
        bg,
      );
      expect(contrastRatio(blended, bg), lessThan(3));
      final (ink, _) = await inkIn(
        tester,
        oneRun(_run(0, '█', fg: fg, bg: 0xEFF1F5, attrs: RunAttrs.faint)),
        0,
        1,
        bg,
      );
      expect(ink, isNot(terminalColor(fg)));
      expect(contrastRatio(ink, bg), greaterThanOrEqualTo(3));
    });

    testWidgets('faint text the color of its background stays hidden', (
      tester,
    ) async {
      await loadTerminalFont();
      const bg = Color(0xFFEFF1F5);
      final (ink, _) = await inkIn(
        tester,
        oneRun(_run(0, '█', fg: 0xEFF1F5, bg: 0xEFF1F5, attrs: RunAttrs.faint)),
        0,
        1,
        bg,
      );
      expect(contrastRatio(ink, bg), lessThan(1.05));
    });

    testWidgets('the glyph under a block cursor keeps 3:1 on a light theme', (
      tester,
    ) async {
      await loadTerminalFont();
      const bg = 0xEFF1F5;
      const cursor = 0xDC8A78;
      final frame = Frame(
        flags: FrameFlags.full | FrameFlags.cursorVisible,
        cols: 32,
        rows: 7,
        defaultFg: 0x4C4F69,
        defaultBg: bg,
        cursorColor: cursor,
        cursorCol: 2,
        cursorRow: 1,
        rowData: [
          FrameRow(
            index: 1,
            runs: [
              _run(0, 'abWcd', fg: 0x4C4F69, bg: bg, attrs: RunAttrs.bold),
            ],
          ),
        ],
      );
      final (ink, corner) = await inkIn(
        tester,
        frame,
        2,
        1,
        terminalColor(cursor),
      );
      expect(corner, terminalColor(cursor), reason: 'the block is the cursor');
      expect(
        contrastRatio(ink, terminalColor(cursor)),
        greaterThanOrEqualTo(3),
        reason: 'glyph ink vs the block',
      );
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
